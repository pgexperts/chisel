//! transaction::packing — R1 data-page slot packing as an owned unit.
//!
//! `SlotPacker` owns the three fields that together implement the R1
//! live-slot / insert-cursor model: `committed_live_slots`,
//! `current_live_slots`, and `insert_cursor`. No code outside this file
//! touches those fields directly; everything goes through the narrow
//! interface below. `TransactionManager` holds one `SlotPacker` (the
//! `packer` field) and delegates via thin wrappers (`insert_into_data_page`
//! / `release_data_slot`) plus the lifecycle/savepoint/stats hooks.
//!
//! Live-slot model (ISSUES.md R1): `current_live_slots[page_id]` counts how
//! many handle-table entries currently point at each data page — the
//! information needed to decide when a page is fully empty and can be
//! returned to the freemap. `committed_live_slots` is the durable state
//! (rebuilt at open time by scanning the handle table); `current_live_slots`
//! is the in-transaction working copy. Both are kept purely in memory:
//! storing a slot count ON the data page would force a COW (and a
//! handle-table rewrite for every entry pointing into it) on every delete —
//! an O(live-slots-in-page) amplification shadow paging does not handle well.
//!
//! Packing cursor: a data page allocated earlier in THIS transaction that
//! still has free space. New values pack into it until it fills, at which
//! point a new page is allocated and becomes the new cursor. `None` at the
//! start of each transaction (only pages allocated during the current
//! transaction — dirty in the cache and safe to modify — are ever the
//! cursor; a committed data page is never the cursor, since that would
//! require COW). Packing is disabled entirely when savepoints are active
//! (the `packing_enabled` gate below): the savepoint-snapshot cost becomes
//! manageable when only one code path interacts with packing state. The
//! packer itself does NOT know about savepoints — the caller passes the gate.
//!
//! Split out of the historical `transaction.rs`; see the parent module for
//! `TransactionManager` and the freemap fields the wrappers borrow.

use super::*;

/// Owned R1 slot-packing state. See the module header for the live-slot and
/// cursor models. Constructed empty (`new`) by `create_new`, or seeded from a
/// scanned committed map (`from_committed`) by `open_existing`.
pub(super) struct SlotPacker {
    // Durable live-slot counts (promoted from `current_live_slots` at commit;
    // rebuilt at open time by scanning the handle table).
    committed_live_slots: FxHashMap<u64, u32>,
    // In-transaction working copy of the live-slot counts.
    current_live_slots: FxHashMap<u64, u32>,
    // The current in-progress insert cursor; see the module header.
    insert_cursor: Option<u64>,
}

impl SlotPacker {
    /// Empty packer: no live slots, no cursor. Used for a freshly created
    /// database (`create_new`), which has no data pages yet.
    pub(super) fn new() -> Self {
        SlotPacker {
            committed_live_slots: FxHashMap::default(),
            current_live_slots: FxHashMap::default(),
            insert_cursor: None,
        }
    }

    /// Seed from a committed live-slot map scanned at open time. The working
    /// copy starts equal to the committed map; the cursor starts None (it only
    /// ever tracks pages allocated during a live transaction).
    pub(super) fn from_committed(committed: FxHashMap<u64, u32>) -> Self {
        let current_live_slots = committed.clone();
        SlotPacker {
            committed_live_slots: committed,
            current_live_slots,
            insert_cursor: None,
        }
    }

