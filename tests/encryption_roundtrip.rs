// tests/encryption_roundtrip.rs — end-to-end public-API encryption contract.
//
// Documents the three-case guarantee for encrypted databases: create + write
// with a key → reopen with the SAME key reads the value back; reopen with a
// WRONG key → InvalidEncryptionKey; reopen with NO key → NoEncryptionKey.
//
// Uses a raw 32-byte key to avoid paying the Argon2id cost. Passphrase
// derivation is exercised in the crypto unit tests. Uses only the public API
// (chisel::{Chisel, ChiselError, Key, Options}); no crate-internal paths.

use chisel::{ChiselError, Chisel, Key, Options};
use zeroize::Zeroizing;

fn raw_key(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

#[test]
fn encrypted_roundtrip_and_wrong_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("enc.db");

    // Create encrypted, write a value, capture the raw handle id, close.
    let raw_handle = {
        let mut db =
            Chisel::open(&path, Options::default().encryption_key(raw_key(0xAB)))
                .expect("create encrypted");
        db.begin().expect("begin");
        let h = db.allocate(b"secret-payload").expect("allocate");
        db.commit().expect("commit");
        h.get()
    };

    // Reopen with the SAME key: value reads back.
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .create_if_missing(false)
                .encryption_key(raw_key(0xAB)),
        )
        .expect("reopen with correct key");
        let v = db
            .read(chisel::Handle::from(raw_handle))
            .expect("read after reopen");
        assert_eq!(&v, b"secret-payload");
    }

    // Reopen with the WRONG key: must return InvalidEncryptionKey.
    {
        let result = Chisel::open(
            &path,
            Options::default()
                .create_if_missing(false)
                .encryption_key(raw_key(0x00)),
        );
        assert!(result.is_err(), "wrong key must fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, ChiselError::InvalidEncryptionKey),
            "expected InvalidEncryptionKey, got {err:?}"
        );
    }

    // Reopen with NO key: must return NoEncryptionKey.
    {
        let result = Chisel::open(&path, Options::default().create_if_missing(false));
        assert!(result.is_err(), "missing key must fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, ChiselError::NoEncryptionKey),
            "expected NoEncryptionKey, got {err:?}"
        );
    }
}
