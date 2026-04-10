// transaction.rs — Transaction lifecycle, savepoints, commit protocol, and data operations.
// This is the orchestration layer that ties together the handle table, data pages,
// overflow pages, freemap, superblock, and page cache into a coherent transactional API.

use crate::data_page::DataPage;
use crate::error::{ChiselError, Result};
use crate::handle_table::{HandleEntry, HandleFlags, HandleTable};
use crate::overflow::Overflow;
use crate::page::{self, PAGE_ID_NONE, PAGE_SIZE};
use crate::page_cache::PageCache;
use crate::superblock::Superblock;

const MAX_INLINE_VALUE: usize = 8162;

#[derive(Debug, Clone)]
struct Roots {
    handle_table_page: u64,
    freemap_page: u64,
    next_handle: u64,
    total_pages: u64,
}

#[derive(Debug)]
struct Savepoint {
    name: String,
    roots: Roots,
    dirty_pages: Vec<u64>,
    freed_pages: Vec<u64>,
}

pub struct TransactionManager {
    cache: PageCache,
    committed_roots: Roots,
    current_roots: Roots,
    handle_table: HandleTable,
    txn_counter: u64,
    active_txn: bool,
    savepoints: Vec<Savepoint>,
    txn_dirty_pages: Vec<u64>,
    txn_freed_pages: Vec<u64>,
}

impl TransactionManager {
    /// Create a new database (initialize superblocks).
    pub fn create_new(mut cache: PageCache) -> Result<TransactionManager> {
        let sb = Superblock::new_empty();
        let buf_a = sb.serialize();
        let buf_b = [0u8; PAGE_SIZE]; // Invalid superblock B.

        cache.io_mut().write_page(0, &buf_a)?;
        cache.io_mut().write_page(1, &buf_b)?;
        cache.io_mut().fsync()?;
        cache.set_next_page_id(2);

        let roots = Roots {
            handle_table_page: PAGE_ID_NONE,
            freemap_page: PAGE_ID_NONE,
            next_handle: 0,
            total_pages: 2,
        };

        Ok(TransactionManager {
            cache,
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: HandleTable::new(),
            txn_counter: sb.txn_counter,
            active_txn: false,
            savepoints: Vec::new(),
            txn_dirty_pages: Vec::new(),
            txn_freed_pages: Vec::new(),
        })
    }

    /// Open an existing database from file.
    pub fn open_existing(mut cache: PageCache) -> Result<TransactionManager> {
        let buf_a = cache.io_mut().read_page(0)?;
        let buf_b = cache.io_mut().read_page(1)?;
        let sb = Superblock::select(&[buf_a, buf_b])
            .ok_or(ChiselError::CorruptSuperblock)?;

        let page_count = cache.io_mut().page_count()?;
        if page_count < sb.total_pages {
            return Err(ChiselError::FileSizeMismatch {
                expected: sb.total_pages * PAGE_SIZE as u64,
                actual: page_count * PAGE_SIZE as u64,
            });
        }
        cache.set_next_page_id(sb.total_pages);

        let roots = Roots {
            handle_table_page: sb.root_handle_table_page,
            freemap_page: sb.root_freemap_page,
            next_handle: sb.next_handle,
            total_pages: sb.total_pages,
        };

        let mut ht = HandleTable::new();
        if sb.root_handle_table_page != PAGE_ID_NONE {
            // Determine depth by walking down the left spine.
            let root_buf = cache.get(sb.root_handle_table_page)?;
            if root_buf[1] == 0x02 {
                // Interior node — walk down to find depth.
                let mut depth = 0u32;
                let mut current = sb.root_handle_table_page;
                loop {
                    let buf = cache.get(current)?;
                    if buf[1] != 0x02 {
                        break;
                    }
                    depth += 1;
                    let child_offset = page::DATA_PAGE_HEADER_SIZE;
                    let child = u64::from_le_bytes(
                        buf[child_offset..child_offset + 8].try_into().unwrap(),
                    );
                    if child == 0 {
                        break;
                    }
                    current = child;
                }
                ht.set_depth(depth);
            }
        }

        Ok(TransactionManager {
            cache,
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: ht,
            txn_counter: sb.txn_counter,
            active_txn: false,
            savepoints: Vec::new(),
            txn_dirty_pages: Vec::new(),
            txn_freed_pages: Vec::new(),
        })
    }

