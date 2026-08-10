// rekey.rs — bulk DEK rotation: re-encrypt an entire database under a fresh
// data-encryption key.
//
// Role in the system: this is the heavy sibling of the O(1) credential rotation
// in `transaction::keys`. Those operations (`add_key`/`rotate_key`/`remove_key`)
// re-wrap the SAME DEK under a different KEK — they change who can get in, and
// touch only the superblock. This one changes the key the data itself is sealed
// with, so it must rewrite every page in the file.
//
// Which one you want depends on what leaked:
//
//   * a credential (passphrase, raw key)  -> credential rotation, O(1)
//   * the DEK itself (process memory dump) -> this, O(total_pages)
//
// Layer: sits above the engine rather than inside it. It opens a normal
// `Chisel` handle to validate the key and take the exclusive flock, reads
// through that handle's `PageIo`, and writes a whole new file — so it depends
// on the public open path plus crate-internal access to the live cipher.
//
// Crash safety is copy-then-atomic-rename, and it has to be. The obvious
// in-place alternative is NOT safe under shadow paging: shadow paging protects
// writes that go to NEW pages, and this rewrites pages in place. A crash
// halfway through leaves some pages sealed under the new DEK and some under the
// old, with the surviving superblock naming whichever DEK it names — an
// unrecoverable mix. `rename(2)` is atomic within a filesystem, so the database
// path only ever names a complete file: the fully-rotated one, or the original.

use std::path::Path;

use crate::crypto::{Key, PageCipher};
use crate::error::{ChiselError, Result};
use crate::page::PAGE_SIZE;
use crate::superblock::{
    CryptoHeader, KeySlot, Superblock, ALGO_XCHACHA20POLY1305, KEY_SLOT_COUNT,
};
use crate::{Chisel, Options};

/// Suffix for the scratch file the rotated database is built in. It lives
/// beside the database so `rename` stays within one filesystem — a rename
/// across filesystems is not atomic and would defeat the whole strategy.
const TMP_SUFFIX: &str = ".rekey-tmp";

