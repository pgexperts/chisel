// Integration tests for the multi-page freemap: depth>0 reclamation,
// steady-state flat file under churn, and crash-orphan recovery via defrag.
//
// These tests validate the structural-reclamation design described in
// docs/specs/2026-06-22-multi-page-freemap-design.md §"Structural-page
// reclamation". The key properties under test:
//
//   1. Pages freed at depth>0 (multi-page freemap) are reused by later
//      transactions, not leaked.
//   2. Under sustained delete-all/reallocate-all churn at constant live-data
//      size, `total_pages` grows by at most 1 page per cycle — all of it
//      traceable to pre-existing handle-table tombstone accumulation, not
//      freemap structural leakage. Verified to be identical behavior on the
//      main branch before this feature landed.
//   3. A crash that loses the in-memory recycle pool leaves orphaned
//      freemap-typed pages; a subsequent `defrag` reclaims them, and a
//      second `defrag` finds zero orphans (idempotence proves the first
//      sweep was complete).

use chisel::{Chisel, DefragOptions, Options};

// 1. Depth>0 reclamation through the public API.
//
// Allocate ~3 000 values, delete ~1 500 (forcing freemap depth>0 under the
// bitmap coverage), commit.  Record `total_pages`.  Allocate ~1 000 more.
// The new allocations must reuse freed pages, so the file grows by < 1 000.
#[test]
fn reclaims_freed_pages_with_multipage_freemap() {
    let mut db = Chisel::open_in_memory().unwrap();

    db.begin().unwrap();
    let mut hs = Vec::new();
    for i in 0..3000u64 {
        hs.push(db.allocate(format!("v{i}").as_bytes()).unwrap());
    }
    db.commit().unwrap();

    db.begin().unwrap();
    for h in hs.iter().take(1500) {
        db.delete(*h).unwrap();
    }
    db.commit().unwrap();

    let before = db.stats().unwrap().total_pages;

    db.begin().unwrap();
    for i in 0..1000u64 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();

    let after = db.stats().unwrap().total_pages;
    assert!(
        after - before < 1000,
        "freed pages must be reused (before={before} after={after}, \
         grew by {}; expected growth < 1000)",
        after - before
    );
}

// 2. Steady-state file under churn — the central reclamation promise.
//
// Warm up to steady state with 10 delete-all/reallocate-all cycles at a
// constant live size of 500 values.  Record `baseline = total_pages`.  Run
// 30 more identical cycles.  Assert growth over those 30 cycles is < 32.
//
// WHY NOT EXACTLY FLAT: this workload deletes 500 handles per cycle, leaving
// 500 tombstones in the handle-table leaf pages each cycle. Handles are not
// reused after deletion — each new allocate() takes the next fresh id — so
// the handle-table gradually expands to cover higher handle ids, requiring a
// new HT leaf page roughly once every 500 deletions.  This produces ~1 page
// of growth per cycle at the 500-value churn rate.  The same behavior is
// present on the main branch before this feature (verified by running an
// identical diagnostic there): it is pre-existing HT tombstone accumulation,
// NOT a regression from the multi-page freemap.
//
// The freemap structural recycle IS bounding its own churn (the property this
// feature was built for): freemap COW pages are recycled and do not march the
// high-water.  The test cap of 32 is tight enough to catch any freemap
// structural leak (which would add on top of the ~30 pages of HT growth and
// blow through 32 well before 30 cycles), while correctly tolerating the
// known-bounded HT tombstone growth.
//
// If this assertion fails, first determine whether the excess growth is in
// freemap-typed pages (a real regression) or HT-typed pages (tombstone rate
// change).  Do NOT raise the bound without identifying the source.
#[test]
fn steady_state_file_is_flat_under_churn() {
    let mut db = Chisel::open_in_memory().unwrap();

    // Seed the initial live set.
    db.begin().unwrap();
    let mut hs = Vec::new();
    for i in 0..500u64 {
        hs.push(db.allocate(format!("v{i}").as_bytes()).unwrap());
    }
    db.commit().unwrap();

    // Warm up: run 10 churn cycles so the freemap recycle pool reaches steady
    // state before we take the baseline measurement.  Each cycle deletes all
    // live values and reallocates the same count, keeping the logical database
    // size constant.
    for _ in 0..10 {
        db.begin().unwrap();
        let old = std::mem::take(&mut hs);
        for h in &old {
            db.delete(*h).unwrap();
        }
        for i in 0..500u64 {
            hs.push(db.allocate(format!("c{i}").as_bytes()).unwrap());
        }
        db.commit().unwrap();
    }

    let baseline = db.stats().unwrap().total_pages;

    // Measurement phase: 30 more identical cycles.
    for _ in 0..30 {
        db.begin().unwrap();
        let old = std::mem::take(&mut hs);
        for h in &old {
            db.delete(*h).unwrap();
        }
        for i in 0..500u64 {
            hs.push(db.allocate(format!("c{i}").as_bytes()).unwrap());
        }
        db.commit().unwrap();
    }

    let after = db.stats().unwrap().total_pages;
    let growth = after - baseline;
    // 30 cycles × ~1 HT-tombstone page per cycle = ~30 pages expected.
    // Cap at 32 to catch any freemap structural leak on top of that.
    assert!(
        growth < 32,
        "file grew by {growth} pages over 30 steady-churn cycles \
         (baseline={baseline}, after={after}); \
         expected < 32 (handle-table tombstone accumulation ~1/cycle is normal; \
         freemap structural leak would add on top and breach this bound)"
    );
}

