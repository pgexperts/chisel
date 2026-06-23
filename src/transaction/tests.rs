// transaction::tests — the full unit-test suite for TransactionManager.
// Moved verbatim out of transaction.rs as part of the by-concern module
// split; kept as a single file (the tests share fixtures like fresh_manager
// and assert_no_reachable_page_is_free).

use super::freemap::take_structural_reuse_log;
use super::*;
use crate::page_io::{Fault, PageIo};
use tempfile::{NamedTempFile, TempDir};

fn fresh_manager() -> TransactionManager {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    // Match Options::default()'s cache_max_bytes of 8 MiB (1024 pages)
    // so tests that intentionally allocate many pages in a single
    // transaction (e.g. the I3+I7 handle-table-growth test allocates
    // 510+) stay well under the strict cache cap. spillway_max_bytes=0
    // preserves the legacy CacheFull-at-cap behavior in tests.
    let cache = PageCache::new(
        io,
        1024 * PAGE_SIZE as u64,
        0,
        crate::DrainInsertion::LruTail,
        crate::SpillwayLocation::InMemory,
    );
    let mut tm = TransactionManager::create_new(cache, 2).unwrap();
    // Commit once so there's a real baseline to read/write against.
    tm.begin().unwrap();
    tm.commit().unwrap();
    tm
}

/// C1 invariant: after a commit, NO page reachable from `committed_roots`
/// may be marked free in `committed_freemap`. A correct COW frees only
/// superseded pages; freeing a still-referenced page (the textbook C1
/// violation — e.g. `grow` freeing the reparented old root, or `update`
/// freeing the OLD value before the new entry is installed) shows up here as
/// a page that is both reachable and free. This is deterministic regardless
/// of the freemap's lowest-id-first selection order, which makes black-box
/// reopen tests unreliable for catching C1.
///
/// Reachability covers BOTH the index spines (handle-table + membership
/// outer/inner) AND the value storage every live handle points at (its
/// inline data page or its full overflow chain). The value-storage half is
/// essential: a spine-only walk cannot catch a value-page premature-free.
///
/// PRECONDITION: call only between transactions (right after a commit), where
/// `committed_roots == current_roots` and the in-memory `handle_table.depth`
/// matches the committed root. `iter_live` descends with that live depth, so
/// calling this mid-transaction after a grow would mis-descend.
fn assert_no_reachable_page_is_free(tm: &TransactionManager) {
    let mut reachable = Vec::new();
    {
        let mut cache = tm.cache.borrow_mut();
        tm.handle_table
            .collect_page_ids(
                &mut cache,
                tm.committed_roots.handle_table_page,
                &mut reachable,
            )
            .unwrap();
        tm.membership_index
            .collect_page_ids(
                &mut cache,
                tm.committed_roots.membership_index_page,
                &mut reachable,
            )
            .unwrap();

        // Value storage reachable through each live HandleEntry.
        if tm.committed_roots.handle_table_page != PAGE_ID_NONE {
            let live = tm
                .handle_table
                .iter_live(&mut cache, tm.committed_roots.handle_table_page)
                .unwrap();
            for (_handle, entry) in live {
                match entry.flags {
                    HandleFlags::Live => reachable.push(entry.page_id),
                    HandleFlags::Overflow => {
                        let chain =
                            Overflow::collect_chain_pages(&mut cache, entry.page_id).unwrap();
                        reachable.extend(chain);
                    }
                    HandleFlags::Deleted => {}
                }
            }
        }
    }
    // Query freeness through the committed freemap TREE (reconstructed from
    // {root, depth}) rather than a flat in-memory bitmap — same C1 invariant,
    // new storage representation.
    let mut cache = tm.cache.borrow_mut();
    let tree = FreeMapTree::from_roots(
        tm.committed_roots.freemap_page,
        tm.committed_roots.freemap_depth,
    );
    for id in reachable {
        assert!(
            !tree.is_free(&mut cache, id).unwrap(),
            "page {id} is reachable from committed_roots but marked FREE in \
                 the committed freemap — a still-referenced page was freed (C1 violation)"
        );
    }
}

// COW page reclamation must never free a page still referenced by the
// committed tree, even after the trees GROW (the reparenting paths). Forces
// a handle-table grow (>510 handles) and a membership inner-tree grow
// (>1021 members under one tag), then churns with reclamation, asserting the
// C1 invariant after every commit.
#[test]
fn reclamation_never_frees_a_reachable_page_after_grow() {
    let mut tm = fresh_manager();
    let tag = 9u32;
    let mut handles = Vec::new();
    let mut v: u32 = 0;

    // Build >1021 tagged members in small batches (stay under the 1024-page
    // cache cap), forcing both trees to grow to depth >= 1.
    for _ in 0..12 {
        tm.begin().unwrap();
        for _ in 0..100 {
            let h = tm.allocate_tagged(&v.to_le_bytes(), tag).unwrap();
            handles.push(h);
            v += 1;
        }
        tm.commit().unwrap();
        assert_no_reachable_page_is_free(&tm);
    }
    assert!(
        handles.len() > 1021,
        "workload must exceed one membership leaf to force an inner grow"
    );

    // Churn with reclamation across committed transactions: update relocates
    // the value and COWs the handle-table spine (freeing the old spine);
    // set_client_byte COWs only the leaf. Batched per ~100 handles so a
    // single transaction's dirty COW pages stay under the cache cap (within
    // a txn, this-txn frees are not yet reusable). Re-check after each commit.
    for round in 0..12u32 {
        for (chunk_idx, chunk) in handles.chunks(100).enumerate() {
            tm.begin().unwrap();
            for (j, h) in chunk.iter().enumerate() {
                if (round as usize + chunk_idx + j) % 2 == 0 {
                    tm.set_client_byte(*h, round as u8).unwrap();
                } else {
                    tm.update(*h, &round.to_le_bytes()).unwrap();
                }
            }
            tm.commit().unwrap();
            assert_no_reachable_page_is_free(&tm);
        }
    }

    // Every handle still carries its tag and is enumerable after the churn.
    for h in &handles {
        assert_eq!(tm.tag(*h).unwrap(), tag);
    }
    assert_eq!(tm.handles_with_tag(tag).unwrap().len(), handles.len());
}

#[test]
fn tagged_membership_survives_rolled_back_outer_grow() {
    let mut tm = fresh_manager();
    // Commit a small tag; the outer (tag-keyed) tree stays depth 0.
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"keep", 3).unwrap();
    tm.commit().unwrap();
    assert_eq!(tm.handles_with_tag(3).unwrap(), vec![h]);
    // New txn: a tag >= 1021 forces the outer tree to grow (depth 0 -> 1).
    // Roll back. The grown root is discarded and current_roots snaps back to
    // the depth-0 committed root; outer_depth must be restored to match.
    tm.begin().unwrap();
    let _ = tm.allocate_tagged(b"discard", 5000).unwrap();
    tm.rollback().unwrap();
    // The committed small tag must still be readable (was silently lost before the fix).
    assert_eq!(
        tm.handles_with_tag(3).unwrap(),
        vec![h],
        "rolled-back outer grow corrupted committed membership"
    );
    assert_eq!(tm.tag(h).unwrap(), 3);
    // The discarded tag is gone.
    assert_eq!(tm.handles_with_tag(5000).unwrap(), Vec::<u64>::new());
}

#[test]
fn tagged_membership_survives_rollback_to_savepoint() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"keep", 7).unwrap();
    tm.savepoint("sp").unwrap();
    // Grow the outer tree past depth 0 inside the savepoint, then roll back to it.
    let _ = tm.allocate_tagged(b"discard", 6000).unwrap();
    tm.rollback_to("sp").unwrap();
    // Still inside the active txn: the pre-savepoint tag must remain readable.
    assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
    assert_eq!(tm.handles_with_tag(6000).unwrap(), Vec::<u64>::new());
    tm.commit().unwrap();
    assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
}

// Regression test for ISSUES.md I1. Once the manager is poisoned,
// every public entry point must return ChiselError::Poisoned rather
// than attempting the operation. This is the core invariant of the
// poison model — the test asserts it for each method independently
// so a future refactor that forgets to wrap a new entry point will
// fail loudly.
#[test]
fn poisoned_manager_rejects_every_public_entry_point() {
    let mut tm = fresh_manager();
    tm.force_poison_for_test();
    assert!(tm.is_poisoned());

    assert!(matches!(tm.begin(), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.commit(), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.rollback(), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.savepoint("x"), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.rollback_to("x"), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.release("x"), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.allocate(b"v"), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.read(0), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.update(0, b"v"), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.delete(0), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.handles(), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.file_page_count(), Err(ChiselError::Poisoned)));
    assert!(matches!(
        tm.allocate_tagged(b"v", 1),
        Err(ChiselError::Poisoned)
    ));
    assert!(matches!(tm.tag(0), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.handles_with_tag(1), Err(ChiselError::Poisoned)));
    assert!(matches!(tm.delete_tagged(0, 1), Err(ChiselError::Poisoned)));
    assert!(matches!(
        tm.delete_with_tag(1, 10),
        Err(ChiselError::Poisoned)
    ));
    assert!(matches!(tm.client_byte(0), Err(ChiselError::Poisoned)));
    assert!(matches!(
        tm.set_client_byte(0, 1),
        Err(ChiselError::Poisoned)
    ));
}

#[test]
fn fatal_error_outside_commit_also_poisons() {
    // I112: a REAL fatal IoError on a cold read OUTSIDE any transaction
    // poisons the manager (the non-commit fatal path, poison_on_fatal). This
    // replaces the old force_poison_for_test() tautology with an injected
    // fault. We reopen over the committed file so read(h) is a cache MISS
    // that actually reaches read_page(pid).
    let file = NamedTempFile::new().unwrap();
    let h;
    let pid;
    {
        let io = PageIo::open(file.path(), false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm = TransactionManager::create_new(cache, 2).unwrap();
        tm.begin().unwrap();
        h = tm.allocate(b"durable").unwrap();
        tm.commit().unwrap();
        pid = tm.handle_live_page_id(h).unwrap().expect("live data page");
    }

    // Reopen: cold cache, so read(h) misses and calls read_page(pid).
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(
        io,
        1024 * PAGE_SIZE as u64,
        0,
        crate::DrainInsertion::LruTail,
        crate::SpillwayLocation::InMemory,
    );
    let tm = TransactionManager::open_existing(cache).unwrap();
    tm.cache.borrow().io().arm_fault(Fault::FailReadPage(pid));
    let result = tm.read(h);
    assert!(
        matches!(result, Err(ChiselError::IoError(_))),
        "cold read fault must surface IoError, got {result:?}"
    );
    assert!(
        tm.is_poisoned(),
        "a fatal read error outside commit must poison"
    );
}

// Regression test for ISSUES.md I3 + I7. A transaction that forces
// handle-table growth allocates many pages (the data pages for each
// value, the handle-table leaves, the COW spine clones, and the
// new interior root from grow()). After rollback, every one of those
// pages must be gone — both from the in-memory cache AND from the
// file itself.
//
// Pre-I7, the old per-page dirty list missed intermediate COW pages.
// Pre-I3, rollback only discarded cache entries without truncating
// the file, so the extended pages leaked permanently. This test
// exercises both conditions in one shot by asserting the
// `next_page_id` watermark and the cache page-count return to their
// pre-transaction values after rollback.
#[test]
fn rollback_truncates_cache_and_file_to_pre_txn_watermark() {
    let mut tm = fresh_manager();
    let pre_watermark = tm.cache.borrow().next_page_id();
    let pre_file_pages = tm.cache.borrow_mut().file_page_count().unwrap();

    tm.begin().unwrap();
    tm.allocate(b"seed").unwrap();
    // Force handle-table growth by crossing the 510-entry leaf boundary.
    for _ in 0..510 {
        tm.allocate(b"f").unwrap();
    }
    // Sanity: the transaction must have extended the cache past the
    // pre-transaction watermark. Otherwise the test below is vacuous.
    let mid_watermark = tm.cache.borrow().next_page_id();
    assert!(
            mid_watermark > pre_watermark + 510,
            "expected the transaction to allocate many pages beyond {pre_watermark}, got {mid_watermark}"
        );

    tm.rollback().unwrap();

    let post_watermark = tm.cache.borrow().next_page_id();
    let post_file_pages = tm.cache.borrow_mut().file_page_count().unwrap();
    assert_eq!(
        post_watermark, pre_watermark,
        "rollback must rewind next_page_id to the pre-transaction watermark"
    );
    assert_eq!(
        post_file_pages, pre_file_pages,
        "rollback must truncate the file back to its pre-transaction page count"
    );
}

// rollback_to(name) must truncate cache+file to the savepoint's
// watermark, discarding every page allocated after the savepoint
// while preserving those allocated before it. This is the per-
// savepoint analogue of the full-rollback test above.
#[test]
fn rollback_to_savepoint_truncates_to_savepoint_watermark() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h1 = tm.allocate(b"before").unwrap();
    tm.savepoint("sp").unwrap();
    let savepoint_watermark = tm.cache.borrow().next_page_id();
    let _h2 = tm.allocate(b"after").unwrap();
    let _h3 = tm.allocate(b"after-2").unwrap();
    assert!(tm.cache.borrow().next_page_id() > savepoint_watermark);

    tm.rollback_to("sp").unwrap();
    assert_eq!(
        tm.cache.borrow().next_page_id(),
        savepoint_watermark,
        "rollback_to must rewind to the savepoint's watermark"
    );
    // The pre-savepoint handle must still be readable.
    assert_eq!(tm.read(h1).unwrap(), b"before");
    tm.commit().unwrap();
}

