//! transaction::lifecycle — transaction state machine and poison
//! machinery: begin / commit / rollback (+ their `_inner` cores),
//! `check_alive` / `poison_on_fatal` / `is_poisoned` /
//! `force_poison_for_test` / `is_active`. Split out of `transaction.rs`
//! verbatim; see the parent module for the type and fields.

use super::*;

impl TransactionManager {
    // --- Poison machinery (ISSUES.md I1) ---
    //
    // Every public entry point below follows the same wrapper pattern:
    //
    //     pub fn foo(&mut self, ...) -> Result<T> {
    //         self.check_alive()?;          // fast path: refuse if already poisoned
    //         let result = self.foo_inner(...);
    //         self.poison_on_fatal(result)  // poison iff the inner call returned a fatal error
    //     }
    //
    // commit() is the one exception: ANY error from the commit protocol
    // poisons (not just fatal variants), because partial-commit state is
    // fragile enough that we do not trust the in-memory view after a
    // half-finished commit even if the variant would otherwise be
    // operational. See commit() for the full reasoning.

    /// Returns Err(Poisoned) if the manager has previously seen a fatal
    /// error. Called at the top of every public entry point. Cheap.
    ///
    /// Takes `&self` because the poison flag lives in a `Cell<bool>`
    /// (F3: `read()` takes `&self`, and read paths must also check/set
    /// the flag).
    pub(super) fn check_alive(&self) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        Ok(())
    }

    /// Inspect a Result and set the poison flag if it contains a fatal
    /// error. Returns the Result unchanged so the caller can `?` or return
    /// it. Never fires on an Ok or on an operational error.
    ///
    /// Takes `&self` (not `&mut self`) because the flag is a `Cell` —
    /// essential for the `&self`-taking read paths under F3.
    pub(super) fn poison_on_fatal<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(ref e) = result {
            if e.is_fatal() {
                self.poisoned.set(true);
            }
        }
        result
    }

    /// Force the manager into the poisoned state. Test-only hook used by
    /// the I1 regression test to avoid needing a real I/O failure injection.
    #[cfg(test)]
    pub fn force_poison_for_test(&self) {
        self.poisoned.set(true);
    }

    /// True if this manager has been poisoned by a previous fatal error.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.get()
    }

    /// Begin a new transaction.
    ///
    /// Single-writer: returns TransactionAlreadyActive if one is already in
    /// flight. current_roots is reseeded from committed_roots so that any prior
    /// (aborted) in-progress state is discarded. The dirty/freed bookkeeping is
    /// cleared — this is the only place (besides commit/rollback) those vectors
    /// are zeroed, so callers must not rely on them surviving a begin().
    pub fn begin(&mut self) -> Result<()> {
        self.check_alive()?;
        let result = self.begin_inner();
        self.poison_on_fatal(result)
    }

    fn begin_inner(&mut self) -> Result<()> {
        // Fail fast on read-only mounts so callers don't build up
        // transaction state only to hit a ReadOnlyMode at the first
        // write_page call during commit.
        if self.cache.borrow().io().is_read_only() {
            return Err(ChiselError::ReadOnlyMode);
        }
        if self.active_txn {
            return Err(ChiselError::TransactionAlreadyActive);
        }
        self.current_roots = self.committed_roots.clone();
        // The freemap root+depth ride in current_roots (cloned just above), so
        // there is no separate freemap working copy to reset here. Reset the
        // recycle's per-transaction state: clear the session set and reseed the
        // structural reuse pool from the prior commit's deferred dead freemap
        // pages. The hint persists across transactions but is SNAPSHOTTED here
        // so rollback can rewind it — a hint left too high after a rollback
        // strands every free id below it (FREEMAP-1, issue #107); only the
        // too-low direction is the harmless "costs a scan" case.
        // See `FreemapRecycle::begin`.
        self.freemap.begin();
        // R1: clone the live-slot counts and reset the insert cursor.
        // The cursor is always None at begin — it only tracks pages
        // allocated during the current transaction.
        self.packer.begin();
        self.active_txn = true;
        self.savepoints.clear();
        self.txn_freed_pages.clear();
        Ok(())
    }

    /// Durably commit the active transaction.
    ///
    /// Commit protocol — ORDERING IS LOAD-BEARING. Each numbered step encodes a
    /// specific crash-safety guarantee; reordering any of them can lose data or
    /// expose torn state on recovery.
    ///
    /// A commit issues THREE fsyncs, not two: a pre-drain (step 0) plus the two
    /// numbered below. Both pre-drain and step 1 are part of the "all data
    /// durable BEFORE the superblock" phase — the pre-drain just moves some of
    /// that flushing earlier; the superblock fsync (step 4) is the second phase.
    ///
    /// 0. Pre-drain the page cache (I28). BEFORE step 1, `commit_inner` flushes
    ///    the cache once so that `persist_freemap`'s own page allocation cannot
    ///    trip the spill / `CacheFull` ceiling mid-commit (which would poison on
    ///    an operational error). This is the FIRST of the three fsyncs. See the
    ///    I28 comment in `commit_inner` for why it is conditional-safe.
    ///
    /// 1. Flush all dirty data pages to disk AND fsync.
    ///    PageCache::flush() writes every dirty page then calls fsync(). After
    ///    this returns, every page the new superblock will reference is durable
    ///    on the storage medium. WHY FIRST: the new superblock is the pointer
    ///    that makes these pages "live". If we wrote the superblock before the
    ///    data pages were durable and crashed, recovery would pick up a
    ///    superblock whose root_handle_table_page points into a page whose
    ///    contents were never persisted — corruption with a valid checksum on
    ///    the superblock but garbage at the referenced page.
    ///
    /// 2. Compute the new superblock in memory.
    ///    Bump txn_counter first so (a) the new superblock outranks the old one
    ///    via Superblock::select()'s max_by_key, and (b) `txn_counter %
    ///    superblock_count` selects which slot to overwrite (step 3). For N=2
    ///    this is the original parity alternation; for N>=3 (R4) it is true
    ///    round-robin across all N slots. total_pages is queried from the file
    ///    AFTER flush() so any new_page() allocations are reflected.
    ///
    /// 3. Write the new superblock to the INACTIVE slot.
    ///    The target is `txn_counter % superblock_count`, which always
    ///    points at the stalest slot. The N-1 other slots (including the
    ///    previously-active one, at counter txn_counter-1) are untouched
    ///    and still hold valid superblocks at strictly smaller counters.
    ///    WHY: if we crash during this write, the target slot may be torn
    ///    (bad checksum) but every other slot still holds the last
    ///    committed state (or earlier ones). Recovery picks the highest
    ///    surviving valid counter and the transaction is simply lost —
    ///    never half-applied. Overwriting an active slot in place would
    ///    be catastrophic: a torn write there could destroy a valid
    ///    superblock. Higher N buys survival of CONSECUTIVE torn writes
    ///    to the same target slot on retry (see `create_new` docstring).
    ///
    /// 4. fsync the superblock write.
    ///    This is the LINEARIZATION POINT of the commit. Before this fsync the
    ///    transaction is not durable, even if write_page returned; the kernel
    ///    may still be holding the superblock page in its buffer cache. After
    ///    this fsync returns successfully, a crash-and-recover will observe the
    ///    new state. A SINGLE fsync (combining data pages and superblock) would
    ///    be unsafe because the OS is free to reorder writes within an fsync
    ///    boundary — the superblock could reach the disk before the data pages
    ///    it references, creating a window where a crash leaves a valid-looking
    ///    superblock pointing at non-durable data.
    ///
    /// 5. Update in-memory committed_roots and clear txn state.
    ///    Only after the superblock fsync succeeds do we promote current_roots
    ///    to committed_roots. If ANY step in the protocol fails the manager is
    ///    poisoned (see the I1 block below) — active_txn / committed_roots are
    ///    left untouched but no public API will accept further calls; the only
    ///    legal recovery is close + reopen, which picks the last-durable
    ///    superblock via `Superblock::select`. Retry-in-place is forbidden
    ///    because a half-committed state (dirty flags already cleared in the
    ///    cache, txn_counter possibly bumped, target slot possibly torn on
    ///    disk) cannot be safely continued, and Linux fsyncgate semantics make
    ///    re-calling fsync() after a failed fsync unsafe regardless.
    pub fn commit(&mut self) -> Result<()> {
        self.check_alive()?;
        // Special poison policy for commit: we refuse BOTH operational and
        // fatal errors that arise after the commit protocol has started.
        // The operational NoActiveTransaction case is checked BEFORE any
        // protocol state is touched, so it stays operational and does not
        // poison. But once cache.flush() has run, any subsequent error —
        // even an otherwise operational one — leaves the manager in a
        // partial-commit state (dirty flags cleared in the cache, counter
        // possibly bumped, superblock possibly torn on disk) that cannot be
        // safely continued. Under Linux fsyncgate semantics a failed fsync
        // cannot be retried at all, so we poison and force the caller to
        // reopen.
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn commit_inner(&mut self) -> Result<()> {
        // The 3-fsync commit protocol lives in `commit::run_commit`, operating
        // over the twelve pieces of manager state it touches via `CommitCtx`. The
        // ordering there is load-bearing (see the step-by-step rationale on
        // `commit()` above and in `commit.rs`); this stays a thin caller. All
        // `&mut self.<field>` borrows are distinct fields and `&self.cache` is a
        // shared borrow, so the borrow checker accepts the simultaneous borrows.
        commit::run_commit(&mut commit::CommitCtx {
            cache: &self.cache,
            savepoints: &mut self.savepoints,
            txn_freed_pages: &mut self.txn_freed_pages,
            freemap: &mut self.freemap,
            packer: &mut self.packer,
            committed_roots: &mut self.committed_roots,
            current_roots: &mut self.current_roots,
            txn_counter: &mut self.txn_counter,
            active_txn: &mut self.active_txn,
            superblock_count: self.superblock_count,
            cipher: self.cipher.as_ref(),
            crypto_header: self.crypto_header.as_ref(),
        })
    }

    /// Abort the active transaction and discard all in-memory changes.
    ///
    /// Uses watermark-based rollback (ISSUES.md I3): `cache.truncate` is
    /// called with `committed_roots.total_pages`, which both drops every
    /// cache entry for pages allocated during the transaction AND truncates
    /// the file back to its pre-transaction size. This fixes the earlier
    /// bug where rollback would leave zeroed trailing pages in the file
    /// because the cache-level discard did not propagate to `ftruncate`.
    ///
    /// Because `PageCache::new_page()` hands out monotonically increasing
    /// ids, the pre-transaction watermark cleanly separates "pages that
    /// existed at begin() time" (< watermark, preserved) from "pages
    /// allocated during this transaction" (>= watermark, discarded). No
    /// per-page tracking list is required.
    pub fn rollback(&mut self) -> Result<()> {
        self.check_alive()?;
        let result = self.rollback_inner();
        self.poison_on_fatal(result)
    }

    fn rollback_inner(&mut self) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // Rollback the cache in two steps:
        //   (a) Discard every dirty entry. This catches pages REUSED from
        //       the freemap whose id is less than the watermark — the
        //       watermark-based truncate below only catches extended
        //       pages. After discard, the next read for such a page id
        //       will re-load the last-committed content from disk, which
        //       is exactly the pre-transaction state. Safe because
        //       `flush()` (commit) always clears dirty flags, so any
        //       dirty entry was created in the current transaction.
        //   (b) Truncate to committed_roots.total_pages. This rewinds
        //       next_page_id AND shrinks the file, dropping every page
        //       allocated via extension (id >= watermark). Together with
        //       (a), this returns the cache and file to their exact
        //       pre-transaction state.
        {
            let mut cache = self.cache.borrow_mut();
            cache.discard_all_dirty();
            cache.truncate(self.committed_roots.total_pages)?;
        }

        self.current_roots = self.committed_roots.clone();
        // C1: MembershipIndex.outer_depth is in-memory state that index grows
        // mutate during the transaction, but it is NOT carried in Roots, so the
        // snapshot restore above does not rewind it. Re-derive it from the (now
        // committed) root — mirroring the open-time recovery — so the in-memory
        // descent depth matches the page it descends. Otherwise handles_with_tag
        // mis-descends a rolled-back-shallow root with a stale-deep depth.
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                RadixU64::recover_depth(&mut cache, self.current_roots.membership_index_page)?;
            self.membership_index.set_outer_depth(depth);
        }
        // I99: HandleTable.depth is the same kind of in-memory radix-depth cache
        // as outer_depth above -- mutated by grows, not carried in Roots -- so it
        // must also be re-derived from the restored root. Otherwise a rolled-back
        // handle-table grow leaves the descent depth too deep and lookups
        // mis-descend, returning InvalidHandle for committed handles.
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                HandleTable::recover_depth(&mut cache, self.current_roots.handle_table_page)?;
            self.handle_table.set_depth(depth);
        }
        // The freemap root+depth were restored by `current_roots =
        // committed_roots.clone()` above; any dirty freemap pages this
        // transaction COW'd sit above the watermark and were dropped by the
        // truncate. Discard the in-transaction structural working state
        // (`structural_superseded` + `structural_reuse` + the session set) while
        // leaving `pending_structural_frees` intact as the pre-transaction
        // baseline. See `FreemapRecycle::rollback` for the per-stream reasoning.
        self.freemap.rollback();
        // R1: revert the live-slot counts and drop the insert cursor.
        self.packer.rollback();
        self.active_txn = false;
        self.savepoints.clear();
        self.txn_freed_pages.clear();
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active_txn
    }
}