/// Re-encrypt every page of the database at `path` under a freshly generated
/// DEK, then atomically replace the original.
///
/// See [`Chisel::rekey`] for the caller-facing contract; this is its body.
pub(crate) fn rekey(
    path: &Path,
    key: &Key,
    argon2_params: Option<crate::crypto::Argon2Params>,
) -> Result<()> {
    // Open normally. This does all the validation we would otherwise duplicate
    // — file exists, superblock selects, format version and page size accepted,
    // algorithm known, key unlocks a slot — and takes the exclusive flock for
    // the duration. `create_if_missing(false)` because rotating the key of a
    // database that does not exist is a caller error, not a create.
    let mut db = Chisel::open(
        path,
        Options::default()
            .encryption_key(key.clone())
            .create_if_missing(false),
    )?;

    let old_cipher = match db.txm.cipher.clone() {
        Some(c) => c,
        // A plaintext database has no DEK to rotate. Operational: the caller
        // asked for something that does not apply, nothing is damaged.
        None => return Err(ChiselError::EncryptionNotSupported),
    };

    // Snapshot the geometry we are about to reproduce. `page_count` is in
    // stride units, and the stride is ENC_PAGE_SIZE for every encrypted file.
    let (page_count, superblock_count, stride) = {
        let mut cache = db.txm.cache.borrow_mut();
        let io = cache.io_mut();
        (io.page_count(), db.txm.superblock_count, io.stride())
    };
    debug_assert_eq!(stride, crate::crypto::ENC_PAGE_SIZE);

    // The winning superblock, which every slot of the new file will carry. Its
    // roots are the committed state; older slots' roots are history we
    // deliberately do not preserve, because their sealed bodies are under the
    // OLD DEK and re-sealing states we cannot reach buys nothing.
    let winner = current_superblock(&mut db)?;

    // Fresh DEK, and a crypto header holding exactly ONE credential: the one
    // supplied. See `Chisel::rekey`'s doc — re-wrapping the new DEK for the
    // other active slots is impossible, because each of those slots' KEK is
    // derived from a credential we were not given.
    // Cost parameters: an explicit override wins, otherwise INHERIT what the
    // slot this key just unlocked was using.
    //
    // Defaulting to `Argon2Params::default()` here would silently DOWNGRADE a
    // database whose owner had deliberately hardened its KDF cost — during the
    // operation you run because you were compromised, with no signal. The old
    // slot's parameters are right there in the header we are replacing, and
    // "keep what you had" is the only defensible default for a key rotation.
    let effective_params = match argon2_params {
        Some(p) => Some(p),
        None => inherited_argon2(&db, key),
    };

    let new_dek = crate::crypto::random_dek();
    let mut new_header = CryptoHeader {
        algorithm: ALGO_XCHACHA20POLY1305,
        stride: crate::crypto::ENC_PAGE_SIZE as u32,
        slots: [KeySlot::EMPTY; KEY_SLOT_COUNT],
    };
    new_header.wrap_into(0, key, &new_dek, effective_params)?;
    let new_cipher = PageCipher::new(new_dek);

    let tmp_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(TMP_SUFFIX);
        std::path::PathBuf::from(p)
    };

    // Build the replacement, cleaning up the scratch file on any failure so a
    // botched rotation does not leave debris that the next attempt trips over
    // (the create is O_EXCL — see `create_scratch`).
    let build = build_rotated_file(
        &mut db,
        &tmp_path,
        page_count,
        superblock_count,
        &winner,
        &new_header,
        &old_cipher,
        &new_cipher,
    );
    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // The linearization point. Before this the database path names the
    // original; after it, the rotated file. There is no in-between state a
    // reader can observe, which is the entire reason for the copy.
    std::fs::rename(&tmp_path, path).map_err(ChiselError::IoError)?;

    // A rename is durable only once the DIRECTORY entry is durable. Without
    // this, a crash right after a successful rename can resurrect the old name
    // binding on some filesystems — the file contents are safe (they were
    // fsynced) but the path could still point at the original inode.
    if let Some(dir) = path.parent() {
        // An empty parent means `path` was relative with no directory
        // component; the current directory is the right target then.
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(f) = std::fs::File::open(dir) {
            // Best-effort: some platforms refuse to fsync a directory handle.
            // The data itself is already durable, so a failure here costs
            // rename durability across a crash, not correctness of the file.
            let _ = f.sync_all();
        }
    }

    // Drop the handle only now, so the flock is held across the rename and no
    // other process can open the database mid-rotation. The handle's file
    // descriptor refers to the ORIGINAL (now unlinked) inode, which is exactly
    // why `rekey` does not hand it back: any further use would silently read
    // and write a deleted file.
    drop(db);
    Ok(())
}

/// The Argon2 cost parameters the supplied key's CURRENT slot uses, if any.
///
/// `None` when the database has no crypto header, when the key unlocks no slot
/// (unreachable — `Chisel::open` already proved it does), or when the slot is
/// an HKDF slot, whose params are zero by format definition and meaningless to
/// carry forward. In every one of those cases `wrap_into` falls back to its own
/// default, which is correct: a raw key never reads these fields at all.
fn inherited_argon2(db: &Chisel, key: &Key) -> Option<crate::crypto::Argon2Params> {
    let header = db.txm.crypto_header.as_ref()?;
    let (idx, _dek) = header.unlock(key).ok()?;
    let slot = &header.slots[idx];
    if slot.kdf_id == crate::crypto::KdfId::Argon2id as u8 {
        Some(slot.argon2)
    } else {
        None
    }
}

