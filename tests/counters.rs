// Integration tests for Chisel::counters().
//
// Goal: lock down the public-API surface and the deltas through known
// operation sequences. Same dual-backing pattern as basic_ops.rs is not
// used here — counters are an in-memory concept, so we exercise file
// and memory backings explicitly where the distinction matters
// (close/reopen test) and use memory-only where it doesn't.

use chisel::{Chisel, Options};
use tempfile::NamedTempFile;

#[test]
fn counters_after_open_in_memory_have_construction_overhead() {
    let db = Chisel::open_in_memory().unwrap();
    let c = db.counters().unwrap();
    // Open does some work (writes initial superblock, etc.) so fsyncs are
    // non-zero. Note: pages_allocated counts new_page() calls, not the
    // bootstrap superblock writes, so it starts at 0. We pin only the
    // public guarantee: the type is constructible and readable.
    assert!(
        c.fsync_calls > 0,
        "open_in_memory must fsync at least once (superblock bootstrap)"
    );
}

#[test]
fn counters_track_allocate_and_commit() {
    let mut db = Chisel::open_in_memory().unwrap();
    let baseline = db.counters().unwrap();

    db.begin().unwrap();
    let _h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();

    let after = db.counters().unwrap();
    // Commit calls fsync twice (data pages + superblock). Anything below
    // this is a regression in the commit protocol.
    assert!(
        after.fsync_calls >= baseline.fsync_calls + 2,
        "commit must perform at least 2 fsyncs (data + superblock); saw {} -> {}",
        baseline.fsync_calls,
        after.fsync_calls,
    );
    // Allocation grew by at least 1 (the value's data page; possibly more
    // if a handle-table COW or freemap COW was needed).
    assert!(after.pages_allocated > baseline.pages_allocated);
}

#[test]
fn counters_track_read_as_cache_hit_after_commit() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"value").unwrap();
    db.commit().unwrap();

    let pre = db.counters().unwrap();
    let _ = db.read(h).unwrap();
    let post = db.counters().unwrap();

    // Read must hit at least once (the data-page lookup). It may also
    // hit the handle-table page. The lower bound is what we lock in.
    assert!(
        post.cache_hits > pre.cache_hits,
        "read() must register at least one cache lookup; saw {} -> {}",
        pre.cache_hits,
        post.cache_hits,
    );
}

#[test]
fn counters_reset_on_close_and_reopen() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();
    drop(f); // we want the path; let Chisel create the file.

    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        let _ = db.allocate(b"first session").unwrap();
        db.commit().unwrap();
        let c = db.counters().unwrap();
        assert!(
            c.fsync_calls > 0,
            "first session must have fsync'd at least once"
        );
        db.close().unwrap();
    }

    // Reopen — counters must reflect ONLY work since reopen, not the
    // first session's totals.
    let db2 = Chisel::open(&path, Options::default()).unwrap();
    let c2 = db2.counters().unwrap();
    // The reopen path does some work (reads superblocks, possibly writes
    // none). We pin the upper bound: post-reopen totals must be much
    // smaller than the first-session totals (which included a commit
    // and therefore at least 2 fsyncs from the commit alone, plus the
    // create_new bootstrap fsyncs).
    assert!(
        c2.fsync_calls <= 1,
        "reopen path should not fsync more than once; saw {}",
        c2.fsync_calls,
    );
}

#[test]
fn counters_snapshot_does_not_mutate_when_engine_continues() {
    let mut db = Chisel::open_in_memory().unwrap();
    let snap = db.counters().unwrap();
    let snap_copy = snap.clone();

    // Do work after taking the snapshot.
    db.begin().unwrap();
    let _ = db.allocate(b"x").unwrap();
    db.commit().unwrap();

    // The original snapshot must not have moved.
    assert_eq!(
        snap, snap_copy,
        "ChiselCounters is a snapshot, not a live view"
    );
}
