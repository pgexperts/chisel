// in_memory.rs — Mode-specific tests for in-memory Chisel.
//
// Behavior parity with file-backed Chisel is verified by the dual-backing
// integration suite; these tests cover only what is specific to memory mode:
// the open constructors, option validation, and isolation between instances.

mod common;

use chisel::{Chisel, Options};

#[test]
fn open_in_memory_round_trip_commit() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"hello".to_vec());
}

#[test]
fn open_in_memory_round_trip_rollback() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.rollback().unwrap();
    // A fresh transaction must not see rolled-back handles.
    db.begin().unwrap();
    assert!(db.read(h).is_err());
}

#[test]
fn open_in_memory_with_options_respects_superblock_count() {
    // Non-default superblock_count flows through to the memory bootstrap.
    // We cannot inspect the superblock count from the public API directly,
    // but an invalid value must still be rejected by the same validation
    // path used for file-backed open.
    let bad = Options {
        superblock_count: 1, // below MIN_SUPERBLOCKS (2)
        ..Options::default()
    };
    assert!(Chisel::open_in_memory_with_options(bad).is_err());

    let good = Options {
        superblock_count: 4,
        ..Options::default()
    };
    let mut db = Chisel::open_in_memory_with_options(good).unwrap();
    db.begin().unwrap();
    db.allocate(b"payload").unwrap();
    db.commit().unwrap();
}

#[test]
fn open_in_memory_rejects_read_only_option() {
    // A read-only, freshly-created memory database cannot bootstrap
    // (nothing to read, and the superblock-write step is blocked).
    // We surface this early as an explicit InvalidArgument-style error
    // rather than letting the bootstrap fail obliquely.
    let opts = Options {
        read_only: true,
        ..Options::default()
    };
    assert!(Chisel::open_in_memory_with_options(opts).is_err());
}

#[test]
fn dropping_in_memory_db_releases_backing() {
    // Smoke test: we can create, fill, drop, and recreate many times
    // without accumulating state. No fd leaks to check here; this mostly
    // guards against "accidentally leaked into a static" regressions.
    for _ in 0..8 {
        let mut db = Chisel::open_in_memory().unwrap();
        db.begin().unwrap();
        for i in 0..100u32 {
            db.allocate(&i.to_le_bytes()).unwrap();
        }
        db.commit().unwrap();
    }
}

fn sanity_body(b: &common::Backing) {
    let mut db = common::open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"ok").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"ok".to_vec());
}

dual_backing_test!(harness_sanity, sanity_body);
