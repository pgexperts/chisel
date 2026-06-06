//! Integration tests for the per-chunk client byte: in-session set/read (both
//! backings), default 0, transactional revert (rollback + savepoint),
//! preservation across update() and across the inline->overflow transition,
//! durability across reopen (including overflow-resident values updated before
//! commit), independence from the tag, and the invalid/deleted-handle error
//! contracts.
//!
//! Poison-path coverage lives in the in-crate unit test
//! `poisoned_manager_rejects_every_public_entry_point` (src/transaction.rs),
//! which has access to the `#[cfg(test)]`-gated `force_poison_for_test` hook;
//! that hook is unreachable from integration tests.
mod common;

use chisel::{Chisel, ChiselError};
use common::{open_chisel, Backing};
use tempfile::NamedTempFile;

fn set_and_read_in_session_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"row").unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 0, "default is 0");
    db.set_client_byte(h, 0xAB).unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 0xAB);
    db.commit().unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 0xAB, "survives commit");
    db.close().unwrap();
}
dual_backing_test!(set_and_read_in_session, set_and_read_in_session_body);

#[test]
fn reverts_on_rollback_and_savepoint() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"row").unwrap();
    db.set_client_byte(h, 10).unwrap();
    db.commit().unwrap();

    // Whole-transaction rollback restores the committed byte.
    db.begin().unwrap();
    db.set_client_byte(h, 99).unwrap();
    db.rollback().unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 10, "rollback reverts the byte");

    // Savepoint rollback reverts to the savepoint's value.
    db.begin().unwrap();
    db.set_client_byte(h, 20).unwrap();
    db.savepoint("sp").unwrap();
    db.set_client_byte(h, 30).unwrap();
    db.rollback_to("sp").unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 20, "rollback_to reverts to savepoint");
    db.commit().unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 20);
    db.close().unwrap();
}

#[test]
fn preserved_across_update_including_overflow_growth() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"small").unwrap();
    db.set_client_byte(h, 0x7F).unwrap();
    // Grow the value well past the inline limit (MAX_INLINE_VALUE = 8162) so
    // update() relocates it to an overflow chain; the client byte must ride
    // the entry rewrite.
    let big = vec![b'x'; 64 * 1024];
    db.update(h, &big).unwrap();
    db.commit().unwrap();
    assert_eq!(db.client_byte(h).unwrap(), 0x7F, "preserved across update + overflow");
    assert_eq!(db.read(h).unwrap(), big);
    db.close().unwrap();
}

#[test]
fn durable_and_independent_of_tag_across_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h = db.allocate_tagged(b"row", 1234).unwrap();
        db.set_client_byte(h, 0xC9).unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.client_byte(h).unwrap(), 0xC9, "client byte durable");
        assert_eq!(db.tag(h).unwrap(), 1234, "tag undisturbed by client byte");
        db.close().unwrap();
    }
}

#[test]
fn tagged_overflow_value_byte_and_tag_durable_across_update_and_reopen() {
    // Spec items 6 + 7: an overflow-resident entry whose value was relocated by
    // update() must persist BOTH the client byte and the tag across a full
    // close/reopen (serialize -> deserialize) cycle.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h = db.allocate_tagged(b"small", 4321).unwrap();
        db.set_client_byte(h, 0x5E).unwrap();
        // Grow past the inline limit so update() relocates the value to an
        // overflow chain and rewrites the entry — exercising carry-forward of
        // both tag and client_byte, then durability of an overflow entry.
        let big = vec![b'z'; 64 * 1024];
        db.update(h, &big).unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.client_byte(h).unwrap(), 0x5E, "client byte durable for overflow value");
        assert_eq!(db.tag(h).unwrap(), 4321, "tag durable across update + reopen");
        assert_eq!(db.read(h).unwrap(), vec![b'z'; 64 * 1024]);
        db.close().unwrap();
    }
}

#[test]
fn invalid_and_deleted_handles_error() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    // Unknown handle without any transaction.
    assert!(matches!(db.client_byte(999), Err(ChiselError::InvalidHandle(999))));
    db.begin().unwrap();
    assert!(matches!(db.set_client_byte(999, 1), Err(ChiselError::InvalidHandle(999))));
    // Deleted handle (tombstone) is rejected like read(), not read as a stale value.
    let h = db.allocate(b"row").unwrap();
    db.delete(h).unwrap();
    assert!(matches!(db.client_byte(h), Err(ChiselError::InvalidHandle(_))));
    assert!(matches!(db.set_client_byte(h, 1), Err(ChiselError::InvalidHandle(_))));
    db.rollback().unwrap();
    db.close().unwrap();
}
