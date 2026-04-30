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
use chisel::stats::ChiselCounters;
use chisel::{Chisel, Options};
use std::path::Path;

pub struct ChiselEngine {
    db: Chisel,
}

impl ChiselEngine {
    /// Open or create a file-backed Chisel database.
    ///
    /// `cache_size` is the page-cache budget in 8 KB pages. The
    /// engine's `PageCache::new` clamps this to a minimum of 1
    /// internally, so passing 0 is safe but degenerate (you get a
    /// 1-page cache, not a no-cache mode — Chisel does not have one).
    pub fn open_file(path: &Path, cache_size: usize) -> EngineResult<Self> {
        let db = Chisel::open(
            path,
            Options {
                cache_size,
                ..Default::default()
            },
        )?;
        Ok(Self { db })
    }

    /// Open an in-memory Chisel database. Same engine, no durability;
    /// for smoke tests and any benchmark that doesn't need a real file.
    ///
    /// `cache_size` semantics match `open_file`: pages, clamped to a
    /// minimum of 1 by the engine.
    pub fn open_in_memory(cache_size: usize) -> EngineResult<Self> {
        let db = Chisel::open_in_memory_with_options(Options {
            cache_size,
            ..Default::default()
        })?;
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
        Ok(Identifier(self.db.allocate(value)?))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        Ok(self.db.read(id.0)?)
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        Ok(self.db.update(id.0, value)?)
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        Ok(self.db.delete(id.0)?)
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        // SAFETY: Identifier is #[repr(transparent)] over u64, so a
        // slice of Identifier and a slice of u64 have identical
        // layout. The borrow ends with this call; no aliasing
        // concern; no 'static lifetime escapes. Saves the per-call
        // Vec<u64> allocation that the previous safe-collect form
        // required (audit F5).
        let handles: &[u64] =
            unsafe { std::slice::from_raw_parts(ids.as_ptr() as *const u64, ids.len()) };
        Ok(self.db.delete_many(handles)?)
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        Ok(self.db.stats()?.file_size_bytes)
    }

    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
        // Propagate poison via ?, in contrast to the previous
        // `.ok()` mapping that silently masked poison as
        // Ok(None). Audit F4 fix.
        Ok(Some(self.db.counters()?))
    }
}