/// Re-read the winning superblock through the live handle.
///
/// `open_existing` already selected and decrypted it, but the decrypted result
/// lives inside the manager rather than being handed back, so read slot
/// `txn_counter % superblock_count` — the slot the last commit wrote — and
/// decrypt it with the session cipher.
fn current_superblock(db: &mut Chisel) -> Result<Superblock> {
    let txn_counter = db.txm.txn_counter;
    let n = db.txm.superblock_count as u64;
    let mut cache = db.txm.cache.borrow_mut();
    let cipher = db
        .txm
        .cipher
        .as_ref()
        .ok_or(ChiselError::EncryptionNotSupported)?;

    // Try the expected slot first, then every other, so a torn slot that
    // `select` already routed around does not defeat this.
    let mut unit = [0u8; crate::crypto::ENC_PAGE_SIZE];
    let expected = txn_counter % n;
    let order = std::iter::once(expected).chain((0..n).filter(|s| *s != expected));
    for slot in order {
        cache.io_mut().read_page_unit_into(slot, &mut unit)?;
        let mut image = [0u8; PAGE_SIZE];
        image.copy_from_slice(&unit[..PAGE_SIZE]);
        if let Some(mut sb) = Superblock::deserialize(&image) {
            if sb.txn_counter == txn_counter && sb.decrypt_body(cipher, &image).is_ok() {
                return Ok(sb);
            }
        }
    }
    // The manager opened from one of these slots moments ago, so failing here
    // means the file changed underneath a held flock.
    Err(ChiselError::CorruptSuperblock {
        defects: Vec::new(),
    })
}

/// Create the scratch file with the same hardening the database itself gets:
/// mode 0600, `O_EXCL` so we never adopt a file we did not create, and
/// `O_NOFOLLOW` so a planted symlink at the predictable scratch path cannot
/// redirect the write.
fn create_scratch(tmp_path: &Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match opts.open(tmp_path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Debris from a crashed earlier attempt, or someone squatting on a
            // predictable path. Same discipline as the spillway sidecar: remove
            // it ONLY if it is a plain file this uid owns with a single link,
            // and never unlink another user's entry. Then retry, still
            // exclusively, so a re-plant between the two syscalls loses.
            reclaim_scratch(tmp_path)?;
            opts.open(tmp_path).map_err(ChiselError::IoError)
        }
        Err(e) => Err(ChiselError::IoError(e)),
    }
}

/// Remove a pre-existing scratch entry, but only when it is plausibly our own
/// debris. Mirrors `spillway::reclaim_stale_sidecar` — same threat (a
/// predictable path in a directory another local user may write), same answer.
#[cfg(unix)]
fn reclaim_scratch(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path).map_err(ChiselError::IoError)?;
    // SAFETY: geteuid() is a pure read of process credentials; it cannot fail
    // and touches no memory we own.
    let ours =
        md.file_type().is_file() && md.nlink() == 1 && md.uid() == unsafe { libc::geteuid() };
    if !ours {
        return Err(ChiselError::IoError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "rekey scratch path is occupied by an entry this process did not create \
             (symlink, foreign owner, or extra hard link) — refusing to remove it",
        )));
    }
    std::fs::remove_file(path).map_err(ChiselError::IoError)
}

#[cfg(not(unix))]
fn reclaim_scratch(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(ChiselError::IoError)
}