// An operational error (NoActiveTransaction, DuplicateSavepoint,
// InvalidHandle, etc.) must NOT poison. These are caller mistakes,
// not integrity failures — the manager stays usable.
#[test]
fn operational_error_does_not_poison() {
    let mut tm = fresh_manager();

    // NoActiveTransaction from commit — operational.
    assert!(matches!(tm.commit(), Err(ChiselError::NoActiveTransaction)));
    assert!(!tm.is_poisoned());

    // NoActiveTransaction from allocate — operational.
    assert!(matches!(
        tm.allocate(b"v"),
        Err(ChiselError::NoActiveTransaction)
    ));
    assert!(!tm.is_poisoned());

    // DuplicateSavepoint — operational.
    tm.begin().unwrap();
    tm.savepoint("a").unwrap();
    assert!(matches!(
        tm.savepoint("a"),
        Err(ChiselError::DuplicateSavepoint(_))
    ));
    assert!(!tm.is_poisoned());

    // InvalidHandle from read — operational.
    assert!(matches!(tm.read(999), Err(ChiselError::InvalidHandle(_))));
    assert!(!tm.is_poisoned());
}

// Regression test for ISSUES.md I18. Inside commit_inner's
// persist_freemap step, the new-freemap-page allocation must never
// return an id that is still referenced by the currently-committed
// on-disk superblock. The two sources of such at-risk ids are:
//
//   (1) `committed_roots.freemap_page` itself — the current
//       on-disk freemap page; overwriting it mid-commit destroys
//       the committed freemap snapshot.
//   (2) Any id in `txn_freed_pages` — pages that held handle
//       values reachable through the committed handle table;
//       overwriting any of them mid-commit destroys a
//       committed value.
//
// A crash in the window between `cache.flush()` and the superblock
// fsync would then leave the last-durable superblock pointing at
// a page whose bytes no longer match what it committed to —
// breaking the core shadow-paging invariant. The fix defers the
// merge of both at-risk sets into `current_freemap` until AFTER
// the new-freemap-page allocate has run, so `FreeMap::allocate_first`
// cannot return any of them during the vulnerable window.
#[test]
fn persist_freemap_does_not_reuse_committed_live_pages() {
    let mut tm = fresh_manager();

    // Commit 1: seed a non-trivial committed state. We need
    // persist_freemap to actually materialize a freemap page,
    // not take the early-exit path. That requires `txn_freed_pages`
    // to be non-empty at commit, which means freeing at least one
    // WHOLE data page — R1 slot packing keeps multi-slot data
    // pages live even after individual deletes. The simplest way
    // to guarantee whole-page frees is to use overflow-sized
    // values (> MAX_INLINE_VALUE): each gets its own overflow
    // chain, and delete releases every page in the chain into
    // txn_freed_pages via Overflow::collect_chain_pages.
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];
    tm.begin().unwrap();
    let h_throwaway = tm.allocate(&big).unwrap();
    let h_live_a = tm.allocate(&big).unwrap();
    let h_live_b = tm.allocate(&big).unwrap();
    let h_live_c = tm.allocate(&big).unwrap();
    tm.delete(h_throwaway).unwrap();
    tm.commit().unwrap();

    let committed_freemap_page = tm.committed_roots.freemap_page;
    assert_ne!(
        committed_freemap_page, PAGE_ID_NONE,
        "test precondition: commit 1 should have established a freemap page"
    );

    // Commit 2: delete two more handles. release_data_slot pushes
    // their data pages into `txn_freed_pages`; those pages are
    // still referenced by commit 1's (currently-on-disk)
    // superblock at the moment commit_inner runs persist_freemap.
    tm.begin().unwrap();
    tm.delete(h_live_a).unwrap();
    tm.delete(h_live_b).unwrap();

    let frozen_txn_freed: Vec<u64> = tm.txn_freed_pages.clone();
    assert!(
        !frozen_txn_freed.is_empty(),
        "test precondition: deletes should have populated txn_freed_pages"
    );

    tm.commit().unwrap();

    // The at-risk set: anything that was still live under the
    // prior committed superblock at the moment persist_freemap
    // started allocating.
    let mut still_live_pre_commit = frozen_txn_freed.clone();
    still_live_pre_commit.push(committed_freemap_page);

    let new_freemap_page = tm.committed_roots.freemap_page;
    assert!(
        !still_live_pre_commit.contains(&new_freemap_page),
        "I18: persist_freemap allocated the new freemap page at an id \
             that was still referenced by the last-durable superblock. \
             new_freemap_page={new_freemap_page}, \
             committed_freemap_page was {committed_freemap_page}, \
             txn_freed_pages at commit time = {frozen_txn_freed:?}"
    );

    // Sanity: the un-deleted handle still reads back correctly
    // (rules out a subtler corruption that survived the invariant
    // check but poisoned the data plane).
    assert_eq!(tm.read(h_live_c).unwrap(), big);
}

// Regression test for ISSUES.md I27. `savepoint_inner` moves
// `txn_freed_pages` into the savepoint record via `std::mem::take`.
// If commit runs with savepoints still on the stack, the pre-fix
// `commit_inner` just called `self.savepoints.clear()` at step 5
// and those `freed_pages` lists were dropped — never reaching the
// freemap. `persist_freemap` iterates only `self.txn_freed_pages`.
// The post-fix merge in commit_inner flattens every active
// savepoint's `freed_pages` back into `txn_freed_pages` before
// `persist_freemap` runs, so every page freed anywhere in the
// transaction reaches the committed freemap.
//
// Observable via `FreeMap::is_free(&committed_freemap, id)` — the
// freemap's public predicate avoids any reliance on subsequent
// allocator reuse (which depends on `savepoints.is_empty()` too and
// would muddy the test).
#[test]
fn commit_with_active_savepoint_returns_freed_pages_to_freemap() {
    let mut tm = fresh_manager();

    // Seed: enough overflow-sized handles that deleting them
    // produces genuine page frees (R1 slot-packing would otherwise
    // keep multi-slot data pages live).
    let big: Vec<u8> = vec![0xCD; MAX_INLINE_VALUE + 32];
    tm.begin().unwrap();
    let h_a = tm.allocate(&big).unwrap();
    let h_b = tm.allocate(&big).unwrap();
    let h_keepalive = tm.allocate(&big).unwrap();
    tm.commit().unwrap();

    // The leak pattern: delete first, THEN open a savepoint. The
    // savepoint captures the accumulated `txn_freed_pages`, leaving
    // the outer `txn_freed_pages` empty for the rest of the txn.
    tm.begin().unwrap();
    tm.delete(h_a).unwrap();
    tm.delete(h_b).unwrap();
    let frozen_txn_freed: Vec<u64> = tm.txn_freed_pages.clone();
    assert!(
        !frozen_txn_freed.is_empty(),
        "test precondition: overflow-sized deletes should free at least one page"
    );

    tm.savepoint("s").unwrap();
    assert!(
        tm.txn_freed_pages.is_empty(),
        "savepoint_inner should have moved txn_freed_pages into the savepoint"
    );

    // Commit WITHOUT releasing the savepoint. Pre-fix this silently
    // drops savepoint.freed_pages on `savepoints.clear()`; post-fix
    // commit_inner merges them into txn_freed_pages first.
    tm.commit().unwrap();

    {
        let mut cache = tm.cache.borrow_mut();
        let tree = FreeMapTree::from_roots(
            tm.committed_roots.freemap_page,
            tm.committed_roots.freemap_depth,
        );
        for id in &frozen_txn_freed {
            assert!(
                tree.is_free(&mut cache, *id).unwrap(),
                "I27: freed page {id} should be marked free in the committed \
                     freemap tree after commit-with-active-savepoint; \
                     frozen_txn_freed={frozen_txn_freed:?}"
            );
        }
    }

    // Sanity: the surviving handle still reads back (rules out a
    // wider corruption that happens to also trip the is_free check).
    assert_eq!(tm.read(h_keepalive).unwrap(), big);
}

// ── Structural-page recycle: adversarial pin-tests ──────────────────────
//
// These three lock down the durability-critical freemap structural recycle
// (docs/specs/2026-06-22 "Structural-page reclamation"): the one-commit
// defer, the rollback reset of the recycle pools, and no lost/double free
// across reuse cycles. They are the GATE for the Phase 2 work — a violation
// is a crash-safety bug, not a cosmetic one.

// Drive a commit that actually COWs the freemap and supersedes structural
// pages: allocate `n` overflow-sized values (each its own whole page chain),
// commit, then delete `del` of them and commit. The second commit's
// `persist_freemap` marks the freed pages free, COWing the freemap leaf/spine
// and superseding the old freemap pages — exactly the churn the recycle
// model is built around. Returns the surviving handles.
fn structural_churn(tm: &mut TransactionManager, big: &[u8], n: usize, del: usize) -> Vec<u64> {
    tm.begin().unwrap();
    let mut handles: Vec<u64> = (0..n).map(|_| tm.allocate(big).unwrap()).collect();
    tm.commit().unwrap();

    tm.begin().unwrap();
    for h in handles.drain(..del) {
        tm.delete(h).unwrap();
    }
    tm.commit().unwrap();
    handles
}

