//! transaction::freemap — freemap staging and structural-page recycling:
//! the freemap-aware COW allocator (`cow_alloc` / `structural_extend`),
//! `persist`, orphan reclamation, and the transient-tree trio that the
//! data-page / handle-table / membership COW allocation paths use.
//!
//! The mutable recycle/commit state lives in `FreemapRecycle`, an owned unit
//! held by `TransactionManager` (one field, `freemap`). It bundles the five
//! pieces of structural-recycle + freemap-hint state that move together across
//! begin/commit/rollback. `TransactionManager` reaches the freemap through this
//! type's narrow surface (the trio + the commit-path methods + the lifecycle
//! hooks); see the struct doc below for the recycle model.
//!
//! `cow_alloc` and `structural_extend` stay FREE functions (not methods) so the
//! `#[cfg(test)]` `record_structural_reuse` hook the recycle pin-tests depend on
//! sits at a stable, easily-targeted seam; the `FreemapRecycle` methods wrap
//! them, passing the recycle's own fields by `&mut`.

use super::*;

/// Freemap-aware page allocator shared by data-page allocation and the
/// handle-table / membership-index COW paths.
///
/// When `reuse_enabled`, it asks the freemap `tree` for the lowest free id
/// at/above `*hint` (clearing its bit via a COW so it cannot be handed out
/// twice), falling back to extending the file via `PageCache::new_page`.
/// `reuse_enabled` is false while savepoints are active (R2: savepoint scopes
/// disable freemap reuse to keep `rollback_to` semantics simple) — matching the
/// historical `allocate_data_page` behavior, which this also routes through.
///
/// The tree's own COW of the claimed leaf supersedes pages, which the caller
/// drains from `tree.pending_superseded` into `structural_superseded` after the
/// call (see `FreemapRecycle::put_tree`).
///
/// LAZY-CREATE GUARD: a fresh database has `tree.root == PAGE_ID_NONE` (no tree
/// materialized yet). `PAGE_ID_NONE` is `u64::MAX`, NOT the tree's internal
/// zero-child sentinel, so `allocate_first` would try to read page u64::MAX and
/// error rather than reporting "nothing free". We short-circuit that here: a
/// None-root tree holds nothing reusable, so we fall straight through to
/// `new_page`. The tree is first materialized when a page is *freed* (see
/// `FreemapRecycle::persist`), never on the allocation side.
///
/// Pages freed during the CURRENT transaction live in `txn_freed_pages` and are
/// NOT in the committed tree until commit, so `allocate_first` can never hand
/// back a page still referenced by the live tree (the I18 invariant). Routing
/// handle-table and membership COW allocation through here — rather than the
/// monotonic `new_page` — is what lets those structures reach a bounded
/// steady-state page count ACROSS COMMITS instead of leaking one page per
/// mutation forever.
///
/// HANDLES-INDEX-2 (issue #112): that bound used not to extend WITHIN a single
/// transaction, for exactly the reason in the paragraph above — nothing this
/// transaction frees reaches the committed bitmap before commit, so a long
/// transaction's handle-table and membership churn marched the file high-water
/// up monotonically. `pool` closes that: it is a within-transaction supply of
/// pages this transaction both allocated and superseded, drawn from BEFORE the
/// committed bitmap. See `TxnPageRecycle` for the invariant that makes handing
/// one back safe.
pub(super) fn cow_alloc(
    cache: &mut PageCache,
    tree: &mut FreeMapTree,
    hint: &mut u64,
    structural_reuse: &mut Vec<u64>,
    pool: &mut TxnPageRecycle,
    reuse_enabled: bool,
) -> Result<u64> {
    // Pool BEFORE bitmap, for three independent reasons: a pooled page needs no
    // freemap COW to claim (the bitmap path COWs a leaf per claim), it is space
    // no other transaction could have used anyway, and draining it here is what
    // keeps the pool bounded instead of letting it accumulate until commit.
    //
    // Same `reuse_enabled` gate as the bitmap path, which is
    // `savepoints.is_empty()` at every call site. That is not incidental
    // sharing: it is half of the pool's savepoint argument (see
    // `TxnPageRecycle`).
    if reuse_enabled {
        if let Some(id) = pool.draw(cache)? {
            pool.record(id);
            return Ok(id);
        }
    }
    if reuse_enabled && tree.root != PAGE_ID_NONE {
        // `allocate_first` claims a free DATA page (clearing its bit), which COWs
        // the freemap leaf. That leaf COW's structural `extend` reuses a dead
        // freemap page from `structural_reuse` before extending the file — what
        // keeps the freemap from marching the file upward one page per commit.
        let mut extend = |c: &mut PageCache| structural_extend(c, structural_reuse);
        if let Some(id) = tree.allocate_first(cache, hint, &mut extend)? {
            cache.claim_page(id)?;
            pool.record(id);
            return Ok(id);
        }
    }
    let id = cache.new_page()?;
    pool.record(id);
    Ok(id)
}

