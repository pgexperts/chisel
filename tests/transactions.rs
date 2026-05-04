use chisel::page_cache::PageCache;
use chisel::page_io::PageIo;
use chisel::transaction::TransactionManager;
use tempfile::NamedTempFile;

mod common;
use common::{open_chisel, Backing};

fn test_begin_allocate_commit_read_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"hello world").unwrap();
    db.commit().unwrap();
    let data = db.read(handle).unwrap();
    assert_eq!(data, b"hello world");
}

dual_backing_test!(
    test_begin_allocate_commit_read,
    test_begin_allocate_commit_read_body
);

fn test_rollback_discards_changes_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"doomed").unwrap();
    db.rollback().unwrap();
    assert!(db.read(handle).is_err());
}

dual_backing_test!(
    test_rollback_discards_changes,
    test_rollback_discards_changes_body
);

fn test_update_preserves_handle_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"original").unwrap();
    db.commit().unwrap();
    db.begin().unwrap();
    db.update(handle, b"updated value").unwrap();
    db.commit().unwrap();
    let data = db.read(handle).unwrap();
    assert_eq!(data, b"updated value");
}

dual_backing_test!(
    test_update_preserves_handle,
    test_update_preserves_handle_body
);

fn test_delete_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"gone soon").unwrap();
    db.commit().unwrap();
    db.begin().unwrap();
    db.delete(handle).unwrap();
    db.commit().unwrap();
    assert!(db.read(handle).is_err());
}

dual_backing_test!(test_delete, test_delete_body);

fn test_savepoint_rollback_to_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"kept").unwrap();
    db.savepoint("alpha").unwrap();
    let h2 = db.allocate(b"discarded").unwrap();
    db.rollback_to("alpha").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"kept");
    assert!(db.read(h2).is_err());
}

dual_backing_test!(test_savepoint_rollback_to, test_savepoint_rollback_to_body);

fn test_savepoint_release_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"first").unwrap();
    db.savepoint("alpha").unwrap();
    let h2 = db.allocate(b"second").unwrap();
    db.release("alpha").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"first");
    assert_eq!(db.read(h2).unwrap(), b"second");
}

dual_backing_test!(test_savepoint_release, test_savepoint_release_body);

fn test_savepoint_rollback_preserves_savepoint_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    db.savepoint("retry").unwrap();
    let _h1 = db.allocate(b"attempt 1").unwrap();
    db.rollback_to("retry").unwrap();
    let h2 = db.allocate(b"attempt 2").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h2).unwrap(), b"attempt 2");
}

dual_backing_test!(
    test_savepoint_rollback_preserves_savepoint,
    test_savepoint_rollback_preserves_savepoint_body
);

fn test_nested_savepoints_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"base").unwrap();
    db.savepoint("alpha").unwrap();
    let h2 = db.allocate(b"in alpha").unwrap();
    db.savepoint("beta").unwrap();
    let h3 = db.allocate(b"in beta").unwrap();
    db.rollback_to("alpha").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"base");
    assert!(db.read(h2).is_err());
    assert!(db.read(h3).is_err());
}

dual_backing_test!(test_nested_savepoints, test_nested_savepoints_body);

// Kept file-only: reopens the same path to verify on-disk persistence.
#[test]
fn test_reopen_preserves_data() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            64 * chisel::page::PAGE_SIZE as u64,
            0,
            chisel::DrainInsertion::LruTail,
            chisel::SpillwayLocation::InMemory,
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
            64 * chisel::page::PAGE_SIZE as u64,
            0,
            chisel::DrainInsertion::LruTail,
            chisel::SpillwayLocation::InMemory,
        );
        let txm = TransactionManager::open_existing(cache).unwrap();
        let data = txm.read(handle).unwrap();
        assert_eq!(data, b"persistent");
    }
}
