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

use chisel::{Chisel, ChiselError, Options, Result, DEFAULT_SUPERBLOCK_COUNT, MAX_SUPERBLOCKS};
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
    // I36: external callers can't construct Options via struct literal
    // (or `..Options::default()`); use the chained-setter builder.
    // create_if_missing=true and read_only=false are the defaults so
    // they aren't repeated here.
    Options::default()
        .cache_max_bytes(16 * chisel::PAGE_SIZE as u64)
        .spillway_max_bytes(0)
        .drain_insertion(chisel::DrainInsertion::LruTail)
        .superblock_count(superblock_count)
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
        Options::default().create_if_missing(false),
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
        Options::default().create_if_missing(false),
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
        Options::default()
            .cache_max_bytes(16 * chisel::PAGE_SIZE as u64)
            .spillway_max_bytes(0)
            .drain_insertion(chisel::DrainInsertion::LruTail)
            .create_if_missing(false)
            .read_only(true)
            .superblock_count(DEFAULT_SUPERBLOCK_COUNT),
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

// --- argon2_params validation (PUBLIC-API-8, issue #106) ---
//
// Same negative-path dual-open pattern as the superblock_count cases above,
// and for the same reason: the Options are rejected before any I/O, so both
// constructors take the identical path.
//
// What these pin is not just "bad input is rejected" but WHICH error names it.
// Before the fix these values reached `Params::new` deep inside the create
// path, failed as `CryptoError::Kdf`, and were collapsed by the blanket
// `From<CryptoError> for ChiselError` into `InvalidEncryptionKey` — whose
// documented meaning is "a key was supplied but no key-slot's wrapped DEK
// could be unwrapped". On a database being CREATED there is no key slot to
// mismatch, so the reported cause was not merely unhelpful, it was impossible.

/// Options with a passphrase key and the given cost parameters. A passphrase
/// (not a raw key) is required for the Argon2 path to be selected at all —
/// `wrap_into` picks HKDF for raw keys and never reads these fields.
fn argon2_opts(m_cost: u32, t_cost: u32, p_cost: u32) -> Options {
    Options::default()
        .cache_max_bytes(16 * chisel::PAGE_SIZE as u64)
        .encryption_key(chisel::Key::Passphrase(zeroize::Zeroizing::new(
            "correct horse battery".to_string(),
        )))
        .argon2_params(chisel::Argon2Params {
            m_cost,
            t_cost,
            p_cost,
        })
}

fn assert_rejected(m_cost: u32, t_cost: u32, p_cost: u32) {
    let bad = argon2_opts(m_cost, t_cost, p_cost);

    let f = NamedTempFile::new().unwrap();
    let err = expect_err(Chisel::open(f.path(), bad.clone()));
    assert!(
        matches!(
            err,
            ChiselError::InvalidArgon2Params { m_cost: m, t_cost: t, p_cost: p }
                if m == m_cost && t == t_cost && p == p_cost
        ),
        "({m_cost}, {t_cost}, {p_cost}) should be InvalidArgon2Params, got {err:?}"
    );

    let err = expect_err(Chisel::open_in_memory_with_options(bad));
    assert!(
        matches!(err, ChiselError::InvalidArgon2Params { .. }),
        "({m_cost}, {t_cost}, {p_cost}) should be InvalidArgon2Params in memory mode, got {err:?}"
    );
}

#[test]
fn test_argon2_zero_m_cost_rejected() {
    // The exact reproducer from the issue: m_cost 0 used to return
    // InvalidEncryptionKey on a database that was being created.
    assert_rejected(0, 1, 1);
}

#[test]
fn test_argon2_zero_t_cost_rejected() {
    assert_rejected(19456, 0, 1);
}

#[test]
fn test_argon2_zero_p_cost_rejected() {
    assert_rejected(19456, 2, 0);
}

#[test]
fn test_argon2_m_cost_below_lanes_times_eight_rejected() {
    // argon2 requires 8 KiB of memory per lane. Each field is individually in
    // range here — this is the cross-field constraint, which is the one a
    // caller violates by accident when raising p_cost alone.
    assert_rejected(8, 2, 2);
}

#[test]
fn test_argon2_m_cost_above_cap_rejected() {
    // The upper caps already existed but were enforced inside derive_kek and
    // reported as InvalidEncryptionKey. This asserts the boundary check now
    // names them, end to end.
    assert_rejected(chisel::MAX_ARGON2_M_COST + 1, 2, 1);
}

#[test]
fn test_argon2_t_cost_above_cap_rejected() {
    assert_rejected(19456, chisel::MAX_ARGON2_T_COST + 1, 1);
}

#[test]
fn test_argon2_boundary_values_accepted() {
    // The exact minimums must be ACCEPTED — an off-by-one in the comparison
    // would make the engine refuse parameters the KDF is perfectly happy with,
    // and no rejection test can catch that.
    let f = NamedTempFile::new().unwrap();
    let ok = argon2_opts(
        chisel::MIN_ARGON2_M_COST,
        chisel::MIN_ARGON2_T_COST,
        chisel::MIN_ARGON2_P_COST,
    );
    Chisel::open(f.path(), ok).expect("minimum legal argon2 params must be accepted");
}

#[test]
fn test_argon2_maximum_values_accepted() {
    // The other accept-side boundary. A `>` mistyped as `>=` in the cap checks
    // would falsely reject the documented maximum, and no rejection test can
    // catch that. m_cost stays modest so the KDF work is trivial — the caps are
    // independent, so exercising t/p at MAX is what matters here.
    let f = NamedTempFile::new().unwrap();
    let ok = argon2_opts(
        chisel::MIN_ARGON2_M_COST * chisel::MAX_ARGON2_P_COST,
        chisel::MAX_ARGON2_T_COST,
        chisel::MAX_ARGON2_P_COST,
    );
    Chisel::open(f.path(), ok).expect("maximum legal argon2 params must be accepted");
}

#[test]
fn test_argon2_params_none_is_always_valid() {
    // No explicit params means "use the OWASP default on create, or the
    // per-slot stored params on reopen". The validator must not reject it.
    let f = NamedTempFile::new().unwrap();
    let opts = Options::default()
        .cache_max_bytes(16 * chisel::PAGE_SIZE as u64)
        .encryption_key(chisel::Key::Passphrase(zeroize::Zeroizing::new(
            "correct horse battery".to_string(),
        )));
    Chisel::open(f.path(), opts).expect("absent argon2_params must be accepted");
}
