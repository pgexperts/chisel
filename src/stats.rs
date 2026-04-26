// stats.rs — Maintenance layer (layer 7). A plain snapshot struct returned
// by Chisel::stats() for observability: handle count, page count, and raw
// file size. Defined as its own module so that lib.rs and the public API
// don't have to pull in transaction.rs just to expose these three numbers.
//
// This is a snapshot, not a live view — callers should not cache it across
// commits. Values reflect the state at the time stats() was called.

/// Read-only summary of database size/usage. Populated by the transaction
/// manager; no methods here because there are no invariants to enforce.
#[derive(Debug, Clone)]
pub struct Stats {
    /// Number of live handles (u64 ids currently mapped in the handle table).
    pub handle_count: u64,
    /// Total allocated pages in the file, matching Superblock.total_pages.
    pub total_pages: u64,
    /// Raw size of the database file on disk. May exceed
    /// `total_pages * PAGE_SIZE` when a previous crash left orphan
    /// pages in the file tail — the last-durable superblock's
    /// `total_pages` is authoritative, anything beyond it is dead
    /// weight that the next allocation will overwrite (see I4).
    /// Chisel is single-writer, so there is no concurrent commit
    /// that could cause a transient divergence.
    pub file_size_bytes: u64,
}

/// Cumulative engine-activity counters since `open()`.
///
/// Snapshot semantics: `Chisel::counters()` returns a value-type copy. The
/// returned struct does NOT update as the engine continues to do work — read
/// it again to observe new totals. Counters are cumulative from open; they
/// reset implicitly on `close()` + reopen because the underlying `PageCache`
/// and `PageIo` are reconstructed.
///
/// Intended use: the bench harness reads `counters()` before and after each
/// measurement, reports the delta. General-purpose introspection (debugging,
/// observability) is also supported — the counters are cheap (Cell<u64>
/// increment in single-writer code paths).
///
/// Fields:
/// - `cache_hits` — `PageCache::get` returned a cached page without disk I/O.
/// - `cache_misses` — `PageCache::get` had to load from disk (and validate
///   checksum). Hit rate is `hits / (hits + misses)`.
/// - `pages_allocated` — `PageCache::new_page` invocations. Each is one new
///   page id past the prior high-water mark; the actual disk write happens
///   on the next `flush()`.
/// - `fsync_calls` — `PageIo::fsync` invocations. Two per Chisel commit
///   (data pages, then superblock); zero between commits in a normal txn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChiselCounters {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub pages_allocated: u64,
    pub fsync_calls: u64,
}
