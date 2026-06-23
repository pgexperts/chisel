//! transaction::packing — R1 data-page slot packing: releasing a data
//! slot, lazily materializing the handle table, and inserting a value into
//! a data page. Split out of `transaction.rs` verbatim; see the parent
//! module for the type and fields.

use super::*;

impl TransactionManager {
    // --- Private helpers ---

    /// Release one slot from a data page (ISSUES.md R1). Decrements
    /// `current_live_slots[page_id]`; if the count reaches zero, the
    /// whole page becomes unreferenced and is pushed to
    /// `txn_freed_pages` so commit can return it to the freemap.
    /// Otherwise the slot becomes a tombstone: dead weight inside a
    /// still-live page, reclaimable only via defrag.
    ///
    /// If the page is somehow not tracked in `current_live_slots` (a
    /// bug; open-time scan should catch every live data page), this is
    /// a no-op — we prefer leaking to a spurious free.
    ///
    /// NOTE: a stray orphaned line "Lazily create a handle table root
    /// on first insert. A fresh database has" previously sat at the
    /// top of this doc block (an interleaved remnant of
    /// `ensure_handle_table`'s docstring); removed 2026-04-17 during
    /// the commenting pass. The counterpart ("root_handle_table_page
    /// == PAGE_ID_NONE; we don't materialize...") still sits above
    /// `ensure_handle_table` below — both belong together.
    pub(super) fn release_data_slot(&mut self, page_id: u64) {
        let Some(count) = self.current_live_slots.get_mut(&page_id) else {
            return;
        };
        if *count > 0 {
            *count -= 1;
        }
        if *count == 0 {
            self.current_live_slots.remove(&page_id);
            // If this page is the active insert cursor, clear the
            // cursor — it's about to become free space, and we don't
            // want future inserts to pack into it and then find it
            // disappearing at commit time.
            if self.insert_cursor == Some(page_id) {
                self.insert_cursor = None;
            }
            self.txn_freed_pages.push(page_id);
        }
    }

    /// Lazily create a handle table root on first insert. A fresh
    /// database has `root_handle_table_page == PAGE_ID_NONE`; we don't
    /// materialize the root until there is a handle to put in it, so
    /// empty databases never pay for a handle-table page. No per-page
    /// rollback bookkeeping — the watermark rollback mechanism (I3)
    /// handles any page allocated here automatically.
    pub(super) fn ensure_handle_table(&mut self) -> Result<()> {
        if self.current_roots.handle_table_page == PAGE_ID_NONE {
            let root = {
                let mut cache = self.cache.borrow_mut();
                self.handle_table.create_root(&mut cache)?
            };
            self.current_roots.handle_table_page = root;
        }
        Ok(())
    }

    /// Place a value in a data page and return (page_id, slot_index).
    ///
    /// Post-R1 packing model: the transaction maintains an "insert
    /// cursor" — a data page allocated earlier in THIS transaction
    /// that still has space — and packs successive small-value inserts
    /// into it until it fills. When the cursor is absent/full, a new
    /// page is allocated (via `allocate_data_page`, which prefers
    /// freemap reuse over file extension — R2) and becomes the new
    /// cursor. Packing is disabled while savepoints are active: the
    /// cursor is force-cleared by `savepoint()` and is NOT set when a
    /// new page is allocated inside a savepoint scope, so each insert
    /// under a savepoint gets its own page (the pre-R1 behavior). This
    /// keeps the per-savepoint snapshot cheap to restore.
    ///
    /// Checksum is stamped eagerly after every mutation so the page carries a
    /// valid internal checksum before any path could write it to the main
    /// file — either the `flush` `write_page` at commit, or a spill-then-drain
    /// write (an LRU-pressured dirty page is spilled to the spillway and later
    /// drained back out to the main file). The next cold-load
    /// (`page_cache::load_page`) verifies that checksum.
    ///
    /// Note: the spillway *transfer* does NOT rely on this. `rehydrate`
    /// verifies the spillway's own per-slot checksum (`spillway::slot_checksum`),
    /// never the page's internal bytes 8184..8192 — so a spilled page round-trips
    /// safely whether or not its internal checksum is current. The internal
    /// checksum only matters on the way to the main file.
    ///
    /// I78 proposes deferring this re-stamp to flush/drain time so a packed page
    /// is hashed once, not once per value (a large bulk-insert win on fast
    /// storage). It is deferred pending a benchmark; the difficulty is exactly
    /// the spill-then-drain path, which would then have to re-stamp before the
    /// main-file write. See ISSUES.md.
    ///
    /// Live-slot bookkeeping: every successful insert increments
    /// `current_live_slots[page_id]`. `delete`/`update` consult this
    /// map (via `release_data_slot`) to decide when a page is fully
    /// empty and can be freed back to the freemap on commit. The map
    /// is kept purely in memory — storing a slot count ON the data
    /// page would force a COW (and a handle-table rewrite for every
    /// entry pointing into it) on every delete.
    pub(super) fn insert_into_data_page(&mut self, value: &[u8]) -> Result<(u64, u16)> {
        // Packing path: try to reuse the current cursor page if it
        // has room. The cursor only exists when savepoints are empty
        // (see savepoint_inner) so this branch implicitly respects
        // the "no packing under savepoints" rule.
        if let Some(cursor_page_id) = self.insert_cursor {
            let slot_option = {
                let mut cache = self.cache.borrow_mut();
                let buf = cache.get_mut(cursor_page_id)?;
                let result = DataPage::insert(buf, value);
                if result.is_some() {
                    page::stamp_checksum(buf);
                }
                result
            };
            if let Some(slot) = slot_option {
                *self.current_live_slots.entry(cursor_page_id).or_insert(0) += 1;
                return Ok((cursor_page_id, slot));
            }
            // Cursor page is full. Fall through to allocate a new one;
            // the new page becomes the new cursor.
        }

        // Allocate a fresh data page. Under active savepoints, the
        // cursor stays None (set below, then cleared by the savepoint
        // check in subsequent calls) so each insert gets its own page —
        // matching the pre-R1 "one value per page" behavior within
        // savepoint scopes, which is the price of keeping rollback_to
        // semantics simple.
        let page_id = self.allocate_data_page()?;
        let slot = {
            let mut cache = self.cache.borrow_mut();
            let buf = cache.get_mut(page_id)?;
            DataPage::init_page(buf);
            // I46 INVARIANT: DataPage::insert can only return None for
            // "no room"; the page was just init'd via DataPage::init_page
            // (empty), and the value's length was already checked against
            // MAX_INLINE_VALUE upstream (the overflow path catches anything
            // larger before we get here). If DataPage::insert ever grows
            // other failure modes, this expect needs to translate them to
            // typed errors instead of panicking.
            let slot = DataPage::insert(buf, value).expect("value fits in empty page");
            page::stamp_checksum(buf);
            slot
        };

        // Only install the new page as the cursor if we're outside any
        // savepoint scope. During a savepoint scope the cursor stays
        // None so packing is effectively disabled.
        if self.savepoints.is_empty() {
            self.insert_cursor = Some(page_id);
        }
        *self.current_live_slots.entry(page_id).or_insert(0) += 1;
        Ok((page_id, slot))
    }
}
