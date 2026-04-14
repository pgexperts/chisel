// options_validation.rs — Verifies Chisel::open honors the bounds and
// conditional behavior of `Options`.
//
// Covered:
//   * superblock_count range: 0, 1, 17 reject with InvalidSuperblockCount;
//     2, 3, 8, 16 succeed and roundtrip across reopen.
//   * create_if_missing=false on a truly missing path returns FileNotFound.
//   * A zero-length file on disk is treated as "does not exist" per the
//     documented lib.rs open() behavior: create_if_missing=false rejects
//     it with FileNotFound, and create_if_missing=true creates a fresh DB.
//   * Reopening an existing database ignores options.superblock_count —
//     the value is discovered from the on-disk superblock.
//
// Dual-backing coverage: the three superblock_count rejection tests are
// validated against both file-backed and memory-backed constructors using
// the negative-path dual-open pattern (explicit double-assert). All other
// tests remain file-only for reasons noted inline.

mod common;

use chisel::superblock::{DEFAULT_SUPERBLOCK_COUNT, MAX_SUPERBLOCKS};
use chisel::{Chisel, ChiselError, Options, Result};
use tempfile::{NamedTempFile, TempDir};

// `Chisel` does not implement `Debug`, so `Result::unwrap_err()` is not
// available. This helper extracts the error by pattern-matching so tests
// can assert on the ChiselError variant directly.
fn expect_err(r: Result<Chisel>) -> ChiselError {
    match r {
        Ok(_) => panic!("expected an error, got a live Chisel handle"),
        Err(e) => e,
    }
}

fn opts(superblock_count: u32) -> Options {
    Options {
        cache_size: 16,
        create_if_missing: true,
        read_only: false,
        superblock_count,
    }
}

// --- superblock_count validation ---
//
// These three tests use the negative-path dual-open pattern: the invalid
// Options are rejected before any I/O, so the same validation path fires
// for both constructors. dual_backing_test! is not used here because it
// calls .unwrap() on the open result, which would panic on the expected Err.

#[test]
fn test_superblock_count_zero_rejected() {
    let bad = opts(0);

    // File-backed: rejection happens before the file is opened.
    let f = NamedTempFile::new().unwrap();
    assert!(matches!(
        expect_err(Chisel::open(f.path(), bad.clone())),
        ChiselError::InvalidSuperblockCount { value: 0 }
    ));

    // Memory-backed: same validation path.
    assert!(matches!(
        expect_err(Chisel::open_in_memory_with_options(bad)),
        ChiselError::InvalidSuperblockCount { value: 0 }
    ));
}

#[test]
fn test_superblock_count_one_rejected() {
    let bad = opts(1);

    // File-backed: rejection happens before the file is opened.
    let f = NamedTempFile::new().unwrap();
    assert!(matches!(
        expect_err(Chisel::open(f.path(), bad.clone())),
        ChiselError::InvalidSuperblockCount { value: 1 }
    ));

    // Memory-backed: same validation path.
    assert!(matches!(
        expect_err(Chisel::open_in_memory_with_options(bad)),
        ChiselError::InvalidSuperblockCount { value: 1 }
    ));
}

#[test]
fn test_superblock_count_seventeen_rejected() {
    let bad = opts(17);

    // File-backed: rejection happens before the file is opened.
    let f = NamedTempFile::new().unwrap();
    assert!(matches!(
        expect_err(Chisel::open(f.path(), bad.clone())),
        ChiselError::InvalidSuperblockCount { value: 17 }
    ));

    // Memory-backed: same validation path.
    assert!(matches!(
        expect_err(Chisel::open_in_memory_with_options(bad)),
        ChiselError::InvalidSuperblockCount { value: 17 }
    ));
}

// FILE-ONLY: creates a DB, drops it, then reopens the same path to verify
// the on-disk superblock_count survives a round-trip. Memory mode has no
// persistent backing to reopen.
#[test]
fn test_superblock_count_min_valid() {
    // The minimum valid value.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db_min.chsl");
    let mut db = Chisel::open(&path, opts(2)).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();
    db.close().unwrap();

    // Reopen with an unrelated Options.superblock_count — the value on
    // disk should win.
    let db = Chisel::open(&path, opts(DEFAULT_SUPERBLOCK_COUNT)).unwrap();
    assert_eq!(db.read(h).unwrap(), b"hello");
}

// FILE-ONLY: persistence test (drop + reopen).
#[test]
fn test_superblock_count_max_valid_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db_max.chsl");
    let mut db = Chisel::open(&path, opts(MAX_SUPERBLOCKS)).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"max slots").unwrap();
    db.commit().unwrap();
    db.close().unwrap();

    let db = Chisel::open(&path, opts(MAX_SUPERBLOCKS)).unwrap();
    assert_eq!(db.read(h).unwrap(), b"max slots");
    let stats = db.stats().unwrap();
    // At least MAX_SUPERBLOCKS pages are reserved for the slot layout.
    assert!(stats.total_pages >= MAX_SUPERBLOCKS as u64);
}

