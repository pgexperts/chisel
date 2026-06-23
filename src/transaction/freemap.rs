//! transaction::freemap — freemap staging and structural-page recycling:
//! the freemap-aware COW allocator (`cow_alloc` / `structural_extend`),
//! `persist_freemap`, orphan reclamation, and the data-page / handle-table
//! COW allocation paths. Split out of `transaction.rs` verbatim; see the
//! parent module for the type and fields.

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
/// drains from `tree.pending_superseded` into `txn_freed_pages` after the call.
///
/// LAZY-CREATE GUARD: a fresh database has `tree.root == PAGE_ID_NONE` (no tree
/// materialized yet). `PAGE_ID_NONE` is `u64::MAX`, NOT the tree's internal
/// zero-child sentinel, so `allocate_first` would try to read page u64::MAX and
/// error rather than reporting "nothing free". We short-circuit that here: a
/// None-root tree holds nothing reusable, so we fall straight through to
/// `new_page`. The tree is first materialized when a page is *freed* (see
/// persist_freemap), never on the allocation side.
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
    // `allocate_data_page` is the single entry point for allocating a
    // fresh data page during a transaction. It first tries to reuse an
    // id from `current_freemap` and falls back to extending the file.
    //
    // Two important scoping rules:
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
    //      is enforced by only merging `txn_freed_pages` into
    //      `current_freemap` during commit, after the new roots have
    //      been computed.
    //
    // Handle-table and membership-index COW pages now share this same
    // freemap-aware allocator via `cow_alloc` (each `insert`/`delete` takes an
    // `alloc` closure that calls it), so they reuse freed pages before
    // extending — that is what bounds their steady-state page count. Overflow
    // pages still call `cache.new_page()` directly and always extend, but their
    // frees feed the freemap, so a later data- or handle-table allocation can
    // reclaim them. Routing overflow through the freemap too would need the
    // same allocator-closure plumbing at the overflow module boundary; left as
    // a v1 simplification since overflow churn is far smaller than HT churn.
    /// Build a transient `FreeMapTree` handle from the current freemap roots,
    /// MOVING the transaction's `freemap_session_owned` set into it so this
    /// handle treats pages an earlier site already COW'd this transaction as
    /// in-place-mutable. Pair with `put_freemap_tree`, which moves the (possibly
    /// grown) set back out — never drop a handle from `take_` without a matching
    /// `put_`, or the session set is lost and later sites re-COW.
    pub(super) fn take_freemap_tree(&mut self) -> FreeMapTree {
        let mut tree = FreeMapTree::from_roots(
            self.current_roots.freemap_page,
            self.current_roots.freemap_depth,
        );
        tree.session_owned = std::mem::take(&mut self.freemap_session_owned);
        tree
    }

    /// Write a transient handle's grown root/depth back into the current roots,
    /// move its session-owned set back into the manager, and drain its
    /// COW-superseded freemap pages into `structural_superseded` (the one-commit
    /// defer stream — NOT `txn_freed_pages`, since freed freemap pages are
    /// recycled as structural reuse, not as data frees).
    pub(super) fn put_freemap_tree(&mut self, mut tree: FreeMapTree) {
        self.current_roots.freemap_page = tree.root;
        self.current_roots.freemap_depth = tree.depth;
        self.structural_superseded
            .append(&mut tree.pending_superseded);
        self.freemap_session_owned = std::mem::take(&mut tree.session_owned);
    }

    pub(super) fn allocate_data_page(&mut self) -> Result<u64> {
        let reuse = self.savepoints.is_empty();
        let mut tree = self.take_freemap_tree();
        let id = {
            let mut cache = self.cache.borrow_mut();
            cow_alloc(
                &mut cache,
                &mut tree,
                &mut self.freemap_hint,
                &mut self.structural_reuse,
                reuse,
            )
        };
        // Write back tree growth + drain supersedes even on error: the freemap
        // pages were extended (never freed), so on a non-fatal failure they are
        // harmless above-watermark scratch, and the session set must still be
        // returned so a retry/commit in the same transaction stays consistent.
        self.put_freemap_tree(tree);
        id
    }

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
        // Build the freemap-tree handle (with the session set moved in) and
        // borrow the hint + structural-reuse pool as locals, all disjoint from
        // `self.handle_table`, so the alloc closure (which mutates them) and the
        // handle-table insert can both borrow `self` at once.
        let mut tree = self.take_freemap_tree();
        let result = {
            let hint = &mut self.freemap_hint;
            let pool = &mut self.structural_reuse;
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| cow_alloc(c, &mut tree, hint, pool, reuse);
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
        // via put_freemap_tree). Done before the `?` so a freemap COW that
        // happened before an insert error still returns the session set and
        // records the extended root. Handle-table supersedes (`freed`) only land
        // in txn_freed_pages after the new root is installed.
        self.put_freemap_tree(tree);
        let new_root = result?;
        self.current_roots.handle_table_page = new_root;
        self.txn_freed_pages.append(&mut freed);
        Ok(())
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
    //   * `txn_freed_pages` (DATA frees) — pages freed by this commit's
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
    //     could cascade. Instead they ride a separate recycle: superseded this
    //     commit (`structural_superseded`) -> deferred one commit
    //     (`pending_structural_frees`, since the old page is still referenced
    //     until the superblock flips) -> reused as structural COW targets next
    //     transaction (`structural_reuse`). This makes the freemap pages ROTATE
    //     among a small set rather than marching the file upward ~1/commit.
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
    /// Mark a single page id free in the working freemap tree, routing every
    /// structural COW target through the pooled `structural_extend` (reuse a dead
    /// freemap page before extending the file) and lazily materializing the
    /// depth-0 root on first use. Lowers `freemap_hint` to cover `id` so the next
    /// `allocate_first` scan can reach it.
    ///
    /// The ONE marking path shared by `persist_freemap` (this commit's data
    /// frees) and `reclaim_freemap_orphans` (the defrag orphan-sweep). Both must
    /// flow through the same COW + recycle discipline so the structural reuse pool
    /// and supersede streams stay consistent; a second marking implementation
    /// could silently diverge from the one-commit-defer crash-safety the recycle
    /// depends on. Take/put the tree per call: the session-owned set and the
    /// reuse pool persist on the manager across calls, so a multi-id loop still
    /// COWs each leaf at most once (the session dedup carries across handles).
    fn freemap_mark_free_committed_path(&mut self, id: u64) -> Result<()> {
        // Take the working handle WITH the transaction's session set so a leaf an
        // earlier call (or this commit's data allocations) already COW'd is
        // recognized as in-place here, not re-COW'd.
        let mut tree = self.take_freemap_tree();
        // RefCell so the structural-`extend` closure can drain the shared reuse
        // pool by `&mut` while the rest of the method still owns `self`.
        let structural_reuse = std::cell::RefCell::new(std::mem::take(&mut self.structural_reuse));
        let result = (|| {
            let mut cache = self.cache.borrow_mut();
            let mut extend =
                |c: &mut PageCache| structural_extend(c, &mut structural_reuse.borrow_mut());

            // Lazy materialization: a database that has never freed a page has no
            // tree yet (root == PAGE_ID_NONE). Create the depth-0 leaf now, before
            // marking, since `mark_free_growing` needs a real root to COW.
            // Preserve the session set across the swap.
            if tree.root == PAGE_ID_NONE {
                let session = std::mem::take(&mut tree.session_owned);
                tree = FreeMapTree::create(&mut cache, &mut extend)?;
                tree.session_owned.extend(session);
            }
            tree.mark_free_growing(&mut cache, id, &mut extend)
        })();
        // Pull the hint back to cover `id`: the hint advances monotonically via
        // `allocate_first`, so a too-high hint would start the next scan above
        // `id` and never reuse it. A too-low hint only costs a wasted scan.
        // (Mirrors the oracle proptest's `hint = hint.min(id)`.)
        self.freemap_hint = self.freemap_hint.min(id);
        // Return the (partly drained) reuse pool and write the tree back even on
        // error: its COW supersedes flow to `structural_superseded` via
        // put_freemap_tree; commit promotes structural_superseded + the leftover
        // reuse pool into pending_structural_frees (the one-commit defer).
        self.structural_reuse = structural_reuse.into_inner();
        self.put_freemap_tree(tree);
        result
    }

    pub(super) fn persist_freemap(&mut self) -> Result<()> {
        // Nothing freed this commit => the committed tree is still exactly right,
        // no COW needed. (Structural reuse / supersede streams are only ever
        // non-empty when there were frees, so this single check suffices.)
        if self.txn_freed_pages.is_empty() {
            return Ok(());
        }

        // Mark this commit's DATA frees free in the new tree via the shared
        // marking path. Each call take/puts the tree, but the session-owned set
        // persists on the manager, so a leaf hit by several frees is COW'd once.
        let freed: Vec<u64> = std::mem::take(&mut self.txn_freed_pages);
        for id in freed.iter().copied() {
            self.freemap_mark_free_committed_path(id)?;
        }
        self.txn_freed_pages = freed;
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
    /// `freemap_mark_free_committed_path` the commit uses (COW + recycle), so a
    /// reclaimed orphan lands in the BITMAP (data-reusable), disjoint from the
    /// in-memory recycle pool. Requires an active transaction (called by defrag).
    /// Returns the count reclaimed.
    ///
    /// THE EXCLUSION SET (get this exactly right): a page in the CURRENT
    /// in-memory recycle pool (`structural_reuse` ∪ `structural_superseded` ∪
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
    pub(crate) fn reclaim_freemap_orphans(&mut self) -> Result<u64> {
        // Skip the sweep entirely while a savepoint is active. The sweep is the
        // ONLY path that COWs the freemap (draining committed-LIVE pages into the
        // structural recycle streams) while a savepoint is open — ordinary
        // allocation already disables structural reuse under a savepoint
        // (`reuse = self.savepoints.is_empty()`). But `rollback_to` rewinds only
        // the roots + cache watermark, NOT the structural streams: a page the
        // sweep drained into `structural_superseded` would survive the rollback,
        // get promoted at commit, and be reused as a COW target in the next
        // transaction while the last-durable superblock still references it —
        // silent durable freemap corruption. Deferring orphan reclamation to a
        // defrag run with no active savepoint avoids the whole interaction, so
        // `rollback_to_inner` correctly needs no structural-stream reset.
        if !self.savepoints.is_empty() {
            return Ok(0);
        }
        let root = self.current_roots.freemap_page;
        let depth = self.current_roots.freemap_depth;
        if root == PAGE_ID_NONE {
            return Ok(0); // no tree yet => no freemap pages can be orphaned
        }

        // Pages that are NOT orphans even though unreachable + not-free: the live
        // recycle pool (all three streams). See "THE EXCLUSION SET" above.
        let mut excluded: FxHashSet<u64> = FxHashSet::default();
        excluded.extend(self.structural_reuse.iter().copied());
        excluded.extend(self.structural_superseded.iter().copied());
        // Belt-and-suspenders: `begin()` clones `pending_structural_frees`
        // into `structural_reuse`, so every id here is already covered by the
        // `structural_reuse` term above. Kept explicitly so the exclusion
        // remains correct if `begin()`'s seeding ever changes.
        excluded.extend(self.pending_structural_frees.iter().copied());

        // Collect orphan ids read-only inside a single cache-borrow scope, then
        // drop the borrow before marking (the mark path re-borrows the cache).
        let tree = FreeMapTree::from_roots(root, depth);
        let mut orphans: Vec<u64> = Vec::new();
        {
            let mut cache = self.cache.borrow_mut();
            // Upper bound: the allocation high-water (`next_page_id`), NOT the
            // committed `total_pages`. After a real crash + reopen these are
            // equal (open seeds next_page_id from the committed superblock), and
            // every orphan — a structural supersede from a committed transaction —
            // sits below it. Using next_page_id also covers a page extended
            // earlier in THIS session (e.g. the forge-orphan test), which a stale
            // committed total_pages would miss.
            let total = cache.next_page_id();
            let reachable = tree.reachable_pages(&mut cache)?;
            // Pages 0..superblock_count are superblocks; start the scan above them.
            for id in self.superblock_count as u64..total {
                if reachable.contains(&id) || excluded.contains(&id) {
                    continue;
                }
                // Skip a non-reachable page that is GARBAGE/corrupt rather than
                // letting it poison the whole maintenance pass (2026-06-22 review
                // decision: "skip unreadable dead pages"). A page that is not in
                // the live tree cannot be confirmed as a freemap orphan if we
                // cannot read its type, and a DEAD page's corruption does not
                // affect correctness — so on `CorruptPage`/`ChecksumMismatch` we
                // `continue`. We deliberately PROPAGATE every other read error
                // (e.g. `IoError`): a real device fault should still surface and
                // poison, not be silently swallowed. NOTE: the live-tree walk
                // (`reachable_pages` above) still propagates fatal on a corrupt
                // LIVE page — only the dead-page scan is softened.
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
                    && !tree.is_free(&mut cache, id)?
                {
                    orphans.push(id);
                }
            }
        }
        // Mark each orphan free through the shared committed-marking path (COW +
        // recycle), landing them in the bitmap as data-reusable space.
        for id in &orphans {
            self.freemap_mark_free_committed_path(*id)?;
        }
        Ok(orphans.len() as u64)
    }
}
