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
// Test 17 covers crash recovery via the lazy-open discard-stale-sidecar
// guarantee. The final test covers the encrypted on-disk sidecar: spilled
// pages must reach `<db_path>.spillway` as ciphertext, never plaintext.

use chisel::{Chisel, ChiselError, DrainInsertion, Options};

// ---------------------------------------------------------------- Task 14a

#[test]
fn large_transaction_with_spill_produces_identical_state() {
    // Tiny cache so the working set definitely overflows into the spillway.
    // 16 pages × 8 KiB = 128 KiB; allocating 200 × 1 KiB payloads
    // (each requiring at least one data page) will push well past that.
    let cache_max_bytes = 16 * 8192; // 16 pages
    let opts_with_spillway = Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(1024 * cache_max_bytes)
        .drain_insertion(DrainInsertion::LruTail);

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

    // Both runs must produce IDENTICAL handle→payload mappings — the central
    // spec claim that the spillway lets a transaction touch a working set larger
    // than the cache with no semantic difference. Handle ids are deterministic
    // (monotonic next_handle, same untagged insertion order), so the spill run
    // and the control run allocate the same ids in the same order; we
    // cross-compare pairwise rather than only checking each run against its own
    // inputs (which would pass even if the spill path produced a different-but-
    // internally-consistent mapping).
    assert_eq!(
        handles_a.len(),
        handles_b.len(),
        "both runs must allocate the same number of chunks"
    );
    assert_eq!(handles_a.len(), 200, "sanity: the full workload ran");
    for (i, (h_a, expected)) in handles_a.iter().enumerate() {
        let (h_b, _) = handles_b[i];
        // Same id in both runs: spilling must not perturb allocation.
        assert_eq!(
            *h_a, h_b,
            "handle id diverged between the spill run and the control run at index {i}"
        );
        let a = db_a.read(*h_a).unwrap();
        let b = db_b.read(h_b).unwrap();
        // Per-run correctness AND the load-bearing cross-run equivalence.
        assert_eq!(a, *expected, "spill-run handle {h_a} content corrupt");
        assert_eq!(
            a, b,
            "spill run and control run disagree on handle {h_a} — \
             the spill path produced a different mapping"
        );
    }
}

// ---------------------------------------------------------------- Task 14b

#[test]
fn rollback_with_spill_leaves_main_file_unchanged() {
    let cache_max_bytes = 16 * 8192;
    let opts = Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(1024 * cache_max_bytes)
        .drain_insertion(DrainInsertion::LruTail);
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
        assert!(
            matches!(db.read(h), Err(ChiselError::InvalidHandle(_))),
            "handle {h} survived rollback: expected InvalidHandle"
        );
    }

    // Subsequent commits work normally — the engine is not wedged.
    db.begin().unwrap();
    let _h = db.allocate(b"post-rollback").unwrap();
    db.commit().unwrap();
}

// ---------------------------------------------------------------- Task 15

