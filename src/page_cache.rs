// page_cache.rs — LRU page cache with dirty tracking and checksum validation.
// All page I/O flows through this cache. Pages read from disk are checksum-verified
// before entering the cache. Dirty pages are never evicted — they are flushed
// to disk during commit.

use std::collections::{HashMap, VecDeque};

use crate::error::{ChiselError, Result};
use crate::page::{self, PAGE_SIZE};
use crate::page_io::PageIo;

struct CacheEntry {
    buf: Box<[u8; PAGE_SIZE]>,
    dirty: bool,
}

pub struct PageCache {
    io: PageIo,
    entries: HashMap<u64, CacheEntry>,
    lru: VecDeque<u64>,
    max_pages: usize,
    next_page_id: u64,
}

impl PageCache {
    pub fn new(mut io: PageIo, max_pages: usize) -> PageCache {
        let next_page_id = io.page_count().unwrap_or(0);
        PageCache {
            io,
            entries: HashMap::new(),
            lru: VecDeque::new(),
            max_pages,
            next_page_id,
        }
    }

    /// Read a page (cache hit or load from disk with checksum validation).
    pub fn get(&mut self, page_id: u64) -> Result<&[u8; PAGE_SIZE]> {
        if !self.entries.contains_key(&page_id) {
            self.load_page(page_id)?;
        }
        self.touch_lru(page_id);
        Ok(&self.entries.get(&page_id).unwrap().buf)
    }

    /// Get a mutable reference to a page, marking it dirty.
    pub fn get_mut(&mut self, page_id: u64) -> Result<&mut [u8; PAGE_SIZE]> {
        if !self.entries.contains_key(&page_id) {
            self.load_page(page_id)?;
        }
        self.touch_lru(page_id);
        let entry = self.entries.get_mut(&page_id).unwrap();
        entry.dirty = true;
        Ok(&mut entry.buf)
    }

    /// Allocate a new zeroed page, mark it dirty, return its page_id.
    pub fn new_page(&mut self) -> Result<u64> {
        let page_id = self.next_page_id;
        self.next_page_id += 1;
        let entry = CacheEntry {
            buf: Box::new([0u8; PAGE_SIZE]),
            dirty: true,
        };
        self.entries.insert(page_id, entry);
        self.lru.push_front(page_id);
        self.maybe_evict()?;
        Ok(page_id)
    }

    /// Write all dirty pages to disk and fsync.
    pub fn flush(&mut self) -> Result<()> {
        let dirty_ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&id, _)| id)
            .collect();
        for page_id in dirty_ids {
            let entry = self.entries.get_mut(&page_id).unwrap();
            self.io.write_page(page_id, &entry.buf)?;
            entry.dirty = false;
        }
        self.io.fsync()?;
        Ok(())
    }

    /// Discard a page from the cache (used during rollback).
    pub fn discard(&mut self, page_id: u64) {
        self.entries.remove(&page_id);
        self.lru.retain(|&id| id != page_id);
    }

    /// Return the number of whole pages the underlying file can hold.
    pub fn file_page_count(&mut self) -> Result<u64> {
        self.io.page_count()
    }

    /// Truncate the file to `n` pages.
    pub fn truncate(&mut self, n: u64) -> Result<()> {
        let to_remove: Vec<u64> = self
            .entries
            .keys()
            .filter(|&&id| id >= n)
            .copied()
            .collect();
        for id in to_remove {
            self.entries.remove(&id);
            self.lru.retain(|&lid| lid != id);
        }
        self.io.set_page_count(n)?;
        if self.next_page_id > n {
            self.next_page_id = n;
        }
        Ok(())
    }

    /// Expose the PageIo for direct superblock I/O during commit.
    pub fn io_mut(&mut self) -> &mut PageIo {
        &mut self.io
    }

    /// Set the next page ID (used when loading from an existing file).
    pub fn set_next_page_id(&mut self, id: u64) {
        self.next_page_id = id;
    }

    /// Check if a page is dirty in the cache.
    pub fn is_dirty(&self, page_id: u64) -> bool {
        self.entries.get(&page_id).is_some_and(|e| e.dirty)
    }

    fn load_page(&mut self, page_id: u64) -> Result<()> {
        self.maybe_evict()?;
        let buf = self.io.read_page(page_id)?;
        if !page::verify_checksum(&buf) {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        self.entries.insert(
            page_id,
            CacheEntry {
                buf: Box::new(buf),
                dirty: false,
            },
        );
        self.lru.push_front(page_id);
        Ok(())
    }

    fn touch_lru(&mut self, page_id: u64) {
        self.lru.retain(|&id| id != page_id);
        self.lru.push_front(page_id);
    }

    fn maybe_evict(&mut self) -> Result<()> {
        while self.entries.len() > self.max_pages {
            let victim = self
                .lru
                .iter()
                .rev()
                .find(|&&id| !self.entries.get(&id).is_none_or(|e| e.dirty))
                .copied();
            match victim {
                Some(id) => {
                    self.entries.remove(&id);
                    self.lru.retain(|&lid| lid != id);
                }
                None => break, // All pages are dirty; can't evict.
            }
        }
        Ok(())
    }
}
