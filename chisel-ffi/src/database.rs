// database.rs — the exported UniFFI object. Wraps chisel::Chisel in
// Mutex<Option<Chisel>> so lifecycle (open/close), transaction control, and
// CRUD can all be exposed as &self methods to Swift despite the engine's
// close(self) taking ownership. Every &self method funnels through
// `with_db`, the single lock-and-dispatch point — adding a new mutating or
// reading method means adding one arm here, not re-deriving the locking
// discipline.
//
// Mirrors python/src/db.rs's PyChisel in shape (same Mutex<Option<..>>,
// same Closed-vs-Poisoned distinction, same lock-poisoning recovery via
// `into_inner()` — the engine reports fatal conditions as
// `ChiselError::Poisoned` rather than panicking while the lock is held, so a
// genuinely poisoned std Mutex would only come from a bug external to this
// crate; degrading to the ordinary poisoned-engine path beats permanently
// bricking the handle, including `close()` itself). Diverges from Python on
// two points: (1) `close()` is one-shot here — a second call is
// `ChiselError::Closed` rather than Python's silent no-op, since the Swift
// design spec calls for a stricter lifecycle; (2) begin/commit/rollback live
// on this object rather than a separate transaction handle (folded in from
// the plan's original Task 6 split — see the controller adjustment note on
// Task 5) because allocate/update/delete require an active transaction with
// no auto-begin (`chisel::Chisel::allocate` returns `NoActiveTransaction`
// otherwise; see src/lib.rs).

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::{ChiselError, FfiResult};
use crate::types::{
    ChiselKey, Counters, DefragOptions, DefragStats, DrainInsertion, Options, Stats,
    TagDropProgress,
};

#[derive(uniffi::Object)]
pub struct NativeDatabase {
    inner: Mutex<Option<chisel::Chisel>>,
}

impl NativeDatabase {
    /// Lock, reject if closed, run `f` with `&mut` engine. The one place
    /// every exported method — mutating or reading — takes the lock, so the
    /// Closed handling and lock-poisoning recovery live in exactly one spot.
    fn with_db<R>(
        &self,
        f: impl FnOnce(&mut chisel::Chisel) -> Result<R, chisel::ChiselError>,
    ) -> FfiResult<R> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let db = guard.as_mut().ok_or_else(closed_err)?;
        f(db).map_err(Into::into)
    }
}

