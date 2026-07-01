//! Integration tests for Chisel::add_key, Chisel::rotate_key, and Chisel::remove_key.
//!
//! Each test uses the public API only (Key / Chisel / ChiselError / Options).
//! The underlying DEK is never re-generated, so add_key / rotate_key are pure
//! superblock operations: no page is touched, data survives every credential change.

use chisel::{ChiselError, Chisel, Options};
use chisel::Key;
use tempfile::TempDir;
use zeroize::Zeroizing;

fn raw(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

// ── add_key ──────────────────────────────────────────────────────────────────

/// After add_key, the original key and the new key both open the database and
/// decrypt the same data (the DEK is shared between slots).
#[test]
fn add_key_lets_either_credential_open() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret").unwrap();
        db.commit().unwrap();
        db.add_key(&raw(1), &raw(2)).unwrap();
        db.close().unwrap();
        h
    };

    // Original key still decrypts.
    let db1 = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    assert_eq!(db1.read(h).unwrap(), b"secret");
    db1.close().unwrap();

    // New key also decrypts the same data (same DEK, different slot).
    let db2 = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert_eq!(db2.read(h).unwrap(), b"secret");
    db2.close().unwrap();
}

/// Wrong `existing` key returns InvalidEncryptionKey; the database is unmodified.
#[test]
fn add_key_wrong_existing_is_invalid_encryption_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    let err = db.add_key(&raw(9), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// Filling all 8 key slots returns NoFreeKeySlot on the ninth attempt.
#[test]
fn add_key_full_table_is_no_free_key_slot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Slot 0 is occupied by raw(1) at open; add keys 2..=8 to fill the other 7.
    for k in 2u8..=8 {
        db.add_key(&raw(1), &raw(k)).unwrap();
    }
    let err = db.add_key(&raw(1), &raw(99)).unwrap_err();
    assert!(
        matches!(err, ChiselError::NoFreeKeySlot),
        "expected NoFreeKeySlot, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// add_key on a plaintext database returns EncryptionNotSupported.
#[test]
fn add_key_plaintext_db_returns_encryption_not_supported() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let err = db.add_key(&raw(1), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::EncryptionNotSupported),
        "expected EncryptionNotSupported, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

// ── rotate_key ───────────────────────────────────────────────────────────────

/// After rotate_key(old, new): old no longer opens, new does, and data is intact.
#[test]
fn rotate_key_revokes_old_and_admits_new() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"data").unwrap();
        db.commit().unwrap();
        db.rotate_key(&raw(1), &raw(2)).unwrap();
        db.close().unwrap();
        h
    };

    // Old key is now refused.
    let err = Chisel::open(&path, Options::default().encryption_key(raw(1)))
        .err()
        .expect("old key must be rejected after rotate");
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );

    // New key opens and data is readable.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert!(!db.is_poisoned());
    assert_eq!(db.read(h).unwrap(), b"data");
    db.close().unwrap();
}

/// rotate_key on a plaintext database returns EncryptionNotSupported.
#[test]
fn rotate_key_plaintext_db_returns_encryption_not_supported() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let err = db.rotate_key(&raw(1), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::EncryptionNotSupported),
        "expected EncryptionNotSupported, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// rotate_key with a wrong `old` key returns InvalidEncryptionKey.
#[test]
fn rotate_key_wrong_old_is_invalid_encryption_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    let err = db.rotate_key(&raw(9), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// rotate_key when the slot table is full (no room to stage new) returns
/// NoFreeKeySlot.  The old slot is NOT pre-cleared to make room, since that
/// would create a zero-key window on crash.
#[test]
fn rotate_key_full_table_is_no_free_key_slot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Fill all 8 slots — slot 0 is raw(1), add 7 more.
    for k in 2u8..=8 {
        db.add_key(&raw(1), &raw(k)).unwrap();
    }
    // Full table: rotate must refuse rather than clear old first.
    let err = db.rotate_key(&raw(1), &raw(99)).unwrap_err();
    assert!(
        matches!(err, ChiselError::NoFreeKeySlot),
        "expected NoFreeKeySlot, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

// ── remove_key ───────────────────────────────────────────────────────────────

/// After remove_key the revoked credential is rejected at open; all other
/// credentials continue to decrypt the same data (DEK is shared across slots).
#[test]
fn remove_key_leaves_others_working() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"v").unwrap();
        db.commit().unwrap();
        db.add_key(&raw(1), &raw(2)).unwrap();
        db.remove_key(&raw(1)).unwrap(); // drop the first credential
        db.close().unwrap();
        h
    };
    // raw(1) is gone — open must fail.
    let err = Chisel::open(&path, Options::default().encryption_key(raw(1)))
        .err()
        .expect("old key must be rejected after remove");
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    // raw(2) still opens and reads the original data.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert_eq!(db.read(h).unwrap(), b"v");
    db.close().unwrap();
}

/// remove_key with the only active credential returns LastKeySlot and leaves
/// the database intact (the rejected op must not mutate anything).
#[test]
fn remove_last_key_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Only one active slot — removing it would permanently brick the database.
    let err = db.remove_key(&raw(1)).unwrap_err();
    assert!(
        matches!(err, ChiselError::LastKeySlot),
        "expected LastKeySlot, got {err:?}"
    );
    assert!(!db.is_poisoned());
    // Reopen with the same key to confirm nothing was mutated.
    drop(db);
    let db2 = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    assert!(!db2.is_poisoned());
    db2.close().unwrap();
}

/// remove_key with a key that unlocks no slot returns InvalidEncryptionKey.
#[test]
fn remove_unknown_key_is_invalid_encryption_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    db.add_key(&raw(1), &raw(2)).unwrap();
    let err = db.remove_key(&raw(9)).unwrap_err();
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// remove_key on a plaintext database returns EncryptionNotSupported.
#[test]
fn remove_key_plaintext_db_returns_encryption_not_supported() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let err = db.remove_key(&raw(1)).unwrap_err();
    assert!(
        matches!(err, ChiselError::EncryptionNotSupported),
        "expected EncryptionNotSupported, got {err:?}"
    );
    assert!(!db.is_poisoned());
}
