//! transaction::savepoints — nested savepoint scopes: savepoint /
//! rollback_to / release (+ their `_inner` cores). Split out of
//! `transaction.rs` verbatim; see the parent module for the type and fields.

use super::*;

impl TransactionManager {
    /// Push a named savepoint onto the stack. Captures the current
    /// `next_page_id` watermark so `rollback_to(name)` can truncate the
    /// cache back to this exact point. `freed_pages` is moved INTO the
    /// savepoint record so the enclosing transaction's `txn_freed_pages`
    /// accumulates only frees from the savepoint's own scope.
    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.savepoint_inner(name);
        self.poison_on_fatal(result)
    }

    fn savepoint_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if self.savepoints.iter().any(|sp| sp.name == name) {
            return Err(ChiselError::DuplicateSavepoint(name.to_string()));
        }
        let watermark = self.cache_watermark();
        // R1: snapshot the live-slot map. The cursor is NOT snapshotted — it
        // is simply dropped, here and again on restore, because once a
        // savepoint exists the insert path must stop packing into it (same
        // posture as freemap reuse: savepoints disable the optimization so the
        // rollback_to semantics stay simple). Snapshotting it is what
        // TXN-COMMIT-1 was; see `SlotPacker::restore`.
        let live_slots = self.packer.snapshot();
        self.packer.clear_cursor();
        // Issue #112: empty the within-transaction recycle pool into the
        // ordinary free queue BEFORE the `mem::take` below moves that queue into
        // the savepoint record. Same posture as freemap reuse and slot packing —
        // a savepoint scope turns the optimization off — but here it is required,
        // not merely simplifying: the pool must be provably empty for the whole
        // scope, so that `rollback_to` (which can only rewind the cache above the
        // watermark) never has to undo a write into a pooled page allocated
        // below it.
        //
        // Queueing rather than discarding is the pre-#112 behaviour restored
        // exactly: every id in the pool was superseded before the roots snapshot
        // taken on the next line, so it is dead under this savepoint too, and
        // `txn_freed_pages` is where such pages went before the pool existed.
        self.txn_pages.drain_into(&mut self.txn_freed_pages);
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            roots: self.current_roots.clone(),
            watermark,
            freed_pages: std::mem::take(&mut self.txn_freed_pages),
            live_slots,
            // GAP-1: capture the freemap recycle's savepoint-scoped state too.
            // rollback_to was the only rewind path that never rewound it.
            freemap: self.freemap.savepoint_mark(),
        });
        Ok(())
    }

    /// Roll back to a named savepoint without ending the transaction.
    /// Truncates the cache to the savepoint's watermark (discarding every
    /// page allocated after the savepoint), restores the roots snapshot,
    /// and pops any savepoints layered on top. The named savepoint itself
    /// remains on the stack and can be rolled back to again or released.
    ///
    /// NOTE: `freed_pages` from savepoints layered on top (and from
    /// `self.txn_freed_pages`) are dropped here, which is correct —
    /// those frees never became durable, and the roots/page contents
    /// those frees described have been rewound along with the cache
    /// truncate. Post-R2, `commit()` DOES return freed pages to the
    /// freemap; this rollback path simply discards the unfinished
    /// accounting.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.rollback_to_inner(name);
        self.poison_on_fatal(result)
    }

    fn rollback_to_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let idx = self
            .savepoints
            .iter()
            .position(|sp| sp.name == name)
            .ok_or_else(|| ChiselError::SavepointNotFound(name.to_string()))?;

        let watermark = self.savepoints[idx].watermark;
        self.cache.borrow_mut().truncate(watermark)?;

        self.current_roots = self.savepoints[idx].roots.clone();
        // C1: re-derive outer_depth from the restored savepoint root (see rollback_inner).
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                RadixU64::recover_depth(&mut cache, self.current_roots.membership_index_page)?;
            self.membership_index.set_outer_depth(depth);
        }
        // I99: re-derive handle-table depth from the restored savepoint root
        // (same rationale as rollback_inner / outer_depth).
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                HandleTable::recover_depth(&mut cache, self.current_roots.handle_table_page)?;
            self.handle_table.set_depth(depth);
        }
        // R1: restore live-slot counts from the savepoint snapshot.
        // `SlotPacker::restore` also clears the insert cursor — the savepoint
        // below survives this rewind (`truncate(idx + 1)`), and packing into a
        // below-watermark page while a savepoint is live is exactly what
        // TXN-COMMIT-1 was. See `SlotPacker::restore`.
        self.packer.restore(self.savepoints[idx].live_slots.clone());
        // GAP-1: rewind the freemap recycle's savepoint-scoped streams. The
        // restored roots re-reference the pages `structural_superseded` names,
        // and the cache truncate above destroyed the COW targets in
        // `session_owned`; leaving either standing is the live-page overwrite
        // that `FreemapRecycle::rollback`'s doc forbids.
        // Cloned, not moved: the savepoint survives the truncate below and can
        // be rolled back to again, so its mark has to stay intact.
        let mark = self.savepoints[idx].freemap.clone();
        self.freemap.rollback_to_mark(mark);
        self.savepoints.truncate(idx + 1);
        self.txn_freed_pages.clear();
        // Issue #112: the recycle pool must be empty here, and this asserts it
        // rather than assuming it. `savepoint_inner` drained it on the way in
        // and `retire_superseded` refuses to refill it while any savepoint is
        // open, so the only way an entry could exist is if one of those two
        // gates had been moved — and a pooled entry surviving a rewind is
        // exactly the hazard the drain exists to prevent. `truncate(idx + 1)`
        // above keeps this savepoint on the stack, so the scope (and the gate)
        // continues past this call.
        debug_assert!(
            self.txn_pages.recyclable().is_empty(),
            "recycle pool must stay empty inside a savepoint scope"
        );

        Ok(())
    }

    /// Release (flatten) a named savepoint and everything layered on top
    /// of it. Under watermark-based rollback, this is just `savepoints
    /// .truncate(idx)` plus a merge of freed-page lists — the released
    /// savepoints' allocated pages remain reachable via the outer
    /// watermark (i.e. `committed_roots.total_pages`), which is still the
    /// correct rollback destination for the enclosing transaction.
    pub fn release(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.release_inner(name);
        self.poison_on_fatal(result)
    }

    fn release_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let idx = self
            .savepoints
            .iter()
            .position(|sp| sp.name == name)
            .ok_or_else(|| ChiselError::SavepointNotFound(name.to_string()))?;

        // Merge freed_pages from all released savepoints back into the
        // current transaction's list. This preserves the invariant that
        // txn_freed_pages holds every "frees that would go to the freemap
        // on commit" across the entire enclosing transaction, so a later
        // rollback correctly drops them.
        let mut merged_freed = Vec::new();
        for sp in self.savepoints[idx..].iter() {
            merged_freed.extend_from_slice(&sp.freed_pages);
        }
        merged_freed.append(&mut self.txn_freed_pages);

        self.savepoints.truncate(idx);
        self.txn_freed_pages = merged_freed;

        Ok(())
    }
}
