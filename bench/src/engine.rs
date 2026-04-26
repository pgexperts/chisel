// Engine trait — uniform façade over chisel, redb, and sqlite.
//
// API mapping policy (per spec §2.3, "handle-as-natural-identifier"):
// each engine returns its native identifier on insert, and we use that
// identifier for subsequent reads/updates/deletes. Chisel's handle,
// redb's caller-generated monotonic key, and SQLite's rowid are all
// valid `Identifier(u64)` values; the trait does not synthesize an
// external key layer.
//
// Read takes `&self`; mutating methods take `&mut self`. This matches
// Chisel's post-F3 shape and fits redb / SQLite naturally.

use chisel::stats::ChiselCounters;
use std::error::Error;

/// Opaque identifier for a value stored in an engine.
///
/// Maps to the native identifier each engine returns on insert:
/// Chisel handle, redb caller-generated key, or SQLite rowid. The
/// wrapper exists so the harness operates on a uniform `u64`-shaped
/// identifier across engines without leaking engine-specific types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identifier(pub u64);

/// Trait-wide error type. Each engine impl boxes its native error
/// into this — `ChiselError`, `redb::Error`, `rusqlite::Error` all
/// implement `std::error::Error` and convert via the standard
/// `Box<dyn Error>` blanket `From` impl.
///
/// `Send + Sync` is included so a `Box<dyn Engine>` can be moved
/// across thread boundaries even though the engines themselves are
/// single-threaded — a future Criterion configuration may want to
/// drop this constraint or retain it; including it now is no
/// runtime cost and keeps options open.
pub type EngineResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Uniform façade over a transactional storage engine.
///
/// The trait excludes engine construction (each impl has its own
/// `new` / `open` constructors with engine-specific options), and
/// excludes durability-mode configuration (PR 3 will add that as
/// either constructor parameters or builder methods on each impl).
///
/// Method ordering: transaction control first, then the five CRUD
/// operations (4 mutating + 1 read), then introspection.
pub trait Engine {
    fn begin(&mut self) -> EngineResult<()>;
    fn commit(&mut self) -> EngineResult<()>;
    fn rollback(&mut self) -> EngineResult<()>;

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier>;
    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>>;
    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()>;
    fn delete(&mut self, id: Identifier) -> EngineResult<()>;
    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()>;

    /// Current size of the engine's backing file in bytes. For
    /// in-memory backings, returns the size of the in-memory
    /// representation. The runner reports deltas of this for the
    /// `file_size_delta` column of the diagnostic table.
    fn file_size_bytes(&self) -> EngineResult<u64>;

    /// Engine-internal counters. Returns `Some` for `ChiselEngine`
    /// (where the counters live as `Chisel::counters()`) and `None`
    /// for engines without instrumentation. The runner surfaces
    /// these as Chisel-only sub-columns in the output.
    fn internal_counters(&self) -> Option<ChiselCounters>;
}
