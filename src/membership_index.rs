//! Membership index for chunk tags: maps a `u32` tag to the set of handles that
//! carry it. Built from one generic copy-on-write radix (`RadixU64`: u64 key ->
//! u64 value, 0 = absent) used twice — an outer tree keyed by tag whose value
//! bit-packs `(inner_depth:6 | inner_root:58)`, and per-tag inner trees keyed by
//! handle storing `1` for "present". See docs/specs/2026-06-02-chunk-tags-design.md.
//!
//! Layer dependency: page + page_cache only (strictly below transaction.rs).
//! Like the handle table, this module returns the new root page id after a COW
//! mutation; all page dirtiness lives in `PageCache`, flushed at commit.

// Staged ahead of its consumer: `RadixU64` is the reusable radix that the
// two-level membership index (next task) composes; nothing in production wires
// it in yet, so the whole module reads as dead code to the non-test lib build.
// The module-level allow keeps `cargo clippy -- -D warnings` green until the
// index lands — it is the staged-component analogue of the per-method
// `#[allow(dead_code)]` on `new()` below, not a license to leave anything
// genuinely unused once the consumer exists.
#![allow(dead_code)]

use crate::error::Result;
use crate::page::{
    self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_ID_NONE, PAGE_SIZE,
};
use crate::page_cache::PageCache;

// Leaf values and interior child pointers are both 8-byte little-endian u64s,
// so one constant is the fan-out at every level. 1021 = (8184 - 16) / 8.
const SLOT_SIZE: usize = 8;
const SLOTS_PER_PAGE: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / SLOT_SIZE; // 1021

