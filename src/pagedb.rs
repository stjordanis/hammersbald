//
// Copyright 2018 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Three paged files that together implement the db
//!
use page::{Page, PAGE_SIZE};
use types::Offset;
use logfile::LogFile;
use keyfile::KeyFile;
use datafile::{DataFile, DataEntry};
use error::BCSError;

use std::sync::{Mutex,Arc};
use std::io::{Read,Write,Seek};

pub trait PageDBFactory {
    fn new_pagedb (name: &str) -> Result<PageDB, BCSError>;
}

pub trait RW : Read + Write + Seek + Send {
    fn len (&mut self) -> Result<usize, BCSError>;
    fn truncate(&mut self, new_len: usize) -> Result<(), BCSError>;
    fn sync (&self) -> Result<(), BCSError>;
}

pub trait DBFile : PageFile {
    fn flush(&mut self) -> Result<(), BCSError>;
    fn sync (&mut self) -> Result<(), BCSError>;
    fn truncate(&mut self, offset: Offset) -> Result<(), BCSError>;
    fn len(&mut self) -> Result<Offset, BCSError>;
}

pub trait PageFile {
    fn read_page (&self, offset: Offset) -> Result<Arc<Page>, BCSError>;
}


/// The database page layer
pub struct PageDB {
    table: KeyFile,
    data: DataFile,
    log: Arc<Mutex<LogFile>>
}

impl PageDB {
    pub fn new (mut table: KeyFile, mut data: DataFile) -> Result<PageDB, BCSError> {
        let log = table.log_file();
        PageDB::check(&mut table, &[0xBC, 0xDB])?;
        PageDB::check(&mut data, &[0xBC, 0xDA])?;
        PageDB::check_log(log.clone(), &[0xBC, 0x00])?;
        let mut pagedb = PageDB {table, data, log};
        pagedb.recover()?;
        pagedb.batch()?;
        Ok(pagedb)
    }

    fn check_log(log: Arc<Mutex<LogFile>>, magic: &[u8]) -> Result<(), BCSError> {
        let mut file = log.lock().unwrap();
        if file.len()?.as_usize() > 0 {
            let offset = Offset::new(0)?;
            let first = file.read_page(offset)?;
            let mut m = [0u8;2];
            first.read(0, &mut m)?;
            if m != magic {
                return Err(BCSError::BadMagic);
            }
        }
        Ok(())
    }

    fn check(file: &mut DBFile, magic: &[u8]) -> Result<(), BCSError> {
        if file.len()?.as_usize() > 0 {
            let offset = Offset::new(0)?;
            let mut m = [0u8;2];
            let first = file.read_page(offset)?;
            first.read(0, &mut m)?;
            if m != magic {
                return Err(BCSError::BadMagic);
            }
        }
        Ok(())
    }

    fn recover(&mut self) -> Result<(), BCSError> {
        let mut log = self.log.lock().unwrap();
        if log.len()?.as_usize() > 0 {
            {
                let mut log_pages = log.page_iter();
                if let Some(first) = log_pages.next() {
                    let mut size = [0u8; 6];

                    first.read(2, &mut size)?;
                    let data_len = Offset::from_slice(&size)?;
                    self.data.truncate(data_len)?;

                    first.read(8, &mut size)?;
                    let table_len = Offset::from_slice(&size)?;
                    self.table.truncate(table_len)?;

                    for page in log_pages {
                        if page.offset.as_usize() < table_len.as_usize() {
                            self.table.write_page(page);
                        }
                    }
                }
            }
            log.truncate(Offset::new(0)?)?;
            log.sync()?;
        }
        Ok(())
    }

    pub fn batch (&mut self)  -> Result<(), BCSError> {
        self.data.flush()?;
        self.data.sync()?;
        self.table.flush()?;
        self.table.sync()?;
        let data_len = self.data.len()?;
        let table_len = self.table.len()?;

        let mut log = self.log.lock().unwrap();
        log.truncate(Offset::new(0).unwrap())?;
        log.reset();

        let mut first = Page::new(Offset::new(0).unwrap());
        first.write(0, &[0xBC, 0x00]).unwrap();
        let mut size = [0u8; 6];
        data_len.serialize(&mut size);
        first.write(2, &size).unwrap();
        table_len.serialize(&mut size);
        first.write(8, &size).unwrap();


        log.append_page(Arc::new(first))?;
        log.flush()?;
        log.sync()?;

        Ok(())
    }

    pub fn shutdown (&mut self) {
        self.data.shutdown();
        self.table.shutdown();
    }

    pub fn write_table_page(&mut self, page: Page) -> Result<(), BCSError> {
        let br = Arc::new(page);
        self.table.write_page(br);
        Ok(())
    }

    pub fn read_table_page (&self, offset: Offset) -> Result<Arc<Page>, BCSError> {
        self.table.read_page(offset)
    }

    pub fn read_data_page (&self, offset: Offset) -> Result<Arc<Page>, BCSError> {
        self.data.read_page(offset)
    }

    pub fn append_data_entry (&mut self, entry: DataEntry) -> Result<Offset, BCSError> {
        self.data.append(entry)
    }
}

pub struct PageIterator<'file> {
    pagenumber: usize,
    file: &'file PageFile
}

impl<'file> PageIterator<'file> {
    pub fn new (file: &'file PageFile, pagenumber: usize) -> PageIterator {
        PageIterator{pagenumber, file}
    }
}

impl<'file> Iterator for PageIterator<'file> {
    type Item = Arc<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pagenumber < (1 << 47) / PAGE_SIZE {
            let offset = Offset::new(self.pagenumber* PAGE_SIZE).unwrap();
            if let Ok(page) = self.file.read_page(offset) {
                self.pagenumber += 1;
                return Some(page);
            }
        }
        None
    }
}

#[cfg(test)]
mod test {
    extern crate hex;

    use inmemory::InMemory;

    use super::*;
    #[test]
    fn test () {
        let mut pagedb = InMemory::new_pagedb("").unwrap();
        pagedb.shutdown();
    }
}