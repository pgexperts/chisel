// tests/encryption_open.rs — integration tests for the open_existing-with-key path.
//
// Exercises: correct-key round-trip, wrong-key operational error, missing-key
// error, spurious-key-on-plaintext error, and plaintext-DB regression.

use chisel::{Chisel, Key, Options};
use zeroize::Zeroizing;

fn raw_key(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

/// Create an encrypted DB, insert a value, close, reopen with the same key,
/// verify the value is still readable.
#[test]
fn round_trip_open_with_correct_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    let handle;
    {
        let mut db = Chisel::open(
            &path,
            Options::default().with_encryption_key(raw_key(0x11)),
        )
        .unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"hello world").unwrap();
        db.commit().unwrap();
    }
    // Reopen with the same key: data must come back.
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .with_encryption_key(raw_key(0x11))
                .create_if_missing(false),
        )
        .unwrap();
        assert_eq!(db.read(handle).unwrap(), b"hello world");
    }
}

/// Wrong key must fail cleanly — not panic or poison the manager — and a
/// subsequent correct-key open must succeed (retryable error).
#[test]
fn wrong_key_is_operational_error_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::default().with_encryption_key(raw_key(0x11)),
        )
        .unwrap();
        db.begin().unwrap();
        db.commit().unwrap();
    }
    // Wrong key: must return an error.
    let err = Chisel::open(
        &path,
        Options::default()
            .with_encryption_key(raw_key(0x22))
            .create_if_missing(false),
    );
    assert!(err.is_err(), "wrong key must fail to open");

    // Correct key after a failed attempt: must succeed (wrong key is NOT fatal/poison).
    let ok = Chisel::open(
        &path,
        Options::default()
            .with_encryption_key(raw_key(0x11))
            .create_if_missing(false),
    );
    assert!(ok.is_ok(), "correct key must succeed after a wrong-key attempt");
}

/// Opening an encrypted DB without supplying a key must error.
#[test]
fn missing_key_on_encrypted_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::default().with_encryption_key(raw_key(0x11)),
        )
        .unwrap();
        db.begin().unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(&path, Options::default().create_if_missing(false));
    assert!(err.is_err(), "opening an encrypted DB without a key must fail");
}

/// Supplying a key to a plaintext DB must error.
#[test]
fn key_supplied_for_plaintext_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.chisel");
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(
        &path,
        Options::default()
            .with_encryption_key(raw_key(0x11))
            .create_if_missing(false),
    );
    assert!(err.is_err(), "supplying a key to a plaintext DB must fail");
}

/// Plaintext DB created and reopened without a key must still work (regression
/// guard: the version gate must not accidentally break MAJOR=1 DBs).
#[test]
fn plaintext_db_round_trips_without_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.chisel");
    let handle;
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"plain text").unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(&path, Options::default().create_if_missing(false)).unwrap();
        assert_eq!(db.read(handle).unwrap(), b"plain text");
    }
}

/// Passphrase-keyed DB round-trips: create with a passphrase, reopen with the
/// same passphrase, data is intact.
#[test]
fn passphrase_key_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("passphrase.chisel");
    let pass = || Key::Passphrase(Zeroizing::new("correct horse battery staple".to_string()));
    let handle;
    {
        let mut db =
            Chisel::open(&path, Options::default().with_encryption_key(pass())).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"secret").unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .with_encryption_key(pass())
                .create_if_missing(false),
        )
        .unwrap();
        assert_eq!(db.read(handle).unwrap(), b"secret");
    }
}

/// Named root written under an encrypted DB must round-trip through open.
#[test]
fn named_root_round_trips_through_encrypted_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("named.chisel");
    let handle;
    {
        let mut db = Chisel::open(
            &path,
            Options::default().with_encryption_key(raw_key(0xAB)),
        )
        .unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"payload").unwrap();
        db.set_root_name("myroot", handle).unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .with_encryption_key(raw_key(0xAB))
                .create_if_missing(false),
        )
        .unwrap();
        let h = db.get_root_name("myroot").unwrap();
        assert!(h.is_some(), "named root must survive close+reopen");
        assert_eq!(db.read(h.unwrap()).unwrap(), b"payload");
    }
}
