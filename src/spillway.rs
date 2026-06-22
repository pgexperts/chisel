// spillway.rs — sidecar overflow file for oversized dirty sets.
//
// Architecture: layer 3-adjacent — owned by PageCache, invisible to all
// modules above. Holds dirty pages that the in-cache LRU has been
// forced to spill because the cache is full of dirty pages and a new
// allocation would push it past its strict cap.
//
// Lifecycle (spec 2026-05-03-chisel-spillway-design.md, "Lifecycle"):
//   open       file is created (or reused) and truncated to zero. Any
//              pre-existing content is garbage from a crashed prior
//              process and unconditionally discarded.
//   spill      page_id allocates a slot (or overwrites its existing
//              one), bytes + per-slot checksum are written.
//   rehydrate  slot is read, checksum verified, bytes returned.
//   truncate   file shrunk to zero, resident-set index cleared. Called
//              at commit, rollback, and defrag.
//
// Slot layout (PAGE_SIZE + 16 bytes):
//   u64  page_id     (the main-file page id this slot shadows)
//   u64  checksum    (XXH3 over (page_id || page_bytes))
//   [u8] page bytes  (PAGE_SIZE = 8192 bytes)
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
use crate::page::PAGE_SIZE;

/// Per-slot header: u64 page_id + u64 XXH3 checksum.
pub const SLOT_HEADER_SIZE: usize = 16;
/// Total bytes a slot occupies on disk (header + page).
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
    /// Monotonic on-disk WRITE CURSOR — the next free slot offset. Bumped by
    /// every new spill; reused on re-spill of an already-resident page id (no
    /// bump); reset to 0 on truncate. NOT the capacity accounting basis: it
    /// never decrements on `forget`, so a forget/respill cycle climbs it past
    /// the live set. Capacity (`logical_bytes` / `SpillwayFull`) is charged
    /// against `slots.len()` instead; the cursor's tail garbage past the live
    /// set is reclaimed by `truncate`.
    next_slot_index: u64,
    /// Strict upper bound on the LIVE resident set's logical size in bytes,
    /// excluding per-slot headers (`slots.len() * PAGE_SIZE`). Captured at
    /// construction; runtime-mutable via PageCache::set_spillway_max_bytes. The
    /// physical backing file may transiently exceed this by the unforgotten
    /// write-cursor tail, which `truncate` reclaims at commit/rollback.
    max_bytes: u64,
}

impl Spillway {
    /// Open (or create + truncate) a file-backed spillway alongside the
    /// main database. The path is `<db_path>.spillway`. Any pre-existing
    /// content is discarded — no superblock can possibly point at
    /// spillway bytes, so this is always safe.
    pub fn open_file(db_path: &Path, max_bytes: u64) -> Result<Spillway> {
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(ChiselError::IoError)?;
        Ok(Spillway {
            backing: Backing::File { file },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
        })
    }