#[uniffi::export]
impl NativeDatabase {
    /// Open (or create) a file-backed database. `path == None` opens in-memory.
    #[uniffi::constructor]
    pub fn new(path: Option<String>, options: Options) -> FfiResult<Arc<Self>> {
        let opts = options.into_engine()?;
        let engine = match path {
            Some(p) => chisel::Chisel::open(Path::new(&p), opts),
            None => chisel::Chisel::open_in_memory_with_options(opts),
        }
        .map_err(ChiselError::from)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Some(engine)),
        }))
    }

    /// Convenience: in-memory with explicit options. Equivalent to
    /// `new(path: nil, options: options)`.
    #[uniffi::constructor]
    pub fn new_in_memory(options: Options) -> FfiResult<Arc<Self>> {
        Self::new(None, options)
    }

    /// Flush and release the database: drops the flock and closes the file.
    /// One-shot — a second call returns `Closed` rather than silently
    /// no-op'ing (see module doc comment for why this diverges from Python).
    pub fn close(&self) -> FfiResult<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let db = guard.take().ok_or_else(closed_err)?;
        db.close().map_err(Into::into)
    }

    /// True if the handle can no longer do work: the engine reports
    /// `is_poisoned()`, or the handle is already closed. Never panics, even
    /// if the underlying std Mutex is itself poisoned.
    pub fn is_poisoned(&self) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(chisel::Chisel::is_poisoned)
            .unwrap_or(true)
    }

    /// Begin a new transaction. Every mutating CRUD method below requires
    /// one open; there is no auto-begin.
    pub fn begin(&self) -> FfiResult<()> {
        self.with_db(chisel::Chisel::begin)
    }

    /// Commit the active transaction (three fsyncs; see `Chisel::commit`).
    pub fn commit(&self) -> FfiResult<()> {
        self.with_db(chisel::Chisel::commit)
    }

    /// Abort the active transaction. Shadow paging makes this free: pages
    /// written since `begin` are simply never linked from a superblock.
    pub fn rollback(&self) -> FfiResult<()> {
        self.with_db(chisel::Chisel::rollback)
    }

    /// Mark `name` as a rollback target within the active transaction.
    pub fn savepoint(&self, name: String) -> FfiResult<()> {
        self.with_db(|db| db.savepoint(&name))
    }

    /// Undo everything since `savepoint(name)` without ending the transaction.
    pub fn rollback_to(&self, name: String) -> FfiResult<()> {
        self.with_db(|db| db.rollback_to(&name))
    }

    /// Discard `name` as a rollback target; the transaction keeps its work.
    pub fn release(&self, name: String) -> FfiResult<()> {
        self.with_db(|db| db.release(&name))
    }

    pub fn allocate(&self, value: Vec<u8>) -> FfiResult<chisel::Handle> {
        self.with_db(|db| db.allocate(&value))
    }

    pub fn read(&self, handle: chisel::Handle) -> FfiResult<Vec<u8>> {
        self.with_db(|db| db.read(handle))
    }

    pub fn update(&self, handle: chisel::Handle, value: Vec<u8>) -> FfiResult<()> {
        self.with_db(|db| db.update(handle, &value))
    }

    pub fn delete(&self, handle: chisel::Handle) -> FfiResult<()> {
        self.with_db(|db| db.delete(handle))
    }

    pub fn delete_many(&self, handles: Vec<chisel::Handle>) -> FfiResult<()> {
        self.with_db(|db| db.delete_many(&handles))
    }

    pub fn allocate_tagged(&self, value: Vec<u8>, tag: chisel::Tag) -> FfiResult<chisel::Handle> {
        self.with_db(|db| db.allocate_tagged(&value, tag))
    }

    pub fn tag(&self, handle: chisel::Handle) -> FfiResult<Option<chisel::Tag>> {
        self.with_db(|db| db.tag(handle))
    }

    pub fn handles_with_tag(&self, tag: chisel::Tag) -> FfiResult<Vec<chisel::Handle>> {
        self.with_db(|db| db.handles_with_tag(tag))
    }

    pub fn delete_tagged(&self, handle: chisel::Handle, tag: chisel::Tag) -> FfiResult<()> {
        self.with_db(|db| db.delete_tagged(handle, tag))
    }

    /// `max` is `u64` at the FFI boundary (usize never crosses it); cast down
    /// for the engine call. `.map(TagDropProgress::from)` runs on the Ok path
    /// only, converting the engine's progress struct to the FFI record.
    pub fn delete_with_tag(&self, tag: chisel::Tag, max: u64) -> FfiResult<TagDropProgress> {
        self.with_db(|db| db.delete_with_tag(tag, max as usize))
            .map(TagDropProgress::from)
    }

    pub fn client_byte(&self, handle: chisel::Handle) -> FfiResult<u8> {
        self.with_db(|db| db.client_byte(handle))
    }

    pub fn set_client_byte(&self, handle: chisel::Handle, byte: u8) -> FfiResult<()> {
        self.with_db(|db| db.set_client_byte(handle, byte))
    }

    pub fn set_root_name(&self, name: String, handle: chisel::Handle) -> FfiResult<()> {
        self.with_db(|db| db.set_root_name(&name, handle))
    }

    pub fn get_root_name(&self, name: String) -> FfiResult<Option<chisel::Handle>> {
        self.with_db(|db| db.get_root_name(&name))
    }

    pub fn clear_root_name(&self, name: String) -> FfiResult<()> {
        self.with_db(|db| db.clear_root_name(&name))
    }

    pub fn handles(&self) -> FfiResult<Vec<chisel::Handle>> {
        self.with_db(|db| db.handles())
    }

    /// Handle count, page count, and spillway usage, scanning the current
    /// handle table. See `chisel::Chisel::stats` for the O(live handles) cost
    /// vs. `file_size_bytes` below.
    pub fn stats(&self) -> FfiResult<Stats> {
        self.with_db(|db| db.stats()).map(Stats::from)
    }

    /// Cumulative cache/fsync activity counters since the most recent open.
    pub fn counters(&self) -> FfiResult<Counters> {
        self.with_db(|db| db.counters()).map(Counters::from)
    }

    /// Page-aligned on-disk size without the handle-table scan `stats` does.
    pub fn file_size_bytes(&self) -> FfiResult<u64> {
        self.with_db(|db| db.file_size_bytes())
    }

    /// Run a defragmentation pass within the caller's active transaction
    /// (does not begin or commit one itself).
    pub fn defrag(&self, options: DefragOptions) -> FfiResult<DefragStats> {
        self.with_db(|db| db.defrag(options.into()))
            .map(DefragStats::from)
    }

    /// Resize the in-memory cache cap. `TransactionInProgress` if a
    /// transaction is active.
    pub fn set_cache_max_bytes(&self, bytes: u64) -> FfiResult<()> {
        self.with_db(|db| db.set_cache_max_bytes(bytes))
    }

    /// Resize the spillway cap; 0 disables the spillway.
    /// `TransactionInProgress` if a transaction is active.
    pub fn set_spillway_max_bytes(&self, bytes: u64) -> FfiResult<()> {
        self.with_db(|db| db.set_spillway_max_bytes(bytes))
    }

    /// Change the drain insertion policy applied at the next commit.
    /// `TransactionInProgress` if a transaction is active.
    pub fn set_drain_insertion(&self, policy: DrainInsertion) -> FfiResult<()> {
        self.with_db(|db| db.set_drain_insertion(policy.into()))
    }

    /// Add a second credential that unlocks this database; `existing` must
    /// already unlock it. O(1) — no page is re-encrypted.
    pub fn add_key(&self, existing: ChiselKey, new: ChiselKey) -> FfiResult<()> {
        self.with_db(|db| db.add_key(&existing.into_engine(), &new.into_engine()))
    }

    /// Replace `old` with `new` in one atomic superblock commit.
    pub fn rotate_key(&self, old: ChiselKey, new: ChiselKey) -> FfiResult<()> {
        self.with_db(|db| db.rotate_key(&old.into_engine(), &new.into_engine()))
    }

    /// Revoke `key`. Refuses to remove the only remaining credential
    /// (`LastKeySlot`).
    pub fn remove_key(&self, key: ChiselKey) -> FfiResult<()> {
        self.with_db(|db| db.remove_key(&key.into_engine()))
    }
}

