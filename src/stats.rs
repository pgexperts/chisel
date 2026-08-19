// stats.rs — Foundation layer (layer 1). Plain snapshot structs returned by
// Chisel::stats() and Chisel::counters() for observability: handle count,
// page count, raw file size, and the cumulative engine-activity counters.
// Defined as its own module so that lib.rs and the public API don't have to
// pull in transaction.rs just to expose these numbers.
//
// Layer 1 despite reading like a top-of-stack observability concern: this
// module has no `use` statements at all, and layers are assigned by
// dependency depth rather than conceptual altitude (ARCHITECTURE.md, "Layer
// model"). It was filed at layer 7 until issue #161, which made every
// consumer of `ChiselCounters` — `PageCache` at layer 3 above all — look
// like an upward reference when the real dependency runs downward. `handle.rs`
// sits at layer 1 for exactly the same reason.
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
    /// PHYSICAL page count: how many whole stride-units the file currently
    /// holds, from `PageIo::page_count`. This is NOT `Superblock.total_pages`,
    /// which is the last-durable *logical* count. The two diverge when a
    /// previous crash left orphan pages in the file tail: those pages are
    /// counted here but are dead weight the next allocation will overwrite,
    /// and the superblock's figure remains the authoritative one for what the
    /// database actually contains.
    pub total_pages: u64,
    /// Raw size of the database file on disk, as `total_pages × stride` —
    /// `stride` being `PAGE_SIZE` for a plaintext database and
    /// `ENC_PAGE_SIZE` (8232) for an encrypted one, whose pages each carry a
    /// 24-byte nonce and a 16-byte tag on top of their 8192 plaintext bytes.
    ///
    /// Since both fields come from the same physical page count, this is
    /// exactly `total_pages × stride` and never diverges from it. It is a
    /// page-aligned figure rather than a `stat(2)` call, so it will not
    /// reflect a trailing partial page mid-extend.
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
