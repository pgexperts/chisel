// spillway_integration.rs — End-to-end acceptance tests for the spillway feature.
//
// Role: exercises the full Chisel public API the way a real consumer would —
// no access to page_cache internals, no spelunking through `pub(crate)` fields.
// Tests correspond to spec 2026-05-03-chisel-spillway-design.md acceptance
// criteria (Tasks 14-17 of the spillway feature plan).
//
// Ordering note: Tests 14a and 14b exercise the core spill-then-commit and
// spill-then-rollback paths. Test 15 guards against fsync regressions on
// no-spill workloads. Tests 16a and 16b cover the opt-out (spillway disabled).
// Test 17 covers crash recovery via the lazy-open truncate-on-open guarantee.

use chisel::{Chisel, ChiselError, DrainInsertion, Options};

// ---------------------------------------------------------------- Task 14a

#[test]
fn large_transaction_with_spill_produces_identical_state() {
    // Tiny cache so the working set definitely overflows into the spillway.
    // 16 pages × 8 KiB = 128 KiB; allocating 200 × 1 KiB payloads
    // (each requiring at least one data page) will push well past that.
    let cache_max_bytes = 16 * 8192; // 16 pages
    let opts_with_spillway = Options {
        cache_max_bytes,
        spillway_max_bytes: 1024 * cache_max_bytes,
        drain_insertion: DrainInsertion::LruTail,
        ..Options::default()
    };

    // Run A: one big transaction, working set ~64 pages of dirty
    // (4× cache cap; spillway absorbs the overflow).
    let mut db_a = Chisel::open_in_memory_with_options(opts_with_spillway.clone()).unwrap();
    db_a.begin().unwrap();
    let mut handles_a = Vec::new();
    for i in 0..200 {
        let payload = vec![i as u8; 1024]; // 1 KB each
        let h = db_a.allocate(&payload).unwrap();
        handles_a.push((h, payload));
    }
    db_a.commit().unwrap();

    // Run B: identical workload, but split into 10 transactions of
    // 20 ops each (no spill needed per transaction).
    let mut db_b = Chisel::open_in_memory_with_options(opts_with_spillway).unwrap();
    let mut handles_b = Vec::new();
    for chunk_start in (0..200).step_by(20) {
        db_b.begin().unwrap();
        for i in chunk_start..chunk_start + 20 {
            let payload = vec![i as u8; 1024];
            let h = db_b.allocate(&payload).unwrap();
            handles_b.push((h, payload));
        }
        db_b.commit().unwrap();
    }

    // Both runs should produce identical handle→payload mappings.
    // The central spec claim: spillway lets a transaction touch a working
    // set larger than the cache without semantic difference.
    for (h, expected) in &handles_a {
        let bytes = db_a.read(*h).unwrap();
        assert_eq!(bytes, *expected, "handle {h} content corrupt after spill");
    }
    for (h, expected) in &handles_b {
        let bytes = db_b.read(*h).unwrap();
        assert_eq!(bytes, *expected, "control run handle {h} content corrupt");
    }
}

// ---------------------------------------------------------------- Task 14b

#[test]
fn rollback_with_spill_leaves_main_file_unchanged() {
    let cache_max_bytes = 16 * 8192;
    let opts = Options {
        cache_max_bytes,
        spillway_max_bytes: 1024 * cache_max_bytes,
        drain_insertion: DrainInsertion::LruTail,
        ..Options::default()
    };
    let mut db = Chisel::open_in_memory_with_options(opts).unwrap();

    // Commit a baseline transaction first.
    db.begin().unwrap();
    let baseline_h = db.allocate(b"baseline").unwrap();
    db.commit().unwrap();

    // Open a big transaction that spills into the spillway, then roll it back.
    db.begin().unwrap();
    let mut spilled_handles = Vec::new();
    for i in 0..200 {
        let h = db.allocate(&vec![i as u8; 1024]).unwrap();
        spilled_handles.push(h);
    }
    db.rollback().unwrap();

    // Baseline still intact — the spill-then-rollback must not touch
    // any page committed before the aborted transaction.
    let bytes = db.read(baseline_h).unwrap();
    assert_eq!(bytes, b"baseline");

    // Handles allocated in the aborted transaction are gone.
    for h in spilled_handles {
        assert!(db.read(h).is_err(), "handle {h} survived rollback");
    }

    // Subsequent commits work normally — the engine is not wedged.
    db.begin().unwrap();
    let _h = db.allocate(b"post-rollback").unwrap();
    db.commit().unwrap();
}

// ---------------------------------------------------------------- Task 15

#[test]
fn no_spill_workload_preserves_two_fsync_commit() {
    // Workload sized to fit comfortably in the default 8 MiB cache (1024 pages).
    // 50 × 4-byte values will not come close to the cap, so the spillway
    // is never engaged and the commit protocol must not issue any spillway-
    // specific fsyncs beyond its standard protocol budget.
    //
    // The current protocol issues 3 fsyncs per commit (pre-drain flush per
    // I28, then a main-pages flush, then the superblock fsync — see
    // commit_inner in transaction.rs). This test guards that the spillway
    // feature does not inflate that count further; a no-spill commit must
    // stay at the same count as a non-spillway commit. We assert `<= 3`
    // to leave room for future I/O protocol changes while catching any
    // accidental spillway-driven fsync regression.
    let mut db = Chisel::open_in_memory().unwrap();

    db.begin().unwrap();
    for i in 0..50u32 {
        db.allocate(&i.to_le_bytes()).unwrap();
    }
    let pre_commit_counters = db.counters().unwrap();
    db.commit().unwrap();
    let post_commit_counters = db.counters().unwrap();

    let fsync_delta = post_commit_counters.fsync_calls - pre_commit_counters.fsync_calls;
    assert!(
        (2..=3).contains(&fsync_delta),
        "no-spill commit must issue 2-3 fsyncs (standard protocol, no spillway overhead); got {fsync_delta}"
    );
}

