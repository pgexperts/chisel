//! transaction::stats — read-only stats / introspection and the
//! defrag-support surface, plus the test-only corruption forges. Split out
//! of `transaction.rs` verbatim; see the parent module for the type and fields.

use super::*;

impl TransactionManager {
    /// Test-only: forge a freemap orphan exactly as a crash would leave one.
    /// Extend a fresh page, stamp it as a checksum-valid `FreeMapInterior`, and
    /// return its id WITHOUT referencing it from the live tree or marking it free
    /// — the precise state of a structural-recycle-pool page stranded when an
    /// in-memory pool is lost to a crash. The orphan sweep
    /// (`reclaim_freemap_orphans`) must reclaim it. Returns the forged page id.
    ///
    /// FreeMapInterior (not FreeMap) is used deliberately: it cannot be mistaken
    /// for a freed-bit leaf, and it exercises the interior arm of the type test.
    #[cfg(test)]
    pub(crate) fn test_forge_freemap_orphan(&mut self) -> Result<u64> {
        let mut cache = self.cache.borrow_mut();
        let id = cache.new_page()?;
        let buf = cache.get_mut(id)?;
        buf.fill(0);
        buf[0] = crate::page::PageType::FreeMapInterior as u8;
        buf[1] = page::current_version(crate::page::PageType::FreeMapInterior);
        page::stamp_checksum(buf);
        Ok(id)
    }

    /// Test-only: forge a CORRUPT, non-reachable page on disk. Extend a fresh
    /// page, fill it with garbage, deliberately do NOT stamp a valid checksum,
    /// flush it to the backing file, then drop it from the cache so a later
    /// `get(id)` re-reads it from disk and fails with `ChecksumMismatch`. The
    /// page is never referenced from any tree, so it is a corrupt DEAD page —
    /// exactly what the orphan sweep must SKIP rather than poison on. Returns the
    /// forged page id.
    #[cfg(test)]
    pub(crate) fn test_forge_corrupt_dead_page(&mut self) -> Result<u64> {
        let mut cache = self.cache.borrow_mut();
        let id = cache.new_page()?;
        let buf = cache.get_mut(id)?;
        // Garbage bytes with a freemap-ish type byte but a checksum that will not
        // verify (we never call stamp_checksum). The type byte is irrelevant —
        // the read fails the checksum gate before the type is ever inspected.
        buf.fill(0xAB);
        buf[0] = crate::page::PageType::FreeMap as u8;
        cache.flush()?; // write the garbage bytes to the main file
        cache.test_drop_from_cache(id); // force a disk re-read (and checksum check) next get
        Ok(id)
    }

    /// Snapshot the four engine-activity counters (cache hits/misses,
    /// pages allocated, fsync calls). Counters are cumulative from the
    /// most recent open; the bench harness reads-subtract-reads for
    /// per-cell deltas. Takes `&self` (F3); poison-aware via
    /// `check_alive`.
    pub fn counters(&self) -> Result<ChiselCounters> {
        self.check_alive()?;
        Ok(self.cache.borrow().counters())
    }

    /// I74 (ISSUES.md, 2026-05-22): peek the spillway's current
    /// (logical_bytes, max_bytes) for `Chisel::stats`. `None` if the
    /// spillway has never been opened. Routes through the same
    /// poison check as `counters()` so a fatal-error state surfaces
    /// here too — operators reading `stats()` get a `Poisoned`
    /// error rather than stale-looking Some(0,0).
    pub fn spillway_capacity(&self) -> Result<Option<(u64, u64)>> {
        self.check_alive()?;
        Ok(self.cache.borrow().spillway_capacity())
    }

    /// Poisoning-aware wrapper around `PageCache::file_page_count`. Called
    /// by `Chisel::stats()` so that a fatal I/O error while measuring the
    /// file size also poisons the manager.
    ///
    /// F3: takes `&self`.
    pub fn file_page_count(&self) -> Result<u64> {
        self.check_alive()?;
        let result = self.cache.borrow_mut().file_page_count();
        self.poison_on_fatal(result)
    }

    // --- Selective defragmentation support (ISSUES.md R3 + I17) ---
    //
    // These methods expose just enough of the R1 live-slot tracking
    // for `defrag::defrag` to do selective page compaction. The
    // defrag module is in-crate and could in principle access the
    // fields directly, but going through named methods keeps the
    // intent obvious at each call site.