fn read_slot(buf: &[u8; PAGE_SIZE], index: usize) -> u64 {
    let off = DATA_PAGE_HEADER_SIZE + index * SLOT_SIZE;
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_slot(buf: &mut [u8; PAGE_SIZE], index: usize, value: u64) {
    let off = DATA_PAGE_HEADER_SIZE + index * SLOT_SIZE;
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// Initialize a fresh zeroed radix page of the given type and stamp it.
fn init_page(cache: &mut PageCache, page_type: PageType) -> Result<u64> {
    let id = cache.new_page()?;
    debug_assert_ne!(id, 0, "membership-index pages must not use page id 0");
    let buf = cache.get_mut(id)?;
    buf.fill(0);
    buf[0] = page_type as u8;
    buf[1] = page::PAGE_FORMAT_VERSION_CURRENT; // version at byte 1 (no FLAG byte)
    page::stamp_checksum(buf);
    Ok(id)
}

/// A copy-on-write radix tree: `u64` key -> `u64` value, where `0` means absent
/// (a zeroed leaf reads as all-absent, mirroring the handle table's tombstone
/// trick). The caller owns `depth` and the root page id.
pub(crate) struct RadixU64 {
    pub depth: u32,
}

impl RadixU64 {
    #[allow(dead_code)] // used by tests; production builds RadixU64 { depth } directly
    pub fn new() -> RadixU64 {
        RadixU64 { depth: 0 }
    }

    /// Create a new empty root leaf. Returns its page id.
    pub fn create_root(&mut self, cache: &mut PageCache) -> Result<u64> {
        self.depth = 0;
        init_page(cache, PageType::MembershipLeaf)
    }

    fn capacity(&self) -> u64 {
        let mut cap = SLOTS_PER_PAGE as u64;
        for _ in 0..self.depth {
            cap *= SLOTS_PER_PAGE as u64;
        }
        cap
    }

    fn span_at_level(&self, level: u32) -> u64 {
        let mut span = SLOTS_PER_PAGE as u64;
        for _ in 1..level {
            span *= SLOTS_PER_PAGE as u64;
        }
        span
    }

    fn find_leaf(
        &self,
        cache: &mut PageCache,
        root: u64,
        key: u64,
    ) -> Result<Option<(u64, usize)>> {
        if self.depth > 0 && key >= self.capacity() {
            return Ok(None);
        }
        if self.depth == 0 {
            return Ok(Some((root, (key % SLOTS_PER_PAGE as u64) as usize)));
        }
        let mut current = root;
        let mut remaining = key;
        for level in (1..=self.depth).rev() {
            let span = self.span_at_level(level);
            let child_idx = (remaining / span) as usize;
            remaining %= span;
            let child = read_slot(cache.get(current)?, child_idx);
            if child == 0 {
                return Ok(None);
            }
            current = child;
        }
        Ok(Some((
            current,
            (remaining % SLOTS_PER_PAGE as u64) as usize,
        )))
    }

    /// Return the value for `key`, or `0` if absent.
    pub fn lookup(&self, cache: &mut PageCache, root: u64, key: u64) -> Result<u64> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let Some((leaf, idx)) = self.find_leaf(cache, root, key)? else {
            return Ok(0);
        };
        Ok(read_slot(cache.get(leaf)?, idx))
    }

    fn grow(&mut self, cache: &mut PageCache, old_root: u64) -> Result<u64> {
        let new_root = cache.new_page()?;
        debug_assert_ne!(new_root, 0);
        let buf = cache.get_mut(new_root)?;
        buf.fill(0);
        buf[0] = PageType::MembershipInterior as u8;
        buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
        write_slot(buf, 0, old_root); // old root becomes child 0
        page::stamp_checksum(buf);
        self.depth += 1;
        Ok(new_root)
    }

    /// Insert `value` (must be non-zero) at `key`. Returns the new root.
    pub fn insert(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        key: u64,
        value: u64,
    ) -> Result<u64> {
        debug_assert_ne!(value, 0, "0 is the absent sentinel; cannot be stored");
        let mut current_root = root;
        while key >= self.capacity() {
            current_root = self.grow(cache, current_root)?;
        }
        self.insert_recursive(cache, current_root, key, value, self.depth)
    }

    fn insert_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        key: u64,
        value: u64,
        level: u32,
    ) -> Result<u64> {
        let new_page = cache.new_page()?;
        debug_assert_ne!(new_page, 0);
        {
            let old: [u8; PAGE_SIZE] = *cache.get(page)?;
            cache.get_mut(new_page)?.copy_from_slice(&old);
        }
        if level == 0 {
            let idx = (key % SLOTS_PER_PAGE as u64) as usize;
            let buf = cache.get_mut(new_page)?;
            write_slot(buf, idx, value);
            page::stamp_checksum(buf);
            Ok(new_page)
        } else {
            let span = self.span_at_level(level);
            let child_idx = (key / span) as usize;
            let child = read_slot(cache.get(new_page)?, child_idx);
            let actual_child = if child == 0 {
                let pt = if level == 1 {
                    PageType::MembershipLeaf
                } else {
                    PageType::MembershipInterior
                };
                init_page(cache, pt)?
            } else {
                child
            };
            let new_child =
                self.insert_recursive(cache, actual_child, key % span, value, level - 1)?;
            let buf = cache.get_mut(new_page)?;
            write_slot(buf, child_idx, new_child);
            page::stamp_checksum(buf);
            Ok(new_page)
        }
    }

    /// Set `key` to absent. Returns `(new_root, prev_value)`; `prev_value == 0`
    /// means it was already absent and no COW happened.
    pub fn delete(&mut self, cache: &mut PageCache, root: u64, key: u64) -> Result<(u64, u64)> {
        if root == PAGE_ID_NONE || key >= self.capacity() {
            return Ok((root, 0));
        }
        self.delete_recursive(cache, root, key, self.depth)
    }

    fn delete_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        key: u64,
        level: u32,
    ) -> Result<(u64, u64)> {
        if level == 0 {
            let idx = (key % SLOTS_PER_PAGE as u64) as usize;
            let prev = read_slot(cache.get(page)?, idx);
            if prev == 0 {
                return Ok((page, 0));
            }
            let new_leaf = cache.new_page()?;
            debug_assert_ne!(new_leaf, 0);
            {
                let old: [u8; PAGE_SIZE] = *cache.get(page)?;
                *cache.get_mut(new_leaf)? = old;
            }
            {
                let buf = cache.get_mut(new_leaf)?;
                write_slot(buf, idx, 0);
                page::stamp_checksum(buf);
            }
            Ok((new_leaf, prev))
        } else {
            let span = self.span_at_level(level);
            let child_idx = (key / span) as usize;
            let child = read_slot(cache.get(page)?, child_idx);
            if child == 0 {
                return Ok((page, 0));
            }
            let (new_child, prev) = self.delete_recursive(cache, child, key % span, level - 1)?;
            if prev == 0 {
                return Ok((page, 0));
            }
            let new_page = cache.new_page()?;
            debug_assert_ne!(new_page, 0);
            {
                let old: [u8; PAGE_SIZE] = *cache.get(page)?;
                *cache.get_mut(new_page)? = old;
            }
            {
                let buf = cache.get_mut(new_page)?;
                write_slot(buf, child_idx, new_child);
                page::stamp_checksum(buf);
            }
            Ok((new_page, prev))
        }
    }

    /// Enumerate all `(key, value)` pairs with a non-zero value. Order is
    /// unspecified.
    pub fn iter(&self, cache: &mut PageCache, root: u64) -> Result<Vec<(u64, u64)>> {
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.iter_recursive(cache, root, 0, self.depth, &mut out)?;
        Ok(out)
    }

    fn iter_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        base: u64,
        level: u32,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                let v = read_slot(buf, i);
                if v != 0 {
                    out.push((base + i as u64, v));
                }
            }
        } else {
            let span = self.span_at_level(level);
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| (i, read_slot(buf, i)))
                    .filter(|(_, c)| *c != 0)
                    .collect()
            };
            for (i, child) in children {
                self.iter_recursive(cache, child, base + i as u64 * span, level - 1, out)?;
            }
        }
        Ok(())
    }

    /// Like `iter` but stops after collecting `limit` pairs — touches at most
    /// ~`limit` leaves' worth of work, giving the caller a bounded-time pass.
    pub fn iter_bounded(
        &self,
        cache: &mut PageCache,
        root: u64,
        limit: usize,
    ) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        if root == PAGE_ID_NONE || limit == 0 {
            return Ok(out);
        }
        self.iter_bounded_recursive(cache, root, 0, self.depth, limit, &mut out)?;
        Ok(out)
    }

    fn iter_bounded_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        base: u64,
        level: u32,
        limit: usize,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        if out.len() >= limit {
            return Ok(());
        }
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                if out.len() >= limit {
                    break;
                }
                let v = read_slot(buf, i);
                if v != 0 {
                    out.push((base + i as u64, v));
                }
            }
        } else {
            let span = self.span_at_level(level);
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| (i, read_slot(buf, i)))
                    .filter(|(_, c)| *c != 0)
                    .collect()
            };
            for (i, child) in children {
                if out.len() >= limit {
                    break;
                }
                self.iter_bounded_recursive(
                    cache,
                    child,
                    base + i as u64 * span,
                    level - 1,
                    limit,
                    out,
                )?;
            }
        }
        Ok(())
    }

    /// Early-exit emptiness check: `true` if any key has a non-zero value.
    /// Cheap when the tree is non-empty (stops at the first hit); `O(tree)` only
    /// when it is empty.
    pub fn any_present(&self, cache: &mut PageCache, root: u64) -> Result<bool> {
        if root == PAGE_ID_NONE {
            return Ok(false);
        }
        self.any_recursive(cache, root, self.depth)
    }

    fn any_recursive(&self, cache: &mut PageCache, page: u64, level: u32) -> Result<bool> {
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                if read_slot(buf, i) != 0 {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            let children: Vec<u64> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| read_slot(buf, i))
                    .filter(|c| *c != 0)
                    .collect()
            };
            for child in children {
                if self.any_recursive(cache, child, level - 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    /// Recover a tree's depth by walking the leftmost spine from `root` (mirrors
    /// the handle-table open-time depth recovery; relies on `grow` always
    /// installing the old root at child 0).
    pub fn recover_depth(cache: &mut PageCache, root: u64) -> Result<u32> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let mut depth = 0u32;
        let mut current = root;
        loop {
            let buf = cache.get(current)?;
            if buf[0] != PageType::MembershipInterior as u8 {
                break;
            }
            depth += 1;
            let child = read_slot(buf, 0);
            if child == 0 {
                break;
            }
            current = child;
        }
        Ok(depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    // Same fixture shape as handle_table.rs tests: reserve pages 0/1 so
    // new_page() never returns the zero-child sentinel.
    fn cache(max_pages: usize) -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut c = PageCache::new(
            io,
            max_pages as u64 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        c.set_next_page_id(2);
        c
    }

    #[test]
    fn insert_lookup_delete_single_level() {
        let mut c = cache(64);
        let mut t = RadixU64::new();
        let root = t.create_root(&mut c).unwrap();
        assert_eq!(t.lookup(&mut c, root, 5).unwrap(), 0);
        let r = t.insert(&mut c, root, 5, 99).unwrap();
        assert_eq!(t.lookup(&mut c, r, 5).unwrap(), 99);
        assert!(t.any_present(&mut c, r).unwrap());
        let (r2, prev) = t.delete(&mut c, r, 5).unwrap();
        assert_eq!(prev, 99);
        assert_eq!(t.lookup(&mut c, r2, 5).unwrap(), 0);
        assert!(!t.any_present(&mut c, r2).unwrap());
    }

    #[test]
    fn grows_and_iterates_across_levels() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        for k in [0u64, 1, 1021, 2000, 1_000_000] {
            root = t.insert(&mut c, root, k, k + 7).unwrap();
        }
        assert!(t.depth >= 1, "tree should have grown");
        for k in [0u64, 1, 1021, 2000, 1_000_000] {
            assert_eq!(t.lookup(&mut c, root, k).unwrap(), k + 7);
        }
        let mut pairs = t.iter(&mut c, root).unwrap();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (0, 7),
                (1, 8),
                (1021, 1028),
                (2000, 2007),
                (1_000_000, 1_000_007)
            ]
        );
    }

    #[test]
    fn iter_bounded_caps_collection() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        for k in 0..50u64 {
            root = t.insert(&mut c, root, k, k + 1).unwrap();
        }
        assert_eq!(t.iter_bounded(&mut c, root, 10).unwrap().len(), 10);
        assert_eq!(t.iter_bounded(&mut c, root, 100).unwrap().len(), 50);
    }

    #[test]
    fn delete_absent_is_noop() {
        let mut c = cache(64);
        let mut t = RadixU64::new();
        let root = t.create_root(&mut c).unwrap();
        let (r, prev) = t.delete(&mut c, root, 42).unwrap();
        assert_eq!(prev, 0);
        assert_eq!(r, root, "no COW for an absent key");
    }

    #[test]
    fn recover_depth_matches_after_grow() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        root = t.insert(&mut c, root, 5_000_000, 1).unwrap();
        let recovered = RadixU64::recover_depth(&mut c, root).unwrap();
        assert_eq!(recovered, t.depth);
    }
}
