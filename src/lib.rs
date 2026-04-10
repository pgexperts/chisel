// lib.rs — Chisel: a transactional slot-based storage engine.
// This module provides the public API. It wraps TransactionManager
// and exposes a clean interface.

pub mod data_page;
pub mod defrag;
pub mod error;
pub mod freemap;
pub mod handle_table;
pub mod overflow;
pub mod page;
pub mod page_cache;
pub mod page_io;
pub mod stats;
pub mod superblock;
pub mod transaction;

pub use error::{ChiselError, Result};

use std::path::Path;

use page_cache::PageCache;
use page_io::PageIo;
use transaction::TransactionManager;

#[derive(Debug, Clone)]
pub struct Options {
    pub cache_size: usize,
    pub create_if_missing: bool,
    pub read_only: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            cache_size: 1024,
            create_if_missing: true,
            read_only: false,
        }
    }
}

pub struct Chisel {
    txm: TransactionManager,
}

impl Chisel {
    /// Open or create a Chisel database.
    pub fn open(path: &Path, options: Options) -> Result<Chisel> {
        let file_exists = path.exists()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);

        if !file_exists && !options.create_if_missing {
            return Err(ChiselError::FileNotFound);
        }

        let io = PageIo::open(path, options.read_only)?;
        let cache = PageCache::new(io, options.cache_size);

        let txm = if file_exists {
            TransactionManager::open_existing(cache)?
        } else {
            TransactionManager::create_new(cache)?
        };

        Ok(Chisel { txm })
    }

    pub fn close(self) -> Result<()> {
        drop(self);
        Ok(())
    }

    pub fn begin(&mut self) -> Result<()> {
        self.txm.begin()
    }

    pub fn commit(&mut self) -> Result<()> {
        self.txm.commit()
    }

    pub fn rollback(&mut self) -> Result<()> {
        self.txm.rollback()
    }

    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        self.txm.savepoint(name)
    }

    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        self.txm.rollback_to(name)
    }

    pub fn release(&mut self, name: &str) -> Result<()> {
        self.txm.release(name)
    }

    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        self.txm.allocate(value)
    }

    pub fn read(&mut self, handle: u64) -> Result<Vec<u8>> {
        self.txm.read(handle)
    }

    pub fn update(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        self.txm.update(handle, value)
    }

    pub fn delete(&mut self, handle: u64) -> Result<()> {
        self.txm.delete(handle)
    }

    pub fn handles(&mut self) -> Result<Vec<u64>> {
        self.txm.handles()
    }

    pub fn stats(&mut self) -> Result<stats::Stats> {
        let handles = self.txm.handles()?;
        let page_count = self.txm.cache_mut().file_page_count()?;
        Ok(stats::Stats {
            handle_count: handles.len() as u64,
            total_pages: page_count,
            file_size_bytes: page_count * page::PAGE_SIZE as u64,
        })
    }

    pub fn defrag(&mut self, options: defrag::DefragOptions) -> Result<defrag::DefragStats> {
        defrag::defrag(&mut self.txm, &options)
    }
}