// PROPERTY 1 — one-commit-defer crash-safety.
//
// A freemap page `P` superseded in transaction `T` is still referenced by
// the pre-`T` superblock until `T` commits, so it may be reused as a
// structural COW target ONLY starting in `T+1` (the one-commit defer). If
// `T+1` ever drew a COW target from a page it superseded THIS transaction,
// a crash before `T+1`'s superblock fsync would corrupt the page the
// recovered (pre-`T+1`) superblock still points at — a durability BUG.
//
// We capture the promoted recycle set at the START of the measured
// transaction `T+1` (== what `begin()` cloned into `structural_reuse`), then
// instrument every structural-reuse pop in `T+1` and assert each popped id is
// drawn from EXACTLY that promoted set — never an id minted or superseded
// within `T+1`. The thread-local reuse log records both pop sites
// (`structural_extend` and `persist_freemap`'s inline closure).
//
// Crucially, `T+1` is a WARMED-UP steady-state transaction doing many
// interleaved allocate+delete ops: each allocate reuses a bitmap-free data
// page, which COWs the freemap leaf in the transaction BODY and supersedes a
// freemap page mid-flight — so a later body allocation in the same
// transaction WOULD pop that just-superseded page if the defer were broken.
// (A single-op transaction supersedes the freemap only at persist_freemap,
// the last structural op, leaving no later pop to expose the bug — this test
// is structured to defeat that blind spot.)
#[test]
fn structural_recycle_one_commit_defer() {
    let mut tm = fresh_manager();
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];

    // Warm up to steady state: a rotating live population so the bitmap holds
    // free data pages (making body allocations COW the freemap) and the
    // structural recycle is non-trivially populated. Run several
    // delete-then-reallocate commits.
    let mut live: Vec<u64> = Vec::new();
    tm.begin().unwrap();
    for _ in 0..16 {
        live.push(tm.allocate(&big).unwrap());
    }
    tm.commit().unwrap();
    for _ in 0..6 {
        tm.begin().unwrap();
        let recycled: Vec<u64> = live.drain(..8).collect();
        for h in recycled {
            tm.delete(h).unwrap();
        }
        for _ in 0..8 {
            live.push(tm.allocate(&big).unwrap());
        }
        tm.commit().unwrap();
    }

    // Capture the promoted recycle the measured transaction inherits, and
    // drain the warm-up's reuse log so only the measured transaction is seen.
    let promoted: std::collections::HashSet<u64> =
        tm.pending_structural_frees.iter().copied().collect();
    assert!(
        !promoted.is_empty(),
        "precondition: warm-up must leave a non-empty deferred recycle"
    );
    let _ = take_structural_reuse_log();

    // T+1 (measured): MANY interleaved allocate+delete ops. Each allocate
    // claims a bitmap-free data page (COWing + superseding the freemap in the
    // body), each delete frees a page; the heavy interleave means a freemap
    // page superseded early in the body has many later body allocations that
    // would pop it if the defer leaked same-txn supersedes into the pool.
    tm.begin().unwrap();
    for _ in 0..6 {
        let recycled: Vec<u64> = live.drain(..4).collect();
        for h in recycled {
            tm.delete(h).unwrap();
        }
        for _ in 0..4 {
            live.push(tm.allocate(&big).unwrap());
        }
    }
    // Pages T+1 superseded so far (the body). persist_freemap adds more at
    // commit; both must stay out of the reuse pops.
    let superseded_in_t1: std::collections::HashSet<u64> =
        tm.structural_superseded.iter().copied().collect();
    tm.commit().unwrap();

    // Read the log IMMEDIATELY after T+1's commit — before any later
    // transaction can pop from its OWN (legitimately) promoted recycle and
    // pollute the capture with ids that were never inherited here.
    let reused_in_t1 = take_structural_reuse_log();
    assert!(
        !reused_in_t1.is_empty(),
        "precondition: T+1 must actually reuse at least one deferred page \
             (else the defer is untested)"
    );
    // NOTE: this block is a weak guard on its own. Under session-COW dedup a
    // leaf is COW'd at most once per commit, so a same-transaction supersede
    // and its only in-txn reuse pop are the SAME event — they cannot both be
    // observed here. The load-bearing defer check is the cross-commit
    // REACHABILITY assertion below; this block is kept as a cheap sanity rail.
    for id in &reused_in_t1 {
        assert!(
            promoted.contains(id),
            "one-commit-defer VIOLATION: T+1 reused freemap page {id} that was \
                 NOT in the promoted recycle set {promoted:?} — it was minted or \
                 superseded within T+1, so a pre-commit crash would corrupt the page \
                 the last-durable superblock still references"
        );
        assert!(
            !superseded_in_t1.contains(id),
            "one-commit-defer VIOLATION: T+1 reused page {id} that T+1 itself \
                 superseded this transaction (still live under the last-durable \
                 superblock) — reusing it pre-commit is a crash-safety bug"
        );
    }

    // The defer's DURABLE consequence: a page T+1 superseded is now (post-
    // commit) dead and queued for T+2 — but it must NOT be reachable in the
    // just-committed live tree. A broken defer that re-routed a same-txn
    // supersede into the reuse pool would surface here as a pool page still
    // live in the committed tree (the corruption a pre-commit crash would
    // expose). This is the cross-boundary half of the defer the in-txn pop
    // check above cannot see (session-COW dedup COWs each leaf once, so the
    // supersede and its only in-txn pop are the same event).
    let reachable: std::collections::HashSet<u64> = {
        let mut cache = tm.cache.borrow_mut();
        let committed = FreeMapTree::from_roots(
            tm.committed_roots.freemap_page,
            tm.committed_roots.freemap_depth,
        );
        committed
            .reachable_pages(&mut cache)
            .unwrap()
            .into_iter()
            .collect()
    };
    for id in &tm.pending_structural_frees {
        assert!(
            !reachable.contains(id),
            "one-commit-defer VIOLATION: page {id} is queued for reuse in T+2 but is \
                 still reachable in the committed freemap tree — a same-transaction \
                 supersede leaked into the reuse pool while still live"
        );
    }
}

// PROPERTY 2 — rollback resets the recycle pools and session state.
//
// A rollback must leave the structural recycle exactly as a clean begin would
// see it: `structural_reuse` restored to the committed recycle state (derived
// from `pending_structural_frees`), `structural_superseded` cleared, and
// `freemap_session_owned` cleared. Leaking any of these into the next
// transaction would let it reuse a page the committed tree still references,
// or skip a needed COW on a now-committed page — both corruption.
#[test]
fn structural_recycle_rollback_resets_pools() {
    let mut tm = fresh_manager();
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];

    // Establish a non-empty committed recycle so the test exercises a real
    // restore target, not just emptiness.
    let survivors = structural_churn(&mut tm, &big, 8, 4);
    let committed_recycle: Vec<u64> = tm.pending_structural_frees.clone();
    assert!(
        !committed_recycle.is_empty(),
        "precondition: a prior commit must leave a non-empty deferred recycle"
    );

    // A transaction that mutates all three pools: allocations + deletes COW
    // the freemap (filling session-owned + superseded), and the deletes' frees
    // make persist-side reuse pops drain `structural_reuse`. Do NOT commit.
    tm.begin().unwrap();
    let _fresh: Vec<u64> = (0..6).map(|_| tm.allocate(&big).unwrap()).collect();
    for h in &survivors {
        tm.delete(*h).unwrap();
    }
    // The session-owned set is populated by freemap COWs on the alloc/delete
    // path; assert the test actually dirtied the state it is about to roll
    // back (else the reset assertions are vacuous).
    assert!(
        !tm.freemap_session_owned.is_empty() || !tm.structural_superseded.is_empty(),
        "precondition: the pre-rollback churn must have mutated freemap session/supersede state"
    );

    tm.rollback().unwrap();

    // begin() CLONES `pending_structural_frees` into `structural_reuse`, so an
    // aborted transaction's recycle is exactly the pre-transaction one: the
    // committed recycle must be intact, and the working pools cleared.
    assert_eq!(
        tm.pending_structural_frees, committed_recycle,
        "rollback must leave the committed deferred recycle intact"
    );
    let recycle_after: std::collections::HashSet<u64> =
        tm.pending_structural_frees.iter().copied().collect();
    let committed_set: std::collections::HashSet<u64> = committed_recycle.iter().copied().collect();
    assert_eq!(
        recycle_after, committed_set,
        "the post-rollback recycle (what the next begin will seed structural_reuse from) \
             must equal the committed recycle state"
    );
    assert!(
        tm.structural_superseded.is_empty(),
        "rollback must clear structural_superseded — those committed-tree pages are \
             still referenced and must never be recycled"
    );
    assert!(
        tm.freemap_session_owned.is_empty(),
        "rollback must clear freemap_session_owned — leaking it would suppress a needed \
             COW and mutate a live committed page in place next transaction"
    );

    // Crucial follow-through: the next transaction must reuse ONLY the
    // committed recycle, proving no aborted-transaction page leaked into the
    // pool. (An aborted supersede leaking into reuse is a classic double-free.)
    let _ = take_structural_reuse_log();
    tm.begin().unwrap();
    let mut next: Vec<u64> = (0..8).map(|_| tm.allocate(&big).unwrap()).collect();
    for h in survivors {
        tm.delete(h).unwrap();
    }
    tm.commit().unwrap();
    for id in take_structural_reuse_log() {
        assert!(
            committed_set.contains(&id),
            "post-rollback transaction reused freemap page {id} not in the committed \
                 recycle {committed_set:?} — rollback leaked structural pool state"
        );
    }
    for h in next.drain(..) {
        tm.begin().unwrap();
        tm.delete(h).unwrap();
        tm.commit().unwrap();
    }
}

// PROPERTY 2b — the orphan sweep must NOT run under a savepoint.
//
// `rollback_to(savepoint)` rewinds the roots + cache watermark but does NOT
// reset the structural recycle streams. The only path that COWs the freemap
// (mutating those streams) while a savepoint is open is the defrag orphan
// sweep. If the sweep ran under a savepoint, it could drain a committed-LIVE
// freemap page into `structural_superseded`; after `rollback_to` (which
// leaves the stream intact) + commit (which promotes it), the NEXT
// transaction would reuse that still-durably-referenced page as a COW target
// and overwrite it — silent durable freemap corruption.
//
// The fix guards `reclaim_freemap_orphans` with `savepoints.is_empty()`.
// This test reproduces the trigger end-to-end and asserts the committed
// freemap tree survives intact. Counterfactual: removing the guard makes the
// committed-tree-intact assertion (or the no-reuse-of-committed-page check)
// fail.
#[test]
fn orphan_sweep_skipped_under_savepoint_preserves_committed_freemap() {
    let mut tm = fresh_manager();
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];

    // Build a real multi-page freemap with committed structural state so the
    // committed tree has actual nodes to corrupt. After this the committed
    // freemap root/depth describe a non-trivial tree.
    let survivors = structural_churn(&mut tm, &big, 8, 4);

    // Snapshot the committed freemap's free-set and reachable node set BEFORE
    // the savepoint episode. These are the ground truth the episode must not
    // disturb.
    let committed_root = tm.committed_roots.freemap_page;
    let committed_depth = tm.committed_roots.freemap_depth;
    assert_ne!(
        committed_root, PAGE_ID_NONE,
        "precondition: a committed freemap tree must exist"
    );
    let (free_before, reachable_before): (
        std::collections::BTreeSet<u64>,
        std::collections::HashSet<u64>,
    ) = {
        let mut cache = tm.cache.borrow_mut();
        let tree = FreeMapTree::from_roots(committed_root, committed_depth);
        let reachable: std::collections::HashSet<u64> = tree
            .reachable_pages(&mut cache)
            .unwrap()
            .into_iter()
            .collect();
        // The set of currently-free ids, scanned over the allocation range.
        let total = cache.next_page_id();
        let mut free = std::collections::BTreeSet::new();
        for id in 0..total {
            if tree.is_free(&mut cache, id).unwrap() {
                free.insert(id);
            }
        }
        (free, reachable)
    };

    // Episode: open a transaction, take a savepoint, forge a freemap orphan,
    // and invoke the sweep. With the guard the sweep is a no-op (returns 0)
    // and touches NO structural stream; without the guard it would reclaim the
    // forged orphan, COWing the committed freemap and draining the superseded
    // live page into `structural_superseded`.
    tm.begin().unwrap();
    tm.savepoint("sp").unwrap();
    let _orphan = tm.test_forge_freemap_orphan().unwrap();
    let reclaimed = tm.reclaim_freemap_orphans().unwrap();
    assert_eq!(
        reclaimed, 0,
        "the orphan sweep must be a no-op under an active savepoint (got {reclaimed})"
    );
    // The streams the rollback_to does NOT reset must be untouched by the
    // sweep, or the rollback leaves dangerous residue.
    assert!(
        tm.structural_superseded.is_empty(),
        "sweep under savepoint leaked into structural_superseded: {:?}",
        tm.structural_superseded
    );

    // Roll back to the savepoint (discards the forged page) and commit the
    // now-empty transaction. With the guard this commit promotes nothing
    // dangerous; without it, the committed-live page the sweep superseded is
    // promoted into the reusable pool.
    tm.rollback_to("sp").unwrap();
    tm.commit().unwrap();

    // Next transaction does a freemap-COWing operation (delete a survivor,
    // which marks its page free and COWs the freemap). If a committed-live
    // freemap page had been promoted into the reuse pool, this is where it
    // would be drawn as a COW target and OVERWRITTEN.
    tm.begin().unwrap();
    tm.delete(survivors[0]).unwrap();
    tm.commit().unwrap();

    // The committed freemap tree the ORIGINAL (pre-episode) commit described
    // must still be readable and self-consistent: no node it referenced was
    // overwritten. We re-open the ORIGINAL committed root/depth and confirm
    // its reachable set and free-set are unchanged by everything above.
    // (Deleting survivors[0] in the final txn only ADDS a free bit; it never
    // removes one and never makes a previously-reachable node unreadable.)
    let (free_after, reachable_after): (
        std::collections::BTreeSet<u64>,
        std::collections::HashSet<u64>,
    ) = {
        let mut cache = tm.cache.borrow_mut();
        let tree = FreeMapTree::from_roots(committed_root, committed_depth);
        let reachable: std::collections::HashSet<u64> = tree
            .reachable_pages(&mut cache)
            .unwrap()
            .into_iter()
            .collect();
        let total = cache.next_page_id();
        let mut free = std::collections::BTreeSet::new();
        for id in 0..total {
            if tree.is_free(&mut cache, id).unwrap() {
                free.insert(id);
            }
        }
        (free, reachable)
    };
    assert_eq!(
        reachable_after, reachable_before,
        "the original committed freemap tree's node set changed — a committed \
             freemap page was reused-and-overwritten (savepoint guard regression)"
    );
    assert_eq!(
        free_after, free_before,
        "the original committed freemap tree's free-set changed — its on-disk \
             bitmap pages were overwritten by a COW into a reused committed page"
    );
}