/// Write the fully-rotated database into `tmp_path` and fsync it.
///
/// Page classes are handled differently, and the distinction is the crux of
/// the whole operation:
///
///   * superblock slots are NOT `PageCipher`-sealed — their sensitive body is
///     sealed by `serialize_encrypted`, and their crypto header is cleartext.
///     They are rebuilt from `winner` under the new header and new DEK.
///   * every other page IS `PageCipher`-sealed with AAD = page_id, so it is
///     opened under the old DEK and re-sealed under the new one at the same
///     page id. Keeping the page id fixed is what lets the roots stay valid:
///     no pointer in the tree changes, only the bytes under each pointer.
#[allow(clippy::too_many_arguments)]
fn build_rotated_file(
    db: &mut Chisel,
    tmp_path: &Path,
    page_count: u64,
    superblock_count: u32,
    winner: &Superblock,
    new_header: &CryptoHeader,
    old_cipher: &PageCipher,
    new_cipher: &PageCipher,
) -> Result<()> {
    use std::io::Write;

    let mut out = create_scratch(tmp_path)?;
    let mut cache = db.txm.cache.borrow_mut();

    // Superblock bank. Every slot carries the same roots; counters descend from
    // the winner's in rotation order, so the winner still wins selection and
    // the next commit's `(counter + 1) % N` target is still the slot it would
    // have been. Staggering rather than writing one counter N times keeps
    // `select`'s tie-break from having to choose between identical slots.
    let n = superblock_count as u64;
    let top = winner.txn_counter;
    for k in 0..n {
        let slot = (top + n - k) % n;
        let mut sb = winner.clone();
        sb.txn_counter = top.saturating_sub(k);
        sb.encryption = Some(*new_header);
        let image = sb.serialize_encrypted(new_cipher);
        let mut unit = [0u8; crate::crypto::ENC_PAGE_SIZE];
        unit[..image.len()].copy_from_slice(&image);
        out.seek_write_unit(slot, &unit)?;
    }

    // Data pages.
    let mut unit = [0u8; crate::crypto::ENC_PAGE_SIZE];
    for page_id in n..page_count {
        cache.io_mut().read_page_unit_into(page_id, &mut unit)?;
        // A unit that does not decrypt under the current DEK is copied through
        // verbatim rather than failing the rotation, and the distinction
        // matters more than it looks.
        //
        // Two kinds of unit legitimately do not decrypt in a HEALTHY database:
        //
        //   * never-written space — the file grew past a page id that was never
        //     filled, which POSIX zero-fills; and
        //   * crash debris — a process killed mid-commit can leave a TORN unit
        //     (a partial write of the 8232-byte blob) at a page id the winning
        //     superblock's freemap marks free. Shadow paging recovers by simply
        //     never reading those pages, which is why the database opens and
        //     works perfectly afterwards.
        //
        // Refusing to rotate on either would make this operation fail
        // deterministically on a database that is completely fine — and fail at
        // precisely the moment it exists for, since you reach for a DEK
        // rotation when something has already gone wrong. That is the worst
        // possible time to discover an availability trap.
        //
        // Copying verbatim cannot lose committed data: a page that is part of
        // the committed state MUST decrypt under the current DEK, or the
        // database could not be read at all. So anything landing here is by
        // construction not live. What it does give up is using rekey as a
        // corruption detector — a genuinely damaged LIVE page would be carried
        // across rather than reported. That is the right trade: rekey is a key
        // rotation, not an fsck, and a damaged live page already surfaces as
        // DecryptionFailed on the read path where it belongs.
        //
        // Preserving the bytes (rather than zeroing) also keeps the file
        // byte-length and page geometry identical, which the tests assert.
        match old_cipher.open(page_id, &unit) {
            Ok(plaintext) => {
                let sealed = new_cipher.seal(page_id, &plaintext);
                out.seek_write_unit(page_id, &sealed)?;
            }
            Err(_) => out.seek_write_unit(page_id, &unit)?,
        }
    }

    out.flush().map_err(ChiselError::IoError)?;
    // Durable before the rename, not after: the rename publishes this file, and
    // publishing bytes that are not yet on disk is exactly the crash window the
    // copy strategy exists to avoid.
    out.sync_all().map_err(ChiselError::IoError)?;
    Ok(())
}

/// Small helper so the two write sites above read as "put this unit at this
/// page id" rather than repeating the offset arithmetic.
trait SeekWriteUnit {
    fn seek_write_unit(
        &mut self,
        page_id: u64,
        unit: &[u8; crate::crypto::ENC_PAGE_SIZE],
    ) -> Result<()>;
}

