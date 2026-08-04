// ChiselEngine — Engine trait impl backed by the Chisel storage engine.
//
// Constructors:
//   ChiselEngine::open_file(path, cache_size) — file-backed.
//   ChiselEngine::open_in_memory(cache_size)  — Vec<u8>-backed; same
//                                              code path, no fsync
//                                              durability, used for
//                                              fast smoke tests.
//
// Identifier(u64) ↔ chisel handle is a 1:1 transparent mapping.
// All trait method bodies are 1-line delegations to the Chisel
// public API.

use crate::engine::{Engine, EngineResult, Identifier};
use chisel::{Chisel, ChiselCounters, Options, PAGE_SIZE};
use std::path::Path;

pub struct ChiselEngine {
    db: Chisel,
}

impl ChiselEngine {
    /// Open or create a file-backed Chisel database.
    ///
    /// `cache_size` is the page-cache budget in 8 KB pages. Converted to
    /// bytes at the Options boundary via `cache_size * PAGE_SIZE`. The
    /// spillway is enabled at the production-default scale (1024 × cache
    /// budget in bytes), matching what a real Chisel deployment would use
    /// and putting Chisel on parity with SQLite's temp-file overflow and
    /// redb's on-disk B-tree pages for large-transaction handling.
    pub fn open_file(path: &Path, cache_size: usize) -> EngineResult<Self> {
        let cache_max_bytes = cache_size as u64 * PAGE_SIZE as u64;
        // I36: Options is #[non_exhaustive] so external callers must
        // build via the chained-setter builder; drain_insertion stays
        // at the LruTail default so it's not chained here.
        let db = Chisel::open(
            path,
            Options::default()
                .cache_max_bytes(cache_max_bytes)
                .spillway_max_bytes(cache_max_bytes * 1024),
        )?;
        Ok(Self { db })
    }

    /// Open an in-memory Chisel database. Same engine, no durability;
    /// for smoke tests and any benchmark that doesn't need a real file.
    ///
    /// `cache_size` semantics match `open_file`: pages, converted to bytes
    /// at the Options boundary. The spillway is enabled at the same
    /// production-default scale as `open_file` (1024 × cache budget).
    pub fn open_in_memory(cache_size: usize) -> EngineResult<Self> {
        let cache_max_bytes = cache_size as u64 * PAGE_SIZE as u64;
        let db = Chisel::open_in_memory_with_options(
            Options::default()
                .cache_max_bytes(cache_max_bytes)
                .spillway_max_bytes(cache_max_bytes * 1024),
        )?;
        Ok(Self { db })
    }
}

impl Engine for ChiselEngine {
    fn begin(&mut self) -> EngineResult<()> {
        Ok(self.db.begin()?)
    }

    fn commit(&mut self) -> EngineResult<()> {
        Ok(self.db.commit()?)
    }

    fn rollback(&mut self) -> EngineResult<()> {
        Ok(self.db.rollback()?)
    }

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier> {
        Ok(Identifier(self.db.allocate(value)?.get()))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        Ok(self.db.read(chisel::Handle::from(id.0))?)
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        Ok(self.db.update(chisel::Handle::from(id.0), value)?)
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        Ok(self.db.delete(chisel::Handle::from(id.0))?)
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        // This used to reinterpret `&[Identifier]` as `&[chisel::Handle]`
        // through `slice::from_raw_parts`, on the strength of both types being
        // `#[repr(transparent)]` over u64. The layout guarantee is real, but
        // nothing enforced it across the crate boundary: chisel's const
        // assertions check only size and align, which a `#[repr(Rust)]
        // struct Handle(u64)` also satisfies. Deleting `#[repr(transparent)]`
        // would have left both crates building green while this line became UB.
        //
        // The transmute bought one Vec allocation per `delete_many` call,
        // against an operation that performs three fsyncs. That is far below the
        // floor this harness can measure, so the safe form costs nothing real
        // and removes an unsafe block that no compiler check was guarding.
        let handles: Vec<chisel::Handle> =
            ids.iter().map(|id| chisel::Handle::from(id.0)).collect();
        Ok(self.db.delete_many(&handles)?)
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        // I53 (ISSUES.md, 2026-05-22): use the dedicated
        // Chisel::file_size_bytes accessor that skips the handle-table
        // walk that stats() does to populate handle_count. Bench
        // harness reads this per measurement cell, so the O(live
        // handles) cost from stats() was load-bearing at 100K-handle
        // scenarios. The new accessor is O(1) — just page_count *
        // PAGE_SIZE.
        Ok(self.db.file_size_bytes()?)
    }

    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
        // Propagate poison via ?, in contrast to the previous
        // `.ok()` mapping that silently masked poison as
        // Ok(None). Audit F4 fix.
        Ok(Some(self.db.counters()?))
    }
}