// Small helper so the `Closed` construction (and its message text) is not
// duplicated across `with_db` and `close`. Phrasing mirrors
// python/src/db.rs's `closed_err()` so the two bindings read the same error
// under the same condition.
fn closed_err() -> ChiselError {
    ChiselError::Closed {
        message: "database handle has been closed".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_read_round_trip_in_memory() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"hello".to_vec()).unwrap();
        assert_eq!(db.read(h).unwrap(), b"hello".to_vec());
        db.commit().unwrap();
    }

    #[test]
    fn rollback_discards() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"hello".to_vec()).unwrap();
        db.rollback().unwrap();
        db.begin().unwrap();
        assert!(matches!(db.read(h), Err(ChiselError::InvalidHandle { .. })));
        db.rollback().unwrap();
    }

    #[test]
    fn use_after_close_is_closed_error() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.close().unwrap();
        // Closed is poisoned-ish from the caller's point of view (matches
        // the Python binding's is_poisoned/ClosedError precedent).
        assert!(db.is_poisoned());
        assert!(matches!(db.begin(), Err(ChiselError::Closed { .. })));
    }

    #[test]
    fn double_close_is_closed_error() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.close().unwrap();
        assert!(matches!(db.close(), Err(ChiselError::Closed { .. })));
    }

    #[test]
    fn tag_and_root_name_round_trip() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.begin().unwrap();
        let tag = chisel::Tag::new(7).unwrap();
        let h = db.allocate_tagged(b"v".to_vec(), tag).unwrap();
        assert_eq!(db.tag(h).unwrap(), Some(tag));
        assert_eq!(db.handles_with_tag(tag).unwrap(), vec![h]);
        db.set_client_byte(h, 42).unwrap();
        assert_eq!(db.client_byte(h).unwrap(), 42);
        db.set_root_name("root".into(), h).unwrap();
        assert_eq!(db.get_root_name("root".into()).unwrap(), Some(h));
        db.commit().unwrap();
    }

    #[test]
    fn savepoint_rollback_to() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.begin().unwrap();
        let a = db.allocate(b"a".to_vec()).unwrap();
        db.savepoint("sp".into()).unwrap();
        let _b = db.allocate(b"b".to_vec()).unwrap();
        db.rollback_to("sp".into()).unwrap();
        assert_eq!(db.read(a).unwrap(), b"a".to_vec());
        db.commit().unwrap();
    }

    #[test]
    fn stats_available() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        let s = db.stats().unwrap();
        assert_eq!(s.handle_count, 0);
        let _ = db.counters().unwrap();
    }

    // File-backed (not in-memory) since encryption round-trips through the
    // on-disk superblock's wrapped-key slots; there's no in-memory analogue
    // to reopen with a different key. Also exercises NativeDatabase::new's
    // file-backed constructor path (Some(path)), which otherwise has no
    // direct test — every other test here uses new_in_memory.
    #[test]
    fn encrypted_round_trip_wrong_key_rejected() {
        let dir = std::env::temp_dir().join(format!("chisel-ffi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("enc.db");
        let key = ChiselKey::Passphrase {
            phrase: "correct horse".into(),
        };
        let mut opts = Options::default_ffi();
        opts.encryption_key = Some(key);
        let db = NativeDatabase::new(Some(path.to_string_lossy().into()), opts).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret".to_vec()).unwrap();
        db.set_root_name("r".into(), h).unwrap();
        db.commit().unwrap();
        db.close().unwrap();

        // Wrong passphrase -> InvalidEncryptionKey.
        let mut bad = Options::default_ffi();
        bad.encryption_key = Some(ChiselKey::Passphrase {
            phrase: "wrong".into(),
        });
        assert!(matches!(
            NativeDatabase::new(Some(path.to_string_lossy().into()), bad),
            Err(ChiselError::InvalidEncryptionKey { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