impl SeekWriteUnit for std::fs::File {
    fn seek_write_unit(
        &mut self,
        page_id: u64,
        unit: &[u8; crate::crypto::ENC_PAGE_SIZE],
    ) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.seek(SeekFrom::Start(
            page_id * crate::crypto::ENC_PAGE_SIZE as u64,
        ))
        .map_err(ChiselError::IoError)?;
        self.write_all(unit).map_err(ChiselError::IoError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Handle;
    use tempfile::TempDir;
    use zeroize::Zeroizing;

    fn raw(b: u8) -> Key {
        Key::Raw(Zeroizing::new(vec![b; 32]))
    }

    /// Build a database with enough content to span several pages, including an
    /// overflow chain and a tagged chunk, so the rotation has real structure to
    /// preserve rather than one page of nothing.
    fn seed(path: &Path, key: &Key) -> Vec<(Handle, Vec<u8>)> {
        let mut db = Chisel::open(path, Options::default().encryption_key(key.clone())).unwrap();
        let mut out = Vec::new();
        db.begin().unwrap();
        for i in 0..40u8 {
            let v = vec![i; 300];
            out.push((db.allocate(&v).unwrap(), v));
        }
        // Larger than a page: exercises the overflow chain, whose pages are
        // sealed exactly like any other and must survive the rotation.
        let big = vec![0xC7u8; PAGE_SIZE * 3];
        out.push((db.allocate(&big).unwrap(), big));
        db.set_root_name("primary", out[0].0).unwrap();
        db.commit().unwrap();
        drop(db);
        out
    }

    fn assert_intact(path: &Path, key: &Key, expected: &[(Handle, Vec<u8>)]) {
        let db = Chisel::open(path, Options::default().encryption_key(key.clone())).unwrap();
        for (h, v) in expected {
            assert_eq!(&db.read(*h).unwrap(), v, "handle {h:?} did not survive");
        }
        assert_eq!(db.get_root_name("primary").unwrap(), Some(expected[0].0));
    }

    #[test]
    fn rekey_preserves_every_value_under_a_new_dek() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let k = raw(0x31);
        let expected = seed(&path, &k);

        // Capture the on-disk bytes of a data page so we can prove they really
        // changed — a rotation that silently no-ops would otherwise pass every
        // behavioural assertion here.
        let before = std::fs::read(&path).unwrap();

        Chisel::rekey(&path, &k, None).unwrap();

        let after = std::fs::read(&path).unwrap();
        assert_eq!(before.len(), after.len(), "page count must not change");
        assert_ne!(
            before, after,
            "every page is sealed under a new DEK, so the ciphertext must differ"
        );

        // The SAME credential still opens it: rekey rotates the data key, not
        // the credential.
        assert_intact(&path, &k, &expected);
    }

    #[test]
    fn rekey_actually_changes_the_data_key() {
        // The byte-comparison in the test above does NOT prove this, and it is
        // worth being explicit about why: `PageCipher::seal` draws a fresh
        // random nonce on every call, so re-sealing under the SAME DEK also
        // changes every byte in the file. A bug that reused `old_cipher` for
        // sealing, or wrapped the OLD DEK into the new header, would sail past
        // every other assertion here.
        //
        // So assert the property directly: after the rotation, the old cipher
        // must no longer be able to open a data page.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let k = raw(0x91);
        seed(&path, &k);

        // Recover the pre-rotation DEK the same way an attacker holding the
        // credential would: unwrap it from the on-disk key slot.
        let old_cipher = {
            let db = Chisel::open(&path, Options::default().encryption_key(k.clone())).unwrap();
            db.txm.cipher.clone().expect("encrypted fixture")
        };
        // A page that is definitely a sealed data page (past the superblock bank).
        let probe = crate::DEFAULT_SUPERBLOCK_COUNT as u64;
        let read_unit = |p: &Path, id: u64| {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(p).unwrap();
            let mut u = [0u8; crate::crypto::ENC_PAGE_SIZE];
            f.seek(SeekFrom::Start(id * crate::crypto::ENC_PAGE_SIZE as u64))
                .unwrap();
            f.read_exact(&mut u).unwrap();
            u
        };
        assert!(
            old_cipher.open(probe, &read_unit(&path, probe)).is_ok(),
            "fixture must have a decryptable data page at {probe} before the rotation"
        );

        Chisel::rekey(&path, &k, None).unwrap();

        assert!(
            old_cipher.open(probe, &read_unit(&path, probe)).is_err(),
            "the old DEK must no longer open any page — if it does, the data key \
             was not actually rotated and only the nonces changed"
        );
    }

