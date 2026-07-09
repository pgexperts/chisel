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
        // R1: snapshot the live-slot map and the cursor. Also drop the
        // cursor in the active scope — once a savepoint exists, the
        // insert path stops packing into the cursor (same posture as
        // freemap reuse: savepoints disable the optimization so the
        // rollback_to semantics stay simple).
        let (live_slots, insert_cursor) = self.packer.snapshot();
        self.packer.clear_cursor();
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            roots: self.current_roots.clone(),
            watermark,
            freed_pages: std::mem::take(&mut self.txn_freed_pages),
            live_slots,
            insert_cursor,
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
        // R1: restore live-slot counts and cursor from the savepoint
        // snapshot. The cursor was force-cleared when the savepoint was
        // created, so this sets the cursor back to whatever value it
        // held BEFORE the savepoint was taken (typically also None,
        // since savepoint-bearing transactions disable packing).
        let snap = (
            self.savepoints[idx].live_slots.clone(),
            self.savepoints[idx].insert_cursor,
        );
        self.packer.restore(snap);
        self.savepoints.truncate(idx + 1);
        self.txn_freed_pages.clear();

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