    pub fn begin(&mut self) -> Result<()> {
        if self.active_txn {
            return Err(ChiselError::TransactionAlreadyActive);
        }
        self.current_roots = self.committed_roots.clone();
        self.active_txn = true;
        self.savepoints.clear();
        self.txn_dirty_pages.clear();
        self.txn_freed_pages.clear();
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // Phase 1: Flush all dirty pages.
        self.cache.flush()?;

        // Phase 2: Write new superblock.
        self.txn_counter += 1;
        let total_pages = self.cache.file_page_count()?;
        let sb = Superblock {
            magic: page::MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: self.txn_counter,
            root_handle_table_page: self.current_roots.handle_table_page,
            root_freemap_page: self.current_roots.freemap_page,
            total_pages,
            next_handle: self.current_roots.next_handle,
            page_size: PAGE_SIZE as u32,
        };
        let buf = sb.serialize();
        // Write to the inactive superblock (alternate between 0 and 1).
        let inactive = if self.txn_counter % 2 == 0 { 0 } else { 1 };
        self.cache.io_mut().write_page(inactive, &buf)?;
        self.cache.io_mut().fsync()?;

        // Phase 3: Update committed roots.
        self.committed_roots = self.current_roots.clone();
        self.committed_roots.total_pages = total_pages;
        self.active_txn = false;
        self.savepoints.clear();
        self.txn_dirty_pages.clear();
        self.txn_freed_pages.clear();

        Ok(())
    }

    pub fn rollback(&mut self) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        for &page_id in &self.txn_dirty_pages {
            self.cache.discard(page_id);
        }
        for sp in &self.savepoints {
            for &page_id in &sp.dirty_pages {
                self.cache.discard(page_id);
            }
        }

