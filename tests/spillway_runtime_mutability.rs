// spillway_runtime_mutability.rs — Tests for the between-transactions runtime
// config setters: set_cache_max_bytes, set_spillway_max_bytes, set_drain_insertion.
//
// Each setter routes through TransactionManager which rejects mid-transaction
// calls with ChiselError::TransactionInProgress. Between transactions the
// setters are always valid; the spillway and drain policy have no persistent
// state to unwind.

use chisel::Chisel;
use chisel::{ChiselError, DrainInsertion};

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
    let mut db = Chisel::open_in_memory().unwrap();
    // Setting to 0 disables the spillway — always valid between transactions.
    db.set_spillway_max_bytes(0).unwrap();
}

#[test]
fn set_drain_insertion_returns_transaction_in_progress_mid_txn() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let err = db.set_drain_insertion(DrainInsertion::Mru).unwrap_err();
    assert!(matches!(err, ChiselError::TransactionInProgress));
    db.rollback().unwrap();
}
