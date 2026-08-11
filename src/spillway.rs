// spillway.rs — sidecar overflow file for oversized dirty sets.
//
// Architecture: layer 3-adjacent — owned by PageCache, invisible to all
// modules above. Holds dirty pages that the in-cache LRU has been
// forced to spill because the cache is full of dirty pages and a new
// allocation would push it past its strict cap.
//
// Lifecycle (spec 2026-05-03-chisel-spillway-design.md, "Lifecycle"):
//   open       file is created fresh (O_EXCL | O_NOFOLLOW, mode 0600). Any
//              pre-existing content is garbage from a crashed prior process
//              and is discarded — but only after the entry is confirmed to be
//              a plain file this user owns. The path is derived from the
//              database path and is therefore predictable, so a symlink or a
//              foreign-owned file there is a plant, not debris, and is refused
//              (see `reclaim_stale_sidecar`).
//   spill      page_id allocates a slot (or overwrites its existing
//              one), bytes + per-slot checksum are written.
//              Writes are deliberately NOT fsynced: spillway content never
//              crosses a transaction boundary (rebuilt on demand, discarded at
//              `truncate` on commit/rollback and as crash garbage at `open`), so
//              a durability barrier would be wasted I/O. The per-slot XXH3 still
//              guards a torn write; durability is intentionally omitted.
//   rehydrate  slot is read, checksum verified, bytes returned.
//   truncate   file shrunk to zero, resident-set index cleared. Called
//              at commit, rollback, and defrag.
//
// Slot layout (payload_size + SLOT_HEADER_SIZE bytes):
//   u64  page_id     (the main-file page id this slot shadows)
//   u64  checksum    (XXH3 over (page_id || payload bytes))
//   [u8] payload     (PAGE_SIZE = 8192 plaintext, ENC_PAGE_SIZE = 8232 sealed)
//
// The spillway is crypto-agnostic: it stores whatever payload bytes it is
// handed. For plaintext DBs the payload is an 8192-byte page. For encrypted
// DBs the payload is the 8232-byte sealed blob (ct‖tag‖nonce) produced by
// PageCipher::seal before spill and consumed by PageCipher::open after
// rehydrate — seal-once semantics: the blob is stored verbatim and drain
// copies it verbatim to the main file. The per-slot XXH3 checksum is
// distinct from the AEAD tag inside the sealed blob: it catches a torn
// spillway write before the blob reaches the main file.
//
// On-disk format is little-endian (matches the main-file convention).
//
// In-memory state: `slots: HashMap<u64, u64>` maps page_id to slot
// index. The slot index is 0-based and dense; the file is sparse only
// in the sense that slots may be overwritten in place (re-spill of an
// already-resident page reuses the slot).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::error::{ChiselError, Result};
#[cfg(test)]
use crate::page::PAGE_SIZE;

/// Per-slot header: u64 page_id + u64 XXH3 checksum.
pub const SLOT_HEADER_SIZE: usize = 16;
/// Slot size for a plaintext DB, so tests can size a spillway cap in whole
/// slots. TEST-ONLY, and gated as such: there is no production caller and there
/// should not be one. `payload_size` is a runtime value (`PAGE_SIZE` plaintext,
/// `ENC_PAGE_SIZE` encrypted), and both sizing sites — the free `write_slot` and
/// `read_slot` fns — derive the slot width from their `payload_size` argument
/// rather than from a constant, precisely so a constant cannot go stale against
/// an encrypted database.
#[cfg(test)]
pub const SLOT_SIZE: usize = SLOT_HEADER_SIZE + PAGE_SIZE;

/// Spillway backing storage: real file on disk, or in-memory bytes for
/// memory-mode databases.
enum Backing {
    File { file: File },
    Memory { bytes: Vec<u8> },
}