// A corrupt DEAD (non-reachable) page must NOT poison the orphan sweep.
//
// 2026-06-22 review decision ("skip unreadable dead pages"): the sweep scans
// every non-reachable page id to classify it as a freemap orphan. A page that
// is not in the live tree but fails to read because it is GARBAGE cannot be
// confirmed as an orphan, and its corruption is irrelevant (it is dead), so
// the sweep SKIPS it instead of propagating fatal. Contrast: a corrupt page
// REACHABLE from the live tree must still surface fatal via `reachable_pages`
// — that path is deliberately NOT weakened (asserted below).
#[test]
fn corrupt_dead_page_does_not_poison_orphan_sweep() {
    let mut tm = fresh_manager();
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];

    // Build a committed multi-page freemap and a real orphan to reclaim, so
    // the sweep has live work to do AND a corrupt dead page to step over.
    let _survivors = structural_churn(&mut tm, &big, 8, 4);

    tm.begin().unwrap();
    let real_orphan = tm.test_forge_freemap_orphan().unwrap();
    let corrupt = tm.test_forge_corrupt_dead_page().unwrap();
    assert_ne!(real_orphan, corrupt);

    // The sweep must succeed (NOT poison) despite the corrupt dead page, and
    // must still reclaim the legitimate orphan it can read.
    let reclaimed = tm.reclaim_freemap_orphans().unwrap();
    assert!(
        reclaimed >= 1,
        "sweep must skip the corrupt dead page yet still reclaim the readable \
             orphan (reclaimed={reclaimed})"
    );
    assert!(
        !tm.is_poisoned(),
        "a corrupt DEAD page must not poison the orphan sweep"
    );
    tm.commit().unwrap();

    // Contrast: the live-tree walk MUST still propagate fatal on a corrupt
    // node reachable from the committed tree — `reachable_pages` is NOT
    // weakened by the dead-page softening above. Corrupt a REAL live node on
    // disk (flip its type byte to the wrong PageType but keep the checksum
    // valid — a type-corruption reached via a live pointer) and confirm the
    // walk surfaces `CorruptPage` for exactly that node.
    let live_root = tm.committed_roots.freemap_page;
    let live_depth = tm.committed_roots.freemap_depth;
    {
        let mut cache = tm.cache.borrow_mut();
        // Pick a non-root reachable node so the root's own type check passes
        // and the failure happens during descent (the path the softening must
        // NOT touch). If the tree is a single root (depth 0), the root IS the
        // only node; corrupt it directly.
        let tree = FreeMapTree::from_roots(live_root, live_depth);
        let reachable: Vec<u64> = tree
            .reachable_pages(&mut cache)
            .unwrap()
            .into_iter()
            .collect();
        let victim = reachable
            .iter()
            .copied()
            .find(|&id| id != live_root)
            .unwrap_or(live_root);
        // Wrong type byte, valid checksum: not a bit-flip, a position-type
        // corruption that `check_type` rejects. Flip FreeMap<->FreeMapInterior.
        {
            let buf = cache.get_mut(victim).unwrap();
            let wrong = if buf[0] == crate::page::PageType::FreeMap as u8 {
                crate::page::PageType::FreeMapInterior as u8
            } else {
                crate::page::PageType::FreeMap as u8
            };
            buf[0] = wrong;
            page::stamp_checksum(buf);
        }
        let err = tree.reachable_pages(&mut cache).unwrap_err();
        assert!(
            matches!(err, ChiselError::CorruptPage { .. }),
            "reachable_pages must surface fatal CorruptPage on a corrupt LIVE \
                 node — the dead-page softening must not weaken the live walk \
                 (got {err:?})"
        );
    }
}

// PROPERTY 3 — no lost/double free across reuse cycles.
//
// Churn for several commits, each freeing whole pages. After EACH commit:
//   (a) every page freed that commit reads `is_free == true` via a fresh
//       `FreeMapTree::from_roots(committed root, depth)` (no lost free); and
//   (b) NO page id is simultaneously reachable in the LIVE freemap tree and
//       present in `structural_reuse`-derived pool (`pending_structural_frees`)
//       — a reuse-pool page MUST be dead (no double-allocation: the same page
//       cannot be both a live tree node and a free structural target).
#[test]
fn structural_recycle_no_lost_or_double_free() {
    let mut tm = fresh_manager();
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];

    // Seed a rotating population so each round both allocates and frees whole
    // pages, keeping the freemap COWing every commit.
    let mut live: Vec<u64> = Vec::new();
    tm.begin().unwrap();
    for _ in 0..10 {
        live.push(tm.allocate(&big).unwrap());
    }
    tm.commit().unwrap();

    for round in 0..8u32 {
        // Delete half, allocate a fresh half: whole-page frees every commit.
        let to_delete: Vec<u64> = live.drain(..5).collect();
        tm.begin().unwrap();
        for h in &to_delete {
            tm.delete(*h).unwrap();
        }
        // Capture this commit's data frees BEFORE commit clears the vector.
        let freed_this_commit: Vec<u64> = tm.txn_freed_pages.clone();
        for _ in 0..5 {
            live.push(tm.allocate(&big).unwrap());
        }
        // Re-snapshot: allocations may have reused some freed ids already,
        // pulling them back out of the free set. The invariant we pin is on
        // the pages STILL freed at commit time, so take the union of frees and
        // exclude any id re-claimed as a live value this same transaction.
        tm.commit().unwrap();

        assert!(
            !freed_this_commit.is_empty(),
            "round {round}: deletes must free at least one whole page"
        );

        // (a) No lost free: every page this commit freed (and did not re-claim
        // as a live value) reads free in the committed tree.
        let live_set: std::collections::HashSet<u64> = live.iter().copied().collect();
        {
            let mut cache = tm.cache.borrow_mut();
            let committed = FreeMapTree::from_roots(
                tm.committed_roots.freemap_page,
                tm.committed_roots.freemap_depth,
            );
            for id in &freed_this_commit {
                // A freed data page re-claimed as a live value this same
                // transaction is correctly NOT free; skip those.
                if live_set.contains(id) {
                    continue;
                }
                assert!(
                    committed.is_free(&mut cache, *id).unwrap(),
                    "round {round}: page {id} was freed this commit but is NOT free in \
                         the committed freemap tree — a lost free"
                );
            }
        }

        // (b) No double-free: a page in the structural reuse pool must be DEAD,
        // i.e. never simultaneously reachable in the live freemap tree. Walk
        // the committed tree and intersect with the deferred recycle pool.
        let reachable: std::collections::HashSet<u64> = {
            let mut cache = tm.cache.borrow_mut();
            let committed = FreeMapTree::from_roots(
                tm.committed_roots.freemap_page,
                tm.committed_roots.freemap_depth,
            );
            committed
                .reachable_pages(&mut cache)
                .unwrap()
                .into_iter()
                .collect()
        };
        for id in &tm.pending_structural_frees {
            assert!(
                !reachable.contains(id),
                "round {round}: freemap page {id} is in the structural reuse pool AND \
                     still reachable in the live committed freemap tree — a reuse-pool page \
                     must be dead (handing it out as a COW target would double-allocate it)"
            );
        }
        // A reuse-pool page must also not be marked free in the bitmap (the
        // two reclamation channels are disjoint by design — a structural page
        // rides the in-memory pool, never the bitmap).
        {
            let mut cache = tm.cache.borrow_mut();
            let committed = FreeMapTree::from_roots(
                tm.committed_roots.freemap_page,
                tm.committed_roots.freemap_depth,
            );
            for id in &tm.pending_structural_frees {
                assert!(
                    !committed.is_free(&mut cache, *id).unwrap(),
                    "round {round}: structural-reuse page {id} is ALSO marked free in the \
                         bitmap — the two reclamation channels overlap, risking a double hand-out"
                );
            }
        }
    }
}

// The defrag orphan-sweep reclaims a freemap-typed page that a crash would
// have stranded: forge one (a checksum-valid FreeMapInterior unreferenced by
// the live tree and not marked free), sweep, and confirm it now reads free in
// the committed tree. This is the crash-recovery story for the in-memory
// structural recycle — without the sweep these pages leak permanently.
#[test]
fn reclaim_freemap_orphans_marks_lost_freemap_pages_free() {
    let mut tm = fresh_manager();
    // Churn so a real multi-page freemap (leaf + spine) exists: overflow-sized
    // values give each handle its own page, so deletes free whole pages.
    let big: Vec<u8> = vec![0xCD; MAX_INLINE_VALUE + 32];
    tm.begin().unwrap();
    let mut hs = Vec::new();
    for _ in 0..40 {
        hs.push(tm.allocate(&big).unwrap());
    }
    tm.commit().unwrap();
    tm.begin().unwrap();
    for h in hs.iter().step_by(2) {
        tm.delete(*h).unwrap();
    }
    tm.commit().unwrap();

    // Forge an orphan exactly as a crash leaves a lost recycle-pool page:
    // extended, freemap-typed, unreferenced, not free.
    let orphan = tm.test_forge_freemap_orphan().unwrap();

    tm.begin().unwrap();
    let reclaimed = tm.reclaim_freemap_orphans().unwrap();
    assert!(reclaimed >= 1, "the forged orphan must be reclaimed");
    tm.commit().unwrap();

    // The orphan now reads free in the committed tree (data-reusable bitmap
    // space, disjoint from the structural recycle pool).
    let mut cache = tm.cache.borrow_mut();
    let tree = FreeMapTree::from_roots(
        tm.committed_roots.freemap_page,
        tm.committed_roots.freemap_depth,
    );
    assert!(
        tree.is_free(&mut cache, orphan).unwrap(),
        "reclaimed orphan {orphan} must read free in the committed freemap"
    );
}