        self.current_roots = self.committed_roots.clone();
        self.active_txn = false;
        self.savepoints.clear();
        self.txn_dirty_pages.clear();
        self.txn_freed_pages.clear();
        Ok(())
    }

    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if self.savepoints.iter().any(|sp| sp.name == name) {
            return Err(ChiselError::DuplicateSavepoint(name.to_string()));
        }
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            roots: self.current_roots.clone(),
            dirty_pages: std::mem::take(&mut self.txn_dirty_pages),
            freed_pages: std::mem::take(&mut self.txn_freed_pages),
        });
        Ok(())
    }

    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let idx = self
            .savepoints
            .iter()
            .position(|sp| sp.name == name)
            .ok_or_else(|| ChiselError::SavepointNotFound(name.to_string()))?;

        let mut pages_to_discard = std::mem::take(&mut self.txn_dirty_pages);
        for sp in self.savepoints[idx + 1..].iter() {
            pages_to_discard.extend_from_slice(&sp.dirty_pages);
        }

        for &page_id in &pages_to_discard {
            self.cache.discard(page_id);
        }

        self.current_roots = self.savepoints[idx].roots.clone();
        self.savepoints.truncate(idx + 1);
        self.txn_dirty_pages.clear();
        self.txn_freed_pages.clear();

        Ok(())
    }

    pub fn release(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let idx = self
            .savepoints
            .iter()
            .position(|sp| sp.name == name)
            .ok_or_else(|| ChiselError::SavepointNotFound(name.to_string()))?;

        let mut merged_dirty = Vec::new();
        let mut merged_freed = Vec::new();

        for sp in self.savepoints[idx..].iter() {
            merged_dirty.extend_from_slice(&sp.dirty_pages);
            merged_freed.extend_from_slice(&sp.freed_pages);
        }
        merged_dirty.append(&mut self.txn_dirty_pages);
        merged_freed.append(&mut self.txn_freed_pages);

        self.savepoints.truncate(idx);
        self.txn_dirty_pages = merged_dirty;
        self.txn_freed_pages = merged_freed;

        Ok(())
    }

    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        let handle = self.current_roots.next_handle;
        self.current_roots.next_handle += 1;

        if value.len() > MAX_INLINE_VALUE {
            let first_page = Overflow::write(&mut self.cache, value)?;
            let entry = HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
            };
            self.ensure_handle_table()?;
            let new_root = self.handle_table.insert(
                &mut self.cache,
                self.current_roots.handle_table_page,
                handle,
                &entry,
            )?;
            self.txn_dirty_pages.push(new_root);
            self.current_roots.handle_table_page = new_root;
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            let entry = HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
            };
            self.ensure_handle_table()?;
            let new_root = self.handle_table.insert(
                &mut self.cache,
                self.current_roots.handle_table_page,
                handle,
                &entry,
            )?;
            self.txn_dirty_pages.push(new_root);
            self.current_roots.handle_table_page = new_root;
        }

        Ok(handle)
    }

    pub fn read(&mut self, handle: u64) -> Result<Vec<u8>> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };

        if root == PAGE_ID_NONE {
            return Err(ChiselError::InvalidHandle(handle));
        }

        let entry = self
            .handle_table
            .lookup(&mut self.cache, root, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;

        match entry.flags {
            HandleFlags::Live => {
                let buf = self.cache.get(entry.page_id)?;
                DataPage::read(buf, entry.slot_index)
                    .map(|data| data.to_vec())
                    .ok_or(ChiselError::InvalidHandle(handle))
            }
            HandleFlags::Overflow => Overflow::read(&mut self.cache, entry.page_id),
            HandleFlags::Deleted => Err(ChiselError::InvalidHandle(handle)),
        }
    }

    pub fn update(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        let entry = self
            .handle_table
            .lookup(&mut self.cache, self.current_roots.handle_table_page, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;

        if entry.flags == HandleFlags::Overflow {
            let freed = Overflow::delete(&mut self.cache, entry.page_id)?;
            self.txn_freed_pages.extend_from_slice(&freed);
        }

        if value.len() > MAX_INLINE_VALUE {
            let first_page = Overflow::write(&mut self.cache, value)?;
            let new_entry = HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
            };
            let new_root = self.handle_table.insert(
                &mut self.cache,
                self.current_roots.handle_table_page,
                handle,
                &new_entry,
            )?;
            self.txn_dirty_pages.push(new_root);
            self.current_roots.handle_table_page = new_root;
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            let new_entry = HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
            };
            let new_root = self.handle_table.insert(
                &mut self.cache,
                self.current_roots.handle_table_page,
                handle,
                &new_entry,
            )?;
            self.txn_dirty_pages.push(new_root);
            self.current_roots.handle_table_page = new_root;
        }

        Ok(())
    }

    pub fn delete(&mut self, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        let entry = self
            .handle_table
            .lookup(&mut self.cache, self.current_roots.handle_table_page, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;

        if entry.flags == HandleFlags::Overflow {
            let freed = Overflow::delete(&mut self.cache, entry.page_id)?;
            self.txn_freed_pages.extend_from_slice(&freed);
        }

        let new_root = self.handle_table.delete(
            &mut self.cache,
            self.current_roots.handle_table_page,
            handle,
        )?;
        self.txn_dirty_pages.push(new_root);
        self.current_roots.handle_table_page = new_root;

        Ok(())
    }

    /// Iterate over all live handles.
    pub fn handles(&mut self) -> Result<Vec<u64>> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let entries = self.handle_table.iter_live(&mut self.cache, root)?;
        Ok(entries.into_iter().map(|(h, _)| h).collect())
    }

    pub fn cache_mut(&mut self) -> &mut PageCache {
        &mut self.cache
    }

    pub fn current_roots(&self) -> (u64, u64, u64) {
        (
            self.current_roots.handle_table_page,
            self.current_roots.freemap_page,
            self.current_roots.next_handle,
        )
    }

    pub fn is_active(&self) -> bool {
        self.active_txn
    }

    // --- Private helpers ---

    fn ensure_handle_table(&mut self) -> Result<()> {
        if self.current_roots.handle_table_page == PAGE_ID_NONE {
            let root = self.handle_table.create_root(&mut self.cache)?;
            self.txn_dirty_pages.push(root);
            self.current_roots.handle_table_page = root;
        }
        Ok(())
    }

    fn insert_into_data_page(&mut self, value: &[u8]) -> Result<(u64, u16)> {
        let page_id = self.cache.new_page()?;
        self.txn_dirty_pages.push(page_id);
        let buf = self.cache.get_mut(page_id)?;
        DataPage::init_page(buf);
        let slot = DataPage::insert(buf, value).expect("value fits in empty page");
        page::stamp_checksum(buf);
        Ok((page_id, slot))
    }
}