    #[test]
    fn rekey_inherits_the_argon2_cost_of_the_slot_it_unlocks() {
        // Silently re-wrapping at Argon2Params::default() would DOWNGRADE a
        // database whose owner hardened its KDF — during the operation you run
        // because you were compromised, with no signal at all.
        use crate::Argon2Params;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let pass = || Key::Passphrase(Zeroizing::new("a passphrase".to_string()));
        // Distinct from the OWASP default (19456/2/1) in every field, and cheap
        // enough to keep the test fast.
        let hardened = Argon2Params {
            m_cost: 8192,
            t_cost: 3,
            p_cost: 2,
        };
        {
            let mut db = Chisel::open(
                &path,
                Options::default()
                    .encryption_key(pass())
                    .argon2_params(hardened),
            )
            .unwrap();
            db.begin().unwrap();
            db.allocate(b"payload").unwrap();
            db.commit().unwrap();
        }

        Chisel::rekey(&path, &pass(), None).unwrap();

        let db = Chisel::open(&path, Options::default().encryption_key(pass())).unwrap();
        let header = db.txm.crypto_header.expect("encrypted");
        let (idx, _) = header.unlock(&pass()).unwrap();
        assert_eq!(
            header.slots[idx].argon2, hardened,
            "rekey must carry the existing cost parameters forward, not reset them"
        );
    }

