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
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::page::PAGE_SIZE;

/// Per-slot header: u64 page_id + u64 XXH3 checksum.
// Activated by Tasks 5 (spill) and 6 (rehydrate).
#[allow(dead_code)]
pub const SLOT_HEADER_SIZE: usize = 16;
/// Total bytes a slot occupies on disk (header + page).
// Activated by Tasks 5 (spill) and 6 (rehydrate).
#[allow(dead_code)]
pub const SLOT_SIZE: usize = SLOT_HEADER_SIZE + PAGE_SIZE;

/// Spillway backing storage: real file on disk, or in-memory bytes for
/// memory-mode databases.
// Activated by Tasks 5-7 which add spill/rehydrate/truncate.
#[allow(dead_code)]
enum Backing {
    File { file: File, path: PathBuf },
    Memory { bytes: Vec<u8> },
}

// Activated by Tasks 5-7 which add spill/rehydrate/truncate and wire
// Spillway into PageCache.
#[allow(dead_code)]
pub struct Spillway {
    backing: Backing,
    /// page_id -> slot index. Built up by `spill`; consulted by
    /// `is_resident` and `rehydrate`; cleared by `truncate`.
    slots: HashMap<u64, u64>,
    /// High-water mark for slot allocation. Bumped by every new spill;
    /// reused on re-spill of an already-resident page id (no bump).
    /// Reset to 0 on truncate.
    next_slot_index: u64,
    /// Strict upper bound on the spillway file's logical size in bytes,
    /// excluding per-slot headers. Captured at construction; runtime-
    /// mutable via PageCache::set_spillway_max_bytes.
    max_bytes: u64,
}

// All methods activated by Tasks 5-7 (spill / rehydrate / truncate) and
// by the PageCache wiring that consumes them.
#[allow(dead_code)]
impl Spillway {
    /// Open (or create + truncate) a file-backed spillway alongside the
    /// main database. The path is `<db_path>.spillway`. Any pre-existing
    /// content is discarded — no superblock can possibly point at
    /// spillway bytes, so this is always safe.
    pub fn open_file(db_path: &Path, max_bytes: u64) -> Result<Spillway> {
        let mut path = db_path.as_os_str().to_owned();
        path.push(".spillway");
        let path: PathBuf = path.into();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Spillway {
            backing: Backing::File { file, path },
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

    /// Number of slots currently allocated (residents).
    pub fn slot_count(&self) -> u64 {
        self.next_slot_index
    }

    /// Logical size in bytes (excludes per-slot headers).
    pub fn logical_bytes(&self) -> u64 {
        self.next_slot_index * PAGE_SIZE as u64
    }

    /// Strict upper bound on logical size, settable at construction or
    /// via PageCache::set_spillway_max_bytes between transactions.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Update the cap. Caller (PageCache::set_spillway_max_bytes) must
    /// already have ensured no transaction is in flight.
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn open_file_truncates_existing_content() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();
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

        // Cleanup — Spillway has no Drop; manually delete the spillway file.
        let _ = std::fs::remove_file(&spillway_path);
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
}