    /// Place a value in a data page and return (page_id, slot_index).
    ///
    /// Post-R1 packing model: try to reuse the current cursor page if it has
    /// room; when the cursor is absent/full, call `alloc` for a fresh page and
    /// (when `packing_enabled`) install it as the new cursor. `alloc` is the
    /// data-page allocator (formerly `TransactionManager::allocate_data_page`),
    /// hoisted to the call site so the freemap dance it performs does not
    /// borrow-conflict with `&mut self` here; it receives `cache` as a param.
    /// `packing_enabled` is the caller's `savepoints.is_empty()` check: under a
    /// savepoint the cursor stays None so each insert gets its own page (the
    /// pre-R1 behavior), keeping per-savepoint snapshots cheap to restore.
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
    /// `current_live_slots[page_id]`. `delete`/`update` consult this map (via
    /// `release`) to decide when a page is fully empty and can be freed back to
    /// the freemap on commit.
    pub(super) fn insert(
        &mut self,
        cache: &mut PageCache,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        packing_enabled: bool,
        value: &[u8],
    ) -> Result<(u64, u16)> {
        // Packing path: try to reuse the current cursor page if it has room.
        // The cursor only exists when packing is enabled (savepoints empty),
        // so this branch implicitly respects the "no packing under savepoints"
        // rule.
        if let Some(cursor_page_id) = self.insert_cursor {
            let slot_option = {
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

        // Allocate a fresh data page. Under active savepoints
        // (`packing_enabled == false`) the cursor stays None so each insert
        // gets its own page — matching the pre-R1 "one value per page" behavior
        // within savepoint scopes, which is the price of keeping rollback_to
        // semantics simple.
        let page_id = alloc(cache)?;
        let slot = {
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

        // Only install the new page as the cursor when packing is enabled
        // (outside any savepoint scope). During a savepoint scope the cursor
        // stays None so packing is effectively disabled.
        if packing_enabled {
            self.insert_cursor = Some(page_id);
        }
        *self.current_live_slots.entry(page_id).or_insert(0) += 1;
        Ok((page_id, slot))
    }

    /// Release one slot from a data page (ISSUES.md R1). Decrements
    /// `current_live_slots[page_id]`; if the count reaches zero, the whole
    /// page becomes unreferenced. Returns `Some(page_id)` in that case so the
    /// caller can push it to `txn_freed_pages` (commit returns it to the
    /// freemap); `None` otherwise. Either way, if the freed page was the
    /// active insert cursor the cursor is cleared here — it's about to become
    /// free space, and we don't want future inserts to pack into it and then
    /// find it disappearing at commit time. A non-zero residual count leaves
    /// the slot a tombstone: dead weight inside a still-live page, reclaimable
    /// only via defrag.
    ///
    /// If the page is somehow not tracked in `current_live_slots` (a bug;
    /// open-time scan should catch every live data page), this is a no-op — we
    /// prefer leaking to a spurious free.
    pub(super) fn release(&mut self, page_id: u64) -> Option<u64> {
        let count = self.current_live_slots.get_mut(&page_id)?;
        if *count > 0 {
            *count -= 1;
        }
        if *count == 0 {
            self.current_live_slots.remove(&page_id);
            if self.insert_cursor == Some(page_id) {
                self.insert_cursor = None;
            }
            return Some(page_id);
        }
        None
    }

    /// Begin a transaction: clone the committed counts into the working copy
    /// and reset the cursor (it only tracks pages allocated during the current
    /// transaction, so it is always None at begin).
    pub(super) fn begin(&mut self) {
        self.current_live_slots = self.committed_live_slots.clone();
        self.insert_cursor = None;
    }

    /// Commit: promote the working counts to committed and reset the cursor
    /// (per-transaction state that gets re-established at the next begin).
    pub(super) fn commit(&mut self) {
        self.committed_live_slots = self.current_live_slots.clone();
        self.insert_cursor = None;
    }

    /// Roll back: revert the working counts to the committed baseline and drop
    /// the cursor.
    pub(super) fn rollback(&mut self) {
        self.current_live_slots = self.committed_live_slots.clone();
        self.insert_cursor = None;
    }

    /// Read-only snapshot of the working state for a savepoint: the current
    /// live-slot map (cloned) and the cursor. The savepoint-CREATE path clears
    /// the cursor separately via `clear_cursor` after snapshotting — snapshot
    /// itself does not mutate.
    pub(super) fn snapshot(&self) -> (FxHashMap<u64, u32>, Option<u64>) {
        (self.current_live_slots.clone(), self.insert_cursor)
    }

    /// Restore the working state from a savepoint snapshot.
    pub(super) fn restore(&mut self, snap: (FxHashMap<u64, u32>, Option<u64>)) {
        self.current_live_slots = snap.0;
        self.insert_cursor = snap.1;
    }

    /// Clear the insert cursor. Used by the savepoint-create path: once a
    /// savepoint exists the insert path stops packing (same posture as freemap
    /// reuse — savepoints disable the optimization to keep rollback_to simple).
    pub(super) fn clear_cursor(&mut self) {
        self.insert_cursor = None;
    }

    /// The in-transaction working live-slot map. Read-only access for stats
    /// (sparse-page detection, the defrag page-id snapshot) and tests.
    pub(super) fn current_live_slots(&self) -> &FxHashMap<u64, u32> {
        &self.current_live_slots
    }

    /// Whether the working live-slot map is empty (no data page holds a live
    /// slot). Convenience accessor used by tests asserting a clean no-op.
    #[cfg(test)]
    pub(super) fn is_current_empty(&self) -> bool {
        self.current_live_slots.is_empty()
    }

    /// The current insert cursor. Read-only accessor for tests asserting the
    /// packing state after a failed mutation.
    #[cfg(test)]
    pub(super) fn insert_cursor(&self) -> Option<u64> {
        self.insert_cursor
    }
}

impl TransactionManager {
    /// Place a value in a data page and return (page_id, slot_index).
    ///
    /// Thin wrapper over `SlotPacker::insert`. The freemap dance (the
    /// transient-tree trio: `freemap.take_tree` / `cow_alloc_into` / `put_tree`)
    /// is hoisted HERE, around the closure, exactly like `ht_insert`. The closure
    /// captures only `&mut self.freemap` plus the local `tree` — disjoint from
    /// `&mut self.packer` — so the packer borrow and the alloc closure both hold
    /// `self` at once; `cache` is passed as a param.
    ///
    /// `put_tree` runs even on the error path — matching the historical
    /// `allocate_data_page`, which wrote tree growth back before returning the
    /// allocation result. The freemap pages were extended (never freed), so on a
    /// non-fatal failure they are harmless above-watermark scratch, and the
    /// session set must still be returned for a retry/commit in the same
    /// transaction to stay consistent.
    pub(super) fn insert_into_data_page(&mut self, value: &[u8]) -> Result<(u64, u16)> {
        // `reuse` gates freemap reuse inside cow_alloc; `packing_enabled` gates
        // installing the freshly-allocated page as the cursor. Both are the
        // same `savepoints.is_empty()` check today, but they are distinct
        // concerns (the packer must not know about savepoints), so they are
        // named separately.
        let reuse = self.savepoints.is_empty();
        let packing_enabled = self.savepoints.is_empty();
        let mut tree = self.freemap.take_tree(&self.current_roots);
        let result = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| self.freemap.cow_alloc_into(c, &mut tree, reuse);
            self.packer
                .insert(&mut cache, &mut alloc, packing_enabled, value)
        };
        self.freemap.put_tree(&mut self.current_roots, tree);
        result
    }

    /// Release one slot from a data page. Thin wrapper over
    /// `SlotPacker::release`: when the page hits zero live slots the packer
    /// returns its id and we push it to `txn_freed_pages` so commit returns it
    /// to the freemap.
    pub(super) fn release_data_slot(&mut self, page_id: u64) {
        if let Some(freed) = self.packer.release(page_id) {
            self.txn_freed_pages.push(freed);
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
}