pub struct Spillway {
    backing: Backing,
    /// page_id -> slot index. Built up by `spill`; consulted by
    /// `is_resident` and `rehydrate`; cleared by `truncate`.
    slots: HashMap<u64, u64>,
    /// Monotonic WRITE CURSOR — the next free slot index. Bumped by every
    /// new spill; reused on re-spill of an already-resident page id (no bump);
    /// reset to 0 on truncate. NOT the capacity accounting basis: it never
    /// decrements on `forget`, so a forget/respill cycle climbs it past the
    /// live set. Capacity (`logical_bytes` / `SpillwayFull`) is charged against
    /// `slots.len()` instead; the cursor's tail garbage is reclaimed by
    /// `truncate`.
    ///
    /// The cursor advances once per spill of a page that is not CURRENTLY
    /// resident. Re-spilling a page that is still in `slots` reuses its slot
    /// (the `if let Some(&existing)` arm of `spill`), but `forget` removes the
    /// key, so a rehydrate-then-respill of the SAME page id takes the `else`
    /// arm and burns a fresh slot. Backing size is therefore
    /// `next_slot_index * slot_size` and is bounded by the number of SPILL
    /// EVENTS in the transaction — not by `max_bytes`, and not by the number
    /// of distinct page ids either. A transaction whose working set exceeds
    /// the cache's `max_pages` churns the same ids through spill/rehydrate and
    /// grows this without limit. (An earlier version of this doc claimed
    /// forget/respill "reuses an existing slot"; it does not, and the
    /// distinct-page bound it derived from that was wrong in the unsafe
    /// direction — it under-predicts the peak.)
    ///
    /// For the FILE backing the cursor is the on-disk write position; for the
    /// MEMORY backing it drives `Vec<u8>` growth. `truncate` at
    /// commit/rollback reclaims all of it, so the exposure is peak file size
    /// (or peak memory) within a single large transaction.
    next_slot_index: u64,
    /// Strict upper bound on the LIVE resident set's logical size in bytes,
    /// excluding per-slot headers (`slots.len() * payload_size`). Captured at
    /// construction; runtime-mutable via PageCache::set_spillway_max_bytes. The
    /// physical backing file may transiently exceed this by the unforgotten
    /// write-cursor tail, which `truncate` reclaims at commit/rollback.
    max_bytes: u64,
    /// Bytes per payload: PAGE_SIZE for plaintext, ENC_PAGE_SIZE for encrypted.
    /// On an encrypted DB the payload IS the sealed `ct‖tag‖nonce` blob — the
    /// spillway stores ciphertext and drain copies it verbatim (seal-once). The
    /// slot is `SLOT_HEADER_SIZE + payload_size` bytes; the per-slot XXH3
    /// checksum covers the payload, catching a torn spillway write before the
    /// blob reaches the main file (distinct from the inner AEAD tag).
    payload_size: usize,
}

/// Remove a pre-existing sidecar entry, but ONLY when it is plausibly debris
/// from a crashed run of this same user — a plain file, owned by us, with
/// exactly one link. Anything else (a symlink, a file owned by someone else, a
/// hard link into a directory we do not control) is a plant at a predictable
/// path, and is refused rather than cleaned up.
///
/// The distinction matters because the two cases want opposite handling. Debris
/// is expected and must not break an open; a plant is an active attempt to
/// redirect or read uncommitted user data and must surface. `IoError` is fatal,
/// so a plant poisons the handle.
#[cfg(unix)]
fn reclaim_stale_sidecar(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // symlink_metadata, not metadata: the whole point is to inspect the entry
    // itself rather than whatever it may point at.
    let md = std::fs::symlink_metadata(path).map_err(ChiselError::IoError)?;
    // SAFETY: geteuid() is a pure read of process credentials. It cannot fail
    // and touches no memory we own.
    let ours =
        md.file_type().is_file() && md.nlink() == 1 && md.uid() == unsafe { libc::geteuid() };
    if !ours {
        return Err(ChiselError::IoError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "spillway sidecar path is occupied by an entry this process did not create \
             (symlink, foreign owner, or extra hard link) — refusing to use it",
        )));
    }
    std::fs::remove_file(path).map_err(ChiselError::IoError)
}