// The sweep's exclusion set is load-bearing: a page CURRENTLY in the live
// in-memory recycle pool (`structural_reuse`) is LIVE recycling state, not an
// orphan. Reclaiming it into the bitmap while it is also pool-reusable would
// double-hand-out the page. Seed the pool with a forged freemap-typed page
// (matching the orphan shape in every respect EXCEPT pool membership) and
// assert the sweep skips it and leaves the pool untouched.
#[test]
fn reclaim_freemap_orphans_excludes_live_recycle_pool() {
    let mut tm = fresh_manager();
    // Establish a real freemap tree so the sweep does not early-exit on a
    // PAGE_ID_NONE root.
    let big: Vec<u8> = vec![0xCD; MAX_INLINE_VALUE + 32];
    tm.begin().unwrap();
    let h = tm.allocate(&big).unwrap();
    tm.commit().unwrap();
    tm.begin().unwrap();
    tm.delete(h).unwrap();
    tm.commit().unwrap();

    // Forge a freemap-typed page that WOULD be flagged as an orphan, then put
    // it in the live reuse pool so the exclusion set must spare it.
    let pooled = tm.test_forge_freemap_orphan().unwrap();

    tm.begin().unwrap();
    tm.structural_reuse.push(pooled);
    let reclaimed = tm.reclaim_freemap_orphans().unwrap();
    assert_eq!(
        reclaimed, 0,
        "a page in the live recycle pool must NOT be reclaimed as an orphan"
    );
    assert!(
        tm.structural_reuse.contains(&pooled),
        "the sweep must leave the live recycle pool untouched"
    );
    tm.rollback().unwrap();
}

// Regression test for ISSUES.md I28. I19 introduced `CacheFull` as
// an **operational** error (documented as "commit or rollback to
// recover"), but `commit_inner` runs `persist_freemap` BEFORE
// `cache.flush()` — and `persist_freemap` itself calls
// `allocate_data_page`, which may trip `maybe_evict`'s ceiling
// check when every existing cache entry is dirty. Pre-fix the
// resulting `CacheFull` propagated out of commit_inner and
// commit()'s poison wrapper poisoned the manager unconditionally.
// The recovery advice ("commit to flush") became impossible to
// follow because commit itself failed.
//
// Post-fix: commit drains the cache BEFORE persist_freemap, so the
// cap is always reachable via eviction when persist_freemap
// itself allocates. CacheFull cannot arise on the commit path.
//
// Setup note: we deliberately use a small `max_pages` so the
// strict cap is cheap to saturate with a few allocations.
// spillway_max_bytes=0 keeps CacheFull reachable (no spillway
// escape hatch), matching the pre-spillway path this test exercises.
#[test]
fn commit_does_not_poison_when_cache_is_at_strict_cap() {
    // I66 (ISSUES.md, 2026-05-22): TempDir for RAII cleanup —
    // replaces the pre-I66 NamedTempFile + std::mem::forget(file)
    // pattern that leaked the temp path on every test run.
    let _dir = TempDir::new().unwrap();
    let db_path = _dir.path().join("test.chisel");
    let io = PageIo::open(&db_path, false).unwrap();
    // max_pages=16 — big enough for baseline operations (handle-table
    // root + superblocks + freemap) to coexist, small enough that a
    // handful of big allocations saturate the strict cap quickly.
    // spillway_max_bytes=0 means CacheFull fires at max_pages itself.
    let cache = PageCache::new(
        io,
        16 * PAGE_SIZE as u64,
        0,
        crate::DrainInsertion::LruTail,
        crate::SpillwayLocation::InMemory,
    );
    let mut tm = TransactionManager::create_new(cache, 2).unwrap();
    tm.begin().unwrap();
    tm.commit().unwrap();

    // Seed one handle so the victim transaction can produce a
    // non-empty `txn_freed_pages` via delete. Without any frees AND
    // with `current_freemap == committed_freemap`, `persist_freemap`
    // takes its early-exit path and never allocates — which would
    // mean it also cannot trip CacheFull, and the test would fail
    // to reproduce the bug.
    let big: Vec<u8> = vec![0x99; MAX_INLINE_VALUE + 32];
    tm.begin().unwrap();
    let victim = tm.allocate(&big).unwrap();
    tm.commit().unwrap();

    // Victim transaction: delete to populate txn_freed_pages, then
    // allocate until the cache saturates at the strict cap.
    // CacheFull from an allocate() is operational — we catch it and
    // proceed to commit, which is what we actually want to stress.
    tm.begin().unwrap();
    tm.delete(victim).unwrap();
    let mut saturated = false;
    for _ in 0..200 {
        match tm.allocate(&big) {
            Ok(_) => continue,
            Err(ChiselError::CacheFull { .. }) => {
                saturated = true;
                break;
            }
            Err(e) => panic!("unexpected error during cache-fill setup: {e:?}"),
        }
    }
    assert!(
        saturated,
        "test precondition: cache did not reach CacheFull in 200 allocations"
    );
    assert!(
        !tm.is_poisoned(),
        "precondition: CacheFull from allocate() is operational and must not poison"
    );

    // The actual I28 check. Pre-fix, `persist_freemap`'s internal
    // `allocate_data_page` trips the ceiling and propagates
    // CacheFull out of commit_inner; commit()'s poison wrapper
    // then sets the poison flag. Post-fix commit drains first.
    let result = tm.commit();
    assert!(
        result.is_ok(),
        "I28: commit over a saturated cache should succeed; got {result:?}"
    );
    assert!(
        !tm.is_poisoned(),
        "I28: CacheFull during commit must not poison — it's operational by design"
    );
}

// ===================================================================
// BUG#2 (2026-06-16 deepdive): forward/reverse tag-map atomic staging.
//
// `allocate_tagged` maintains two maps that must stay in lockstep: the
// FORWARD map (each chunk's `HandleEntry.tag`, in the handle table) and
// the REVERSE map (tag -> handles, in the membership index, powering
// `handles_with_tag`/delete-by-tag). Before the fix the two roots were
// installed sequentially, so a NON-FATAL `CacheFull`/`SpillwayFull`
// striking between them committed a half-update:
//   * allocate: the forward map gained the tag but the reverse did not
//     (a tagged chunk invisible to `handles_with_tag`);
//   * delete:   the tombstone landed but the reverse entry stayed (a
//     stale member that later escalates to a FATAL CorruptPage when
//     surfaced and acted upon).
// Because those errors are non-fatal they do NOT poison the manager, so
// the half-update survives to `commit()` and onto disk.
//
// Atomic staging computes BOTH candidate roots in a fallible prepare
// phase and installs them together in an infallible phase, so a mid-op
// failure is a no-op for the INSTALLED state (neither map changes, the
// handle id is not burned, inline slot bookkeeping is clean). There is a
// bounded freemap-reuse residue on the abnormal path — see the
// `abort_allocate_prepare` doc and the
// `aborted_tagged_allocate_with_freemap_reuse_is_consistent_and_rollback_reclaims`
// test. The `fail_next_membership_op` hook fires a simulated CacheFull at
// exactly the reverse-map step — the precise divergence window — so these
// are deterministic, not timing-dependent.

#[test]
fn allocate_membership_failure_leaves_maps_consistent() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    // The id the about-to-fail allocate will (try to) use.
    let ghost = tm.current_roots.next_handle;

    tm.fail_next_membership_op.set(true);
    let err = tm.allocate_tagged(b"payload", 7).unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected the injected CacheFull, got {err:?}"
    );
    assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

    // The forward (tag) and reverse (membership) maps MUST agree. Pre-fix
    // the forward map carried tag 7 for `ghost` while the reverse index
    // did not — a committed-out-of-sync divergence.
    let in_reverse = tm.handles_with_tag(7).unwrap().contains(&ghost);
    let in_forward = matches!(tm.tag(ghost), Ok(7));
    assert_eq!(
        in_forward, in_reverse,
        "forward/reverse tag maps diverged after a failed tagged allocate"
    );

    // Atomic staging makes the failed allocate a no-op for the installed
    // state: neither map changed and the handle id was not even burned.
    assert!(
        !in_forward,
        "failed allocate must not install the forward entry"
    );
    assert_eq!(
        tm.current_roots.next_handle, ghost,
        "failed allocate must not burn the handle id"
    );

    // ...and the no-op extends to the R1 packing bookkeeping: the inline
    // value's data slot was released, so no phantom live-slot count or ghost
    // insert cursor survives to skew later packing / defrag density / page
    // reclamation. (Pre-fix this leaked `{page: 1}` and `Some(page)`.)
    assert!(
        tm.current_live_slots.is_empty(),
        "failed allocate left a phantom live-slot count: {:?}",
        tm.current_live_slots
    );
    assert_eq!(
        tm.insert_cursor, None,
        "failed allocate left a ghost insert cursor"
    );

    // The manager is still fully usable: a disarmed retry reuses the same
    // id and is consistent across BOTH maps, in-session and after commit.
    let h = tm.allocate_tagged(b"payload", 7).unwrap();
    assert_eq!(h, ghost, "retry should reuse the un-burned handle id");
    assert_eq!(tm.tag(h).unwrap(), 7);
    assert!(tm.handles_with_tag(7).unwrap().contains(&h));
    tm.commit().unwrap();
    assert!(tm.handles_with_tag(7).unwrap().contains(&h));
    // C1: no page reachable from committed_roots may be free in the
    // committed freemap — pins that the dropped prepare freed-lists never
    // queued a still-referenced page.
    assert_no_reachable_page_is_free(&tm);
}

// The FORWARD-step counterpart: a non-fatal CacheFull during the
// handle-table insert (the step that eagerly bumps the descent depth via
// HandleTable::grow, and on a fresh DB lazily materializes the root). The
// prepare-abort must restore BOTH the depth and the handle-table root
// pointer and release the inline slot — a true no-op.
#[test]
fn allocate_handle_table_failure_leaves_maps_consistent() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    // Fresh DB: the handle table has never been materialized.
    let saved_root = tm.current_roots.handle_table_page;
    assert_eq!(saved_root, PAGE_ID_NONE, "precondition: empty handle table");
    let ghost = tm.current_roots.next_handle;

    tm.fail_next_handle_table_op.set(true);
    let err = tm.allocate_tagged(b"payload", 7).unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected the injected CacheFull, got {err:?}"
    );
    assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

    // Complete no-op: the lazily-created root is reverted (back to
    // PAGE_ID_NONE), the handle id is not burned, neither map has content,
    // and the R1 packing state is clean.
    assert_eq!(
        tm.current_roots.handle_table_page, saved_root,
        "failed forward step left a lazily-materialized handle-table root installed"
    );
    assert_eq!(tm.current_roots.next_handle, ghost);
    assert!(
        matches!(tm.tag(ghost), Err(ChiselError::InvalidHandle(_))),
        "ghost handle after failed allocate_tagged must be InvalidHandle"
    );
    assert!(tm.handles_with_tag(7).unwrap().is_empty());
    assert!(
        tm.current_live_slots.is_empty(),
        "failed forward step left a phantom live-slot count: {:?}",
        tm.current_live_slots
    );
    assert_eq!(tm.insert_cursor, None);

    // Disarmed retry succeeds and is consistent across both maps.
    let h = tm.allocate_tagged(b"payload", 7).unwrap();
    assert_eq!(h, ghost, "retry should reuse the un-burned handle id");
    assert_eq!(tm.tag(h).unwrap(), 7);
    assert!(tm.handles_with_tag(7).unwrap().contains(&h));
    tm.commit().unwrap();
    assert_no_reachable_page_is_free(&tm);
}