/// Within-transaction recycle pool for COW targets (HANDLES-INDEX-2, issue
/// #112). Owned by `TransactionManager` as the single `txn_pages` field.
///
/// THE PROBLEM. `cow_alloc` reuses only pages that are free in the COMMITTED
/// freemap bitmap. Everything this transaction supersedes queues in
/// `txn_freed_pages` and does not reach the bitmap until `persist` runs at
/// commit, so N mutations against a depth-d handle table allocate N*(d+1)
/// distinct pages, dirty all of them, and reclaim none until the transaction
/// ends. A bulk load in one transaction grows the file by millions of pages
/// whose contents nothing will ever read.
///
/// WHAT THIS IS NOT. Issue #112 proposed giving both radices the freemap tree's
/// `session_owned` dedup: re-touch a page this transaction already COW'd IN
/// PLACE. That corrupts data here, and the two tripwire tests
/// (`rollback_to_savepoint_undoes_a_later_allocate_in_the_same_transaction`,
/// `a_failed_tagged_allocate_leaves_the_handle_table_untouched`) exist to fail
/// if anyone tries it. In-place mutation breaks two things the freemap tree
/// never has to survive:
///
///   * every mutation path computes a CANDIDATE radix root and discards it on a
///     later fallible step by restoring a saved ROOT ID. Mutated in place the id
///     is unchanged, so the restore restores nothing and the discarded write
///     stays.
///   * `rollback_to` truncates only above the savepoint watermark, so an
///     in-place write into a page allocated BEFORE the savepoint survives the
///     rewind as a phantom entry.
///
/// The freemap tree escapes both only because nothing mutates it inside a
/// savepoint scope and its mutations have no candidate/discard step. Neither
/// escape transfers.
///
/// THE INVARIANT. A page may be handed back as a COW target only if NO
/// RESTORABLE SNAPSHOT references it. There are exactly two kinds: the roots
/// the last durable superblock names (`committed_roots` — where `rollback` and
/// crash recovery land), and each open savepoint's `roots` clone (where
/// `rollback_to` lands). Membership requires BOTH halves, and each half retires
/// one of them:
///
///   * THIS TRANSACTION ALLOCATED IT — recorded by `record`, called on every id
///     `cow_alloc` returns. The base cases are `FreeMapTree::allocate_first`,
///     which by the I18 invariant only ever yields an id whose bit is FREE in
///     the committed bitmap (so no committed structure references it), and
///     `PageCache::new_page`, which is monotonic above the begin watermark (so
///     the id did not exist when the last superblock was written). A pooled
///     draw is the inductive case: it was already recorded. Hence
///     `committed_roots` cannot reference a pooled page.
///   * THIS TRANSACTION SUPERSEDED IT — `retire` is fed ONLY from a `freed` vec
///     at an INSTALL site, i.e. after the replacement root has been written into
///     `current_roots`. Hence `current_roots` cannot reference it either. This
///     is also what makes the candidate/discard hazard structurally absent: a
///     discarded candidate's `freed` vec is dropped, never retired, so it
///     contributes nothing.
///
/// Savepoint roots are retired by gating BOTH the feed (`retire`) and the draw
/// (`cow_alloc`) on `savepoints.is_empty()`, and by draining the pool into
/// `txn_freed_pages` when a savepoint is pushed. So while any savepoint is open
/// the pool is empty and inert, and a page a savepoint's roots reference can
/// never be in it: it would have to have been superseded while that savepoint
/// was open (feed disabled) or before it existed (in which case the savepoint's
/// later roots clone cannot name it, and the drain had already emptied the pool
/// regardless).
///
/// THE STREAM SPLIT, which is the easiest thing to get wrong. A page routed to
/// `recyclable` must NOT also be queued in `txn_freed_pages`: `persist` would
/// mark a page free that the pool has since handed out and made live again —
/// the I18 hazard in new dress. `retire` therefore assigns each freed id to
/// exactly one stream. Symmetrically, whatever is still in `recyclable` at
/// commit IS genuinely dead and must be appended to `txn_freed_pages` before
/// `persist` runs, or the space leaks. Both halves live in `commit.rs`.
pub(super) struct TxnPageRecycle {
    /// Every page id `cow_alloc` has returned this transaction. This is the
    /// "we allocated it" half of the invariant, and it is a SET rather than a
    /// watermark comparison because `cow_alloc` has two base sources: ids above
    /// the begin watermark (`new_page`) and ids below it (committed-free bits).
    /// Both are safe; only an explicit record distinguishes them from a genuine
    /// committed page.
    ///
    /// Cleared at begin/commit/rollback. NOT cleared by `rollback_to`: an entry
    /// for a page the rewind truncated is stale but harmless, because `retire`
    /// re-checks the other half (the page was just superseded from the LIVE
    /// tree) on every use, and "this transaction allocated it" cannot become
    /// false mid-transaction — `committed_roots` is frozen until commit.
    allocated: FxHashSet<u64>,
    /// Dead-this-transaction pages available as COW targets, LIFO. Kept small
    /// by construction: each mutation supersedes about as many pages as it
    /// allocates, and the draw happens before the feed, so the pool oscillates
    /// around one root-to-leaf spine rather than growing with the transaction.
    recyclable: Vec<u64>,
}

impl TxnPageRecycle {
    /// Fresh pool for a newly-opened manager.
    pub(super) fn new() -> Self {
        TxnPageRecycle {
            allocated: FxHashSet::default(),
            recyclable: Vec::new(),
        }
    }

    /// Record an id `cow_alloc` is handing out. Called on ALL THREE of its
    /// sources (pool draw, committed-bitmap claim, file extension) so the
    /// "we allocated it" half of the invariant needs no case analysis at the
    /// `retire` end. Recording a data page costs 8 bytes and buys nothing today
    /// — data-page frees go straight to `txn_freed_pages` via
    /// `release_data_slot` — but a conditional here would be one more thing a
    /// future allocation site could get wrong.
    fn record(&mut self, id: u64) {
        self.allocated.insert(id);
    }

    /// Take the next COW target from the pool, or `None` when it is empty.
    ///
    /// The reissue happens BEFORE the pop, deliberately. `reclaim_dead_txn_page`
    /// is fallible (`maybe_evict` can raise `CacheFull` / `SpillwayFull`), and
    /// leaving the id in the pool on that path keeps it reachable — by a later
    /// draw, or by the commit-time drain into `txn_freed_pages`. Popping first
    /// would strand the page in neither stream, leaking it if the caller
    /// commits after the operational error. Reclaiming an id whose earlier
    /// reclaim failed is harmless: the page is still dead and the reissue is
    /// idempotent.
    fn draw(&mut self, cache: &mut PageCache) -> Result<Option<u64>> {
        let Some(&id) = self.recyclable.last() else {
            return Ok(None);
        };
        cache.reclaim_dead_txn_page(id)?;
        self.recyclable.pop();
        Ok(Some(id))
    }

    /// Route one INSTALL site's superseded pages to the correct reclamation
    /// stream. `pool_enabled` is `savepoints.is_empty()` — see the savepoint
    /// paragraph on the struct.
    ///
    /// Each id lands in EXACTLY ONE stream. That is the whole safety property
    /// of this function: an id in both would be marked free by `persist` while
    /// the pool had already handed it out and made it live again.
    fn retire(&mut self, freed: &mut Vec<u64>, txn_freed_pages: &mut Vec<u64>, pool_enabled: bool) {
        if !pool_enabled {
            txn_freed_pages.append(freed);
            return;
        }
        for id in freed.drain(..) {
            if self.allocated.contains(&id) {
                // A page cannot be superseded twice without being re-allocated
                // in between (once superseded, no descent from `current_roots`
                // can reach it), and re-allocation goes through `draw`, which
                // pops it. So this cannot fire — but if it ever did, the pool
                // would hand the same live page out twice. The scan is
                // debug-only and the pool holds about one root-to-leaf spine.
                debug_assert!(
                    !self.recyclable.contains(&id),
                    "page {id} superseded twice without an intervening allocation; \
                     the recycle pool would hand it out twice"
                );
                self.recyclable.push(id);
            } else {
                txn_freed_pages.push(id);
            }
        }
    }

    /// Move the whole pool into `txn_freed_pages`. Used at commit (the
    /// remainder is genuinely dead and must reach `persist` or it leaks) and
    /// when a savepoint is pushed (the pool must be inert inside a savepoint
    /// scope, and its contents were superseded before the savepoint's roots
    /// snapshot, so queueing them as ordinary frees is exactly the
    /// pre-#112 behaviour).
    pub(super) fn drain_into(&mut self, txn_freed_pages: &mut Vec<u64>) {
        txn_freed_pages.append(&mut self.recyclable);
    }

    /// Drop all per-transaction state. Called at begin, commit, and rollback —
    /// every point at which `committed_roots` and the allocation watermark stop
    /// being the ones the recorded ids were judged against. A surviving entry
    /// would be an id this transaction did NOT allocate, which is precisely the
    /// state that lets a pooled page still be referenced by a durable
    /// superblock.
    pub(super) fn reset(&mut self) {
        self.allocated.clear();
        self.recyclable.clear();
    }

    /// The pool's current contents. Not test-gated: `rollback_to_inner` asserts
    /// on it in every debug build, which is where the savepoint half of the
    /// invariant is actually checked rather than merely argued.
    pub(super) fn recyclable(&self) -> &[u64] {
        &self.recyclable
    }
}