/// Non-unix fallback. Without `st_uid`/`st_nlink` there is nothing to
/// discriminate on, so keep the historical behaviour: treat a pre-existing
/// entry as crash debris and discard it.
#[cfg(not(unix))]
fn reclaim_stale_sidecar(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(ChiselError::IoError)
}

impl Spillway {
    /// Open a fresh file-backed spillway alongside the main database. The path
    /// is `<db_path>.spillway`.
    ///
    /// Any pre-existing entry at that path is REMOVED, not adopted: the sidecar
    /// is created exclusively (`O_EXCL | O_NOFOLLOW`, mode 0600). No superblock
    /// can point at spillway bytes, so discarding old content is always safe —
    /// but reusing the old file is not, because the path is predictable and
    /// another local user may have planted something there. A losing race for
    /// the create (someone re-planted between the unlink and the open) is a hard
    /// error that poisons the handle, which is the correct outcome for "someone
    /// is tampering with my sidecar path".
    ///
    /// `payload_size` is `PAGE_SIZE` for plaintext DBs and `ENC_PAGE_SIZE`
    /// for encrypted DBs. It determines the slot size and capacity accounting.
    pub fn open_file(db_path: &Path, max_bytes: u64, payload_size: usize) -> Result<Spillway> {
        // I65: build the spillway path as an OsString and pass it to
        // OpenOptions directly — OsString impls AsRef<Path>, so we
        // don't need a PathBuf round-trip. The path is not retained
        // anywhere (Backing::File holds only the open File handle);
        // if a future error message needs to mention the spillway
        // path, reconstruct from db_path.
        let mut path = db_path.as_os_str().to_owned();
        path.push(".spillway");
        // Force any open error to the FATAL IoError variant rather than letting
        // it flow through `From<io::Error>`, which demotes ErrorKind::NotFound to
        // the operational FileNotFound (error.rs I105). That demotion is correct
        // only for the initial DB open (a caller-supplied bad path). The spillway
        // is a SECOND file opened lazily MID-TRANSACTION under cache pressure; a
        // NotFound here (e.g. the parent directory vanished) is a degraded
        // in-flight cache state, not a "fix your path and retry" condition, so it
        // must poison rather than mislead the caller into continuing (review
        // 2026-06-22).
        // The sidecar needs sharper handling than the main database file.
        //
        // Its path is fully derived from the database path, so it is predictable
        // to anyone who can see the database. It is created lazily, mid
        // transaction, whenever cache pressure forces a spill — not at open,
        // where a caller might notice something wrong. And it used to be opened
        // `create(true).truncate(true)`, on the documented assumption that any
        // pre-existing content is garbage from a crashed prior process.
        //
        // That assumption fails against a local user who can create entries in
        // the database's directory, in TWO directions:
        //
        //   * a planted SYMLINK pointing at any file the database owner can
        //     write would be followed and the victim's file truncated to zero;
        //   * a planted REGULAR file, owned by the attacker and mode 0666, would
        //     simply be adopted — `truncate` empties it but does not change its
        //     owner or mode, and `mode(0600)` applies only when the open itself
        //     CREATES the file. Chisel would then write spilled pages, which are
        //     uncommitted user values, into a file the attacker can read.
        //
        // Closing only the first direction leaves a data-disclosure hole behind
        // a fix labelled as closing the hazard. So: unlink whatever is there,
        // then create EXCLUSIVELY. `create_new` sets O_EXCL, which makes the
        // open fail rather than adopt anything that reappears between the two
        // syscalls, so the attacker cannot win the race by re-planting — the
        // worst they achieve is a denial of service, which a local user who can
        // write to this directory already has by other means.
        //
        // Encryption does not help with either direction: the hazards are the
        // truncate and the file's ownership, not the contents.
        //
        // Crash debris is still discarded, as the lifecycle doc promises — but
        // only after `reclaim_stale_sidecar` confirms it really is debris this
        // user could have left, rather than another user's plant. That keeps the
        // documented behaviour for the case it was written for (a prior run of
        // ours died) and fails closed for the case it was not.
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                reclaim_stale_sidecar(Path::new(&path))?;
                // Exclusive again on the retry: if the planter re-created the
                // entry between the unlink and this open, we must fail rather
                // than adopt it. A local user who can win that race can already
                // deny service by other means, so a hard error is the correct
                // trade — it never adopts a file we did not create.
                opts.open(&path).map_err(ChiselError::IoError)?
            }
            Err(e) => return Err(ChiselError::IoError(e)),
        };
        Ok(Spillway {
            backing: Backing::File { file },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
            payload_size,
        })
    }

    /// Open a memory-backed spillway. Used by `Chisel::open_in_memory`.
    /// Drops on close like the rest of memory mode.
    ///
    /// `payload_size` is `PAGE_SIZE` for plaintext DBs and `ENC_PAGE_SIZE`
    /// for encrypted DBs.
    pub fn open_memory(max_bytes: u64, payload_size: usize) -> Spillway {
        Spillway {
            backing: Backing::Memory { bytes: Vec::new() },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
            payload_size,
        }
    }

    /// True if `page_id` has a slot in this spillway.
    pub fn is_resident(&self, page_id: u64) -> bool {
        self.slots.contains_key(&page_id)
    }

    /// Number of resident pages (page_ids currently mapped to a slot).
    /// On re-spill of an already-resident page, this stays unchanged
    /// (the slot is overwritten in place); on truncate, this drops to 0.
    /// Distinct from `next_slot_index` which is the high-water mark for
    /// slot allocation in the on-disk file (used internally for
    /// `logical_bytes` accounting).
    pub fn slot_count(&self) -> u64 {
        self.slots.len() as u64
    }

    /// Logical size of the LIVE resident set in bytes (excludes per-slot
    /// headers). Charged against `slots.len()`, not the monotonic write cursor
    /// `next_slot_index`: a spill-then-`forget`-then-respill cycle advances the
    /// cursor without growing the live set, so the cursor would over-report.
    /// This is the figure `SpillwayFull` is judged against, so the two must
    /// agree (see `spill`). The physical backing file can be larger than this —
    /// the unforgotten tail is garbage reclaimed by `truncate`.
    ///
    /// I74 (ISSUES.md, 2026-05-22): exposed via `Chisel::stats` /
    /// `Stats::spillway_logical_bytes` so operators can monitor spillway
    /// capacity use and predict `SpillwayFull` before it fires.
    pub fn logical_bytes(&self) -> u64 {
        self.slots.len() as u64 * self.payload_size as u64
    }

    /// Strict upper bound on logical size, settable at construction or
    /// via PageCache::set_spillway_max_bytes between transactions.
    ///
    /// I74 (ISSUES.md, 2026-05-22): exposed via `Stats::spillway_max_bytes`
    /// so operators can compute `logical_bytes / max_bytes` and predict
    /// `SpillwayFull` before it fires. The setter (`set_max_bytes`) was
    /// already production code via `PageCache::set_spillway_max_bytes`.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Update the cap. Caller (PageCache::set_spillway_max_bytes) must
    /// already have ensured no transaction is in flight.
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = bytes;
    }

    /// Write `blob` to this spillway, keyed by `page_id`. `blob` must be
    /// exactly `payload_size` bytes. If the page is already resident,
    /// overwrites its existing slot in place (no slot-count growth, no
    /// max_bytes check). Otherwise allocates a new slot at `next_slot_index`
    /// — but first checks that the post-write LIVE size stays within
    /// `max_bytes`.
    pub fn spill(&mut self, page_id: u64, blob: &[u8]) -> Result<()> {
        debug_assert_eq!(blob.len(), self.payload_size, "spill blob != payload_size");
        let slot_index = if let Some(&existing) = self.slots.get(&page_id) {
            existing
        } else {
            // Adding a new live page: would the LIVE resident set push past the cap?
            // Charge the cap against `slots.len()` (live residency), not the
            // monotonic write cursor: a forget/respill cycle climbs the cursor
            // without growing the live set, so a cursor-based cap would trip
            // spuriously. `next_slot_index` still advances (no slot reuse
            // mid-transaction); its tail garbage is reclaimed by `truncate`.
            let post_write_bytes = (self.slots.len() as u64 + 1) * self.payload_size as u64;
            if post_write_bytes > self.max_bytes {
                return Err(ChiselError::SpillwayFull {
                    limit_bytes: self.max_bytes,
                });
            }
            let new_index = self.next_slot_index;
            self.next_slot_index += 1;
            self.slots.insert(page_id, new_index);
            new_index
        };

        write_slot(
            &mut self.backing,
            slot_index,
            page_id,
            blob,
            self.payload_size,
        )?;
        Ok(())
    }

    /// Clear all slots, reset the resident-set, and shrink the backing
    /// to zero bytes. Called at every commit (after drain) and every
    /// rollback. The spillway holds no live content between
    /// transactions.
    pub fn truncate(&mut self) -> Result<()> {
        self.slots.clear();
        self.next_slot_index = 0;
        match &mut self.backing {
            Backing::File { file } => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
            }
            Backing::Memory { bytes } => {
                bytes.clear();
            }
        }
        Ok(())
    }

    /// Return up to `batch_size` page_ids currently in the resident set.
    /// This is a read-only peek: `drain_batch` removes nothing from the
    /// resident set and drops no file content — the caller is responsible
    /// for removing drained entries
    /// (`forget()`) and shrinking the spillway (`truncate()`). The PageCache
    /// drain reads each pair, rehydrates the page, then flushes it to the main
    /// file. After all batches are processed, `truncate()` is called
    /// to shrink the spillway.
    ///
    /// Order is unspecified — HashMap iteration order is not stable.
    /// The drain doesn't need a particular order; one batch's
    /// rehydrates all flush together with later batches under a
    /// single fsync.
    pub fn drain_batch(&self, batch_size: usize) -> Vec<u64> {
        let mut ids = Vec::with_capacity(batch_size.min(self.slots.len()));
        for &id in self.slots.keys().take(batch_size) {
            ids.push(id);
        }
        ids
    }

    /// Drop a single page_id from the resident-set after its bytes have
    /// been rehydrated into the cache. The slot is NOT reused for new
    /// allocations until the next `truncate` (mid-drain growth would
    /// be a re-entrancy hazard); the file's tail bytes simply become
    /// garbage and are reclaimed by `truncate`.
    pub fn forget(&mut self, page_id: u64) {
        self.slots.remove(&page_id);
    }

    /// Drop every resident page id >= `n` from the spillway. Matches
    /// `PageCache::truncate(n)` semantics: anything past the watermark
    /// is gone. Slot indices are NOT reused mid-transaction, so this
    /// just removes from the resident-set; the corresponding bytes in
    /// the file become garbage that the next `truncate()` reclaims.
    pub fn forget_above(&mut self, n: u64) {
        self.slots.retain(|&page_id, _| page_id < n);
    }

    /// Read the slot for `page_id`, verify the per-slot checksum, return
    /// the payload bytes as a `Vec<u8>` of length `payload_size`. Returns
    /// `ChecksumMismatch { page_id }` (fatal) on a torn write — caller
    /// poisons the transaction. Returns `InvalidPageId { page_id }` if the
    /// page is not resident (programming error in the caller, not a
    /// torn-write).
    pub fn rehydrate(&mut self, page_id: u64) -> Result<Vec<u8>> {
        let slot_index = match self.slots.get(&page_id) {
            Some(&i) => i,
            None => return Err(ChiselError::InvalidPageId { page_id }),
        };
        let (stored_page_id, stored_checksum, blob) =
            read_slot(&mut self.backing, slot_index, self.payload_size)?;

        // Sanity check: the slot's stored page_id must match what the
        // resident-set says it should be. A mismatch implies in-memory
        // corruption (slots map drifted from disk) and is treated as
        // checksum failure.
        if stored_page_id != page_id {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        let computed = slot_checksum(page_id, &blob);
        if computed != stored_checksum {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        Ok(blob)
    }
}

/// Compute the per-slot checksum: XXH3 over (page_id || blob).
/// Distinct from the main-file page checksum because a spilled page
/// may not yet have a stamped main-file checksum (see spec). For
/// encrypted DBs the blob is the sealed ciphertext; the checksum
/// covers the sealed bytes, guarding the spillway round-trip
/// independently of the AEAD tag inside the blob.
fn slot_checksum(page_id: u64, blob: &[u8]) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&page_id.to_le_bytes());
    hasher.update(blob);
    hasher.digest()
}