    /// Page ids of data pages whose effective density (live slots /
    /// stored slots) is strictly less than `threshold_ratio`. A
    /// freshly-packed page with every slot still live has density
    /// 1.0; a page that originally packed 39 values but now has only
    /// 5 live (34 dead-weight tombstones) has density 0.128 and is a
    /// strong defrag candidate.
    ///
    /// The metric uses the page's OWN stored-slot count (read from
    /// the on-disk header via `DataPage::slot_count`) as the
    /// denominator — not the max-observed count in the database —
    /// because dead-weight slots are what defrag is trying to reclaim.
    /// The older "relative to densest" metric failed for the case of a
    /// single remaining sparse page (density 1.0 against itself).
    ///
    /// Returns an empty set when `threshold_ratio <= 0`. Fallible
    /// because the per-page stored count is read through the cache.
    pub fn sparse_data_pages(
        &self,
        threshold_ratio: f64,
    ) -> Result<std::collections::HashSet<u64>> {
        self.check_alive()?;
        let result = self.sparse_data_pages_inner(threshold_ratio);
        self.poison_on_fatal(result)
    }

    fn sparse_data_pages_inner(
        &self,
        threshold_ratio: f64,
    ) -> Result<std::collections::HashSet<u64>> {
        let mut sparse = std::collections::HashSet::new();
        if threshold_ratio <= 0.0 {
            return Ok(sparse);
        }
        let page_ids: Vec<u64> = self.packer.current_live_slots().keys().copied().collect();
        for page_id in page_ids {
            let live = match self.packer.current_live_slots().get(&page_id) {
                Some(&n) if n > 0 => n,
                _ => continue,
            };
            let stored = {
                let mut cache = self.cache.borrow_mut();
                DataPage::slot_count(cache.get(page_id)?) as u32
            };
            if stored == 0 {
                continue;
            }
            let density = live as f64 / stored as f64;
            if density < threshold_ratio {
                sparse.insert(page_id);
            }
        }
        Ok(sparse)
    }

    /// Snapshot of the page ids currently tracked as holding at least
    /// one live slot. Used by `defrag::defrag` for the I17 stat: after
    /// the sweep, `pages_freed` is the count of ids that were in this
    /// snapshot and are no longer in `current_live_slots` — i.e.,
    /// pages that the sweep fully drained and returned to the freemap.
    /// Net change in the live data-page count is the wrong metric here
    /// because a relocation simultaneously drains a sparse page and
    /// creates a dense one; the former should count as "reclaimed"
    /// even when the latter offsets the net count.
    pub fn data_page_ids_snapshot(&self) -> std::collections::HashSet<u64> {
        self.packer.current_live_slots().keys().copied().collect()
    }

    /// Look up the data page id that currently holds `handle`. Returns
    /// `Ok(None)` if the handle doesn't exist, is deleted, or points
    /// at an overflow chain (for which the notion of "data page" does
    /// not apply).
    ///
    /// Takes `&self`; uses the RefCell around the cache to perform the
    /// handle-table lookup. Poisons the manager on fatal I/O or
    /// checksum errors.
    pub fn handle_live_page_id(&self, handle: u64) -> Result<Option<u64>> {
        self.check_alive()?;
        let result = self.handle_live_page_id_inner(handle);
        self.poison_on_fatal(result)
    }

    fn handle_live_page_id_inner(&self, handle: u64) -> Result<Option<u64>> {
        let root = self.live_handle_table_root();
        if root == PAGE_ID_NONE {
            return Ok(None);
        }
        let mut cache = self.cache.borrow_mut();
        let entry = match self.handle_table.lookup(&mut cache, root, handle)? {
            Some(e) => e,
            None => return Ok(None),
        };
        if entry.flags == HandleFlags::Live {
            Ok(Some(entry.page_id))
        } else {
            Ok(None)
        }
    }

    /// The handle-table root page id of the active transaction's
    /// in-progress roots. Used by `defrag::defrag` to short-circuit
    /// the empty-database fast path.
    ///
    /// I39 (ISSUES.md, 2026-05-22): replaces a `pub fn current_roots()
    /// -> (u64, u64, u64)` tuple return that exposed three fields when
    /// only one was ever read. YAGNI: if a future caller wants
    /// `freemap_page` or `next_handle`, add a sibling accessor at that
    /// time rather than guessing the API shape now. `pub(crate)`
    /// because `defrag` is the sole intended caller (transaction
    /// module became `pub(crate)` in I35).
    pub(crate) fn current_handle_table_root_page(&self) -> u64 {
        self.current_roots.handle_table_page
    }
}