// Verification hook (tests only): every page id drawn from `structural_reuse`
// as a freemap-COW target is recorded here, so the recycle pin-tests can assert
// the one-commit defer (a reused id was promoted by a PRIOR commit, never one
// this transaction itself superseded). A thread-local keeps the production
// `structural_extend` signature and both inline pop sites untouched; the
// recording calls are `#[cfg(test)]` no-ops in release builds. The single-writer
// model means at most one manager drives this per thread at a time.
#[cfg(test)]
thread_local! {
    static STRUCTURAL_REUSE_LOG: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_structural_reuse(id: u64) {
    STRUCTURAL_REUSE_LOG.with(|log| log.borrow_mut().push(id));
}

/// Drain and return every structural-reuse pop recorded since the last drain.
#[cfg(test)]
pub(super) fn take_structural_reuse_log() -> Vec<u64> {
    STRUCTURAL_REUSE_LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
}

/// Structural-page allocator for the freemap tree's COW: reuse a dead freemap
/// page (deferred from a prior commit, now safe to overwrite) before extending
/// the file. NEVER draws from the freemap's own free bits — that would re-COW a
/// leaf and recurse — preserving the extend-only termination guarantee while
/// bounding steady-state growth. `claim_page` evicts any stale cache entry for
/// the reused id before the COW writes its fresh contents.
fn structural_extend(cache: &mut PageCache, structural_reuse: &mut Vec<u64>) -> Result<u64> {
    if let Some(id) = structural_reuse.pop() {
        #[cfg(test)]
        record_structural_reuse(id);
        cache.claim_page(id)?;
        Ok(id)
    } else {
        cache.new_page()
    }
}

/// The savepoint-scoped slice of `FreemapRecycle` state, captured by
/// `savepoint_mark` and put back by `rollback_to_mark`. Stored in `Savepoint`
/// alongside the cache watermark and the roots snapshot.
///
/// Two fields, for two different reasons — see `savepoint_mark` for why
/// `structural_reuse` is not among them.
///
/// `Clone` because `rollback_to` may be called repeatedly against the same
/// savepoint (the engine keeps the mark on the stack), so the record's copy
/// must outlive each restore.
#[derive(Debug, Clone)]
pub(super) struct FreemapMark {
    /// Length of `structural_superseded` at savepoint time. Truncating back to
    /// it drops exactly the entries accumulated since, because the vector is
    /// append-only within a transaction.
    superseded_len: usize,
    /// Full copy of `session_owned` at savepoint time. An `FxHashSet<u64>`
    /// clone per savepoint — smaller than the `live_slots` `FxHashMap` clone
    /// the same code path already takes.
    session_owned: FxHashSet<u64>,
}

/// Owns the structural-page recycle cluster and the freemap commit/alloc/persist
/// machinery — the crash-durability backbone of the engine. Held by
/// `TransactionManager` as the single `freemap` field.
///
/// The recycle exists to bound the extend-only freemap's growth (ISSUES.md I18,
/// generalized to the multi-page tree). A freemap mutation (data-alloc-side leaf
/// COW, or persist's frees) must COW the committed freemap pages it touches — it
/// can never overwrite a page the last-durable superblock still references — and
/// each COW supersedes an OLD freemap page. Those old pages ROTATE through a
/// small pool instead of marching the file upward ~1/commit:
///
///   * superseded THIS transaction -> collected in `structural_superseded`;
///   * promoted at commit -> `pending_structural_frees` (deferred one commit,
///     since the old page is still referenced until the superblock flips);
///   * reused NEXT transaction -> `begin` clones `pending_structural_frees` into
///     `structural_reuse`, which every structural `extend` (freemap COW target)
///     pops from before extending the file.
///
/// Reusing a DEAD page (vs. a free bit in the tree) preserves the extend-only
/// TERMINATION guarantee: no freemap mutation ever draws structural space from
/// the freemap's own bits. These freemap-page frees are NOT data-reusable (never
/// enter `txn_freed_pages`): a freed freemap page sits at a high id where the
/// lowest-first data allocator would starve it, so routing it back as structural
/// reuse — where demand matches supply at steady state — is what reclaims it.
pub(super) struct FreemapRecycle {
    // Best-effort lower bound on the lowest free page id in the committed freemap
    // tree, threaded into `FreeMapTree::allocate_first` so a scan starts near the
    // answer instead of at id 0. `allocate_first` advances it on every claim.
    //
    // The two directions are NOT symmetric, and the old doc here claimed they
    // were ("a too-low hint only costs a wasted left-to-right scan, never
    // correctness ... so it needs no begin/rollback snapshotting"). Too LOW is
    // indeed free: the scan still returns the true lowest free id. Too HIGH
    // strands every free id below it — `mark_free_committed_path` five lines
    // down says as much ("a too-high hint would start the next scan above `id`
    // and never reuse it"), and it is the only thing that ever lowers the hint,
    // reachable solely from commit and the defrag orphan sweep.
    //
    // FREEMAP-1 (issue #107): rollback produced exactly the too-high case.
    // Committed free ids {10, 20, 30}; a transaction claims 10 then 20, leaving
    // hint = 20; rollback restores `current_roots` so both are free again in
    // the committed tree — but the hint stayed at 20, so id 10 was never handed
    // out again and the allocator extended the file instead. Recovery required
    // a later free at an id <= 10, or a reopen. A long-lived session with
    // rollbacks and only high-id frees leaked reusable space monotonically.
    //
    // Now snapshotted at `begin` and restored at `rollback`. Still not reset
    // between successful transactions — across a COMMIT the hint remains a
    // valid lower bound, which is the part of the original argument that held.
    hint: u64,
    // The `hint` value as of the last `begin`, restored by `rollback`. One u64,
    // no on-disk change. Preferred over `hint = 0` on rollback because the
    // begin-value is exactly correct by induction (rollback restores the very
    // committed tree that value was a valid bound for), and it avoids a full
    // left-to-right rescan after every aborted transaction.
    hint_at_begin: u64,
    // The dead-freemap-page pool available to reuse as structural COW targets in
    // the CURRENT transaction. Seeded from `pending_structural_frees` at `begin`;
    // drained by every structural `extend`; the unconsumed remainder is carried
    // forward (back into `pending_structural_frees`) at commit. On rollback it is
    // cleared (begin re-clones `pending_structural_frees` next time).
    structural_reuse: Vec<u64>,
    // This transaction's freemap-COW supersedes (old freemap pages this txn
    // replaced). Accumulated as transient handles drain `tree.pending_superseded`
    // here via `put_tree`; promoted to `pending_structural_frees` at commit (the
    // one-commit defer). Dropped on rollback (those COWs are truncated above the
    // watermark).
    structural_superseded: Vec<u64>,
    // Dead freemap pages carried BETWEEN commits — the committed-baseline recycle.
    // `begin` CLONES (not moves) this into `structural_reuse`, so it stays intact
    // as the rollback fallback; `commit` overwrites it with this commit's
    // superseded + reuse remainder. See the struct doc for the full rotation.
    pending_structural_frees: Vec<u64>,
    // Freemap pages already COW'd/extended by the CURRENT transaction. Because the
    // manager rebuilds a transient `FreeMapTree` handle at every allocation site
    // (data-page alloc, each HT/membership COW, persist), this set is what lets
    // those handles share the "first touch this txn => COW, later touches =>
    // in-place" discipline: without it every site would re-COW the same freemap
    // leaf, turning reclamation into unbounded file growth. Moved into each
    // transient handle and read back out (see `take_tree` / `put_tree`). Cleared
    // at begin/commit/rollback so the next transaction starts empty — a stale
    // entry pointing at a now-committed page would be a CORRECTNESS bug (it would
    // suppress a needed COW and mutate a live committed page in place).
    session_owned: FxHashSet<u64>,
}

impl FreemapRecycle {
    /// Fresh recycle for a newly-opened manager: hint at 0, empty pools/set.
    pub(super) fn new() -> Self {
        FreemapRecycle {
            hint: 0,
            hint_at_begin: 0,
            structural_reuse: Vec::new(),
            structural_superseded: Vec::new(),
            pending_structural_frees: Vec::new(),
            session_owned: FxHashSet::default(),
        }
    }

    // --- The transient-tree trio ---
    //
    // Each allocation site (data-page alloc, every HT/membership COW, persist,
    // and the orphan sweep) materializes a transient `FreeMapTree` from the
    // committed roots, threads it through one downstream `insert`/`delete`/`mark`,
    // then writes it back. `take_tree` MOVES the session set into the handle so a
    // leaf an earlier site already COW'd is treated as in-place-mutable; `put_tree`
    // moves it back out and drains the handle's COW supersedes into
    // `structural_superseded`. Never drop a handle from `take_tree` without a
    // matching `put_tree`, or the session set is lost and later sites re-COW.

    /// Build a transient `FreeMapTree` handle from the current freemap roots,
    /// MOVING `session_owned` into it (so already-COW'd pages are in-place this
    /// transaction). Pair with `put_tree`.
    pub(super) fn take_tree(&mut self, roots: &Roots) -> FreeMapTree {
        let mut tree = FreeMapTree::from_roots(roots.freemap_page, roots.freemap_depth);
        tree.session_owned = std::mem::take(&mut self.session_owned);
        tree
    }

    /// Write a transient handle's grown root/depth back into `roots`, move its
    /// session-owned set back into the recycle, and drain its COW-superseded
    /// freemap pages into `structural_superseded` (the one-commit defer stream —
    /// NOT `txn_freed_pages`, since freed freemap pages are recycled as structural
    /// reuse, not as data frees).
    pub(super) fn put_tree(&mut self, roots: &mut Roots, mut tree: FreeMapTree) {
        roots.freemap_page = tree.root;
        roots.freemap_depth = tree.depth;
        self.structural_superseded
            .append(&mut tree.pending_superseded);
        self.session_owned = std::mem::take(&mut tree.session_owned);
    }

    /// Wrap the free `cow_alloc` with the recycle's own hint + structural-reuse
    /// pool. The `tree` MUST persist across the whole downstream `insert` (insert
    /// may call this several times on the one tree; that shared `session_owned`
    /// accumulation is load-bearing), so this takes `&mut tree` rather than
    /// owning it.
    ///
    /// `txn_pages` is the DATA/radix-side recycle (`TxnPageRecycle`), a
    /// different pool from this type's `structural_reuse` and passed in rather
    /// than owned: the two must not be confused. `structural_reuse` holds dead
    /// FREEMAP pages deferred one commit and is drawn from by `structural_extend`
    /// only; `txn_pages` holds pages this TRANSACTION superseded and is drawn
    /// from by `cow_alloc` only. Crossing them would let a freemap COW draw
    /// space from the structure it is recording, which is the recursion the
    /// extend-only rule exists to prevent.
    pub(super) fn cow_alloc_into(
        &mut self,
        cache: &mut PageCache,
        tree: &mut FreeMapTree,
        txn_pages: &mut TxnPageRecycle,
        reuse: bool,
    ) -> Result<u64> {
        cow_alloc(
            cache,
            tree,
            &mut self.hint,
            &mut self.structural_reuse,
            txn_pages,
            reuse,
        )
    }

    // --- Commit-path machinery ---

    /// Mark a single page id free in the working freemap tree, routing every
    /// structural COW target through the pooled `structural_extend` (reuse a dead
    /// freemap page before extending the file) and lazily materializing the
    /// depth-0 root on first use. Lowers `hint` to cover `id` so the next
    /// `allocate_first` scan can reach it.
    ///
    /// The ONE marking path shared by `persist` (this commit's data frees) and
    /// `reclaim_orphans` (the defrag orphan-sweep). Both must flow through the
    /// same COW + recycle discipline so the structural reuse pool and supersede
    /// streams stay consistent; a second marking implementation could silently
    /// diverge from the one-commit-defer crash-safety the recycle depends on.
    /// Take/put the tree per call: the session-owned set and the reuse pool persist
    /// on the recycle across calls, so a multi-id loop still COWs each leaf at most
    /// once (the session dedup carries across handles).
    pub(super) fn mark_free_committed_path(
        &mut self,
        cache: &mut PageCache,
        roots: &mut Roots,
        id: u64,
    ) -> Result<()> {
        // Take the working handle WITH the transaction's session set so a leaf an
        // earlier call (or this commit's data allocations) already COW'd is
        // recognized as in-place here, not re-COW'd.
        let mut tree = self.take_tree(roots);
        // RefCell so the structural-`extend` closure can drain the shared reuse
        // pool by `&mut` while the rest of the method still holds `&mut self`.
        let structural_reuse = std::cell::RefCell::new(std::mem::take(&mut self.structural_reuse));
        let result = (|| {
            let mut extend =
                |c: &mut PageCache| structural_extend(c, &mut structural_reuse.borrow_mut());

            // Lazy materialization: a database that has never freed a page has no
            // tree yet (root == PAGE_ID_NONE). Create the depth-0 leaf now, before
            // marking, since `mark_free_growing` needs a real root to COW.
            // Preserve the session set across the swap.
            if tree.root == PAGE_ID_NONE {
                let session = std::mem::take(&mut tree.session_owned);
                tree = FreeMapTree::create(cache, &mut extend)?;
                tree.session_owned.extend(session);
            }
            tree.mark_free_growing(cache, id, &mut extend)
        })();
        // Pull the hint back to cover `id`: the hint advances monotonically via
        // `allocate_first`, so a too-high hint would start the next scan above
        // `id` and never reuse it. A too-low hint only costs a wasted scan.
        // (Mirrors the oracle proptest's `hint = hint.min(id)`.)
        self.hint = self.hint.min(id);
        // Return the (partly drained) reuse pool and write the tree back even on
        // error: its COW supersedes flow to `structural_superseded` via put_tree;
        // commit promotes structural_superseded + the leftover reuse pool into
        // pending_structural_frees (the one-commit defer).
        self.structural_reuse = structural_reuse.into_inner();
        self.put_tree(roots, tree);
        result
    }

    // Persist the freemap tree at commit time (ISSUES.md R2 / I11 / I18,
    // generalized to the multi-page COW tree).
    //
    // Called once at the very start of `commit_inner`, BEFORE cache.flush(), so
    // the freemap pages it COWs join the same durable write set as every other
    // dirty page this transaction produced.
    //
    // TWO FREE-STREAMS (the load-bearing distinction the reviewer scrutinizes):
    //
    //   * `txn_freed_pages` (DATA frees, passed in) — pages freed by this commit's
    //     data/handle-table/membership COW supersedes. Recorded as FREE in this
    //     commit's new freemap tree, so the NEXT transaction's data/HT
    //     allocations can reuse them. Safe to mark now: the new tree becomes
    //     authoritative only when this commit's superblock flips, by which point
    //     these pages are genuinely dead.
    //
    //   * `structural_superseded` / `pending_structural_frees` /
    //     `structural_reuse` (FREEMAP-page frees) — the freemap tree's OWN COW
    //     supersedes. These are NOT marked free in the tree: a freemap page sits
    //     at a high id where the lowest-first data allocator would starve it, and
    //     marking a freemap page free inside the tree that is recording frees
    //     could cascade. Instead they ride the separate recycle described on the
    //     struct.
    //
    // I18 ORDERING preserved by construction. The structural COW never draws a
    // page from the freemap's own free bits (that would re-COW a leaf and
    // recurse); it only ever extends the file or reuses a DEAD page from a prior
    // commit (one no durable superblock still references). So a to-be-freed id
    // can never be handed back to record these same frees — the I18 window
    // cannot open. `persist_freemap_does_not_reuse_committed_live_pages` is the
    // guardrail.
    //
    // DEPTH-0 EQUIVALENCE. With one leaf this reduces to: COW the leaf once
    // (reusing the prior commit's dead leaf id when available, else extend), set
    // the freed bits, defer the old leaf to the structural recycle. Steady-state
    // page count matches the pre-tree single-page freemap.
    /// Mark this commit's DATA frees (`txn_freed_pages`) free in a COW of the
    /// committed tree, via the shared marking path. Each call take/puts the tree,
    /// but the session-owned set persists on the recycle, so a leaf hit by several
    /// frees is COW'd once.
    ///
    /// The `is_empty` early return is a FAST PATH, not a gate. This method marks
    /// DATA frees and does nothing else, so an empty `txn_freed_pages` leaves the
    /// loop below a no-op anyway — returning early is behaviourally identical to
    /// falling through. In particular it says NOTHING about the structural
    /// streams, which are routinely non-empty with zero data frees: `begin` seeds
    /// `structural_reuse` from the prior commit's deferred frees, and ANY freemap
    /// COW fills `structural_superseded` — the defrag orphan sweep does exactly
    /// that (`reclaim_orphans` -> `mark_free_committed_path` -> `cow_descend` ->
    /// `put_tree`) without ever touching `txn_freed_pages`.
    ///
    /// So do NOT fold the stream promotion into this method, and do not condition
    /// it on this check. `FreemapRecycle::commit` promotes both streams
    /// unconditionally; gating that on data frees would drop a commit's dead
    /// freemap pages out of the recycle, turning bounded steady-state freemap
    /// churn back into unbounded file growth.
    pub(super) fn persist(
        &mut self,
        cache: &mut PageCache,
        roots: &mut Roots,
        txn_freed_pages: &[u64],
    ) -> Result<()> {
        if txn_freed_pages.is_empty() {
            return Ok(());
        }
        for id in txn_freed_pages.iter().copied() {
            self.mark_free_committed_path(cache, roots, id)?;
        }
        Ok(())
    }

    /// Reclaim freemap-typed pages orphaned by a crash that lost the in-memory
    /// recycle pool. The structural recycle (decision 6 of the design) is held
    /// only in memory, so a crash strands its entries: `FreeMap`/`FreeMapInterior`
    /// pages that are no longer reachable from the committed tree and were never
    /// marked free in the bitmap (a bounded handful — the last commit's structural
    /// supersedes). This sweep walks the live tree to find the reachable set,
    /// scans the file for freemap-typed pages that are neither reachable nor
    /// already free, and marks each free — routing the mark through the SAME
    /// `mark_free_committed_path` the commit uses (COW + recycle), so a reclaimed
    /// orphan lands in the BITMAP (data-reusable), disjoint from the in-memory
    /// recycle pool. Returns the count reclaimed.
    ///
    /// MUST run inside an active transaction: it advances `roots.freemap_page`
    /// and pushes committed-LIVE ids into `structural_superseded`, neither of
    /// which has a meaning — or a promotion path — outside one. This function
    /// cannot see `active_txn`; the `TransactionManager::reclaim_freemap_orphans`
    /// wrapper enforces it and is the only sanctioned caller. Do not add a second
    /// entry point that skips the wrapper.
    ///
    /// `savepoint_active` is `!savepoints.is_empty()` from the caller — when true
    /// the sweep is a no-op (returns 0). The sweep is the ONLY path that COWs the
    /// freemap (draining committed-LIVE pages into the structural streams) while a
    /// savepoint is open; ordinary allocation already disables structural reuse
    /// under a savepoint. Historically this bail was the ONLY thing standing
    /// between that and silent durable freemap corruption, because
    /// `rollback_to` rewound the roots and the cache watermark but not the
    /// structural streams — a page the sweep drained into
    /// `structural_superseded` survived the rollback, got promoted at commit,
    /// and was reused as a COW target while the last-durable superblock still
    /// referenced it.
    ///
    /// GAP-1 (issue #107) closed that: `rollback_to_inner` now rewinds
    /// `structural_superseded` and `session_owned` via the savepoint's
    /// `FreemapMark`. This bail is therefore defence in depth rather than the
    /// sole guarantee. The residual it still covers is `structural_reuse`,
    /// which the mark deliberately does not capture — costing a bounded,
    /// self-healing leak rather than corruption (see `savepoint_mark`).
    ///
    /// THE EXCLUSION SET (get this exactly right): a page in the CURRENT in-memory
    /// recycle pool (`structural_reuse` ∪ `structural_superseded` ∪
    /// `pending_structural_frees`) is LIVE recycling state, NOT an orphan —
    /// reclaiming it into the bitmap while it is also pool-reusable would
    /// double-hand-out the page. After a crash the pool is empty, so the
    /// crash-orphaned pages are correctly flagged; in a normal (no-crash) defrag
    /// the live pool is excluded so the two reclamation channels never overlap.
    ///
    /// There is a FOURTH stream of unreachable-but-not-free pages since #112 —
    /// `TxnPageRecycle::recyclable` — and it is deliberately NOT in the set
    /// below, because it cannot reach this walk in the first place. A pooled
    /// page is dirty by construction, and the walk reads through `cache.get`
    /// (cache and spillway before disk), so it always presents this
    /// transaction's radix-typed or zeroed content, never the stale on-disk
    /// bytes that would make it look like a freemap page. That is the whole
    /// argument, and it has a dependency worth stating: if this walk ever
    /// cold-reads the file, a pooled page whose DISK bytes are a stale
    /// `FreeMap` (freed in an earlier commit, re-allocated this transaction via
    /// `allocate_first`, not yet flushed) would be swept, its bit freed
    /// mid-transaction, and `allocate_first` could then hand out a page the
    /// pool has already made live. Add it to the exclusion set before making
    /// this walk bypass the cache.
    ///
    /// Reading each non-reachable page through the cache checksum-verifies it.
    /// A page that fails because it is GARBAGE/corrupt (`CorruptPage` /
    /// `ChecksumMismatch`) is SKIPPED, not propagated (2026-06-22 review:
    /// "skip unreadable dead pages") — a non-reachable page we cannot read
    /// cannot be confirmed as a freemap orphan, and a dead page's corruption is
    /// irrelevant to correctness. Any OTHER read error (e.g. `IoError`, a real
    /// device fault) is propagated and poisons, preserving fail-closed for true
    /// hardware faults. The LIVE-tree walk (`reachable_pages`) still propagates
    /// fatal on a corrupt LIVE node — only the dead-page scan is softened. The
    /// scan is O(total_pages) I/O — off the hot path (defrag), bounded, and
    /// acceptable.
    pub(super) fn reclaim_orphans(
        &mut self,
        cache: &mut PageCache,
        roots: &mut Roots,
        savepoint_active: bool,
        superblock_count: u32,
    ) -> Result<u64> {
        if savepoint_active {
            return Ok(0);
        }
        let root = roots.freemap_page;
        let depth = roots.freemap_depth;
        if root == PAGE_ID_NONE {
            return Ok(0); // no tree yet => no freemap pages can be orphaned
        }

        // Pages that are NOT orphans even though unreachable + not-free: the
        // STRUCTURAL recycle pool (all three of its streams). The fourth stream,
        // `TxnPageRecycle::recyclable`, is excluded by construction rather than
        // by membership here — see "THE EXCLUSION SET" above for why, and for
        // what would change that.
        let mut excluded: FxHashSet<u64> = FxHashSet::default();
        excluded.extend(self.structural_reuse.iter().copied());
        excluded.extend(self.structural_superseded.iter().copied());
        // Belt-and-suspenders: `begin` clones `pending_structural_frees` into
        // `structural_reuse`, so every id here is already covered by the
        // `structural_reuse` term above. Kept explicitly so the exclusion remains
        // correct if `begin`'s seeding ever changes.
        excluded.extend(self.pending_structural_frees.iter().copied());

        // Collect orphan ids, then mark them. The original dropped and re-borrowed
        // the cache between the collection and mark phases; with `cache` a param we
        // hold one continuous borrow — sequential use is equivalent (the mark path
        // does not read any cache state the collection left mid-update).
        let tree = FreeMapTree::from_roots(root, depth);
        let mut orphans: Vec<u64> = Vec::new();
        // Upper bound: the allocation high-water (`next_page_id`), NOT the
        // committed `total_pages`. After a real crash + reopen these are equal
        // (open seeds next_page_id from the committed superblock), and every orphan
        // — a structural supersede from a committed transaction — sits below it.
        // Using next_page_id also covers a page extended earlier in THIS session
        // (e.g. the forge-orphan test), which a stale committed total_pages would
        // miss.
        let total = cache.next_page_id();
        let reachable = tree.reachable_pages(cache)?;
        // Pages 0..superblock_count are superblocks; start the scan above them.
        for id in superblock_count as u64..total {
            if reachable.contains(&id) || excluded.contains(&id) {
                continue;
            }
            // Skip a non-reachable page that is GARBAGE/corrupt rather than letting
            // it poison the whole maintenance pass (2026-06-22 review: "skip
            // unreadable dead pages"). A page not in the live tree cannot be
            // confirmed as a freemap orphan if we cannot read its type, and a DEAD
            // page's corruption does not affect correctness — so on
            // `CorruptPage`/`ChecksumMismatch` we `continue`. We deliberately
            // PROPAGATE every other read error (e.g. `IoError`): a real device
            // fault should still surface and poison. NOTE: the live-tree walk
            // (`reachable_pages` above) still propagates fatal on a corrupt LIVE
            // page — only the dead-page scan is softened.
            let buf = match cache.get(id) {
                Ok(buf) => buf,
                Err(ChiselError::CorruptPage { .. } | ChiselError::ChecksumMismatch { .. }) => {
                    continue;
                }
                Err(e) => return Err(e),
            };
            let ty = buf[0];
            if (ty == crate::page::PageType::FreeMap as u8
                || ty == crate::page::PageType::FreeMapInterior as u8)
                && !tree.is_free(cache, id)?
            {
                orphans.push(id);
            }
        }
        // Mark each orphan free through the shared committed-marking path (COW +
        // recycle), landing them in the bitmap as data-reusable space.
        for id in &orphans {
            self.mark_free_committed_path(cache, roots, *id)?;
        }
        Ok(orphans.len() as u64)
    }

    // --- Lifecycle (operate on own fields; EXACT semantics from lifecycle.rs) ---

    /// At transaction begin: start with an empty session set, and seed the reuse
    /// pool from the prior commit's deferred dead freemap pages (those
    /// superblock-unreferenced pages are now safe to reuse as this transaction's
    /// freemap COW targets). CLONE (not move) `pending_structural_frees` so it
    /// stays intact as the rollback fallback — a rolled-back transaction never
    /// reached commit, so its structural recycle is exactly the pre-transaction
    /// one; `commit` overwrites it on the success path. `structural_superseded` is
    /// empty here (only `persist` fills it); clear defensively. `hint` is not
    /// RESET (across a commit it stays a valid lower bound) but it IS
    /// snapshotted, so `rollback` can put it back — see FREEMAP-1 on the field.
    pub(super) fn begin(&mut self) {
        self.session_owned.clear();
        self.structural_reuse = self.pending_structural_frees.clone();
        self.structural_superseded.clear();
        self.hint_at_begin = self.hint;
    }

    /// At commit: every freemap page COW'd this transaction is now committed
    /// (clear the session set). Promote the structural recycle for the next
    /// transaction: the pages this commit superseded (`structural_superseded`)
    /// become dead the instant the superblock flips, and the reuse-pool remainder
    /// (`structural_reuse` ids not consumed as COW targets) is likewise still dead
    /// and reusable. Both become next transaction's `pending_structural_frees`.
    /// Order: clear pending, then append superseded, then append reuse.
    pub(super) fn commit(&mut self) {
        self.session_owned.clear();
        self.pending_structural_frees.clear();
        self.pending_structural_frees
            .append(&mut self.structural_superseded);
        self.pending_structural_frees
            .append(&mut self.structural_reuse);
    }

    /// At rollback: discard the in-transaction structural working state.
    /// `structural_superseded` holds committed-tree freemap pages this aborted
    /// transaction COW'd-over; the abort means the committed tree still references
    /// them, so they are NOT dead and must never be recycled. `structural_reuse`
    /// was the working copy; drop it.
    ///
    /// The session set is cleared because both kinds of COW target this
    /// transaction produced are gone — but by two DIFFERENT mechanisms, and the
    /// truncate alone does NOT suffice:
    ///
    ///   * pages this transaction EXTENDED sit at/above the rollback watermark
    ///     and die with `cache.truncate(committed_roots.total_pages)`;
    ///   * pages popped from `structural_reuse` are prior-commit ids that lie
    ///     BELOW that watermark by construction (each was allocated before the
    ///     commit that superseded it), so the truncate — whose filter is
    ///     `id >= n` — cannot reach them. `cache.discard_all_dirty()`, which
    ///     `rollback_inner` runs FIRST, is what drops their contents.
    ///
    /// Either kind left in the set would suppress a needed COW next transaction,
    /// over a page whose in-transaction contents no longer exist. Re-offering a
    /// pooled id next transaction is nonetheless correct: `pending_structural_
    /// frees` is untouched here, the page is still dead, and `begin` re-clones
    /// it into the pool.
    ///
    /// The savepoint sibling is safe only by coincidence. `savepoint_mark`'s
    /// GAP-1 note below already records WHICH gates supply that coincidence
    /// (`cow_alloc`'s `reuse_enabled = savepoints.is_empty()`, `reclaim_orphans`
    /// bailing on `savepoint_active`, `persist` being commit-only); what is not
    /// recorded there is the CONSEQUENCE of moving one, which is specific to
    /// this path: `rollback_to_inner` (savepoints.rs) calls `truncate` alone,
    /// with no `discard_all_dirty`, so it would leave post-savepoint bytes dirty
    /// at a below-watermark pooled id — the one case the truncate cannot reach.
    ///
    /// `pending_structural_frees` is NOT touched — `begin` CLONED it into
    /// `structural_reuse` rather than moving it, so it still holds the
    /// pre-transaction dead-freemap-page set (correct: a rolled-back transaction's
    /// structural recycle is exactly the pre-transaction one).
    /// `hint` is rewound to its begin value: this transaction's `allocate_first`
    /// calls advanced it past ids that the roots-restore just made free again,
    /// and a too-high hint strands every free id below it (FREEMAP-1).
    pub(super) fn rollback(&mut self) {
        self.structural_superseded.clear();
        self.structural_reuse.clear();
        self.session_owned.clear();
        self.hint = self.hint_at_begin;
    }

    /// Capture the savepoint-scoped part of the recycle state, for
    /// `rollback_to` to restore. Paired with `rollback_to_mark`.
    ///
    /// GAP-1 (issue #107): `rollback_to` was the only rewind path that never
    /// touched `FreemapRecycle`, while the full-rollback sibling calls
    /// `rollback()`. What that leaves behind is the residue those two streams
    /// exist to prevent — a `structural_superseded` entry naming a page the
    /// restored roots still reference (promoted at the next commit and then
    /// handed out as a COW target, overwriting a live committed page), and a
    /// `session_owned` entry naming a COW target the cache truncate destroyed
    /// (suppressing a needed COW).
    ///
    /// Unreachable today only by coincidence: every freemap-tree mutation site
    /// passes `reuse = savepoints.is_empty()` for the unrelated reason of
    /// allocation-reuse simplicity, and `reclaim_orphans` bails on
    /// `savepoint_active` — so nothing mutates the tree inside a savepoint
    /// scope. Nothing recorded that those gates were load-bearing for savepoint
    /// correctness. This makes the rewind real instead of incidental.
    ///
    /// `structural_reuse` is deliberately NOT captured — because capturing it
    /// would buy nothing, NOT because restoring it would be dangerous. (An
    /// earlier version of this comment claimed the latter, arguing that a
    /// target consumed BEFORE the savepoint could be re-offered and hand out a
    /// live page twice. That is wrong, and wrong in a load-bearing direction,
    /// so it is worth stating why: a mark-time snapshot pairs atomically with
    /// the mark-time roots, so an entry consumed before the savepoint was
    /// already popped and cannot be IN the snapshot. The only entries a restore
    /// could re-offer are ones consumed AFTER the savepoint, and those live in
    /// tree nodes the rewind discards.)
    ///
    /// The real reason is that nothing consumes `structural_reuse` inside a
    /// savepoint scope at all: `structural_extend` is reached only via
    /// `cow_alloc` under `reuse_enabled`, which is `savepoints.is_empty()` at
    /// every call site. Should that ever change, the cost of not capturing is a
    /// bounded leak, not corruption — the consumed id is still in
    /// `pending_structural_frees` (untouched mid-transaction), so the next
    /// `begin` re-offers it, and the defrag orphan sweep is the backstop.
    pub(super) fn savepoint_mark(&self) -> FreemapMark {
        FreemapMark {
            superseded_len: self.structural_superseded.len(),
            session_owned: self.session_owned.clone(),
        }
    }

    /// Restore the state captured by `savepoint_mark`.
    ///
    /// `structural_superseded` is truncated rather than cleared: it is
    /// append-only within a transaction (`put_tree` is the only writer; only
    /// commit/rollback drain it), so the prefix below `superseded_len` is
    /// exactly the pre-savepoint content and must survive — those entries name
    /// pages superseded before the savepoint, which the rewind does not undo.
    ///
    /// `session_owned` is restored from a snapshot rather than cleared
    /// wholesale. Clearing would be SAFE (it can only cause an extra COW, never
    /// suppress a needed one) but lossy in a way that weakens an invariant: a
    /// re-COW of a page this same transaction already COW'd pushes an
    /// UNCOMMITTED id onto `structural_superseded`, contradicting that field's
    /// documented meaning of "committed-tree freemap pages this txn COW'd
    /// over". Every pre-savepoint entry is below the savepoint watermark (ids
    /// come from monotonic `new_page` or from prior-commit `structural_reuse`),
    /// so all of them survive the truncate and the snapshot is exactly right.
    pub(super) fn rollback_to_mark(&mut self, mark: FreemapMark) {
        self.structural_superseded.truncate(mark.superseded_len);
        self.session_owned = mark.session_owned;
    }

    // --- Test-only accessors (the recycle pin-tests read/forge these) ---

    #[cfg(test)]
    pub(super) fn structural_reuse(&self) -> &[u64] {
        &self.structural_reuse
    }

    #[cfg(test)]
    pub(super) fn structural_superseded(&self) -> &[u64] {
        &self.structural_superseded
    }

    #[cfg(test)]
    pub(super) fn pending_structural_frees(&self) -> &[u64] {
        &self.pending_structural_frees
    }

    #[cfg(test)]
    pub(super) fn session_owned(&self) -> &FxHashSet<u64> {
        &self.session_owned
    }

    /// Forge state for a pin-test: push a page id into the live reuse pool so the
    /// orphan-sweep exclusion test can verify the pool is spared.
    #[cfg(test)]
    pub(super) fn push_structural_reuse_for_test(&mut self, id: u64) {
        self.structural_reuse.push(id);
    }

    /// The allocation hint (FREEMAP-1). Read by the rollback-rewind test.
    #[cfg(test)]
    pub(super) fn hint(&self) -> u64 {
        self.hint
    }

    /// Forge a `structural_superseded` entry (GAP-1). The savepoint-rewind test
    /// needs an entry that post-dates the savepoint, and no public operation
    /// produces one — every freemap-tree mutation site is gated on
    /// `savepoints.is_empty()`, which is exactly why the bug is latent rather
    /// than reachable. Mirrors `push_structural_reuse_for_test`.
    #[cfg(test)]
    pub(super) fn push_structural_superseded_for_test(&mut self, id: u64) {
        self.structural_superseded.push(id);
    }

    /// Forge a `session_owned` entry (GAP-1). Same rationale as above.
    #[cfg(test)]
    pub(super) fn insert_session_owned_for_test(&mut self, id: u64) {
        self.session_owned.insert(id);
    }
}

impl TransactionManager {
    // --- Watermark-based rollback (ISSUES.md I3 + I7) ---
    //
    // `PageCache::new_page()` hands out monotonically increasing ids, so
    // every page allocated during a transaction has an id strictly greater
    // than or equal to the `next_page_id` watermark captured at begin() /
    // savepoint() time. `PageCache::truncate(watermark)` drops every cache
    // entry AND truncates the file to `watermark` pages, cleanly discarding
    // every transaction-allocated page without a per-page tracking list.
    //
    // This supersedes an earlier per-page `txn_dirty_pages` vector — the
    // list was a weaker mechanism (I7 showed it missed intermediate COW
    // pages and overflow allocations) and a redundant one once the
    // watermark invariant was in place. See memory
    // project_chisel_i3_watermark_rollback for the reasoning.
    //
    // Savepoints capture `cache.next_page_id()` at creation time (see the
    // `watermark` field on Savepoint) so `rollback_to(name)` can truncate
    // to that specific watermark — discarding every page allocated after
    // the savepoint while preserving those allocated before it.

    /// Snapshot the current `next_page_id` watermark. Cheap — one read
    /// through the RefCell.
    pub(super) fn cache_watermark(&self) -> u64 {
        self.cache.borrow().next_page_id()
    }

    // --- Freemap-aware page allocation (ISSUES.md R2) ---
    //
    // `cow_alloc` (free function above) is the shared freemap-aware allocator
    // for a fresh page during a transaction: it first tries to reuse an id from
    // the committed freemap tree and falls back to extending the file. The six
    // allocation call sites reach it through `self.freemap.cow_alloc_into`,
    // sandwiched between `self.freemap.take_tree` / `put_tree` (the transient-tree
    // trio). Two important scoping rules:
    //
    //   1. Reuse is disabled when any savepoint is active. A rollback_to
    //      would need to per-savepoint distinguish dirty entries at
    //      reused ids from dirty entries at preserved ids, which would
    //      require an 8 KB freemap snapshot per savepoint and a
    //      per-savepoint dirty-page list. For v1, the simpler rule is
    //      "reuse only outside savepoint scopes". Workloads that want
    //      reuse (e.g. F1 delete_subtree / drop_table) typically don't
    //      use savepoints at all.
    //
    //   2. Pages freed during the CURRENT transaction (in
    //      `txn_freed_pages`) are NOT reusable within the same
    //      transaction — their old contents must stay readable via
    //      `committed_roots` until commit swaps the superblock. This
    //      is enforced by only merging `txn_freed_pages` into the freemap
    //      tree during commit (via `persist_freemap`), after the new roots
    //      have been computed.
    //
    // Handle-table and membership-index COW pages share this same freemap-aware
    // allocator (each `insert`/`delete` takes an `alloc` closure that calls it),
    // so they reuse freed pages before extending — that is what bounds their
    // steady-state page count. Overflow pages still call `cache.new_page()`
    // directly and always extend, but their frees feed the freemap, so a later
    // data- or handle-table allocation can reclaim them. Routing overflow through
    // the freemap too would need the same allocator-closure plumbing at the
    // overflow module boundary; left as a v1 simplification since overflow churn
    // is far smaller than HT churn.

    /// COW `handle`'s handle-table entry to `entry`, installing the new root
    /// and queuing the superseded spine pages for freemap reclamation at
    /// commit. Shared by `allocate`, `update`, and `set_client_byte`.
    ///
    /// The superseded pages are appended to `txn_freed_pages` ONLY after the
    /// new root is installed in `current_roots`: if the COW fails partway
    /// (e.g. `CacheFull`), the local `freed` list is dropped and the still-
    /// current old tree keeps all its pages — never freeing a live page.
    pub(super) fn ht_insert(&mut self, handle: u64, entry: &HandleEntry) -> Result<()> {
        let mut freed: Vec<u64> = Vec::new();
        let reuse = self.savepoints.is_empty();
        // Build the freemap-tree handle (with the session set moved in). The
        // alloc closure captures `&mut self.freemap` + `&mut self.txn_pages` +
        // the local `tree`, all disjoint from `self.handle_table`, so both can
        // borrow `self` at once.
        let mut tree = self.freemap.take_tree(&self.current_roots);
        let result = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| {
                self.freemap
                    .cow_alloc_into(c, &mut tree, &mut self.txn_pages, reuse)
            };
            self.handle_table.insert(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                entry,
                &mut alloc,
                &mut freed,
            )
        };
        // Write back freemap growth (its supersedes go to structural_superseded
        // via put_tree). Done before the `?` so a freemap COW that happened before
        // an insert error still returns the session set and records the extended
        // root. Handle-table supersedes (`freed`) are only retired after the new
        // root is installed.
        self.freemap.put_tree(&mut self.current_roots, tree);
        let new_root = result?;
        self.current_roots.handle_table_page = new_root;
        self.retire_superseded(&mut freed);
        Ok(())
    }

    /// Retire one install site's superseded radix pages: pages this transaction
    /// itself allocated go to the within-transaction recycle pool, the rest to
    /// `txn_freed_pages` for `persist` to mark free at commit
    /// (HANDLES-INDEX-2, issue #112).
    ///
    /// CALL THIS ONLY AFTER THE NEW ROOT IS INSTALLED. Every caller replaced a
    /// bare `self.txn_freed_pages.append(&mut freed)` that sat in the same
    /// position for the same reason, and the reason is now doing double duty:
    /// it kept a discarded candidate's pages from being freed while still
    /// referenced, and it is also what keeps a discarded candidate from feeding
    /// the pool. Moving a call above its install would reintroduce the
    /// candidate/discard corruption the pool design exists to avoid.
    ///
    /// The `savepoints.is_empty()` gate is read here rather than passed in so
    /// no caller can supply a different answer than `cow_alloc`'s `reuse` flag;
    /// the two must agree for the savepoint half of the invariant to hold.
    pub(super) fn retire_superseded(&mut self, freed: &mut Vec<u64>) {
        let pool_enabled = self.savepoints.is_empty();
        self.txn_pages
            .retire(freed, &mut self.txn_freed_pages, pool_enabled);
    }

    /// Reclaim crash-orphaned freemap pages. Not on the commit path despite the
    /// name this docblock used to carry: `defrag` is the only caller, and it runs
    /// inside a caller-opened transaction, not from `run_commit`. Read the
    /// savepoint-active flag and superblock count into locals BEFORE borrowing the
    /// cache to keep the borrows clean. `pub(crate)` — defrag calls it.
    ///
    /// Enforces `reclaim_orphans`' active-transaction requirement here, at the
    /// only door into it (TXN-COMMIT-8, issue #114).
    pub(crate) fn reclaim_freemap_orphans(&mut self) -> Result<u64> {
        // The `check_alive` half of the wrapper pattern, which this entry point
        // was missing while already carrying the `poison_on_fatal` half (I145
        // below). Same argument as the active-transaction guard beneath it:
        // enforce the precondition at the door rather than trusting every
        // caller. `defrag` now refuses on a poisoned manager before it ever
        // reaches step 7, so this is defence in depth, not the live guard.
        self.check_alive()?;
        // The sweep advances `current_roots.freemap_page` and drains
        // committed-LIVE ids into `structural_superseded`. With no transaction
        // open there is no commit that can promote either, so both are silently
        // discarded — the advanced root and the free bits are lost and the COW'd
        // freemap pages stay dirty at ids nothing references.
        //
        // Today that is only a bounded, self-healing leak: `begin()` reseeds
        // `current_roots` from `committed_roots` and `FreemapRecycle::begin`
        // clears `structural_superseded`, so no residue survives into a commit,
        // and the sole public door (`Chisel::defrag`) already returns
        // NoActiveTransaction first. But that safety is a prose argument spanning
        // three modules and two lines of `begin()` — exactly the shape of
        // reasoning that `update_inner` (mutate.rs) names as how a bug got in.
        // The requirement is documented on `reclaim_orphans`; enforce it where it
        // is checkable instead of relying on every future caller reading that doc.
        //
        // NoActiveTransaction is operational, not fatal (see error.rs), so the
        // `poison_on_fatal` below correctly leaves the manager usable.
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let savepoint_active = !self.savepoints.is_empty();
        let superblock_count = self.superblock_count;
        // I145: wrap the sweep in poison_on_fatal like every other TM entry point.
        // A fatal error mid-sweep (an IoError/CorruptPage from a page read or a
        // live-page cache.get) otherwise returns un-poisoned, and reclaim_orphans
        // writes the partially-advanced freemap root back into current_roots even
        // on its error path — leaving a usable manager holding an indeterminate
        // freemap. The inner borrow is scoped so the cache is released before
        // poison_on_fatal takes &self.
        let result = {
            let mut cache = self.cache.borrow_mut();
            self.freemap.reclaim_orphans(
                &mut cache,
                &mut self.current_roots,
                savepoint_active,
                superblock_count,
            )
        };
        self.poison_on_fatal(result)
    }
}