    #[test]
    fn rekey_survives_an_undecryptable_page() {
        // A crash mid-commit can leave a TORN unit at a page id the winning
        // superblock's freemap marks free. Shadow paging recovers by never
        // reading it, so the database is healthy — but a rekey that insisted
        // every unit decrypt would fail deterministically on that healthy
        // database, at exactly the moment the operation is needed.
        use std::io::{Seek, SeekFrom, Write};
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let k = raw(0xA5);
        let expected = seed(&path, &k);

        // Grow the file by one unit of garbage: a page id past everything the
        // committed state references, holding bytes that are neither zeros nor
        // valid ciphertext. This is the shape crash debris takes.
        let stray = {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let len = f.metadata().unwrap().len();
            let id = len / crate::crypto::ENC_PAGE_SIZE as u64;
            f.seek(SeekFrom::Start(id * crate::crypto::ENC_PAGE_SIZE as u64))
                .unwrap();
            f.write_all(&[0x5Au8; crate::crypto::ENC_PAGE_SIZE])
                .unwrap();
            f.sync_all().unwrap();
            id
        };

        Chisel::rekey(&path, &k, None).expect("crash debris must not block a rotation");

        assert_intact(&path, &k, &expected);
        // The debris is carried across verbatim, so the file geometry is
        // unchanged — nothing was dropped or zero-filled behind our back.
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len / crate::crypto::ENC_PAGE_SIZE as u64, stray + 1);
    }

    #[test]
    fn rekey_leaves_no_scratch_file_behind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let k = raw(0x32);
        seed(&path, &k);
        Chisel::rekey(&path, &k, None).unwrap();

        let scratch = dir.path().join("rk.db.rekey-tmp");
        assert!(
            !scratch.exists(),
            "the scratch file must be renamed away, not left beside the database"
        );
    }

    #[test]
    fn rekey_collapses_the_key_slot_table_to_the_supplied_credential() {
        // The documented consequence, and the one most likely to surprise: the
        // other credentials cannot be carried across, because each slot's KEK
        // is derived from a credential we were not given. Asserting it here
        // makes the doc a tested claim rather than a promise.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let first = raw(0x41);
        let second = raw(0x42);
        let expected = seed(&path, &first);
        {
            let mut db =
                Chisel::open(&path, Options::default().encryption_key(first.clone())).unwrap();
            db.add_key(&first, &second).unwrap();
        }
        // Both work before.
        assert_intact(&path, &first, &expected);
        assert_intact(&path, &second, &expected);

        Chisel::rekey(&path, &first, None).unwrap();

        assert_intact(&path, &first, &expected);
        assert!(
            Chisel::open(&path, Options::default().encryption_key(second.clone())).is_err(),
            "a credential not supplied to rekey cannot survive it"
        );
        // ...and can be added back, which is the documented remedy.
        {
            let mut db =
                Chisel::open(&path, Options::default().encryption_key(first.clone())).unwrap();
            db.add_key(&first, &second).unwrap();
        }
        assert_intact(&path, &second, &expected);
    }

    #[test]
    fn rekey_refuses_a_wrong_key_without_touching_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let k = raw(0x51);
        let expected = seed(&path, &k);
        let before = std::fs::read(&path).unwrap();

        match Chisel::rekey(&path, &raw(0x52), None) {
            Err(ChiselError::InvalidEncryptionKey) => {}
            other => panic!("expected InvalidEncryptionKey, got {other:?}"),
        }

        assert_eq!(
            before,
            std::fs::read(&path).unwrap(),
            "a refused rekey must not modify a single byte"
        );
        assert_intact(&path, &k, &expected);
    }

    #[test]
    fn rekey_refuses_a_plaintext_database() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plain.db");
        {
            let mut db = Chisel::open(&path, Options::default()).unwrap();
            db.begin().unwrap();
            db.allocate(b"cleartext").unwrap();
            db.commit().unwrap();
        }
        // The open inside rekey supplies a key to a plaintext DB, which the
        // open path itself rejects — the error the caller sees names the real
        // mismatch either way.
        match Chisel::rekey(&path, &raw(0x61), None) {
            Err(ChiselError::EncryptionNotSupported) => {}
            other => panic!("expected EncryptionNotSupported, got {other:?}"),
        }
    }

    #[test]
    fn rekey_refuses_a_missing_file() {
        let dir = TempDir::new().unwrap();
        match Chisel::rekey(&dir.path().join("nope.db"), &raw(0x71), None) {
            Err(ChiselError::FileNotFound) => {}
            other => panic!("expected FileNotFound, got {other:?}"),
        }
    }

    #[test]
    fn the_database_stays_writable_after_a_rotation() {
        // The rotated file must be a fully functional database, not just a
        // readable one: the freemap, handle table and next_handle all come
        // through the superblock body that was re-sealed under the new DEK.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rk.db");
        let k = raw(0x81);
        let expected = seed(&path, &k);

        Chisel::rekey(&path, &k, None).unwrap();

        let mut db = Chisel::open(&path, Options::default().encryption_key(k.clone())).unwrap();
        db.begin().unwrap();
        let fresh = db.allocate(b"written after the rotation").unwrap();
        db.delete(expected[1].0).unwrap();
        db.commit().unwrap();
        drop(db);

        let db = Chisel::open(&path, Options::default().encryption_key(k)).unwrap();
        assert_eq!(db.read(fresh).unwrap(), b"written after the rotation");
        assert!(
            db.read(expected[1].0).is_err(),
            "the delete must have stuck"
        );
        assert_eq!(db.read(expected[0].0).unwrap(), expected[0].1);
    }
}