    /// Open a memory-backed spillway. Used by `Chisel::open_in_memory`.
    /// Drops on close like the rest of memory mode.
    pub fn open_memory(max_bytes: u64) -> Spillway {
        Spillway {
            backing: Backing::Memory { bytes: Vec::new() },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
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
    /// cursor every time but the live set may stay small, so the cursor would
    /// over-report. This is the figure `SpillwayFull` is judged against, so the
    /// two must agree (see `spill`). The physical backing file can be larger
    /// than this — the unforgotten tail is garbage reclaimed by `truncate`.
    ///
    /// I74 (ISSUES.md, 2026-05-22): exposed via `Chisel::stats` /
    /// `Stats::spillway_logical_bytes` so operators can monitor spillway
    /// capacity use and predict `SpillwayFull` before it fires.
    pub fn logical_bytes(&self) -> u64 {
        self.slots.len() as u64 * PAGE_SIZE as u64
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

    /// Write `page_bytes` to this spillway, keyed by `page_id`. If the
    /// page is already resident, overwrites its existing slot in place
    /// (no slot-count growth, no max_bytes check). Otherwise allocates
    /// a new slot at `next_slot_index` — but first checks that the
    /// post-write LIVE size stays within `max_bytes`.
    pub fn spill(&mut self, page_id: u64, page_bytes: &[u8; PAGE_SIZE]) -> Result<()> {
        let slot_index = if let Some(&existing) = self.slots.get(&page_id) {
            existing
        } else {
            // Adding a new live page push the LIVE resident set past the cap?
            // Charge the cap against `slots.len()` (live residency), not the
            // monotonic write cursor: a forget/respill cycle climbs the cursor
            // without growing the live set, so a cursor-based cap would trip
            // spuriously. `next_slot_index` still advances (no slot reuse
            // mid-transaction); its tail garbage is reclaimed by `truncate`.
            let post_write_bytes = (self.slots.len() as u64 + 1) * PAGE_SIZE as u64;
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

        write_slot(&mut self.backing, slot_index, page_id, page_bytes)?;
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
    /// This is a read-only peek: despite the `&mut self` receiver,
    /// `drain_batch` removes nothing from the resident set and drops no file
    /// content — the caller is responsible for removing drained entries
    /// (`forget()`) and shrinking the spillway (`truncate()`). The PageCache
    /// drain reads each pair, rehydrates the page, then flushes it to the main
    /// file. After all batches are processed, `truncate()` is called
    /// to shrink the spillway.
    ///
    /// Order is unspecified — HashMap iteration order is not stable.
    /// The drain doesn't need a particular order; one batch's
    /// rehydrates all flush together with later batches under a
    /// single fsync.
    pub fn drain_batch(&mut self, batch_size: usize) -> Vec<u64> {
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
    /// the bytes. Returns `ChecksumMismatch { page_id }` (fatal) on a
    /// torn write — caller poisons the transaction. Returns
    /// `InvalidPageId { page_id }` if the page is not resident
    /// (programming error in the caller, not a torn-write).
    pub fn rehydrate(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let slot_index = match self.slots.get(&page_id) {
            Some(&i) => i,
            None => return Err(ChiselError::InvalidPageId { page_id }),
        };
        let (stored_page_id, stored_checksum, page_bytes) =
            read_slot(&mut self.backing, slot_index)?;

        // Sanity check: the slot's stored page_id must match what the
        // resident-set says it should be. A mismatch implies in-memory
        // corruption (slots map drifted from disk) and is treated as
        // checksum failure.
        if stored_page_id != page_id {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        let computed = slot_checksum(page_id, &page_bytes);
        if computed != stored_checksum {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        Ok(page_bytes)
    }
}

/// Compute the per-slot checksum: XXH3 over (page_id || page_bytes).
/// Distinct from the main-file page checksum because a spilled page
/// may not yet have a stamped main-file checksum (see spec).
fn slot_checksum(page_id: u64, page_bytes: &[u8; PAGE_SIZE]) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&page_id.to_le_bytes());
    hasher.update(page_bytes);
    hasher.digest()
}

fn write_slot(
    backing: &mut Backing,
    slot_index: u64,
    page_id: u64,
    page_bytes: &[u8; PAGE_SIZE],
) -> Result<()> {
    let checksum = slot_checksum(page_id, page_bytes);
    let offset = slot_index * SLOT_SIZE as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    header[..8].copy_from_slice(&page_id.to_le_bytes());
    header[8..16].copy_from_slice(&checksum.to_le_bytes());
    match backing {
        Backing::File { file } => {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&header)?;
            file.write_all(page_bytes)?;
        }
        Backing::Memory { bytes } => {
            let needed = (offset + SLOT_SIZE as u64) as usize;
            if bytes.len() < needed {
                bytes.resize(needed, 0);
            }
            let off = offset as usize;
            bytes[off..off + SLOT_HEADER_SIZE].copy_from_slice(&header);
            bytes[off + SLOT_HEADER_SIZE..off + SLOT_SIZE].copy_from_slice(page_bytes);
        }
    }
    Ok(())
}

/// Read the (page_id, checksum, page_bytes) triple from the given slot.
/// Symmetric counterpart to write_slot — same offset arithmetic, same
/// backing dispatch. Returns IoError on short read (underlying I/O
/// failure) rather than ChecksumMismatch; callers distinguish the two.
fn read_slot(backing: &mut Backing, slot_index: u64) -> Result<(u64, u64, [u8; PAGE_SIZE])> {
    let offset = slot_index * SLOT_SIZE as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    let mut page_bytes = [0u8; PAGE_SIZE];
    match backing {
        Backing::File { file } => {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut header)?;
            file.read_exact(&mut page_bytes)?;
        }
        Backing::Memory { bytes } => {
            let off = offset as usize;
            if bytes.len() < off + SLOT_SIZE {
                return Err(ChiselError::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("spillway memory backing too short for slot {slot_index}"),
                )));
            }
            header.copy_from_slice(&bytes[off..off + SLOT_HEADER_SIZE]);
            page_bytes.copy_from_slice(&bytes[off + SLOT_HEADER_SIZE..off + SLOT_SIZE]);
        }
    }
    let stored_page_id = u64::from_le_bytes(header[..8].try_into().unwrap());
    let stored_checksum = u64::from_le_bytes(header[8..16].try_into().unwrap());
    Ok((stored_page_id, stored_checksum, page_bytes))
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

        let spw = Spillway::open_file(&db_path, 1024 * 1024).unwrap();
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
        let spw = Spillway::open_memory(1024 * 1024);
        assert!(!spw.is_resident(0));
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert_eq!(spw.max_bytes(), 1024 * 1024);
    }

    #[test]
    fn set_max_bytes_updates_cap() {
        let mut spw = Spillway::open_memory(1024);
        spw.set_max_bytes(2048);
        assert_eq!(spw.max_bytes(), 2048);
    }

    fn page(byte: u8) -> [u8; PAGE_SIZE] {
        [byte; PAGE_SIZE]
    }

    #[test]
    fn spill_inserts_new_slot() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        assert!(spw.is_resident(100));
        assert_eq!(spw.slot_count(), 1);
        assert_eq!(spw.logical_bytes(), PAGE_SIZE as u64);
    }

    #[test]
    fn re_spill_of_resident_page_reuses_slot() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(100, &page(0xBB)).unwrap(); // overwrite
        assert_eq!(spw.slot_count(), 1, "slot count must not grow on re-spill");
    }

    #[test]
    fn spill_full_returns_spillway_full_error() {
        // max_bytes accommodates exactly 2 page payloads (excluding header).
        let max_bytes = (PAGE_SIZE * 2) as u64;
        let mut spw = Spillway::open_memory(max_bytes);
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
        let mut spw = Spillway::open_memory(max_bytes);
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
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        let original = page(0xAB);
        spw.spill(100, &original).unwrap();
        let restored = spw.rehydrate(100).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn rehydrate_after_overwrite_returns_latest_bytes() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(100, &page(0xBB)).unwrap();
        let restored = spw.rehydrate(100).unwrap();
        assert_eq!(restored, page(0xBB));
    }

    #[test]
    fn rehydrate_missing_page_returns_invalid_page_id() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        let err = spw.rehydrate(999).unwrap_err();
        assert!(matches!(err, ChiselError::InvalidPageId { page_id: 999 }));
    }

    #[test]
    fn rehydrate_with_corrupted_byte_returns_checksum_mismatch() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
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
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
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
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 8);
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
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 8);
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
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        assert!(spw.is_resident(100));
        spw.forget(100);
        assert!(!spw.is_resident(100));
    }
}
