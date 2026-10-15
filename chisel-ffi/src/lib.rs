// chisel-ffi — UniFFI binding surface for the Chisel engine.
//
// Role in system: the ONLY crate that speaks the C ABI. It wraps chisel::Chisel
// in a Mutex (the engine is Send + !Sync) and re-presents a flat, synchronous
// API to UniFFI, which generates Swift. All ergonomics (async actor, closures)
// live in the Swift package, not here.

uniffi::setup_scaffolding!();

mod error;
pub use error::{ChiselError, FfiResult};

mod types;
pub use types::{
    Argon2Params, ChiselKey, Counters, DefragOptions, DefragStats, DrainInsertion, Options, Stats,
    TagDropProgress,
};

mod database;
pub use database::NativeDatabase;

// Compile-time proof that the engine can move between threads, which the
// Mutex<Option<Chisel>> wrapper in database.rs relies on. If a future engine
// change (mmap, raw OS handle) makes Chisel !Send, THIS line fails to compile
// with a clear message, and the fallback is a dedicated-thread actor (see spec
// §3.3). Cheap insurance against a wrong assumption.
const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<chisel::Chisel>;
};

/// Returns the crate version, e.g. "0.1.0". Smoke test that the whole
/// Rust→UniFFI→Swift pipeline is wired end to end.
#[uniffi::export]
pub fn library_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Re-encrypt an entire database under a freshly generated data key.
///
/// Takes a path rather than an open handle, mirroring the constructor: the
/// whole file is rewritten and published by rename, so a handle opened
/// beforehand would afterwards refer to the original, now-unlinked inode.
/// Close any open database first, call this, then reopen.
///
/// Use this only when the DATA key itself is believed compromised. If a
/// credential leaked, `rotateKey` is the right tool — it is O(1) and touches
/// only the superblock.
///
/// Every credential other than `key` is revoked: each key slot's wrapping key
/// derives from its own credential and only `key` was supplied. Add the others
/// back with `addKey` afterwards.
///
/// `argon2Params` is a Swift-only addition over the Python surface, which
/// exposes no equivalent. `None` inherits the cost parameters of the slot
/// `key` unlocks, so a deliberately hardened database is not silently
/// downgraded; supplying a value re-hardens as part of the rewrite, which is
/// the natural moment to do it.
#[uniffi::export]
pub fn rekey(path: String, key: ChiselKey, argon2_params: Option<Argon2Params>) -> FfiResult<()> {
    chisel::Chisel::rekey(
        std::path::Path::new(&path),
        &key.into_engine(),
        argon2_params.map(Into::into),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    // `use super::*` is the whole import list: lib.rs already re-exports
    // ChiselError, ChiselKey, Options and NativeDatabase, so naming them again
    // here is E0252 (defined multiple times), not a harmless duplicate.
    use super::*;

    // rekey takes a PATH, not an open handle: it builds a replacement file
    // beside the original and publishes it with a rename, so a handle opened
    // beforehand would afterwards refer to the original, now-unlinked inode.
    // Close, rekey, reopen — which is exactly what this test does.
    #[test]
    fn rekey_rewrites_under_a_fresh_data_key_and_preserves_content() {
        let dir = std::env::temp_dir().join(format!("chisel-ffi-rekey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rk.db");
        let path_s: String = path.to_string_lossy().into();
        let key = || ChiselKey::Passphrase {
            phrase: "correct horse".into(),
        };

        let mut opts = Options::default_ffi();
        opts.encryption_key = Some(key());
        let db = NativeDatabase::new(Some(path_s.clone()), opts).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret".to_vec()).unwrap();
        db.set_root_name("r".into(), h).unwrap();
        db.commit().unwrap();
        db.close().unwrap();

        rekey(path_s.clone(), key(), None).expect("rekey succeeds");

        // Same credential still opens it, and the value survived the rewrite.
        let mut reopen = Options::default_ffi();
        reopen.encryption_key = Some(key());
        let db = NativeDatabase::new(Some(path_s), reopen).unwrap();
        let root = db
            .get_root_name("r".into())
            .unwrap()
            .expect("root survived");
        assert_eq!(db.read(root).unwrap(), b"secret".to_vec());
        db.close().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rekey_on_a_missing_path_is_database_file_not_found() {
        let missing = std::env::temp_dir().join("chisel-ffi-definitely-absent.db");
        assert!(matches!(
            rekey(
                missing.to_string_lossy().into(),
                ChiselKey::Passphrase { phrase: "x".into() },
                None
            ),
            Err(ChiselError::DatabaseFileNotFound { .. })
        ));
    }
}
