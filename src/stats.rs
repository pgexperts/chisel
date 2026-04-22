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
