//! transaction::staging — atomic allocate staging (I18 / BUG#2): the
//! candidate-prepare / install split for handle-table + membership-index
//! roots, the abort-prepare unwind, and the membership-failure injection
//! hook. Split out of `transaction.rs` verbatim; see the parent module.

use super::*;

impl TransactionManager {
    /// Insert a value and return a stable handle.
    ///
    /// Handles are dense u64s drawn from current_roots.next_handle, starting at
    /// 1 — handle 0 is reserved as the "no handle" sentinel and is never
    /// returned. Large values
    /// (> MAX_INLINE_VALUE) go to an overflow chain and the HandleEntry records
    /// the first overflow page directly; small values get a slot in a freshly
    /// allocated data page. Either way the handle_table.insert() COWs the spine
    /// from leaf to root and returns the new root page_id, which becomes the
    /// new current_roots.handle_table_page. This is the fundamental shadow-
    /// paging step: the old root is still reachable via committed_roots and is
    /// untouched on disk until commit swaps the superblock.
    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value, 0);
        self.poison_on_fatal(result)
    }

    pub fn allocate_tagged(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value, tag);
        self.poison_on_fatal(result)
    }

    /// Compute the membership-index root produced by inserting `(tag, handle)`
    /// WITHOUT installing it into `current_roots`; superseded pages are appended
    /// to `freed`. Split out so `allocate_inner` can stage the forward- and
    /// reverse-map updates and install them atomically (BUG#2). On error,
    /// `MembershipIndex::insert` leaves `self.outer_depth` unchanged (it writes
    /// back only on success), so the caller need not restore any reverse-map
    /// in-memory depth.
    fn membership_insert_candidate(
        &mut self,
        tag: u32,
        handle: u64,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        let reuse = self.savepoints.is_empty();
        let mut tree = self.freemap.take_tree(&self.current_roots);
        let result = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| self.freemap.cow_alloc_into(c, &mut tree, reuse);
            self.membership_index.insert(
                &mut cache,
                self.current_roots.membership_index_page,
                tag,
                handle,
                &mut alloc,
                freed,
            )
        };
        // Freemap growth is installed into roots regardless of success: the tree
        // pages were extended (never freed), so a non-fatal failure that discards
        // `freed`/the candidate root leaves these extra pages as harmless
        // above-watermark scratch, exactly like the other COW pages on an aborted
        // prepare. put_tree drains the freemap COW supersedes into
        // structural_superseded and returns the session set so the next site in
        // this transaction stays in-place.
        self.freemap.put_tree(&mut self.current_roots, tree);
        result
    }

    /// Compute the handle-table root produced by inserting `entry` for `handle`
    /// WITHOUT installing it into `current_roots`; superseded spine pages are
    /// appended to `freed`. The forward-map counterpart to
    /// `membership_insert_candidate` for `allocate_inner`'s atomic staging
    /// (BUG#2). NOTE: `HandleTable::insert` may `grow`, which bumps the
    /// in-memory descent depth eagerly — the caller captures and restores that
    /// depth on the prepare-abort path.
    pub(super) fn handle_table_insert_candidate(
        &mut self,
        handle: u64,
        entry: &HandleEntry,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        // Test-only injection (see `fail_next_handle_table_op`): simulate a
        // non-fatal CacheFull at the forward / handle-table step — the one
        // carrying the eager depth bump for allocate. Lives here so BOTH
        // allocate_inner and update_inner exercise the real abort/unwind. No
        // production artifact under `#[cfg(not(test))]`.
        #[cfg(test)]
        if self.fault.fail_next_handle_table_op.replace(false) {
            return Err(ChiselError::CacheFull { limit: 0 });
        }
        let reuse = self.savepoints.is_empty();
        let mut tree = self.freemap.take_tree(&self.current_roots);
        let result = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| self.freemap.cow_alloc_into(c, &mut tree, reuse);
            self.handle_table.insert(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                entry,
                &mut alloc,
                freed,
            )
        };
        // Install freemap growth into roots (and return the session set) so the
        // NEXT candidate in this allocate (the reverse-map insert) threads the
        // up-to-date tree and treats already-COW'd freemap pages as in-place. The
        // freemap COW supersedes go to structural_superseded via put_tree.
        // See membership_insert_candidate for the abort-safety reasoning.
        self.freemap.put_tree(&mut self.current_roots, tree);
        result
    }

    /// Unwind the installed state from a partially-completed `allocate_inner`
    /// PREPARE phase after a non-fatal failure.
    ///
    /// WHAT IS RESTORED (the installed state is a no-op):
    /// - `current_roots.handle_table_page` — reverts to the pre-allocate value,
    ///   undoing any lazy `ensure_handle_table` materialization (empty DB goes
    ///   back to `PAGE_ID_NONE`).
    /// - `handle_table` descent depth — restored from the saved value, undoing
    ///   the eager bump that `HandleTable::grow` applies before its fallible COW.
    /// - Inline value's data slot — released via `release_data_slot` so
    ///   `current_live_slots` and the insert cursor stay consistent with the
    ///   un-installed root. A page allocated solely for this value goes to zero
    ///   occupancy and is queued for reclamation; a shared cursor page keeps a
    ///   defrag-reclaimable dead slot (exactly like a normal delete).
    /// - `next_handle` — never consumed (bumped only in the infallible INSTALL
    ///   phase), so there is nothing to undo here.
    ///
    /// WHAT IS NOT RESTORED (a bounded allocated-but-unreferenced residue):
    /// - Freemap COW pages drawn during the candidate allocations. When reuse is
    ///   enabled, `cow_alloc` may have cleared free bits in the committed freemap
    ///   and advanced the tree to cover the candidate-spine pages, leaving those
    ///   ids allocated-but-unreferenced. Restoring them here would require either
    ///   re-marking them free (fighting the COW dirty-page I20 invariant) or
    ///   re-sorting and re-queuing them as freed data pages (introducing I20
    ///   dirty-page hazards and growth regressions on the abnormal path).
    ///   Instead, the residue is reclaimed by the expected rollback
    ///   (`discard_all_dirty` + watermark truncate restore the freemap to its
    ///   committed state). Overflow value pages are in the same residue class.
    ///   The residue leaks only if the caller commits after the operational
    ///   error rather than rolling back — contrary to documented contract.
    fn abort_allocate_prepare(
        &mut self,
        saved_root: u64,
        saved_depth: u32,
        inline_page: Option<u64>,
    ) {
        self.current_roots.handle_table_page = saved_root;
        self.handle_table.set_depth(saved_depth);
        if let Some(page_id) = inline_page {
            self.release_data_slot(page_id);
        }
    }

    /// Compute the membership-index root produced by removing `(tag, handle)`
    /// WITHOUT installing it; returns `(new_root, was_present)`. Counterpart to
    /// `membership_insert_candidate` for `delete_inner`'s atomic staging (BUG#2).
    pub(super) fn membership_remove_candidate(
        &mut self,
        tag: u32,
        handle: u64,
        freed: &mut Vec<u64>,
    ) -> Result<(u64, bool)> {
        let reuse = self.savepoints.is_empty();
        let mut tree = self.freemap.take_tree(&self.current_roots);
        let result = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| self.freemap.cow_alloc_into(c, &mut tree, reuse);
            self.membership_index.remove(
                &mut cache,
                self.current_roots.membership_index_page,
                tag,
                handle,
                &mut alloc,
                freed,
            )
        };
        self.freemap.put_tree(&mut self.current_roots, tree);
        result
    }

    /// Test-only fault decision for the reverse-map (membership-index) step,
    /// shared by `allocate_inner` and `delete_inner`. Returns true (inject a
    /// non-fatal CacheFull) if the one-shot `fail_next_membership_op` is armed,
    /// or if the `fail_membership_op_after` countdown reaches this op. Consuming
    /// here keeps the injection logic in one place.
    #[cfg(test)]
    pub(super) fn inject_membership_failure(&self) -> bool {
        if self.fault.fail_next_membership_op.replace(false) {
            return true;
        }
        let remaining = self.fault.fail_membership_op_after.get();
        if remaining == 0 {
            return false;
        }
        self.fault.fail_membership_op_after.set(remaining - 1);
        remaining == 1
    }

    fn allocate_inner(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // BUG#2 atomic staging: the FORWARD map (the chunk's HandleEntry.tag in
        // the handle table) and the REVERSE map (tag -> handles in the
        // membership index, powering handles_with_tag / delete-by-tag) must
        // become durable together. We compute both candidate roots in a fallible
        // PREPARE phase that never installs into `current_roots`, then install
        // them together in an infallible phase.
        //
        // A non-fatal CacheFull/SpillwayFull mid-prepare is unwound by
        // `abort_allocate_prepare`, which is a no-op for the INSTALLED state:
        // neither forward nor reverse map changes, the eagerly-bumped
        // handle-table depth and any lazily-created root are restored, the
        // inline value's data slot is released (keeping live-slot / cursor
        // accounting consistent), and the handle id is not consumed
        // (next_handle is bumped only on success).
        //
        // What the abort does NOT restore is the freemap COW that the candidate
        // allocations performed: when reuse is enabled, `cow_alloc` may have
        // drawn candidate-spine pages from the freemap (clearing their bits and
        // advancing the tree), and those page ids are now allocated-but-
        // unreferenced. This is a BOUNDED residue — the same class as any
        // post-allocation failure — fully reclaimed by the expected rollback
        // (discard_all_dirty + watermark truncate restore the freemap to its
        // committed state). It materializes as a leak only if the caller commits
        // after the operational error instead of rolling back, which is contrary
        // to the documented contract.
        let handle = self.current_roots.next_handle;

        // Value storage (PREPARE). For an inline value this also bumps
        // current_live_slots and may set the insert cursor; capture its page id
        // so a later prepare failure can release it via `abort_allocate_prepare`,
        // keeping that bookkeeping consistent with the un-installed root.
        // Overflow storage has no live-slot/cursor side effects.
        let mut inline_page: Option<u64> = None;
        let entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
                tag,
                client_byte: 0,
            }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            inline_page = Some(data_page_id);
            HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
                tag,
                client_byte: 0,
            }
        };

        // Capture the handle-table root/depth BEFORE `ensure_handle_table` may
        // lazily materialize an empty root, so a prepare failure restores
        // current_roots to its true pre-allocate state (an empty DB reverts to
        // PAGE_ID_NONE). The depth capture also covers `HandleTable::grow`, which
        // bumps the in-memory descent depth EAGERLY (before its fallible leaf
        // COW) while we defer the root install. The membership index needs no
        // such save: MembershipIndex::insert writes its outer_depth back only on
        // the success path, so a failed reverse-map op never advances it.
        let saved_ht_root = self.current_roots.handle_table_page;
        let saved_ht_depth = self.handle_table.depth();
        self.ensure_handle_table()?;

        // FORWARD map: compute the new handle-table root; do NOT install yet.
        // (handle_table_insert_candidate carries the #[cfg(test)] forward-step
        // fault injection, shared with update_inner's handle-table step.)
        let mut ht_freed: Vec<u64> = Vec::new();
        let ht_new_root = match self.handle_table_insert_candidate(handle, &entry, &mut ht_freed) {
            Ok(r) => r,
            Err(e) => {
                self.abort_allocate_prepare(saved_ht_root, saved_ht_depth, inline_page);
                return Err(e);
            }
        };

        // REVERSE map: compute the new membership-index root; do NOT install
        // yet. tag 0 = untagged (never indexed).
        let mut mi_new_root: Option<u64> = None;
        let mut mi_freed: Vec<u64> = Vec::new();
        if tag != 0 {
            // Test-only injection (see `fail_next_membership_op`): simulate a
            // non-fatal CacheFull at the reverse-map step so the regression test
            // exercises the REAL failure handling below. No production artifact:
            // the non-test `let res` is the only one compiled outside tests.
            #[cfg(test)]
            let res: Result<u64> = if self.inject_membership_failure() {
                Err(ChiselError::CacheFull { limit: 0 })
            } else {
                self.membership_insert_candidate(tag, handle, &mut mi_freed)
            };
            #[cfg(not(test))]
            let res: Result<u64> = self.membership_insert_candidate(tag, handle, &mut mi_freed);

            match res {
                Ok(r) => mi_new_root = Some(r),
                Err(e) => {
                    // The handle-table insert above already succeeded and may
                    // have grown the tree (bumping the in-memory depth) and
                    // produced a candidate root we are now discarding. Unwind the
                    // whole prepare so current_roots, the depth, and the inline
                    // value's slot all return to their pre-allocate state.
                    self.abort_allocate_prepare(saved_ht_root, saved_ht_depth, inline_page);
                    return Err(e);
                }
            }
        }

        // INSTALL phase (infallible): both maps move together, and only now is
        // the handle id consumed and the superseded spine pages queued for
        // reclamation at commit.
        self.current_roots.next_handle += 1;
        self.current_roots.handle_table_page = ht_new_root;
        self.txn_freed_pages.append(&mut ht_freed);
        if let Some(root) = mi_new_root {
            self.current_roots.membership_index_page = root;
            self.txn_freed_pages.append(&mut mi_freed);
        }

        Ok(handle)
    }
}
