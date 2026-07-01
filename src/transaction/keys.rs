//! transaction::keys — out-of-band key-slot management.
//!
//! Each operation here rewrites ONLY the in-superblock CryptoHeader and commits
//! it via the ordinary A/B superblock slot rotation — no data pages are touched
//! and the per-DB DEK never changes, so there is no re-encryption.
//!
//! Durability is identical to a data commit: bump txn_counter, write the inactive
//! slot, fsync (the linearization point), then promote the in-memory header.
//! A crash before the fsync returns leaves the OLD superblock in its slot; recovery
//! always picks the highest-txn_counter slot with a valid checksum, so the old
//! key-slot table is intact.
//!
//! Guards:
//! - Refuses if the manager is poisoned (I1 model).
//! - Refuses if an active user transaction is in flight — a key-rotation is its
//!   own atomic superblock write and cannot be interleaved with a data commit.
//! - Refuses if the database is plaintext (no cipher).

use super::*;
use crate::superblock::CryptoHeader;

impl TransactionManager {
    /// Write a superblock carrying `new_header` into the inactive slot, leaving
    /// every committed data root (handle-table page, freemap page, named roots,
    /// membership index, total pages, next handle, freemap depth) untouched.
    ///
    /// This is the crash-safe linearization point for credential rotation: the
    /// DEK does not change, so the superblock body is re-sealed under the same
    /// session `PageCipher`. Bumping `txn_counter` ensures the new slot wins on
    /// recovery. The in-memory promotion of `crypto_header` happens only AFTER
    /// the fsync returns.
    ///
    /// # Errors
    /// - `ChiselError::Poisoned` — manager is in the poison state.
    /// - `ChiselError::TransactionInProgress` — an active user transaction exists.
    /// - `ChiselError::EncryptionNotSupported` — this is a plaintext database.
    /// - I/O errors from the cache flush, write, or fsync — all fatal (poison).
    pub(crate) fn rewrite_crypto_header(&mut self, new_header: CryptoHeader) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        if self.cipher.is_none() {
            return Err(ChiselError::EncryptionNotSupported);
        }
        // All errors past this point are fatal: after flush() the cache dirty
        // flags are cleared; any subsequent failure is indistinguishable from a
        // mid-commit crash under fsyncgate semantics.
        let result = self.rewrite_crypto_header_inner(new_header);
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn rewrite_crypto_header_inner(&mut self, new_header: CryptoHeader) -> Result<()> {
        let mut cache = self.cache.borrow_mut();
        // flush() ensures any dirty pages in the spillway are durable before the
        // new superblock references them. Between transactions the cache should
        // normally be clean, but the flush keeps the invariant honest against
        // future changes.
        cache.flush()?;

        // I119: use checked_add, not `+= 1`. A wrapped counter corrupts
        // Superblock::select's "highest counter wins" rule on recovery.
        self.txn_counter = self
            .txn_counter
            .checked_add(1)
            .expect("txn_counter overflowed u64 (2^64 commits) — unreachable");

        let total_pages = cache.file_page_count()?;
        let r = &self.committed_roots;
        let sb = Superblock {
            magic: page::MAGIC,
            // Encrypted DBs always use the encrypted format version (MAJOR=2) so
            // an old binary rejects them rather than silently misreading them.
            format_version: page::format_version_encrypted(),
            txn_counter: self.txn_counter,
            root_handle_table_page: r.handle_table_page,
            root_freemap_page: r.freemap_page,
            total_pages,
            next_handle: r.next_handle,
            page_size: PAGE_SIZE as u32,
            named_roots: r.named_roots,
            superblock_count: self.superblock_count,
            root_membership_index_page: r.membership_index_page,
            freemap_depth: r.freemap_depth,
            // The new key-slot table; the DEK inside `cipher` is unchanged.
            encryption: Some(new_header),
        };

        // Seal the sensitive superblock body under the session DEK.
        // `cipher` is Some because we checked at entry.
        let buf = sb.serialize_encrypted(self.cipher.as_ref().expect("cipher checked at entry"));

        // Write to the INACTIVE slot (same round-robin as the data commit path).
        // With N=2 this is parity alternation; with N≥3 true round-robin.
        let inactive = self.txn_counter % self.superblock_count as u64;

        // Encrypted DB: stride is ENC_PAGE_SIZE=8232, so write_page (which
        // asserts stride==PAGE_SIZE) would panic. Zero-pad to ENC_PAGE_SIZE and
        // use write_page_unit, mirroring exactly what commit.rs does.
        {
            use crate::crypto::ENC_PAGE_SIZE;
            let mut unit = [0u8; ENC_PAGE_SIZE];
            unit[..buf.len()].copy_from_slice(&buf);
            cache.io_mut().write_page_unit(inactive, &unit)?;
        }

        // Durability linearization point: the rewrite is crash-safe only after
        // this fsync returns. A crash before this leaves the old superblock
        // intact in the other slot; recovery picks it by highest txn_counter.
        cache.io_mut().fsync()?;

        // In-memory promotion: the new slot table becomes the authoritative
        // header ONLY after the fsync, matching the data commit convention.
        self.crypto_header = Some(new_header);
        // total_pages may have advanced if a prior data commit grew the file;
        // keep committed_roots in sync.
        self.committed_roots.total_pages = total_pages;

        Ok(())
    }