#[test]
fn delete_membership_failure_leaves_maps_consistent() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"payload", 7).unwrap();
    tm.commit().unwrap();

    tm.begin().unwrap();
    tm.fail_next_membership_op.set(true);
    let err = tm.delete(h).unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected the injected CacheFull, got {err:?}"
    );
    assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

    // Pre-fix the tombstone (forward) was installed but the reverse entry
    // stayed, so `read(h)` failed while `handles_with_tag` still listed
    // `h` — exactly the divergence that later escalates to CorruptPage.
    let in_reverse = tm.handles_with_tag(7).unwrap().contains(&h);
    let still_live = tm.read(h).is_ok();
    assert_eq!(
        still_live, in_reverse,
        "forward/reverse tag maps diverged after a failed tagged delete"
    );
    // Atomic staging makes the failed delete a COMPLETE no-op.
    assert!(still_live, "failed delete must not install the tombstone");
    assert!(in_reverse, "failed delete must not drop the reverse entry");

    // A disarmed retry succeeds and removes `h` from BOTH maps.
    tm.delete(h).unwrap();
    assert!(
        matches!(tm.read(h), Err(ChiselError::InvalidHandle(_))),
        "handle must be InvalidHandle after successful delete"
    );
    assert!(!tm.handles_with_tag(7).unwrap().contains(&h));
    tm.commit().unwrap();
    assert!(!tm.handles_with_tag(7).unwrap().contains(&h));
    assert_no_reachable_page_is_free(&tm);
}

// Delete's durability guard (delete is the more dangerous direction: a
// stale reverse member for a tombstoned handle later escalates to a FATAL
// CorruptPage). Fail the reverse-map step, COMMIT, then REOPEN: the failed
// delete must have committed NOTHING — `h` is still live AND still a tagged
// member on disk. Pre-fix the tombstone committed while the reverse member
// stayed, so the reopened DB would read `h` as deleted yet still list it.
#[test]
fn delete_membership_failure_survives_reopen_consistently() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bug2-del.chisel");

    let open = |create: bool| -> TransactionManager {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        if create {
            TransactionManager::create_new(cache, 2).unwrap()
        } else {
            TransactionManager::open_existing(cache).unwrap()
        }
    };

    let h = {
        let mut tm = open(true);
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"payload", 9).unwrap();
        tm.commit().unwrap();

        tm.begin().unwrap();
        tm.fail_next_membership_op.set(true);
        assert!(matches!(
            tm.delete(h).unwrap_err(),
            ChiselError::CacheFull { .. }
        ));
        // Commit the post-failure state: pre-fix this durably tombstones the
        // forward map while the reverse keeps `h`; post-fix it is a no-op.
        tm.commit().unwrap();
        h
    };

    // Reopen: the forward (read) and reverse (handles_with_tag) views must
    // agree, and since the delete was a no-op both must still see `h`.
    let tm = open(false);
    let still_live = tm.read(h).is_ok();
    let in_reverse = tm.handles_with_tag(9).unwrap().contains(&h);
    assert_eq!(
        still_live, in_reverse,
        "reopened forward/reverse views diverged for the failed delete's handle"
    );
    assert!(
        still_live && in_reverse,
        "a failed tagged delete must commit nothing: h should survive in both maps \
             (read ok = {still_live}, in reverse index = {in_reverse})"
    );
}

// Pins the documented `delete_with_tag` error contract: a mid-pass failure
// returns Err (NO TagDropProgress — the dropped-this-pass set is not
// reported), is non-fatal/recoverable, and leaves a CONSISTENT partial
// state because each delete_inner is atomic (BUG#2 staging) — so exactly the
// members processed before the failure are gone from BOTH maps, and the
// partial drop is committable and resumable.
#[test]
fn delete_with_tag_mid_pass_failure_is_consistent_and_drops_progress() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let members = [
        tm.allocate_tagged(b"m0", 5).unwrap(),
        tm.allocate_tagged(b"m1", 5).unwrap(),
        tm.allocate_tagged(b"m2", 5).unwrap(),
    ];
    tm.commit().unwrap();

    tm.begin().unwrap();
    // Let the 1st delete in the pass commit, fail the 2nd's membership op.
    tm.fail_membership_op_after.set(2);
    let err = tm.delete_with_tag(5, 3).unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected the injected CacheFull, got {err:?}"
    );
    assert!(
        !tm.is_poisoned(),
        "a non-fatal mid-pass error must not poison"
    );

    // Exactly one member dropped before the failure (delete_inner is atomic,
    // so the failed 2nd delete is a no-op). Don't assume index iteration
    // order — count instead.
    let live: Vec<u64> = members
        .into_iter()
        .filter(|&h| tm.read(h).is_ok())
        .collect();
    assert_eq!(
        live.len(),
        2,
        "exactly one member dropped before the failure"
    );
    // Forward (read-ok set) and reverse (membership index) agree exactly.
    let mut idx = tm.handles_with_tag(5).unwrap();
    let mut live_sorted = live.clone();
    idx.sort_unstable();
    live_sorted.sort_unstable();
    assert_eq!(
        live_sorted, idx,
        "forward/reverse maps consistent after a failed delete_with_tag pass"
    );

    // The consistent partial drop is committable...
    tm.commit().unwrap();
    assert_eq!(tm.handles_with_tag(5).unwrap().len(), 2);
    assert_no_reachable_page_is_free(&tm);

    // ...and the bounded loop finishes cleanly on a disarmed retry.
    tm.begin().unwrap();
    let (_, complete) = tm.delete_with_tag(5, 3).unwrap();
    assert!(complete, "retry must drain the tag");
    tm.commit().unwrap();
    assert!(tm.handles_with_tag(5).unwrap().is_empty());
    assert_no_reachable_page_is_free(&tm);
}

// CRITICAL durable-corruption regression (surfaced by the BUG#2 adversarial
// review): `update_inner` must not free the OLD value's pages before the new
// entry is durably installed. A non-fatal CacheFull at the new-value-write
// step — which pre-fix runs AFTER the old free — left the committed handle
// still pointing at pages already queued for reclamation, so commit freed a
// reachable page and a later reuse silently corrupted the live value.
#[test]
fn update_value_write_failure_does_not_free_old_value_pages() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    // Overflow-sized old value, so the whole chain is at stake.
    let big = vec![0xABu8; MAX_INLINE_VALUE * 3];
    let h = tm.allocate(&big).unwrap();
    tm.commit().unwrap();
    assert_eq!(tm.read(h).unwrap(), big, "precondition: old value readable");

    tm.begin().unwrap();
    tm.fail_next_update_value_write.set(true);
    let err = tm.update(h, b"replacement").unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected the injected CacheFull, got {err:?}"
    );
    assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

    // The failed update is a no-op in-session: the old value is intact.
    assert_eq!(
        tm.read(h).unwrap(),
        big,
        "failed update lost the old value in-session"
    );

    // Commit the post-failure state, then assert C1: no page the committed
    // handle still references may be free. Pre-fix the old overflow chain was
    // queued into txn_freed_pages and freed here while `h` still points at it.
    tm.commit().unwrap();
    assert_no_reachable_page_is_free(&tm);
    assert_eq!(
        tm.read(h).unwrap(),
        big,
        "failed update lost the old value after commit"
    );

    // End-to-end: churn allocations to force the freemap to hand out any
    // wrongly-freed pages, then confirm the old value survived. Pre-fix the
    // reachable-but-free chain pages would be reused and overwritten, turning
    // read(h) into a CorruptPage / wrong bytes.
    tm.begin().unwrap();
    for i in 0..40u8 {
        tm.allocate(&[i; 64]).unwrap();
    }
    tm.commit().unwrap();
    assert_eq!(
        tm.read(h).unwrap(),
        big,
        "old value corrupted after its prematurely-freed pages were reused"
    );
}

// Same guarantee for an INLINE old value (the Live old-release path). When
// the old page held only this value, the pre-fix code released it to the
// freemap before the new write, so a failed update committed a
// reachable-but-free data page just like the overflow case.
#[test]
fn update_value_write_failure_does_not_free_old_inline_page() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let old = b"inline-original-value";
    let h = tm.allocate(old).unwrap();
    tm.commit().unwrap();

    tm.begin().unwrap();
    tm.fail_next_update_value_write.set(true);
    assert!(matches!(
        tm.update(h, b"replacement").unwrap_err(),
        ChiselError::CacheFull { .. }
    ));
    assert!(!tm.is_poisoned());
    assert_eq!(
        tm.read(h).unwrap(),
        old,
        "failed update lost the inline value"
    );

    tm.commit().unwrap();
    assert_no_reachable_page_is_free(&tm);
    assert_eq!(tm.read(h).unwrap(), old);

    // Force reuse, then confirm the old inline value survived.
    tm.begin().unwrap();
    for i in 0..40u8 {
        tm.allocate(&[i; 64]).unwrap();
    }
    tm.commit().unwrap();
    assert_eq!(
        tm.read(h).unwrap(),
        old,
        "old inline value corrupted after a prematurely-freed page was reused"
    );
}

// Highest-value coverage for the post-write prepare-unwind contract: the new
// value is ALREADY written when the handle-table install fails. The fix must
// (a) leave the OLD location referenced (never freed) and (b) release the
// just-written NEW inline slot so no phantom live-slot / ghost cursor
// survives. Driven via the shared fail_next_handle_table_op hook, which
// handle_table_insert_candidate honors for both allocate and update. (The
// old-overflow-walk failure exit runs the identical new-inline-slot release,
// so this test covers that contract too.)
#[test]
fn update_handle_table_failure_preserves_old_value_and_releases_new_slot() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let old = b"inline-original-value";
    let h = tm.allocate(old).unwrap();
    tm.commit().unwrap();

    tm.begin().unwrap();
    tm.fail_next_handle_table_op.set(true);
    let err = tm.update(h, b"small-new").unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected the injected CacheFull, got {err:?}"
    );
    assert!(!tm.is_poisoned());

    // (a) The old value is untouched — the old location was never freed.
    assert_eq!(tm.read(h).unwrap(), old, "failed update lost the old value");
    // (b) The new value's inline slot was released: the committed baseline
    // had exactly one live slot (for `old`), and the failed update must
    // leave precisely that — no phantom count for the abandoned new value.
    assert_eq!(
        tm.current_live_slots.values().sum::<u32>(),
        1,
        "failed update left a phantom live-slot: {:?}",
        tm.current_live_slots
    );

    // Durability: commit, assert C1, force reuse, re-read the old value.
    tm.commit().unwrap();
    assert_no_reachable_page_is_free(&tm);
    tm.begin().unwrap();
    for i in 0..40u8 {
        tm.allocate(&[i; 64]).unwrap();
    }
    tm.commit().unwrap();
    assert_eq!(
        tm.read(h).unwrap(),
        old,
        "old value corrupted after reuse following a failed update"
    );
}

// The headline durability guard: a failed tagged allocate, then COMMIT,
// then REOPEN from the last durable superblock. Pre-fix this persisted a
// forward-map ghost (a committed `HandleEntry.tag` with no reverse-index
// member); post-fix the failed allocate committed nothing.
#[test]
fn allocate_membership_failure_survives_reopen_consistently() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bug2.chisel");

    let open = |create: bool| -> TransactionManager {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        if create {
            TransactionManager::create_new(cache, 2).unwrap()
        } else {
            TransactionManager::open_existing(cache).unwrap()
        }
    };

    let ghost = {
        let mut tm = open(true);
        tm.begin().unwrap();
        tm.commit().unwrap();

        tm.begin().unwrap();
        let ghost = tm.current_roots.next_handle;
        tm.fail_next_membership_op.set(true);
        assert!(matches!(
            tm.allocate_tagged(b"payload", 9).unwrap_err(),
            ChiselError::CacheFull { .. }
        ));
        // Commit the post-failure state: pre-fix this makes the forward-map
        // ghost durable; post-fix it commits a clean no-op.
        tm.commit().unwrap();
        ghost
    };

    // Reopen and verify the FORWARD map carries no ghost tag-9 entry that
    // the REVERSE map is missing. `handles_with_tag(9)` reads the reverse
    // map (empty under both code paths); the discriminating check is the
    // forward map via `tag(ghost)`.
    let tm = open(false);
    let forward = tm.tag(ghost);
    assert!(
        forward.is_err() || forward.as_ref().unwrap() != &9,
        "reopened forward map has a ghost tag-9 entry (tag({ghost}) = {forward:?}) \
             with no matching reverse-index member — the maps committed out of sync"
    );
    assert!(
        tm.handles_with_tag(9).unwrap().is_empty(),
        "tag 9 should have no committed members"
    );
}

