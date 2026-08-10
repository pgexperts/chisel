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
/// steady-state page count instead of leaking one page per mutation.
pub(super) fn cow_alloc(
    cache: &mut PageCache,
    tree: &mut FreeMapTree,
    hint: &mut u64,
    structural_reuse: &mut Vec<u64>,
    reuse_enabled: bool,
) -> Result<u64> {
    if reuse_enabled && tree.root != PAGE_ID_NONE {
        // `allocate_first` claims a free DATA page (clearing its bit), which COWs
        // the freemap leaf. That leaf COW's structural `extend` reuses a dead
        // freemap page from `structural_reuse` before extending the file — what
        // keeps the freemap from marching the file upward one page per commit.
        let mut extend = |c: &mut PageCache| structural_extend(c, structural_reuse);
        if let Some(id) = tree.allocate_first(cache, hint, &mut extend)? {
            cache.claim_page(id)?;
            return Ok(id);
        }
    }
    cache.new_page()
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
    pub(super) fn cow_alloc_into(
        &mut self,
        cache: &mut PageCache,
        tree: &mut FreeMapTree,
        reuse: bool,
    ) -> Result<u64> {
        cow_alloc(
            cache,
            tree,
            &mut self.hint,
            &mut self.structural_reuse,
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
    /// frees is COW'd once. A no-op when nothing was freed (the recycle/supersede
    /// streams are only ever non-empty when there were frees, so the single
    /// emptiness check suffices).
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
    /// recycle pool. Requires an active transaction (called by defrag).
    /// Returns the count reclaimed.
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

        // Pages that are NOT orphans even though unreachable + not-free: the live
        // recycle pool (all three streams). See "THE EXCLUSION SET" above.
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
    /// was the working copy; drop it. The session set: any freemap pages this
    /// aborted transaction COW'd sit above the watermark and were just truncated,
    /// so their ids must not be treated as in-place-mutable next transaction.
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
        // alloc closure captures `&mut self.freemap` + the local `tree`, disjoint
        // from `self.handle_table`, so both can borrow `self` at once.
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
                &mut freed,
            )
        };
        // Write back freemap growth (its supersedes go to structural_superseded
        // via put_tree). Done before the `?` so a freemap COW that happened before
        // an insert error still returns the session set and records the extended
        // root. Handle-table supersedes (`freed`) only land in txn_freed_pages
        // after the new root is installed.
        self.freemap.put_tree(&mut self.current_roots, tree);
        let new_root = result?;
        self.current_roots.handle_table_page = new_root;
        self.txn_freed_pages.append(&mut freed);
        Ok(())
    }

    /// Commit-path wrapper: reclaim crash-orphaned freemap pages. Read the
    /// savepoint-active flag and superblock count into locals BEFORE borrowing the
    /// cache to keep the borrows clean. `pub(crate)` — defrag calls it.
    pub(crate) fn reclaim_freemap_orphans(&mut self) -> Result<u64> {
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