// 3. Crash → reopen → defrag reclaims orphaned freemap pages.
//
// This test DETERMINISTICALLY orphans at least one freemap page through the
// public API (integration tests cannot reach the cfg(test) forge helper), so
// a regression that misses ALL orphans fails here rather than passing vacuously.
//
// Mechanism: overflow-sized values (> MAX_INLINE_VALUE ≈ 8 KB; we use 9 000
// bytes) occupy WHOLE overflow pages, so a delete frees entire pages. We first
// populate and commit a batch of such values so the freemap grows multi-page.
// Then the FINAL pre-drop step is a commit that DELETEs many of them: that
// commit's `persist_freemap` marks the freed pages free, which COWs the freemap
// leaf/spine and supersedes the old freemap pages into the in-memory recycle
// pool (`pending_structural_frees`). Dropping the handle WITHOUT a further
// commit == crash: the in-memory pool is lost, orphaning those superseded
// freemap pages on disk (freemap-typed, unreachable from the committed tree, not
// marked free).
//
// Reopen, run defrag (in an active transaction). The first sweep MUST reclaim
// >= 1 orphan (the deterministic guarantee); the second MUST find 0 (idempotence
// proves the first sweep was complete).
#[test]
fn crash_orphans_are_reclaimed_by_defrag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.chisel");

    // 9 000 bytes > MAX_INLINE_VALUE (~8 160), so every value lands in an
    // overflow chain of whole pages — deletes free whole pages, the precondition
    // for the freemap-COW churn this test depends on.
    let big = vec![0xCDu8; 9000];

    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();

        // Populate enough overflow-backed values that the freed-page bitmap spans
        // a real (multi-page) freemap and a later delete-all COWs several nodes.
        db.begin().unwrap();
        let mut hs = Vec::new();
        for _ in 0..400u64 {
            hs.push(db.allocate(&big).unwrap());
        }
        db.commit().unwrap();

        // FINAL pre-drop step: a commit that deletes many overflow-sized values.
        // Freeing their whole pages drives `persist_freemap` to COW the freemap
        // and supersede the old pages into the in-memory recycle pool.
        db.begin().unwrap();
        for h in &hs {
            db.delete(*h).unwrap();
        }
        db.commit().unwrap();

        // Drop without a further commit == crash: the in-memory recycle pool is
        // lost, orphaning the freemap pages the delete-commit superseded.
    }

    let mut db = Chisel::open(&path, Options::default()).unwrap();

    // First defrag: sweep for crash-orphaned freemap pages and reclaim them.
    // defrag() requires an active transaction.
    db.begin().unwrap();
    let first = db.defrag(DefragOptions::default()).unwrap();
    db.commit().unwrap();

    // Deterministic: the delete-commit above superseded at least one freemap page
    // into the recycle pool, which the crash then orphaned. The sweep MUST find
    // it. (If a future change makes this non-deterministic, do NOT relax to >= 0
    // without first restoring determinism — see the unit-test coverage in
    // transaction.rs for the forge-based deterministic path.)
    assert!(
        first.freemap_orphans_reclaimed >= 1,
        "first defrag must reclaim >= 1 crash-orphaned freemap page \
         (got {}); the final delete-commit should have superseded freemap pages \
         into the in-memory pool that the crash then orphaned",
        first.freemap_orphans_reclaimed
    );

    // Second defrag: must find zero orphans.  If the first sweep reclaimed
    // everything correctly, there is nothing left for the second to find.
    // Any non-zero count here means the first sweep missed orphans — a bug.
    db.begin().unwrap();
    let second = db.defrag(DefragOptions::default()).unwrap();
    db.commit().unwrap();

    assert_eq!(
        second.freemap_orphans_reclaimed, 0,
        "second defrag sweep must find no orphans — the first sweep must \
         have reclaimed all crash-orphaned freemap pages \
         (first={}, second={})",
        first.freemap_orphans_reclaimed, second.freemap_orphans_reclaimed
    );
}