#[test]
fn no_spill_workload_preserves_three_fsync_commit() {
    // Workload sized to fit comfortably in the default 8 MiB cache (1024 pages).
    // 50 × 4-byte values will not come close to the cap, so the spillway
    // is never engaged and the commit protocol must not issue any spillway-
    // specific fsyncs beyond its standard protocol budget.
    //
    let mut db = Chisel::open_in_memory().unwrap();

    db.begin().unwrap();
    for i in 0..50u32 {
        db.allocate(&i.to_le_bytes()).unwrap();
    }
    let pre_commit_counters = db.counters().unwrap();
    db.commit().unwrap();
    let post_commit_counters = db.counters().unwrap();

    let fsync_delta = post_commit_counters.fsync_calls - pre_commit_counters.fsync_calls;
    // Three fsyncs per commit is the pre-spillway baseline:
    //   1. I28 pre-drain flush (TransactionManager::commit_inner pre-drains
    //      the cache before persist_freemap to keep CacheFull off the
    //      commit path — see ISSUES.md I28).
    //   2. Main data-pages flush (PageCache::flush phase 2).
    //   3. Superblock write fsync (TransactionManager::commit_inner phase 2).
    // The spillway must add zero additional fsyncs in the no-spill case.
    // Spec §"Commit drain": "spillway itself is never fsynced — its
    // content does not need to survive a crash."
    //
    // The original spec spoke of a "two-fsync" commit; the actual baseline
    // is 3 due to the I28 pre-drain. The important invariant is "spillway
    // adds zero fsyncs," and pinning to == 3 catches both regression directions (more fsyncs from
    // spillway, or fewer from accidental I28 removal).
    assert_eq!(
        fsync_delta, 3,
        "no-spill commit must issue exactly 3 fsyncs (I28 pre-drain + main + superblock); \
         the spillway must add zero. Got {fsync_delta}"
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
    let opts = Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(0) // OPT-OUT
        .drain_insertion(DrainInsertion::LruTail);
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

    let opts = Options::default().spillway_max_bytes(0);
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
    // Design intent: Spillway::open_file creates the sidecar exclusively
    // (`create_new`, i.e. O_EXCL, plus O_NOFOLLOW and mode 0600 on unix). A
    // pre-existing entry is never adopted: if it is a plain file this user
    // owns with a single link it is treated as crash debris, unlinked, and the
    // file re-created fresh; a symlink, foreign-owned file, or extra hard link
    // is refused as a plant (see reclaim_stale_sidecar in spillway.rs). The
    // spillway is lazy-opened on the FIRST spill (see ensure_spillway in
    // page_cache.rs). We therefore need the post-crash reopen to actually
    // trigger a spill — this forces ensure_spillway to run, which discards the
    // garbage-filled file left at the spillway path.
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
    // ensure_spillway calls Spillway::open_file, which unlinks the orphaned
    // garbage from Step 2 (a plain file we own) and creates a fresh sidecar.
    let cache_max_bytes = 4 * 8192; // 4 pages — tiny enough to force spill
    let opts = Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(1024 * cache_max_bytes);
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
    // open_file discarded it and re-created the file when ensure_spillway ran.
    let post_recovery_size = std::fs::metadata(&spillway_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        post_recovery_size, 0,
        "spillway must be truncated to zero after open + commit (garbage from crash must be gone)"
    );
}

// I74 (ISSUES.md, 2026-05-22): Stats exposes the spillway's current
// (logical_bytes, max_bytes) as Optional fields. The lazy-construction
// of the spillway means the values transition through three observable
// states across the workload below:
//
//   1. Fresh open, no spill ever happened → both fields are None.
//   2. After an overflow allocation that triggered a spill → both fields
//      are Some, and logical_bytes > 0.
//   3. After commit (which truncates the spillway) → both fields are
//      still Some, but logical_bytes == 0 (spillway exists, currently
//      drained — operators see "we have spilled before, might again").
#[test]
fn stats_exposes_spillway_capacity_across_lifecycle() {
    let cache_max_bytes: u64 = 16 * 8192; // 16 pages — tiny cache to force a spill
    let spillway_cap = 1024u64 * cache_max_bytes;
    let opts = Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(spillway_cap)
        .drain_insertion(DrainInsertion::LruTail);

    let mut db = Chisel::open_in_memory_with_options(opts).unwrap();

    // Phase 1: fresh open — spillway has not yet been constructed.
    let s = db.stats().unwrap();
    assert_eq!(
        s.spillway_logical_bytes, None,
        "fresh handle: spillway has never been opened"
    );
    assert_eq!(
        s.spillway_max_bytes, None,
        "fresh handle: spillway has never been opened"
    );

    // Phase 2: allocate enough to force at least one spill. 200 × 1 KB
    // payloads against a 16-page cache definitely overflows. The
    // spillway is constructed lazily on the first overflow; after
    // these allocations it MUST exist (Some(_)).
    db.begin().unwrap();
    for i in 0..200 {
        let payload = vec![i as u8; 1024];
        db.allocate(&payload).unwrap();
    }
    let s = db.stats().unwrap();
    assert!(
        s.spillway_logical_bytes.is_some(),
        "after overflow workload: spillway must have been opened"
    );
    assert!(
        s.spillway_logical_bytes.unwrap() > 0,
        "after overflow workload: spillway must hold at least one spilled page"
    );
    assert_eq!(
        s.spillway_max_bytes,
        Some(spillway_cap),
        "max_bytes must report the configured cap"
    );

    // Phase 3: commit truncates the spillway. The spillway object
    // persists (still Some), but logical_bytes drops to 0.
    db.commit().unwrap();
    let s = db.stats().unwrap();
    assert_eq!(
        s.spillway_logical_bytes,
        Some(0),
        "post-commit: spillway exists but is drained"
    );
    assert_eq!(
        s.spillway_max_bytes,
        Some(spillway_cap),
        "max_bytes is unchanged across commit"
    );
}

// I159 (#147): the encrypted spill path on the PRODUCTION backing. The
// page_cache.rs unit tests cover encrypted spills only against
// SpillwayLocation::InMemory, and every other test in this file is plaintext;
// `Chisel::open` always uses SpillwayLocation::Path, so nothing else checks that
// the on-disk `<db_path>.spillway` sidecar holds ciphertext. The sidecar is
// truncated at commit, so the plaintext scan must run while the spilling
// transaction is still open.
#[test]
fn encrypted_spill_writes_only_ciphertext_to_sidecar() {
    use chisel::Key;
    use std::path::PathBuf;
    use tempfile::tempdir;

    // A raw key avoids the Argon2id cost; key derivation is upstream of the
    // page cipher and does not change what the spill path writes.
    fn key() -> Key {
        Key::raw(vec![0x5A; 32])
    }

    // 16 bytes that will not occur by chance in AEAD output. Values are this
    // unit repeated, so any plaintext page carrying part of a value contains at
    // least one complete copy; searching for the whole unit (not a single byte)
    // keeps a random ciphertext match out of reach.
    const SENTINEL: &[u8; 16] = b"#SPILL-SENTINEL#";
    fn value(i: usize) -> Vec<u8> {
        // Prefix the index so each value is distinct (a mis-mapped handle on
        // reopen would compare unequal) without breaking the repeated unit.
        let mut v = (i as u64).to_le_bytes().to_vec();
        v.extend(SENTINEL.repeat(256));
        v
    }
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    let dir = tempdir().unwrap();
    let db_path: PathBuf = dir.path().join("enc-spill.chisel");
    let spillway_path: PathBuf = {
        let mut p = db_path.as_os_str().to_owned();
        p.push(".spillway");
        PathBuf::from(p)
    };

    // Same spill recipe as Task 17: a 4-page cache against 20 values of ~4 KiB
    // overflows the dirty cap well before the loop ends.
    let cache_max_bytes = 4 * 8192;
    let opts = Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(1024 * cache_max_bytes)
        .encryption_key(key());
    let mut db = Chisel::open(&db_path, opts).unwrap();

    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..20 {
        let v = value(i);
        handles.push((db.allocate(&v).unwrap(), v));
    }

    // Non-vacuity: the engine must actually have spilled, to the on-disk
    // sidecar, before the absence of the sentinel means anything.
    let logical = db.stats().unwrap().spillway_logical_bytes;
    assert!(
        logical.is_some_and(|b| b > 0),
        "workload must spill mid-transaction; spillway_logical_bytes = {logical:?}"
    );
    let sidecar = std::fs::read(&spillway_path).expect("sidecar must exist after a spill");
    assert!(!sidecar.is_empty(), "sidecar must hold the spilled pages");
    assert!(
        !contains(&sidecar, SENTINEL),
        "plaintext value bytes found in the encrypted spillway sidecar ({} bytes)",
        sidecar.len()
    );

    // Drain copies the sealed blobs verbatim into the main file; nothing there
    // may be plaintext either.
    db.commit().unwrap();
    drop(db);
    let main = std::fs::read(&db_path).unwrap();
    assert!(
        !contains(&main, SENTINEL),
        "plaintext value bytes found in the encrypted main file"
    );

    // Every value survives spill → drain → reopen with the key.
    let db = Chisel::open(
        &db_path,
        Options::default()
            .create_if_missing(false)
            .encryption_key(key()),
    )
    .unwrap();
    for (h, expected) in &handles {
        assert_eq!(
            &db.read(*h).unwrap(),
            expected,
            "handle {h} content corrupt"
        );
    }
}