// When savepoints are absent (`reuse = true`), a failed tagged-allocate
// prepare may exercise the freemap REUSE path inside `cow_alloc` — the
// candidate COW clears free bits and advances the freemap tree BEFORE
// learning the membership step will fail. The abort restores the installed
// state (maps, handle id, inline slot) but intentionally leaves the
// freemap-reuse residue (bounded allocated-but-unreferenced pages), relying
// on rollback to discard them. This test seeds a committed freemap with free
// pages (so reuse fires), triggers the injection, and verifies:
//
//   1. The failed call returns the injected CacheFull.
//   2. The installed state is unchanged (both maps, next_handle, depth).
//   3. No C1 violation: every page reachable in the committed state is
//      consistent after the aborted allocate.
//   4. rollback() returns the freemap to its committed free-set: a fresh
//      `FreeMapTree::from_roots(committed_root, committed_depth)` reports
//      the same free bits as before the aborted allocate.
#[test]
fn aborted_tagged_allocate_with_freemap_reuse_is_consistent_and_rollback_reclaims() {
    let mut tm = fresh_manager();

    // --- Phase 1: seed a committed freemap with free DATA pages ---
    //
    // Allocate several overflow-sized values (each occupies at least one
    // whole page beyond the superblock/freemap spine), commit, then delete
    // some and commit again. The second commit's persist_freemap marks those
    // pages free, so the committed freemap now has reuse bits set.
    let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];
    let mut live_handles = Vec::new();
    tm.begin().unwrap();
    for _ in 0..6 {
        live_handles.push(tm.allocate_tagged(&big, 5).unwrap());
    }
    tm.commit().unwrap();

    // Delete the first 3 to free their overflow pages into the committed
    // freemap. Tag 5 still has 3 live members after this commit.
    tm.begin().unwrap();
    for h in live_handles.drain(..3) {
        tm.delete(h).unwrap();
    }
    tm.commit().unwrap();
    assert_no_reachable_page_is_free(&tm);

    // Precondition: the committed freemap must have free bits so that the
    // next `cow_alloc` exercises the reuse path (not just extend).
    let pre_fm_root = tm.committed_roots.freemap_page;
    let pre_fm_depth = tm.committed_roots.freemap_depth;
    let pre_next_handle = tm.committed_roots.next_handle;
    let pre_ht_root = tm.committed_roots.handle_table_page;
    let pre_mi_root = tm.committed_roots.membership_index_page;
    assert_ne!(
        pre_fm_root,
        crate::page::PAGE_ID_NONE,
        "precondition: committed freemap must be non-empty (need free bits for reuse)"
    );

    // Snapshot the committed free set so we can compare after rollback.
    let committed_free_before: std::collections::BTreeSet<u64> = {
        let tree = FreeMapTree::from_roots(pre_fm_root, pre_fm_depth);
        let mut cache = tm.cache.borrow_mut();
        let total = tm.committed_roots.total_pages;
        (0..total)
            .filter(|&id| tree.is_free(&mut cache, id).unwrap_or(false))
            .collect()
    };
    assert!(
        !committed_free_before.is_empty(),
        "precondition: at least one free page in committed freemap for reuse path"
    );

    // --- Phase 2: begin a new transaction (no savepoints → reuse enabled)
    // and inject the membership failure ---
    tm.begin().unwrap();

    // Savepoints must be empty so reuse is live.
    assert!(
        tm.savepoints.is_empty(),
        "precondition: no savepoints, so freemap reuse is enabled"
    );

    let ghost = tm.current_roots.next_handle;
    let saved_ht_depth = tm.handle_table.depth();

    // Fire the injection: `cow_alloc` inside `handle_table_insert_candidate`
    // or `membership_insert_candidate` will draw from the committed freemap
    // before the prepare fails.
    tm.fail_next_membership_op.set(true);
    let err = tm.allocate_tagged(&big, 5).unwrap_err();
    assert!(
        matches!(err, ChiselError::CacheFull { .. }),
        "expected injected CacheFull, got {err:?}"
    );
    assert!(!tm.is_poisoned(), "non-fatal CacheFull must not poison");

    // --- Assertion 1: installed state is unchanged ---
    assert_eq!(
        tm.current_roots.next_handle, ghost,
        "aborted allocate must not consume the handle id"
    );
    assert_eq!(
        tm.current_roots.handle_table_page, pre_ht_root,
        "aborted allocate must not install a new handle-table root"
    );
    assert_eq!(
        tm.current_roots.membership_index_page, pre_mi_root,
        "aborted allocate must not install a new membership-index root"
    );
    assert_eq!(
        tm.handle_table.depth(),
        saved_ht_depth,
        "aborted allocate must restore the eagerly-bumped handle-table depth"
    );
    // Neither map has `ghost`.
    assert!(
        matches!(tm.tag(ghost), Err(ChiselError::InvalidHandle(_))),
        "ghost handle must not appear in forward map"
    );
    assert!(
        !tm.handles_with_tag(5).unwrap().contains(&ghost),
        "ghost handle must not appear in reverse (membership) map"
    );
    // next_handle was not advanced.
    assert_eq!(
        tm.current_roots.next_handle, pre_next_handle,
        "next_handle must equal the committed baseline (not burned)"
    );

    // --- Assertion 2: rollback reclaims the freemap residue ---
    tm.rollback().unwrap();

    // After rollback, committed_roots is unchanged (rollback restores
    // current_roots to committed_roots, which did not change mid-transaction).
    assert_eq!(tm.committed_roots.freemap_page, pre_fm_root);
    assert_eq!(tm.committed_roots.freemap_depth, pre_fm_depth);

    // The free set as seen through the committed freemap tree must equal
    // the pre-allocate snapshot: rollback's discard_all_dirty + truncate
    // restored the freemap's bit pattern to the committed state.
    let committed_free_after: std::collections::BTreeSet<u64> = {
        let tree = FreeMapTree::from_roots(
            tm.committed_roots.freemap_page,
            tm.committed_roots.freemap_depth,
        );
        let mut cache = tm.cache.borrow_mut();
        let total = tm.committed_roots.total_pages;
        (0..total)
            .filter(|&id| tree.is_free(&mut cache, id).unwrap_or(false))
            .collect()
    };
    assert_eq!(
        committed_free_before, committed_free_after,
        "rollback must restore the committed freemap free-set: \
             before={committed_free_before:?} after={committed_free_after:?}"
    );

    // --- Assertion 3: C1 holds after rollback (no page reachable from
    // committed_roots is marked free) ---
    assert_no_reachable_page_is_free(&tm);

    // --- Assertion 4: the manager is still fully operational ---
    // A new transaction can retry the allocation and succeed.
    tm.begin().unwrap();
    let h = tm.allocate_tagged(&big, 5).unwrap();
    assert_eq!(h, ghost, "retry must reuse the un-burned handle id");
    assert_eq!(tm.tag(h).unwrap(), 5);
    assert!(tm.handles_with_tag(5).unwrap().contains(&h));
    tm.commit().unwrap();
    assert_no_reachable_page_is_free(&tm);
}

// I29: the open-time format-version gate compares MAJOR only, not
// the full u32. Same-major files (regardless of minor) open cleanly;
// different-major files fail fast with UnsupportedFormatVersion.
// This encodes the "sacred within a major version" promise from the
// README: any file written by an N.x binary is readable by every
// other N.x binary, because minor bumps can only add fields in the
// superblock's reserved region (they never break backward reads).
//
// We exercise both halves in one test because they share setup
// (fresh-DB file → patch slots → reopen). Patching both superblock
// slots in lockstep matters because Superblock::select picks the
// highest-counter valid slot; if only slot 0 were patched, slot 1
// (with the unmodified version) would win on some commits.
#[test]
fn format_version_gate_is_major_only() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();

    // Step 0: create a fresh database so there's something on disk
    // to patch. The default superblock_count of 2 means we need to
    // patch pages 0 AND 1.
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let _ = TransactionManager::create_new(cache, 2).unwrap();
        // drop() releases the flock so the test can read+write the
        // file directly below.
    }

    // Helper: patch every superblock slot to the given packed
    // format_version and re-stamp the trailing checksum so the
    // deserialize path still accepts it as a valid slot.
    let patch_all_slots = |version: u32, slot_count: usize| {
        let mut bytes = std::fs::read(&path).unwrap();
        for slot in 0..slot_count {
            let offset = slot * PAGE_SIZE;
            bytes[offset + 4..offset + 8].copy_from_slice(&version.to_le_bytes());
            let page_arr: &mut [u8; PAGE_SIZE] =
                (&mut bytes[offset..offset + PAGE_SIZE]).try_into().unwrap();
            page::stamp_checksum(page_arr);
        }
        std::fs::write(&path, &bytes).unwrap();
    };

    // Case 1: patch both slots to (FORMAT_MAJOR_VERSION, +42). A minor
    // bump within the same major must open cleanly — this is the
    // whole point of the packed scheme. Pre-fix (exact-match gate)
    // this case rejected with UnsupportedFormatVersion.
    let minor_bump = page::pack_format_version(
        page::FORMAT_MAJOR_VERSION,
        page::FORMAT_MINOR_VERSION.wrapping_add(42),
    );
    patch_all_slots(minor_bump, 2);
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let tm = TransactionManager::open_existing(cache);
        assert!(
            tm.is_ok(),
            "same-major / different-minor file should open cleanly; got {:?}",
            tm.err()
        );
    }

    // Case 2: patch to (FORMAT_MAJOR_VERSION + 1, 0). A major bump
    // is a real format break and must be refused regardless of minor.
    let major_bump = page::pack_format_version(page::FORMAT_MAJOR_VERSION.wrapping_add(1), 0);
    patch_all_slots(major_bump, 2);
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        match TransactionManager::open_existing(cache) {
            Err(ChiselError::UnsupportedFormatVersion { .. }) => {}
            Err(e) => panic!("expected UnsupportedFormatVersion, got {e:?}"),
            Ok(_) => panic!("expected UnsupportedFormatVersion, got Ok"),
        }
    }
}

