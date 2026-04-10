// handle_table.rs — Radix tree mapping u64 handles to (page_id, slot_index).
// Leaf pages hold ENTRIES_PER_LEAF entries (16 bytes each). Interior pages
// hold child pointers. The tree grows in depth as handles exceed leaf capacity.
// All mutations use copy-on-write and return a new root page ID.

use crate::error::Result;
use crate::page::{self, PageType, PAGE_SIZE, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_ID_NONE};
use crate::page_cache::PageCache;

const ENTRY_SIZE: usize = 16;
pub const ENTRIES_PER_LEAF: usize =
    (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / ENTRY_SIZE; // 510

const CHILD_PTR_SIZE: usize = 8;
const PTRS_PER_INTERIOR: usize =
    (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / CHILD_PTR_SIZE; // 1021

// Page flags byte: distinguishes leaf from interior.
const FLAG_LEAF: u8 = 0x01;
const FLAG_INTERIOR: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleFlags {
    Live,
    Deleted,
    Overflow,
}

impl HandleFlags {
    fn to_u8(self) -> u8 {
        match self {
            HandleFlags::Live => 0x01,
            HandleFlags::Deleted => 0x00,
            HandleFlags::Overflow => 0x02,
        }
    }
    fn from_u8(v: u8) -> HandleFlags {
        match v {
            0x01 => HandleFlags::Live,
            0x02 => HandleFlags::Overflow,
            _ => HandleFlags::Deleted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleEntry {
    pub page_id: u64,
    pub slot_index: u16,
    pub flags: HandleFlags,
}

pub struct HandleTable {
    depth: u32, // 0 = root is a leaf, 1 = one level of interior, etc.
}

impl HandleTable {
    pub fn new() -> HandleTable {
        HandleTable { depth: 0 }
    }

    /// Create a new empty root leaf page. Returns its page ID.
    pub fn create_root(&mut self, cache: &mut PageCache) -> Result<u64> {
        let page_id = cache.new_page()?;
        let buf = cache.get_mut(page_id)?;
        buf.fill(0);
        buf[0] = PageType::HandleTable as u8;
        buf[1] = FLAG_LEAF;
        page::stamp_checksum(buf);
        self.depth = 0;
        Ok(page_id)
    }

    /// Look up a handle. Returns None if the handle doesn't exist or is deleted.
    pub fn lookup(
        &self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
    ) -> Result<Option<HandleEntry>> {
        if root == PAGE_ID_NONE {
            return Ok(None);
        }
        let (leaf_page, index) = self.find_leaf(cache, root, handle)?;
        let buf = cache.get(leaf_page)?;
        let entry = Self::read_entry(buf, index);
        if entry.flags == HandleFlags::Deleted {
            Ok(None)
        } else {
            Ok(Some(entry))
        }
    }

    /// Insert or update a handle entry. Returns the new root page ID (COW).
    pub fn insert(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
        entry: &HandleEntry,
    ) -> Result<u64> {
        let mut current_root = root;
        while handle >= self.capacity() {
            current_root = self.grow(cache, current_root)?;
        }
        self.insert_recursive(cache, current_root, handle, entry, self.depth)
    }

    /// Mark a handle as deleted. Returns the new root page ID (COW).
    pub fn delete(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
    ) -> Result<u64> {
        let deleted_entry = HandleEntry {
            page_id: 0,
            slot_index: 0,
            flags: HandleFlags::Deleted,
        };
        self.insert(cache, root, handle, &deleted_entry)
    }

    /// Iterate over all live entries. Returns (handle, HandleEntry) pairs.
    pub fn iter_live(
        &self,
        cache: &mut PageCache,
        root: u64,
    ) -> Result<Vec<(u64, HandleEntry)>> {
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        self.iter_recursive(cache, root, 0, self.depth, &mut result)?;
        Ok(result)
    }

    /// Set the tree depth (used when loading from an existing file).
    pub fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Maximum handle value the tree can currently hold.
    fn capacity(&self) -> u64 {
        let mut cap = ENTRIES_PER_LEAF as u64;
        for _ in 0..self.depth {
            cap *= PTRS_PER_INTERIOR as u64;
        }
        cap
    }

    /// Add a new interior root above the current root, increasing depth by 1.
    fn grow(&mut self, cache: &mut PageCache, old_root: u64) -> Result<u64> {
        let new_root = cache.new_page()?;
        let buf = cache.get_mut(new_root)?;
        buf.fill(0);
        buf[0] = PageType::HandleTable as u8;
        buf[1] = FLAG_INTERIOR;
        buf[DATA_PAGE_HEADER_SIZE..DATA_PAGE_HEADER_SIZE + 8]
            .copy_from_slice(&old_root.to_le_bytes());
        page::stamp_checksum(buf);
        self.depth += 1;
        Ok(new_root)
    }

    fn insert_recursive(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        handle: u64,
        entry: &HandleEntry,
        level: u32,
    ) -> Result<u64> {
        // COW: copy the page.
        let new_page = cache.new_page()?;
        {
            let old_buf = cache.get(page_id)?;
            let old_data: [u8; PAGE_SIZE] = *old_buf;
            let new_buf = cache.get_mut(new_page)?;
            new_buf.copy_from_slice(&old_data);
        }

        if level == 0 {
            let index = (handle % ENTRIES_PER_LEAF as u64) as usize;
            let buf = cache.get_mut(new_page)?;
            Self::write_entry(buf, index, entry);
            page::stamp_checksum(buf);
            Ok(new_page)
        } else {
            let child_span = self.span_at_level(level);
            let child_idx = (handle / child_span) as usize;

            let child_page = {
                let buf = cache.get(new_page)?;
                let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
                u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
            };

            let actual_child = if child_page == 0 {
                if level == 1 {
                    let leaf = cache.new_page()?;
                    let buf = cache.get_mut(leaf)?;
                    buf.fill(0);
                    buf[0] = PageType::HandleTable as u8;
                    buf[1] = FLAG_LEAF;
                    page::stamp_checksum(buf);
                    leaf
                } else {
                    let interior = cache.new_page()?;
                    let buf = cache.get_mut(interior)?;
                    buf.fill(0);
                    buf[0] = PageType::HandleTable as u8;
                    buf[1] = FLAG_INTERIOR;
                    page::stamp_checksum(buf);
                    interior
                }
            } else {
                child_page
            };

            let new_child = self.insert_recursive(
                cache,
                actual_child,
                handle % child_span,
                entry,
                level - 1,
            )?;

            let buf = cache.get_mut(new_page)?;
            let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
            buf[offset..offset + 8].copy_from_slice(&new_child.to_le_bytes());
            page::stamp_checksum(buf);

            Ok(new_page)
        }
    }

    fn find_leaf(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        handle: u64,
    ) -> Result<(u64, usize)> {
        if self.depth == 0 {
            let index = (handle % ENTRIES_PER_LEAF as u64) as usize;
            return Ok((page_id, index));
        }

        let mut current = page_id;
        let mut remaining = handle;

        for level in (1..=self.depth).rev() {
            let child_span = self.span_at_level(level);
            let child_idx = (remaining / child_span) as usize;
            remaining %= child_span;

            let buf = cache.get(current)?;
            let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
            let child = u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
            if child == 0 {
                return Ok((page_id, 0)); // Will read as Deleted.
            }
            current = child;
        }

        let index = (remaining % ENTRIES_PER_LEAF as u64) as usize;
        Ok((current, index))
    }

    fn span_at_level(&self, level: u32) -> u64 {
        let mut span = ENTRIES_PER_LEAF as u64;
        for _ in 1..level {
            span *= PTRS_PER_INTERIOR as u64;
        }
        span
    }

    fn iter_recursive(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        base_handle: u64,
        level: u32,
        result: &mut Vec<(u64, HandleEntry)>,
    ) -> Result<()> {
        if level == 0 {
            let buf = cache.get(page_id)?;
            for i in 0..ENTRIES_PER_LEAF {
                let entry = Self::read_entry(buf, i);
                if entry.flags != HandleFlags::Deleted {
                    result.push((base_handle + i as u64, entry));
                }
            }
        } else {
            let child_span = self.span_at_level(level);
            // Read child pointers into a local vec to avoid borrow issues.
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page_id)?;
                (0..PTRS_PER_INTERIOR)
                    .map(|i| {
                        let offset = DATA_PAGE_HEADER_SIZE + i * CHILD_PTR_SIZE;
                        let child =
                            u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
                        (i, child)
                    })
                    .filter(|(_, child)| *child != 0)
                    .collect()
            };
            for (i, child) in children {
                self.iter_recursive(
                    cache,
                    child,
                    base_handle + (i as u64) * child_span,
                    level - 1,
                    result,
                )?;
            }
        }
        Ok(())
    }

    fn read_entry(buf: &[u8; PAGE_SIZE], index: usize) -> HandleEntry {
        let base = DATA_PAGE_HEADER_SIZE + index * ENTRY_SIZE;
        HandleEntry {
            page_id: u64::from_le_bytes(buf[base..base + 8].try_into().unwrap()),
            slot_index: u16::from_le_bytes(buf[base + 8..base + 10].try_into().unwrap()),
            flags: HandleFlags::from_u8(buf[base + 10]),
        }
    }

    fn write_entry(buf: &mut [u8; PAGE_SIZE], index: usize, entry: &HandleEntry) {
        let base = DATA_PAGE_HEADER_SIZE + index * ENTRY_SIZE;
        buf[base..base + 8].copy_from_slice(&entry.page_id.to_le_bytes());
        buf[base + 8..base + 10].copy_from_slice(&entry.slot_index.to_le_bytes());
        buf[base + 10] = entry.flags.to_u8();
        buf[base + 11..base + 16].fill(0); // reserved
    }
}
