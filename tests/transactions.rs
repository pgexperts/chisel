use chisel::page_io::PageIo;
use chisel::page_cache::PageCache;
use chisel::transaction::TransactionManager;
use tempfile::NamedTempFile;

#[test]
fn test_begin_allocate_commit_read() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let io = PageIo::open(&path, false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let handle = txm.allocate(b"hello world").unwrap();
    txm.commit().unwrap();
    let data = txm.read(handle).unwrap();
    assert_eq!(data, b"hello world");
}

#[test]
fn test_rollback_discards_changes() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let handle = txm.allocate(b"doomed").unwrap();
    txm.rollback().unwrap();
    assert!(txm.read(handle).is_err());
}

#[test]
fn test_update_preserves_handle() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let handle = txm.allocate(b"original").unwrap();
    txm.commit().unwrap();
    txm.begin().unwrap();
    txm.update(handle, b"updated value").unwrap();
    txm.commit().unwrap();
    let data = txm.read(handle).unwrap();
    assert_eq!(data, b"updated value");
}

#[test]
fn test_delete() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let handle = txm.allocate(b"gone soon").unwrap();
    txm.commit().unwrap();
    txm.begin().unwrap();
    txm.delete(handle).unwrap();
    txm.commit().unwrap();
    assert!(txm.read(handle).is_err());
}

#[test]
fn test_savepoint_rollback_to() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let h1 = txm.allocate(b"kept").unwrap();
    txm.savepoint("alpha").unwrap();
    let h2 = txm.allocate(b"discarded").unwrap();
    txm.rollback_to("alpha").unwrap();
    txm.commit().unwrap();
    assert_eq!(txm.read(h1).unwrap(), b"kept");
    assert!(txm.read(h2).is_err());
}

#[test]
fn test_savepoint_release() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let h1 = txm.allocate(b"first").unwrap();
    txm.savepoint("alpha").unwrap();
    let h2 = txm.allocate(b"second").unwrap();
    txm.release("alpha").unwrap();
    txm.commit().unwrap();
    assert_eq!(txm.read(h1).unwrap(), b"first");
    assert_eq!(txm.read(h2).unwrap(), b"second");
}

#[test]
fn test_savepoint_rollback_preserves_savepoint() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    txm.savepoint("retry").unwrap();
    let _h1 = txm.allocate(b"attempt 1").unwrap();
    txm.rollback_to("retry").unwrap();
    let h2 = txm.allocate(b"attempt 2").unwrap();
    txm.commit().unwrap();
    assert_eq!(txm.read(h2).unwrap(), b"attempt 2");
}

#[test]
fn test_nested_savepoints() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let cache = PageCache::new(io, 64);
    let mut txm = TransactionManager::create_new(cache).unwrap();
    txm.begin().unwrap();
    let h1 = txm.allocate(b"base").unwrap();
    txm.savepoint("alpha").unwrap();
    let h2 = txm.allocate(b"in alpha").unwrap();
    txm.savepoint("beta").unwrap();
    let h3 = txm.allocate(b"in beta").unwrap();
    txm.rollback_to("alpha").unwrap();
    txm.commit().unwrap();
    assert_eq!(txm.read(h1).unwrap(), b"base");
    assert!(txm.read(h2).is_err());
    assert!(txm.read(h3).is_err());
}

#[test]
fn test_reopen_preserves_data() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(io, 64);
        let mut txm = TransactionManager::create_new(cache).unwrap();
        txm.begin().unwrap();
        handle = txm.allocate(b"persistent").unwrap();
        txm.commit().unwrap();
    }
    {
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(io, 64);
        let mut txm = TransactionManager::open_existing(cache).unwrap();
        let data = txm.read(handle).unwrap();
        assert_eq!(data, b"persistent");
    }
}
