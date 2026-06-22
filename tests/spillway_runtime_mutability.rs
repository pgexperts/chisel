// spillway_runtime_mutability.rs — Tests for the between-transactions runtime
// config setters: set_cache_max_bytes, set_spillway_max_bytes, set_drain_insertion.
//
// Each setter routes through TransactionManager which rejects mid-transaction
// calls with ChiselError::TransactionInProgress. Between transactions the
// setters are always valid; the spillway and drain policy have no persistent
// state to unwind.

use chisel::Chisel;
use chisel::{ChiselError, DrainInsertion, Options};

#[test]
fn set_cache_max_bytes_returns_transaction_in_progress_mid_txn() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let err = db.set_cache_max_bytes(16 * 1024 * 1024).unwrap_err();
    assert!(matches!(err, ChiselError::TransactionInProgress));
    db.rollback().unwrap();
}

#[test]
fn set_cache_max_bytes_succeeds_between_transactions() {
    let mut db = Chisel::open_in_memory().unwrap();
    // Default is 8 MiB; bump to 16 MiB.
    db.set_cache_max_bytes(16 * 1024 * 1024).unwrap();
    // Verify the engine still functions correctly after the resize.
    db.begin().unwrap();
    let _h = db.allocate(b"x").unwrap();
    db.commit().unwrap();
}

#[test]
fn set_spillway_max_bytes_to_zero_succeeds_between_transactions() {
    // Open with a small cache (32 pages) but a large spillway so there is
    // headroom to spill if the spillway remains enabled.
    let opts = Options::default()
        .cache_max_bytes(32 * 8192)
        .spillway_max_bytes(64 * 8192 * 1024);
    let mut db = Chisel::open_in_memory_with_options(opts).unwrap();

    // Bootstrap a committed state so the cache is warm.
    db.begin().unwrap();
    db.allocate(b"seed").unwrap();
    db.commit().unwrap();

    // Setting to 0 disables the spillway — always valid between transactions.
    db.set_spillway_max_bytes(0).unwrap();

    // Verify the cap took effect: with spillway disabled and a 32-page cache,
    // filling the cache with large values must produce CacheFull rather than
    // spilling. A regression that leaves the spillway enabled would let these
    // allocations succeed and the assert below would fail.
    db.begin().unwrap();
    let mut got_cache_full = false;
    for _ in 0..200 {
        match db.allocate(&vec![0xABu8; 4096]) {
            Ok(_) => {}
            Err(ChiselError::CacheFull { .. }) => {
                got_cache_full = true;
                break;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    db.rollback().unwrap();
    assert!(
        got_cache_full,
        "spillway=0 must produce CacheFull when cache overflows, not silently spill"
    );
}

#[test]
fn set_drain_insertion_returns_transaction_in_progress_mid_txn() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let err = db.set_drain_insertion(DrainInsertion::Mru).unwrap_err();
    assert!(matches!(err, ChiselError::TransactionInProgress));
    db.rollback().unwrap();
}