// FILE-ONLY: persistence test (drop + reopen).
#[test]
fn test_superblock_count_mid_valid_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db_8.chsl");
    let mut db = Chisel::open(&path, opts(8)).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"eight").unwrap();
    db.commit().unwrap();
    db.close().unwrap();
    let db = Chisel::open(&path, opts(8)).unwrap();
    assert_eq!(db.read(h).unwrap(), b"eight");
}

// --- Reopen ignores superblock_count option ---

// FILE-ONLY: explicitly tests that a reopen of an on-disk file uses the
// superblock_count recorded in the file, not the Options value.
#[test]
fn test_reopen_discovers_superblock_count_from_disk() {
    // Create with N=4, then reopen passing a DIFFERENT valid N (3). The
    // reopen must succeed and the on-disk layout must be preserved. We
    // can't directly read private state, but stats().total_pages must be
    // >= 4 (the original slot count), not 3.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db_reopen.chsl");

    let mut db = Chisel::open(&path, opts(4)).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"v").unwrap();
    db.commit().unwrap();
    let pages_after_create = db.stats().unwrap().total_pages;
    db.close().unwrap();
    assert!(pages_after_create >= 4);

    // Reopen passing a smaller valid count. The option is ignored.
    let db = Chisel::open(&path, opts(3)).unwrap();
    assert_eq!(db.read(h).unwrap(), b"v");
    assert!(db.stats().unwrap().total_pages >= 4);
}

// --- create_if_missing semantics ---

// FILE-ONLY: create_if_missing is a file-system concept with no analogue
// in memory mode.
#[test]
fn test_create_if_missing_false_on_missing_file_errors() {
    // A path that does not exist.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("does_not_exist.chsl");
    let err = expect_err(Chisel::open(
        &path,
        Options {
            create_if_missing: false,
            ..Options::default()
        },
    ));
    assert!(matches!(err, ChiselError::FileNotFound));
}

// FILE-ONLY: reads std::fs::metadata to verify the file is zero-length;
// the zero-length-as-missing behavior is inherently file-system specific.
#[test]
fn test_create_if_missing_false_on_zero_length_file_errors() {
    // A zero-length file is treated as "not yet a database" by
    // Chisel::open (lib.rs comment). With create_if_missing=false that
    // condition must surface as FileNotFound, matching the nonexistent
    // case.
    let f = NamedTempFile::new().unwrap(); // tempfile is created empty
    assert_eq!(std::fs::metadata(f.path()).unwrap().len(), 0);
    let err = expect_err(Chisel::open(
        f.path(),
        Options {
            create_if_missing: false,
            ..Options::default()
        },
    ));
    assert!(matches!(err, ChiselError::FileNotFound));
}

// FILE-ONLY: validates "touch + open" recovery story — the zero-length
// file is a file-system artifact that has no meaning in memory mode.
#[test]
fn test_zero_length_file_is_initialized_as_fresh_db() {
    // Counterpart to the previous test: the default (create_if_missing=true)
    // succeeds even on a pre-existing empty file, producing a valid fresh
    // database. Verifies the "touch + open" and "creat(2) crash between
    // creat and first write" recovery story documented in lib.rs.
    let f = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(f.path(), Options::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"fresh").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"fresh");
}

// --- ReadOnly mode ---

// FILE-ONLY: read_only requires reopening an already-created on-disk file;
// memory mode has no persistent backing and no equivalent open mode.
#[test]
fn test_read_only_open_rejects_begin() {
    // A database opened with `read_only: true` must surface
    // `ChiselError::ReadOnlyMode` at the first mutating call, not a
    // generic fatal `IoError` wrapping EBADF from the kernel. The
    // library layer is responsible for checking before any work is
    // done; the per-PageIo guards exist as a defense-in-depth layer.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ro.chsl");
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        let _ = db.allocate(b"seed").unwrap();
        db.commit().unwrap();
    }
    let mut db = Chisel::open(
        &path,
        Options {
            cache_size: 16,
            create_if_missing: false,
            read_only: true,
            superblock_count: DEFAULT_SUPERBLOCK_COUNT,
        },
    )
    .unwrap();
    let err = match db.begin() {
        Ok(()) => panic!("begin() unexpectedly succeeded on a read-only handle"),
        Err(e) => e,
    };
    assert!(
        matches!(err, ChiselError::ReadOnlyMode),
        "expected ReadOnlyMode, got {err:?}"
    );
}

// --- LockFailed ---

// FILE-ONLY: flock is a file-system concept; memory mode has no lock.
#[test]
fn test_second_open_on_same_file_fails_with_lock_error() {
    // flock is exclusive non-blocking; the second open returns LockFailed
    // rather than hanging. Verifies the single-writer contract documented
    // in lib.rs and page_io.rs.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("locked.chsl");
    // Pre-create with size > 0 so the "zero-length => create_new" branch
    // doesn't interfere with the lock test on macOS, which sometimes
    // reports ENOENT before the lock is attempted on a missing path.
    let _first = Chisel::open(&path, Options::default()).unwrap();
    let err = expect_err(Chisel::open(&path, Options::default()));
    assert!(matches!(err, ChiselError::LockFailed));
}