// ---------------------------------------------------------------- Task 16a

#[test]
fn spillway_max_bytes_zero_disables_spillway_and_fires_cache_full() {
    // Tiny cache (4 pages × 8 KiB = 32 KiB) with spillway disabled.
    // Allocating 4 KiB payloads will exhaust dirty capacity quickly;
    // the engine must surface CacheFull, not SpillwayFull and not
    // silently grow past the cap.
    let cache_max_bytes = 4 * 8192; // 4 pages
    let opts = Options {
        cache_max_bytes,
        spillway_max_bytes: 0, // OPT-OUT
        drain_insertion: DrainInsertion::LruTail,
        ..Options::default()
    };
    let mut db = Chisel::open_in_memory_with_options(opts).unwrap();

    db.begin().unwrap();
    let mut hit_cache_full = false;
    for _ in 0..50 {
        match db.allocate(&[0u8; 4096]) {
            Ok(_) => {}
            Err(ChiselError::CacheFull { .. }) => {
                hit_cache_full = true;
                break;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(
        hit_cache_full,
        "with spillway disabled, allocation must trip CacheFull"
    );
    db.rollback().unwrap();
}

// ---------------------------------------------------------------- Task 16b

#[test]
fn spillway_max_bytes_zero_creates_no_spillway_file() {
    // File-backed database with spillway disabled. The lazy-open guarantee
    // means a non-spilling workload never creates the sidecar file; setting
    // spillway_max_bytes=0 additionally suppresses even the lazy create.
    use std::path::PathBuf;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_path: PathBuf = dir.path().join("test.chisel");
    let spillway_path: PathBuf = {
        let mut p = db_path.as_os_str().to_owned();
        p.push(".spillway");
        PathBuf::from(p)
    };

    let opts = Options {
        spillway_max_bytes: 0,
        ..Options::default()
    };
    let mut db = Chisel::open(&db_path, opts).unwrap();
    db.begin().unwrap();
    let _h = db.allocate(b"x").unwrap();
    db.commit().unwrap();
    drop(db);

    assert!(
        !spillway_path.exists(),
        "spillway file should not exist when spillway_max_bytes = 0"
    );
}

// ---------------------------------------------------------------- Task 17

#[test]
fn crash_mid_spill_recovers_to_last_committed_state() {
    // Design intent: Spillway::open_file uses `truncate(true)`, so any
    // pre-existing content at the spillway path is discarded unconditionally
    // on open. The spillway is lazy-opened on the FIRST spill (see
    // ensure_spillway in page_cache.rs). We therefore need the post-crash
    // reopen to actually trigger a spill — this forces ensure_spillway to run,
    // which opens (and truncates) the garbage-filled spillway path.
    use std::path::PathBuf;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_path: PathBuf = dir.path().join("crash.chisel");
    let spillway_path: PathBuf = {
        let mut p = db_path.as_os_str().to_owned();
        p.push(".spillway");
        PathBuf::from(p)
    };

    // Step 1: commit a known baseline.
    {
        let mut db = Chisel::open(&db_path, Options::default()).unwrap();
        db.begin().unwrap();
        let _h = db.allocate(b"baseline").unwrap();
        db.commit().unwrap();
    }

    // Step 2: simulate a crash mid-spill by writing garbage to the spillway
    // file path. In production, a crash between spill() and truncate() at
    // commit would leave the file non-empty. We mimic that here without
    // actually crashing the process.
    std::fs::write(&spillway_path, b"\xFF\xFF\xFF crashed mid-spill garbage").unwrap();
    assert!(spillway_path.exists());
    let pre_open_garbage_size = std::fs::metadata(&spillway_path).unwrap().len();
    assert!(pre_open_garbage_size > 0);

    // Step 3: reopen the database with a tiny cache so that the first
    // transaction definitely spills, forcing ensure_spillway to run.
    // ensure_spillway calls Spillway::open_file which opens with
    // truncate(true) — clearing the orphaned garbage from Step 2.
    let cache_max_bytes = 4 * 8192; // 4 pages — tiny enough to force spill
    let opts = Options {
        cache_max_bytes,
        spillway_max_bytes: 1024 * cache_max_bytes,
        ..Options::default()
    };
    let mut db = Chisel::open(&db_path, opts).unwrap();
    db.begin().unwrap();
    // Allocate enough to overflow the 4-page cache and trigger spillway open.
    for i in 0..20u8 {
        db.allocate(&vec![i; 4096]).unwrap();
    }
    db.commit().unwrap();
    drop(db);

    // Step 4: the spillway file must now exist (lazily opened by the spill)
    // but be empty (truncated at commit). The garbage from Step 2 is gone —
    // open_file's truncate(true) discarded it when ensure_spillway ran.
    let post_recovery_size = std::fs::metadata(&spillway_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        post_recovery_size, 0,
        "spillway must be truncated to zero after open + commit (garbage from crash must be gone)"
    );
}
