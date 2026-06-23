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
        // there is no separate freemap working copy to reset here. The hint is
        // untracked (a stale hint only costs a scan), so it is left as-is too.
        // The session-owned set is strictly per-transaction: a page COW'd last
        // transaction is now committed and must NOT be mutated in place, so start
        // empty. (begin already requires no active txn, so it is normally empty,
        // but clear defensively.)
        self.freemap_session_owned.clear();
        // Seed the structural reuse pool from the prior commit's deferred dead
        // freemap pages: those superblock-unreferenced pages are now safe to
        // reuse as this transaction's freemap COW targets, so the freemap rotates
        // among a bounded set instead of extending. CLONE (not move) so
        // `pending_structural_frees` stays intact as the rollback fallback — a
        // rolled-back transaction never reached commit, so its structural recycle
        // is exactly the pre-transaction one. `commit_inner` overwrites it on the
        // success path. `structural_superseded` is empty here (only
        // persist_freemap fills it); clear defensively.
        self.structural_reuse = self.pending_structural_frees.clone();
        self.structural_superseded.clear();
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
        // I27: flatten every still-active savepoint's `freed_pages`
        // back into `txn_freed_pages` before persist_freemap consumes
        // it. savepoint_inner moves `txn_freed_pages` INTO the
        // savepoint record (via std::mem::take), so any frees that
        // happened before a still-unreleased savepoint otherwise get
        // dropped on the floor when step 5 calls `savepoints.clear()`
        // — a permanent freemap leak for the "commit with savepoint
        // active" pattern. Mirrors `release_inner`'s merge but applied
        // across the full stack. We take the lists out of the
        // savepoints (rather than iterating by reference) so the
        // savepoints hold no stale `freed_pages` if we ever change
        // step 5 to drain instead of clear; current code is equivalent
        // either way.
        for sp in self.savepoints.iter_mut() {
            self.txn_freed_pages.append(&mut sp.freed_pages);
        }

        // I28: drain the page cache BEFORE persist_freemap runs. Without
        // this, `persist_freemap`'s own freemap-page allocation
        // (`structural_extend` → `new_page`) can trip
        // `maybe_evict`'s spill-or-CacheFull decision (every existing entry
        // dirty, nothing evictable, and either spillway disabled or full)
        // and return `ChiselError::CacheFull` or `ChiselError::SpillwayFull`.
        // The CacheFull variant is operational-by-design (I19 docs: "caller
        // recovers by
        // committing or rolling back"), but commit's poison wrapper fires
        // on any error once the protocol has started — demoting an
        // operational signal to fatal for a caller who has no legal
        // action left (commit is precisely what failed). Pre-draining
        // clears every dirty pin so the ceiling is reachable via normal
        // eviction when persist_freemap itself allocates. Cost: one
        // extra fsync on every commit. That is consistent with the
        // project's explicit "durability over performance" posture —
        // the alternative reclassifies CacheFull as fatal inside commit,
        // which is both more surprising and harder to document cleanly.
        //
        // Ordering note: this flush is safe to do before persist_freemap.
        // The shadow-paging invariant requires "new-freemap-page durable
        // before superblock" (step 1's flush does that). The pre-drain
        // only affects user-dirty pages, which are already part of the
        // transaction's durable write set — just flushed earlier. The
        // subsequent step 1 flush handles the one new freemap page
        // persist_freemap adds.
        self.cache.borrow_mut().flush()?;

        // Step 0 (ISSUES.md R2 / I11): persist the freemap tree. This marks
        // `txn_freed_pages` (plus the prior commit's deferred structural frees)
        // free in a COW of the committed tree and updates
        // `current_roots.{freemap_page, freemap_depth}`. Runs BEFORE the main
        // flush so the new freemap pages join the same durable write set as all
        // other dirty data pages.
        self.persist_freemap()?;

        // Hold one RefMut for the remaining steps. Dropping and
        // re-borrowing between steps would be semantically identical
        // but noisier.
        let mut cache = self.cache.borrow_mut();

        // Step 1: Flush all dirty pages (PageCache::flush internally fsyncs).
        // After this, every page the new superblock will reference is on disk.
        cache.flush()?;

        // Step 2: Build the new superblock. Bumping txn_counter here both makes
        // it outrank the current superblock on recovery AND (via parity) picks
        // the target slot in step 3.
        //
        // I119 (ISSUES.md, 2026-06-21): checked, not `+= 1`. A wrapped counter
        // would corrupt `Superblock::select`'s "highest counter wins" (release
        // wrap to 0) — far worse than the loud, controlled panic here. Overflow
        // needs 2^64 commits, so it is structurally unreachable; a dedicated
        // fatal error variant for an impossible event would be speculative
        // public surface, so the `expect` on the invariant is proportionate.
        self.txn_counter = self
            .txn_counter
            .checked_add(1)
            .expect("txn_counter overflowed u64 (2^64 commits) — unreachable");
        let total_pages = cache.file_page_count()?;
        let sb = Superblock {
            magic: page::MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: self.txn_counter,
            root_handle_table_page: self.current_roots.handle_table_page,
            root_freemap_page: self.current_roots.freemap_page,
            total_pages,
            next_handle: self.current_roots.next_handle,
            page_size: PAGE_SIZE as u32,
            named_roots: self.current_roots.named_roots,
            // R4: every slot records the current N so open-time
            // recovery can discover it from the winning slot without
            // external hints.
            superblock_count: self.superblock_count,
            root_membership_index_page: self.current_roots.membership_index_page,
            // Freemap tree depth, paired with root_freemap_page. 0 = today's
            // single-leaf format; grows as the tree deepens.
            freemap_depth: self.current_roots.freemap_depth,
        };
        let buf = sb.serialize();
        // Step 3: Write to the INACTIVE slot. For N superblock slots,
        // the slot is `txn_counter % N` — a round-robin that always
        // targets the stalest slot. With N=2 this is the parity
        // alternation from the original layout; with N>=3 it extends
        // to true round-robin. The currently-active slot (and every
        // other non-target slot) is never touched, so a torn write
        // here can only damage the new superblock, never the N-1
        // last-known-good ones.
        let inactive = self.txn_counter % self.superblock_count as u64;
        cache.io_mut().write_page(inactive, &buf)?;
        // Step 4: Durability linearization point. Until this fsync returns the
        // transaction is not crash-safe; after it returns the new state is
        // observable on recovery.
        cache.io_mut().fsync()?;

        // Step 5: Promote in-memory state. Only now is the txn officially committed.
        self.committed_roots = self.current_roots.clone();
        self.committed_roots.total_pages = total_pages;
        // The committed freemap tree advances automatically: its {root, depth}
        // ride in current_roots, promoted into committed_roots just above. No
        // separate in-memory freemap copy to advance.
        // R1: promote the live-slot counts. The cursor is per-transaction
        // and gets reset for the next begin().
        self.packer.commit();
        self.active_txn = false;
        self.savepoints.clear();
        // txn_freed_pages were already marked free in the new committed freemap
        // tree by persist_freemap; clear the vector now that it's done its job.
        self.txn_freed_pages.clear();
        // Every freemap page COW'd this transaction is now committed; the next
        // transaction must COW (not edit in place) any of them it touches.
        self.freemap_session_owned.clear();
        // Promote the freemap structural recycle for the next transaction: the
        // pages this commit superseded (`structural_superseded`) become dead the
        // instant the superblock flips above — and the reuse-pool remainder
        // (`structural_reuse` ids not consumed as COW targets) is likewise still
        // dead and reusable. Both become next transaction's `pending_structural_frees`.
        self.pending_structural_frees.clear();
        self.pending_structural_frees
            .append(&mut self.structural_superseded);
        self.pending_structural_frees
            .append(&mut self.structural_reuse);

        Ok(())
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
        // truncate. The hint is untracked, so nothing to revert.
        //
        // `pending_structural_frees` is left intact: begin() CLONED it into
        // `structural_reuse` rather than moving it, so it still holds the
        // pre-transaction dead-freemap-page set — correct, since a rolled-back
        // transaction's structural recycle is exactly the pre-transaction one.
        // We DISCARD the in-transaction structural working state:
        //   * `structural_superseded` holds committed-tree freemap pages this
        //     aborted transaction COW'd-over; the abort means the committed tree
        //     still references them, so they are NOT dead and must never be
        //     recycled.
        //   * `structural_reuse` was the working copy; drop it.
        //   * the session-owned set: any freemap pages this aborted transaction
        //     COW'd sit above the watermark and were just truncated, so their ids
        //     must not be treated as in-place-mutable next transaction.
        self.structural_superseded.clear();
        self.structural_reuse.clear();
        self.freemap_session_owned.clear();
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
