// Smoke test for the bench crate's Engine trait surface, exercised
// through ChiselEngine. Goal: every Engine method gets called once
// in a realistic sequence; internal_counters() returns Some with
// monotonically advancing values; the trait abstraction does not
// hide engine-specific bugs.
//
// Uses in-memory Chisel for speed — no real file, no fsync. The
// counters still advance because in-memory Chisel calls fsync as a
// no-op (the counter increments regardless, by design — see PR 1
// commit `a051600` and the note in `PageIo::fsync`).

use chisel_bench::{ChiselEngine, Engine, Identifier};

#[test]
fn smoke_full_lifecycle_through_engine_trait() {
    let mut engine = ChiselEngine::open_in_memory(64).expect("open in-memory");

    // Counters must be Some for ChiselEngine (the spec's contract).
    let baseline = engine.internal_counters().expect("Chisel exposes counters");

    // Allocate three values inside one transaction.
    engine.begin().expect("begin");
    let a: Identifier = engine.allocate(b"alpha").expect("allocate alpha");
    let b: Identifier = engine.allocate(b"beta").expect("allocate beta");
    let c: Identifier = engine.allocate(b"gamma").expect("allocate gamma");
    engine.commit().expect("commit");

    // Read them back outside any transaction (read takes &self).
    assert_eq!(engine.read(a).expect("read a"), b"alpha");
    assert_eq!(engine.read(b).expect("read b"), b"beta");
    assert_eq!(engine.read(c).expect("read c"), b"gamma");

    // Update and verify.
    engine.begin().expect("begin");
    engine.update(b, b"BETA").expect("update b");
    engine.commit().expect("commit");
    assert_eq!(engine.read(b).expect("read b'"), b"BETA");

    // Delete one, batch-delete two.
    engine.begin().expect("begin");
    engine.delete(a).expect("delete a");
    engine.delete_many(&[b, c]).expect("delete_many b,c");
    engine.commit().expect("commit");

    // Verify the deletes actually invalidated the handles. Without
    // this, a no-op delete (or delete_many that drops the slice
    // silently) would slip past the smoke test — the next call to
    // read() on a deleted handle must error, not return stale bytes.
    assert!(engine.read(a).is_err(), "delete must invalidate handle a");
    assert!(
        engine.read(b).is_err(),
        "delete_many must invalidate handle b"
    );
    assert!(
        engine.read(c).is_err(),
        "delete_many must invalidate handle c"
    );

    // Rollback path: begin, allocate, rollback. Resulting handle
    // must not be readable after rollback (it never became durable).
    engine.begin().expect("begin");
    let ghost: Identifier = engine.allocate(b"ghost").expect("allocate ghost");
    engine.rollback().expect("rollback");
    assert!(
        engine.read(ghost).is_err(),
        "rolled-back handle must not be readable"
    );

    // Counters advanced.
    let after = engine
        .internal_counters()
        .expect("Chisel still exposes counters");
    assert!(
        after.fsync_calls > baseline.fsync_calls,
        "commits must advance fsync_calls"
    );
    assert!(
        after.pages_allocated > baseline.pages_allocated,
        "allocations must advance pages_allocated"
    );

    // file_size_bytes is non-zero (in-memory backing reports the
    // representation size; cannot be empty after this much work).
    let size = engine.file_size_bytes().expect("file_size_bytes");
    assert!(size > 0, "file_size_bytes must reflect allocated pages");
}