// I29 write-gate: a file whose MINOR exceeds this binary's opens READ-ONLY,
// not rejected — within a MAJOR every layout change is additive, so reads
// are safe, but writing would drop fields this binary can't see. A
// same-or-older minor file opens read-write as normal.
#[test]
fn file_minor_newer_than_binary_is_forced_read_only() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();

    // Fresh DB so there are 2 superblock slots (pages 0 and 1) to patch.
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let _ = TransactionManager::create_new(cache, 2).unwrap();
    }

    // Patch every slot to (current MAJOR, MINOR + 1) and re-stamp checksums.
    let newer_minor =
        page::pack_format_version(page::FORMAT_MAJOR_VERSION, page::FORMAT_MINOR_VERSION + 1);
    let mut bytes = std::fs::read(&path).unwrap();
    for slot in 0..2 {
        let offset = slot * PAGE_SIZE;
        bytes[offset + 4..offset + 8].copy_from_slice(&newer_minor.to_le_bytes());
        let page_arr: &mut [u8; PAGE_SIZE] =
            (&mut bytes[offset..offset + PAGE_SIZE]).try_into().unwrap();
        page::stamp_checksum(page_arr);
    }
    std::fs::write(&path, &bytes).unwrap();

    // Reopen read-WRITE; the gate must force read-only, so begin() fails.
    let io = PageIo::open(&path, false).unwrap();
    let cache = PageCache::new(
        io,
        1024 * PAGE_SIZE as u64,
        0,
        crate::DrainInsertion::LruTail,
        crate::SpillwayLocation::InMemory,
    );
    let mut tm = TransactionManager::open_existing(cache)
        .expect("a newer-minor file must still OPEN (reads are additive-safe)");
    assert!(
        matches!(tm.begin(), Err(ChiselError::ReadOnlyMode)),
        "a newer-minor file must be forced read-only"
    );
}

#[test]
fn allocate_tagged_then_tag_and_handles_with_tag() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"row", 42).unwrap();
    let u = tm.allocate(b"untagged").unwrap();
    tm.commit().unwrap();
    assert_eq!(tm.tag(h).unwrap(), 42);
    assert_eq!(tm.tag(u).unwrap(), 0);
    assert_eq!(tm.handles_with_tag(42).unwrap(), vec![h]);
    assert_eq!(tm.handles_with_tag(99).unwrap(), Vec::<u64>::new());
}

#[test]
fn handles_with_tag_accumulates_multiple_handles() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let a = tm.allocate_tagged(b"a", 42).unwrap();
    let b = tm.allocate_tagged(b"b", 42).unwrap();
    let c = tm.allocate_tagged(b"c", 42).unwrap();
    tm.commit().unwrap();
    // The reverse index accumulates members; it must not overwrite.
    let mut got = tm.handles_with_tag(42).unwrap();
    got.sort();
    let mut want = vec![a, b, c];
    want.sort();
    assert_eq!(got, want);
    assert_eq!(tm.tag(a).unwrap(), 42);
    assert_eq!(tm.tag(c).unwrap(), 42);
}

#[test]
fn allocate_tagged_overflow_value_preserves_tag() {
    let mut tm = fresh_manager();
    // A value larger than MAX_INLINE_VALUE takes the overflow path in
    // allocate_inner; the tag must be stored on the Overflow HandleEntry,
    // readable via tag(), and indexed for handles_with_tag().
    let big = vec![0xABu8; MAX_INLINE_VALUE + 100];
    tm.begin().unwrap();
    let h = tm.allocate_tagged(&big, 77).unwrap();
    tm.commit().unwrap();
    assert_eq!(tm.tag(h).unwrap(), 77);
    assert_eq!(tm.handles_with_tag(77).unwrap(), vec![h]);
    assert_eq!(tm.read(h).unwrap(), big);
}

#[test]
fn update_preserves_immutable_tag() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"v1", 42).unwrap();
    tm.commit().unwrap();
    tm.begin().unwrap();
    tm.update(h, b"v2").unwrap();
    tm.commit().unwrap();
    // Tags are immutable: changing the value must NOT change the tag, and the
    // membership index must still list the handle under its original tag.
    assert_eq!(tm.tag(h).unwrap(), 42);
    assert_eq!(tm.handles_with_tag(42).unwrap(), vec![h]);
}

#[test]
fn delete_removes_tagged_chunk_from_index() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"row", 7).unwrap();
    tm.commit().unwrap();
    assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
    tm.begin().unwrap();
    tm.delete(h).unwrap();
    tm.commit().unwrap();
    // The tag's last member is gone -> handles_with_tag is empty.
    assert_eq!(tm.handles_with_tag(7).unwrap(), Vec::<u64>::new());
}

#[test]
fn delete_tagged_rejects_wrong_tag() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate_tagged(b"row", 5).unwrap();
    // Wrong tag: error, nothing deleted, index intact.
    let err = tm.delete_tagged(h, 6).unwrap_err();
    assert!(
        matches!(err, ChiselError::TagMismatch { handle, expected: 6, actual: 5 } if handle == h)
    );
    assert_eq!(tm.handles_with_tag(5).unwrap(), vec![h]);
    // TagMismatch is operational, NOT fatal: a wrong tag must leave the
    // manager usable (guards against a future edit moving it into is_fatal).
    assert!(!tm.is_poisoned());
    // Right tag: deletes (and self-maintains the index via delete_inner).
    tm.delete_tagged(h, 5).unwrap();
    assert_eq!(tm.handles_with_tag(5).unwrap(), Vec::<u64>::new());
    tm.commit().unwrap();
}

#[test]
fn delete_one_of_two_tagged_keeps_the_other() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let a = tm.allocate_tagged(b"a", 7).unwrap();
    let b = tm.allocate_tagged(b"b", 7).unwrap();
    tm.commit().unwrap();
    tm.begin().unwrap();
    tm.delete(a).unwrap();
    tm.commit().unwrap();
    // Only `a` is removed from the reverse index; `b` survives under tag 7.
    assert_eq!(tm.handles_with_tag(7).unwrap(), vec![b]);
    assert_eq!(tm.tag(b).unwrap(), 7);
}

// ── Migrated 2026-05-22 from tests/transactions.rs (I35 reshape) ──
//
// Exercises TransactionManager::open_existing end-to-end: a value
// written through one TransactionManager survives a drop + reopen
// on the same path. The other tests in tests/transactions.rs use
// only the public Chisel API and stay in tests/.
#[test]
fn reopen_preserves_committed_data() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            64 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut txm = TransactionManager::create_new(cache, 2).unwrap();
        txm.begin().unwrap();
        handle = txm.allocate(b"persistent").unwrap();
        txm.commit().unwrap();
    }
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            64 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let txm = TransactionManager::open_existing(cache).unwrap();
        let data = txm.read(handle).unwrap();
        assert_eq!(data, b"persistent");
    }
}

#[test]
fn delete_with_tag_drops_in_bounded_batches() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let mut hs = Vec::new();
    for i in 0..10u64 {
        hs.push(tm.allocate_tagged(format!("row{i}").as_bytes(), 3).unwrap());
    }
    tm.commit().unwrap();
    tm.begin().unwrap();
    let (d1, c1) = tm.delete_with_tag(3, 4).unwrap();
    assert_eq!(d1.len(), 4);
    assert!(!c1);
    let (d2, c2) = tm.delete_with_tag(3, 100).unwrap();
    assert_eq!(d2.len(), 6);
    assert!(c2);
    tm.commit().unwrap();
    assert_eq!(tm.handles_with_tag(3).unwrap(), Vec::<u64>::new());
    // The chunks themselves are gone too.
    for h in hs {
        assert!(
            matches!(tm.read(h), Err(ChiselError::InvalidHandle(_))),
            "handle {h} must be InvalidHandle after delete_with_tag"
        );
    }
}

#[test]
fn delete_with_tag_exact_max_reports_complete() {
    // Boundary: exactly `max` members remain, so the max+1 enumeration
    // returns exactly `max` and `complete = max <= max` must be TRUE. This
    // is the <= vs < edge: a `<` here would falsely report incomplete and
    // cost the caller an extra empty pass. (The other delete_with_tag test
    // only exercises len > max and len < max, never len == max.)
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    for i in 0..5u64 {
        tm.allocate_tagged(format!("r{i}").as_bytes(), 8).unwrap();
    }
    tm.commit().unwrap();
    tm.begin().unwrap();
    let (deleted, complete) = tm.delete_with_tag(8, 5).unwrap();
    assert_eq!(deleted.len(), 5);
    assert!(
        complete,
        "deleting exactly all members in one max-sized pass must report complete"
    );
    tm.commit().unwrap();
    assert_eq!(tm.handles_with_tag(8).unwrap(), Vec::<u64>::new());
}

#[test]
fn handle_table_depth_restored_after_rolled_back_grow() {
    // I99 regression: a rolled-back handle-table grow must not leave the
    // in-memory depth too deep. A leaf holds ENTRIES_PER_LEAF (510) ids
    // 0..=509; handle 0 is the reserved "no handle" sentinel, so allocation
    // starts at id 1. Allocating ids 1..=509 (509 handles) stays at depth 0,
    // and the next handle (id 510, where `id >= cap` triggers the grow) goes
    // to depth 1. Roll that back, then a committed handle must still read (it
    // returned InvalidHandle before the fix).
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    for i in 0..509u64 {
        tm.allocate(format!("v{i}").as_bytes()).unwrap();
    }
    tm.commit().unwrap();
    let baseline = tm.read(5).unwrap();
    tm.begin().unwrap();
    tm.allocate(b"grow").unwrap();
    tm.rollback().unwrap();
    assert_eq!(
        tm.read(5).unwrap(),
        baseline,
        "committed handle lost after rolled-back grow"
    );
    // New inserts still work (depth consistent for the next transaction).
    tm.begin().unwrap();
    let h = tm.allocate(b"after").unwrap();
    tm.commit().unwrap();
    assert_eq!(tm.read(h).unwrap(), b"after");
}

#[test]
fn handle_table_depth_restored_after_rollback_to_savepoint() {
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    // Handle 0 is the reserved "no handle" sentinel, so allocation starts at
    // id 1: ids 1..=509 (509 handles) stay at depth 0, and the "grow" below
    // (id 510, where `id >= ENTRIES_PER_LEAF` triggers the grow) is the one
    // that crosses to depth 1, past the savepoint. Handle h holds "v{h-1}".
    for i in 0..509u64 {
        tm.allocate(format!("v{i}").as_bytes()).unwrap();
    }
    tm.savepoint("sp").unwrap();
    tm.allocate(b"grow").unwrap(); // grows depth 0 -> 1 past the savepoint
    tm.rollback_to("sp").unwrap();
    // A handle present at the savepoint must still read within the active txn.
    // Handle 5 was the i=4 allocation, so it holds "v4".
    assert_eq!(tm.read(5).unwrap(), b"v4");
    tm.commit().unwrap();
    assert_eq!(tm.read(5).unwrap(), b"v4");
}

#[test]
fn commit_fsync_failure_poisons_at_each_of_the_three_fsyncs() {
    // I112: commit performs THREE fsyncs (pre-drain, data-flush, superblock).
    // A real IoError at ANY of them must surface as IoError AND poison the
    // manager. The FailFsync countdown targets each in turn. A small inline
    // value keeps commit to exactly three fsyncs (no spillway: fresh_manager
    // sets spillway_max_bytes=0).
    for nth in 0..3u32 {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        tm.allocate(b"v").unwrap();
        tm.cache.borrow().io().arm_fault(Fault::FailFsync(nth));
        let result = tm.commit();
        assert!(
            matches!(result, Err(ChiselError::IoError(_))),
            "commit fsync #{} failure must surface IoError, got {result:?}",
            nth + 1
        );
        assert!(tm.is_poisoned(), "fsync #{} failure must poison", nth + 1);
        assert!(
            matches!(tm.read(0), Err(ChiselError::Poisoned)),
            "a poisoned manager rejects all further ops"
        );
    }
}

#[test]
fn commit_write_failure_poisons() {
    // I112: a real write_page IoError during commit must surface and poison.
    // Target the value's own data page, which is written during commit flush.
    let mut tm = fresh_manager();
    tm.begin().unwrap();
    let h = tm.allocate(b"v").unwrap();
    let pid = tm
        .handle_live_page_id(h)
        .unwrap()
        .expect("allocated value has a live data page");
    tm.cache.borrow().io().arm_fault(Fault::FailWritePage(pid));
    let result = tm.commit();
    assert!(
        matches!(result, Err(ChiselError::IoError(_))),
        "commit write failure must surface IoError, got {result:?}"
    );
    assert!(tm.is_poisoned(), "write failure during commit must poison");
}