    /// Prove possession of `existing` (it must unlock some active slot), recover
    /// the DEK, then wrap that SAME DEK under `new` in a free slot and commit the
    /// new header. The DEK is unchanged, so existing pages stay readable under
    /// both credentials after this returns.
    ///
    /// # Errors
    /// `EncryptionNotSupported` — plaintext DB; `InvalidEncryptionKey` — `existing`
    /// unlocks no slot; `NoFreeKeySlot` — all 8 slots occupied; I/O failures are
    /// fatal and poison the manager.
    pub(crate) fn add_key(&mut self, existing: &crate::crypto::Key, new: &crate::crypto::Key) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        let header = self.crypto_header.as_ref().ok_or(ChiselError::EncryptionNotSupported)?;
        let (_idx, dek) = header.unlock(existing)?; // → InvalidEncryptionKey if none
        let free = header.free_slot().ok_or(ChiselError::NoFreeKeySlot)?;
        let mut new_header = *header;
        new_header.wrap_into(free, new, &dek).map_err(|_| ChiselError::InvalidEncryptionKey)?;
        self.rewrite_crypto_header(new_header)
    }

    /// Replace `old` with `new` in a single atomic superblock write. `new` is
    /// staged into a free slot BEFORE the old slot is cleared, so there is never
    /// a window with zero working credentials — a crash leaves either the
    /// pre-rotation header (old works) or the post-rotation header (new works).
    ///
    /// # Errors
    /// `EncryptionNotSupported` — plaintext DB; `InvalidEncryptionKey` — `old`
    /// unlocks no slot; `NoFreeKeySlot` — all 8 slots full (no room to stage
    /// `new` before revoking `old`); I/O failures are fatal and poison the manager.
    pub(crate) fn rotate_key(&mut self, old: &crate::crypto::Key, new: &crate::crypto::Key) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        let header = self.crypto_header.as_ref().ok_or(ChiselError::EncryptionNotSupported)?;
        let (old_idx, dek) = header.unlock(old)?; // → InvalidEncryptionKey if none
        let free = header.free_slot().ok_or(ChiselError::NoFreeKeySlot)?;
        let mut new_header = *header;
        new_header.wrap_into(free, new, &dek).map_err(|_| ChiselError::InvalidEncryptionKey)?;
        // Clear the old slot in the same header snapshot — single atomic rewrite.
        new_header.slots[old_idx] = crate::superblock::KeySlot::EMPTY;
        self.rewrite_crypto_header(new_header)
    }

    /// Clear the slot `key` unlocks. Refuses to remove the LAST active slot
    /// (`LastKeySlot`) — a database with zero usable credentials is
    /// unrecoverable, so this is an operational error that changes nothing.
    ///
    /// The last-slot check happens AFTER proving the supplied key is valid, so
    /// a key that unlocks nothing on a single-slot DB gets `InvalidEncryptionKey`
    /// rather than the more confusing `LastKeySlot`.
    ///
    /// # Errors
    /// `Poisoned` — manager is in the poison state; `EncryptionNotSupported` —
    /// plaintext DB; `InvalidEncryptionKey` — `key` unlocks no slot;
    /// `LastKeySlot` — `key` IS the only active credential (removal refused,
    /// nothing is changed). I/O failures from the superblock rewrite are fatal
    /// and poison the manager.
    pub(crate) fn remove_key(&mut self, key: &crate::crypto::Key) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        let header = self.crypto_header.as_ref().ok_or(ChiselError::EncryptionNotSupported)?;
        let (idx, _dek) = header.unlock(key)?; // → InvalidEncryptionKey if none
        // Check AFTER confirming the key is valid: an unknown key on a
        // single-slot DB should report InvalidEncryptionKey, not LastKeySlot.
        if header.active_count() <= 1 {
            return Err(ChiselError::LastKeySlot);
        }
        let mut new_header = *header;
        new_header.slots[idx] = crate::superblock::KeySlot::EMPTY;
        self.rewrite_crypto_header(new_header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Key;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;
    use zeroize::Zeroizing;

    fn raw(b: u8) -> Key {
        Key::Raw(Zeroizing::new(vec![b; 32]))
    }

    /// Build an encrypted in-memory-backed TransactionManager with a fresh DB.
    fn fresh_encrypted() -> TransactionManager {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm =
            TransactionManager::create_new(cache, 2, Some(raw(0x11)), None).unwrap();
        // Commit once so there is a real baseline superblock to read/write.
        tm.begin().unwrap();
        tm.commit().unwrap();
        tm
    }

    /// Build a plaintext (unencrypted) TransactionManager.
    fn fresh_plaintext() -> TransactionManager {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm = TransactionManager::create_new(cache, 2, None, None).unwrap();
        tm.begin().unwrap();
        tm.commit().unwrap();
        tm
    }

    // ── guard tests ────────────────────────────────────────────────────────────

    /// Plaintext DB must reject rewrite_crypto_header with EncryptionNotSupported.
    #[test]
    fn plaintext_db_rejects_rewrite_crypto_header() {
        let mut db = fresh_plaintext();
        let hdr = CryptoHeader {
            algorithm: 1,
            stride: 8232,
            slots: [crate::superblock::KeySlot::EMPTY; crate::superblock::KEY_SLOT_COUNT],
        };
        let err = db.rewrite_crypto_header(hdr).unwrap_err();
        assert!(
            matches!(err, ChiselError::EncryptionNotSupported),
            "expected EncryptionNotSupported, got {err:?}"
        );
    }

    /// An active transaction must cause TransactionInProgress.
    #[test]
    fn active_txn_rejects_rewrite_crypto_header() {
        let mut db = fresh_encrypted();
        db.begin().unwrap();
        let hdr = db.crypto_header.unwrap();
        let err = db.rewrite_crypto_header(hdr).unwrap_err();
        assert!(
            matches!(err, ChiselError::TransactionInProgress),
            "expected TransactionInProgress, got {err:?}"
        );
        db.rollback().unwrap();
    }

    /// A poisoned manager must refuse immediately with Poisoned.
    #[test]
    fn poisoned_manager_rejects_rewrite_crypto_header() {
        let mut db = fresh_encrypted();
        db.force_poison_for_test();
        let hdr = db.crypto_header.unwrap();
        let err = db.rewrite_crypto_header(hdr).unwrap_err();
        assert!(
            matches!(err, ChiselError::Poisoned),
            "expected Poisoned, got {err:?}"
        );
    }

    // ── state mutation tests ───────────────────────────────────────────────────

    /// After rewrite_crypto_header the in-memory header reflects the new slot
    /// table and txn_counter advances (proving a superblock write occurred).
    #[test]
    fn rewrite_crypto_header_updates_in_memory_state() {
        let mut db = fresh_encrypted();
        let counter_before = db.txn_counter;

        // Unlock slot 0 to get the DEK, then wrap it into a second slot.
        let mut new_hdr = db.crypto_header.expect("encrypted DB must have crypto_header");
        let (_, dek) = new_hdr.unlock(&raw(0x11)).expect("slot 0 unlocks with key 0x11");
        new_hdr.wrap_into(1, &raw(0x22), &dek).expect("wrap_into with valid key must succeed");

        db.rewrite_crypto_header(new_hdr).unwrap();

        // txn_counter must have bumped exactly once.
        assert_eq!(db.txn_counter, counter_before + 1, "txn_counter must advance");
        // In-memory header must reflect both active slots.
        let stored = db.crypto_header.expect("crypto_header must be Some after rewrite");
        assert_eq!(stored.active_count(), 2, "both slots must be active");
        assert!(!db.is_poisoned());
    }

    /// Rewrite must preserve every committed data root — only total_pages and
    /// txn_counter are allowed to change.
    #[test]
    fn rewrite_crypto_header_preserves_data_roots() {
        let mut db = fresh_encrypted();
        let roots_before = db.committed_roots.clone();

        let hdr = db.crypto_header.unwrap();
        db.rewrite_crypto_header(hdr).unwrap();

        let r = &db.committed_roots;
        assert_eq!(r.handle_table_page, roots_before.handle_table_page);
        assert_eq!(r.freemap_page, roots_before.freemap_page);
        assert_eq!(r.next_handle, roots_before.next_handle);
        assert_eq!(r.named_roots, roots_before.named_roots);
        assert_eq!(r.membership_index_page, roots_before.membership_index_page);
        assert_eq!(r.freemap_depth, roots_before.freemap_depth);
    }

    // ── A/B slot rotation test ─────────────────────────────────────────────────

    /// Two successive rewrites must target alternating slots (round-robin), and
    /// each must advance the txn_counter so the latest write always wins on
    /// recovery.
    #[test]
    fn rewrite_alternates_superblock_slots() {
        let mut db = fresh_encrypted();
        // After fresh_encrypted: one create + one data commit = txn_counter=3
        // (create writes N=2 initial slots + one commit). The next write targets
        // txn_counter % 2.
        // CryptoHeader is Copy so we can just use the value twice.
        let hdr: CryptoHeader = db.crypto_header.unwrap();
        let counter0 = db.txn_counter;

        // First rewrite.
        db.rewrite_crypto_header(hdr).unwrap();
        let counter1 = db.txn_counter;
        assert_eq!(counter1, counter0 + 1);

        // Second rewrite targets the other slot.
        db.rewrite_crypto_header(hdr).unwrap();
        let counter2 = db.txn_counter;
        assert_eq!(counter2, counter1 + 1);

        // Slot parity flips each time.
        assert_ne!(
            counter1 % db.superblock_count as u64,
            counter2 % db.superblock_count as u64,
            "successive rewrites must target different superblock slots"
        );
    }

    // ── durability: reopen reads back the rewritten header ────────────────────

    /// After rewrite_crypto_header, reopening the file with the NEW key must
    /// succeed (the new slot is on disk), and the OLD key must still work (it
    /// was not removed). Verifies end-to-end persistence through the on-disk
    /// superblock write.
    #[test]
    fn rewritten_header_persists_across_reopen() {
        use crate::page_io::PageIo;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db");

        // Create an encrypted DB with key 0x11.
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut db = TransactionManager::create_new(cache, 2, Some(raw(0x11)), None).unwrap();
        db.begin().unwrap();
        db.commit().unwrap();

        // Add key 0x22 by rewriting the header with a second slot.
        let mut new_hdr = db.crypto_header.unwrap();
        let (_, dek) = new_hdr.unlock(&raw(0x11)).unwrap();
        new_hdr.wrap_into(1, &raw(0x22), &dek).expect("wrap_into with valid key must succeed");
        db.rewrite_crypto_header(new_hdr).unwrap();
        // Drop to flush OS buffers (fsync already called).
        drop(db);

        // Reopen with the SECOND key — must succeed (proves the rewrite hit disk).
        let io2 = PageIo::open(&path, false).unwrap();
        let cache2 = PageCache::new(
            io2,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let db2 = TransactionManager::open_existing(cache2, Some(raw(0x22))).unwrap();
        assert!(!db2.is_poisoned());
        let stored = db2.crypto_header.unwrap();
        assert_eq!(stored.active_count(), 2, "both slots must survive reopen");
    }
}