fn write_slot(
    backing: &mut Backing,
    slot_index: u64,
    page_id: u64,
    blob: &[u8],
    payload_size: usize,
) -> Result<()> {
    let slot_size = SLOT_HEADER_SIZE + payload_size;
    let checksum = slot_checksum(page_id, blob);
    let offset = slot_index * slot_size as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    header[..8].copy_from_slice(&page_id.to_le_bytes());
    header[8..16].copy_from_slice(&checksum.to_le_bytes());
    match backing {
        Backing::File { file } => {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&header)?;
            file.write_all(blob)?;
        }
        Backing::Memory { bytes } => {
            // The vec grows to accommodate `slot_index` monotonically; it is
            // never trimmed mid-transaction. See `next_slot_index` field doc
            // for the cumulative-growth worst-case in memory mode.
            let needed = (offset + slot_size as u64) as usize;
            if bytes.len() < needed {
                bytes.resize(needed, 0);
            }
            let off = offset as usize;
            bytes[off..off + SLOT_HEADER_SIZE].copy_from_slice(&header);
            bytes[off + SLOT_HEADER_SIZE..off + slot_size].copy_from_slice(blob);
        }
    }
    Ok(())
}

/// Read the (page_id, checksum, blob) triple from the given slot.
/// Symmetric counterpart to write_slot — same offset arithmetic, same
/// backing dispatch. Returns IoError on short read (underlying I/O
/// failure) rather than ChecksumMismatch; callers distinguish the two.
fn read_slot(
    backing: &mut Backing,
    slot_index: u64,
    payload_size: usize,
) -> Result<(u64, u64, Vec<u8>)> {
    let slot_size = SLOT_HEADER_SIZE + payload_size;
    let offset = slot_index * slot_size as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    let mut blob = vec![0u8; payload_size];
    match backing {
        Backing::File { file } => {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut header)?;
            file.read_exact(&mut blob)?;
        }
        Backing::Memory { bytes } => {
            let off = offset as usize;
            if bytes.len() < off + slot_size {
                return Err(ChiselError::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("spillway memory backing too short for slot {slot_index}"),
                )));
            }
            header.copy_from_slice(&bytes[off..off + SLOT_HEADER_SIZE]);
            blob.copy_from_slice(&bytes[off + SLOT_HEADER_SIZE..off + slot_size]);
        }
    }
    let stored_page_id = u64::from_le_bytes(header[..8].try_into().unwrap());
    let stored_checksum = u64::from_le_bytes(header[8..16].try_into().unwrap());
    Ok((stored_page_id, stored_checksum, blob))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_file_truncates_existing_content() {
        // Use TempDir rather than NamedTempFile so RAII cleanup also
        // covers the .spillway sidecar file even if an assertion below
        // panics. NamedTempFile only manages its own path.
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let spillway_path = {
            let mut p = db_path.as_os_str().to_owned();
            p.push(".spillway");
            PathBuf::from(p)
        };

        // Pre-populate the spillway path with garbage from a "previous
        // process" — open_file must overwrite it.
        std::fs::write(&spillway_path, b"garbage").unwrap();

        let spw = Spillway::open_file(&db_path, 1024 * 1024, PAGE_SIZE).unwrap();
        assert!(!spw.is_resident(42));
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert_eq!(spw.max_bytes(), 1024 * 1024);

        // The on-disk file was truncated by the open path.
        let on_disk = std::fs::read(&spillway_path).unwrap();
        assert_eq!(on_disk.len(), 0);

        // No manual cleanup — TempDir's Drop handles it.
    }

    #[test]
    fn open_memory_starts_empty() {
        let spw = Spillway::open_memory(1024 * 1024, PAGE_SIZE);
        assert!(!spw.is_resident(0));
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert_eq!(spw.max_bytes(), 1024 * 1024);
    }

    #[test]
    fn set_max_bytes_updates_cap() {
        let mut spw = Spillway::open_memory(1024, PAGE_SIZE);
        spw.set_max_bytes(2048);
        assert_eq!(spw.max_bytes(), 2048);
    }

    fn page(byte: u8) -> Vec<u8> {
        vec![byte; PAGE_SIZE]
    }

    #[test]
    fn spill_inserts_new_slot() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        assert!(spw.is_resident(100));
        assert_eq!(spw.slot_count(), 1);
        assert_eq!(spw.logical_bytes(), PAGE_SIZE as u64);
    }

    #[test]
    fn re_spill_of_resident_page_reuses_slot() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(100, &page(0xBB)).unwrap(); // overwrite
        assert_eq!(spw.slot_count(), 1, "slot count must not grow on re-spill");
    }

    #[test]
    fn spill_full_returns_spillway_full_error() {
        // max_bytes accommodates exactly 2 page payloads (excluding header).
        let max_bytes = (PAGE_SIZE * 2) as u64;
        let mut spw = Spillway::open_memory(max_bytes, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(101, &page(0xBB)).unwrap();
        let err = spw.spill(102, &page(0xCC)).unwrap_err();
        match err {
            ChiselError::SpillwayFull { limit_bytes } => {
                assert_eq!(limit_bytes, max_bytes);
            }
            other => panic!("expected SpillwayFull, got {other:?}"),
        }
    }

    #[test]
    fn spill_cap_and_logical_bytes_track_live_residency_not_cumulative_volume() {
        // A long transaction that reads a spilled page back (`forget`) and
        // respills a different page under pressure consumes a fresh on-disk
        // slot each cycle. The `SpillwayFull` admission test and
        // `logical_bytes()` must be charged against the LIVE resident set
        // (`slots.len()`), not the monotonic write cursor (`next_slot_index`):
        // otherwise the cap trips while live occupancy is far below max_bytes,
        // and the reported logical size over-counts. `next_slot_index` stays
        // monotonic (the file tail is reclaimed at `truncate`), so this is an
        // accounting fix only — no slot reuse, no double-free.
        let max_bytes = (PAGE_SIZE * 2) as u64; // room for 2 LIVE pages
        let mut spw = Spillway::open_memory(max_bytes, PAGE_SIZE);
        // Spill-then-forget far more than 2 distinct pages: live residency
        // never exceeds 1, so the 2-page cap is never reached.
        for id in 0..100u64 {
            spw.spill(id, &page(id as u8))
                .expect("a single live page is far below the cap; must not trip SpillwayFull");
            assert_eq!(
                spw.logical_bytes(),
                PAGE_SIZE as u64,
                "one live page must report exactly one page of logical bytes"
            );
            spw.forget(id);
            assert_eq!(
                spw.logical_bytes(),
                0,
                "forgetting the only live page must drop logical bytes to zero"
            );
        }
    }

    #[test]
    fn rehydrate_round_trips_bytes() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        let original = page(0xAB);
        spw.spill(100, &original).unwrap();
        let restored = spw.rehydrate(100).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn rehydrate_after_overwrite_returns_latest_bytes() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(100, &page(0xBB)).unwrap();
        let restored = spw.rehydrate(100).unwrap();
        assert_eq!(restored, page(0xBB));
    }

    #[test]
    fn rehydrate_missing_page_returns_invalid_page_id() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        let err = spw.rehydrate(999).unwrap_err();
        assert!(matches!(err, ChiselError::InvalidPageId { page_id: 999 }));
    }

    #[test]
    fn rehydrate_with_corrupted_byte_returns_checksum_mismatch() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        // Corrupt the page bytes directly (simulating a torn write).
        if let Backing::Memory { ref mut bytes } = spw.backing {
            // Skip the 16-byte header, flip a bit in the page bytes.
            bytes[SLOT_HEADER_SIZE] ^= 0x01;
        }
        let err = spw.rehydrate(100).unwrap_err();
        assert!(matches!(
            err,
            ChiselError::ChecksumMismatch { page_id: 100 }
        ));
    }

    #[test]
    fn truncate_clears_residents_and_resets_index() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(101, &page(0xBB)).unwrap();
        assert_eq!(spw.slot_count(), 2);

        spw.truncate().unwrap();
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert!(!spw.is_resident(100));
        assert!(!spw.is_resident(101));

        // After truncate, fresh spills allocate from index 0 again.
        spw.spill(200, &page(0xCC)).unwrap();
        assert_eq!(spw.slot_count(), 1);
    }

    #[test]
    fn drain_batch_returns_resident_ids_up_to_batch_size() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 8, PAGE_SIZE);
        for id in 100..105 {
            spw.spill(id, &page(id as u8)).unwrap();
        }
        let batch = spw.drain_batch(3);
        assert_eq!(batch.len(), 3);
        for id in &batch {
            assert!((100..105).contains(id), "unexpected id {id} in batch");
        }
    }

    #[test]
    fn forget_above_drops_high_ids_only() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 8, PAGE_SIZE);
        for id in 0..6 {
            spw.spill(id, &page(id as u8)).unwrap();
        }
        spw.forget_above(3);
        for id in 0..3 {
            assert!(spw.is_resident(id), "low id {id} should still be resident");
        }
        for id in 3..6 {
            assert!(!spw.is_resident(id), "high id {id} should be gone");
        }
    }

    #[test]
    fn forget_drops_from_resident_set() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4, PAGE_SIZE);
        spw.spill(100, &page(0xAA)).unwrap();
        assert!(spw.is_resident(100));
        spw.forget(100);
        assert!(!spw.is_resident(100));
    }

    #[test]
    fn wide_slot_round_trips_sealed_blob() {
        use crate::crypto::ENC_PAGE_SIZE;
        // payload_size = ENC_PAGE_SIZE: each slot carries an 8232-byte sealed
        // blob plus the 16-byte header. Round-trip must return the exact bytes.
        let slot = (SLOT_HEADER_SIZE + ENC_PAGE_SIZE) as u64;
        let mut spw = Spillway::open_memory(slot * 4, ENC_PAGE_SIZE);
        let mut blob = vec![0u8; ENC_PAGE_SIZE];
        blob[0] = 0xEE;
        blob[ENC_PAGE_SIZE - 1] = 0x11;
        spw.spill(7, &blob).unwrap();
        assert!(spw.is_resident(7));
        assert_eq!(spw.rehydrate(7).unwrap(), blob);
    }

    #[test]
    fn wide_slot_checksum_catches_tampered_payload() {
        use crate::crypto::ENC_PAGE_SIZE;
        let slot = (SLOT_HEADER_SIZE + ENC_PAGE_SIZE) as u64;
        let mut spw = Spillway::open_memory(slot * 4, ENC_PAGE_SIZE);
        spw.spill(7, &vec![0xAB; ENC_PAGE_SIZE]).unwrap();
        if let Backing::Memory { ref mut bytes } = spw.backing {
            bytes[SLOT_HEADER_SIZE + 5] ^= 0x01; // flip a byte in the blob
        }
        assert!(matches!(
            spw.rehydrate(7).unwrap_err(),
            ChiselError::ChecksumMismatch { page_id: 7 }
        ));
    }
}
