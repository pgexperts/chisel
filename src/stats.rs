// stats.rs — Maintenance layer (layer 7). A plain snapshot struct returned
// by Chisel::stats() for observability: handle count, page count, and raw
// file size. Defined as its own module so that lib.rs and the public API
// don't have to pull in transaction.rs just to expose these three numbers.
//
// This is a snapshot, not a live view — callers should not cache it across
// commits. Values reflect the state at the time stats() was called.

/// Read-only summary of database size/usage. Populated by the transaction
/// manager; no methods here because there are no invariants to enforce.
///
/// I36 (ISSUES.md, 2026-05-22): `#[non_exhaustive]` so adding a fifth
/// summary field (e.g. live-handle/total-handle ratio for retirement
/// pressure) is not a breaking change. The companion `ChiselCounters`
/// has carried the same attribute since the bench-suite series landed.
#[non_exhaustive]
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
    /// I74 (ISSUES.md, 2026-05-22): current spillway logical-bytes in
    /// flight (`PAGE_SIZE` × LIVE resident spilled pages — a page read back
    /// and respilled within a transaction is not double-counted). `None`
    /// when the spillway has never been opened — it is lazily
    /// constructed on the first overflow spill, so `None` means "no
    /// overflow has happened yet on this handle." `Some(0)` means
    /// "spillway exists but is empty (just truncated by commit /
    /// rollback)." The distinction matters for monitoring: a
    /// long-lived `None` says "this workload fits comfortably in
    /// cache," whereas `Some(0)` says "we have spilled before, might
    /// again."
    pub spillway_logical_bytes: Option<u64>,
    /// I74: the spillway's `max_bytes` cap. Same `None`-vs-`Some(0)`
    /// distinction as `spillway_logical_bytes`. Operators predict
    /// `SpillwayFull` by watching `spillway_logical_bytes / spillway_max_bytes`
    /// climb across commits.
    pub spillway_max_bytes: Option<u64>,
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
/// observability) is also supported — the counters are cheap (`Cell<u64>`
/// increment in single-writer code paths).
///
/// Fields:
/// - `cache_hits` — `PageCache::get` returned a cached page without disk I/O.
/// - `cache_misses` — `PageCache::get` had to load from disk (and validate
///   checksum). Hit rate is `hits / (hits + misses)`.
/// - `pages_allocated` — page allocations, counting BOTH file extensions
///   (`PageCache::new_page`, a new id past the high-water mark) AND freemap
///   reuses (`PageCache::claim_page`, a page id freed by a prior committed
///   transaction and handed back out). Reuse is the common case once the
///   handle table / membership index allocate COW pages through the
///   freemap-aware path, so a counter that ignored it would read ~0 for a
///   steady-state mutating workload. The actual disk write happens on the
///   next `flush()`.
/// - `fsync_calls` — `PageIo::fsync` invocations that SUCCEEDED. Three per
///   Chisel commit (pre-drain flush, data-page flush, then superblock fsync);
///   zero between commits in a normal txn. A failed fsync poisons the engine (I1 / fsyncgate) and
///   is not counted; `cache_misses` by contrast counts attempted misses
///   even when the subsequent load fails. The asymmetry is intentional —
///   see `PageIo::fsync` and I1 in ISSUES.md for the rationale.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChiselCounters {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub pages_allocated: u64,
    pub fsync_calls: u64,
}
