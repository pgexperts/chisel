use chisel::page::PAGE_SIZE;
use chisel::{Chisel, Options};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use tempfile::NamedTempFile;

#[test]
fn test_recovery_after_clean_close() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"durable data").unwrap();
        db.commit().unwrap();
    }
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"durable data");
}

#[test]
fn test_recovery_uncommitted_data_lost() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let committed_handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        committed_handle = db.allocate(b"committed").unwrap();
        db.commit().unwrap();
        db.begin().unwrap();
        db.allocate(b"uncommitted").unwrap();
        // Drop without commit — simulates crash.
    }
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(committed_handle).unwrap(), b"committed");
    assert!(db.read(committed_handle + 1).is_err());
}

#[test]
fn test_recovery_corrupt_superblock_b() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"safe").unwrap();
        db.commit().unwrap();
    }
    // Corrupt superblock B (page 1) by zeroing it.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"safe");
}

#[test]
fn test_recovery_both_superblocks_corrupt() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"doomed").unwrap();
        db.commit().unwrap();
    }
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    let result = Chisel::open(&path, Default::default());
    assert!(result.is_err());
}

#[test]
fn test_file_not_found_without_create() {
    let path = std::path::PathBuf::from("/tmp/chisel_nonexistent_test.db");
    let _ = fs::remove_file(&path);
    let result = Chisel::open(
        &path,
        Options {
            create_if_missing: false,
            ..Default::default()
        },
    );
    assert!(result.is_err());
}
