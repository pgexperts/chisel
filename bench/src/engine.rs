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

use chisel::ChiselCounters;
use std::error::Error;

/// Opaque identifier returned by an engine's `allocate` and consumed by
/// later `read`/`update`/`delete` calls. Each engine maps this to its
/// native form (Chisel handle, redb caller-generated key, SQLite rowid).
///
/// `#[repr(transparent)]` lets `&[Identifier]` and `&[u64]` share layout,
/// so engine impls that delegate `delete_many` to a `&[u64]`-shaped
/// inner API can avoid per-call `Vec<u64>` allocations via a documented
/// `unsafe` slice transmute.
///
/// Construction guidance: identifiers should only be obtained from
/// `Engine::allocate`. Constructing one directly (`Identifier(123)`) is
/// supported for testing but carries no semantic guarantees — engines
/// reject identifiers they didn't issue.
#[repr(transparent)]
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

/// Durability mode for engines that support relaxed-fsync configurations
/// (redb's Durability::Eventual, SQLite's synchronous=OFF). Chisel does
/// not have this dimension — its constructor doesn't accept this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilityMode {
    /// fsync per commit. redb: Durability::Immediate (its default).
    /// SQLite: synchronous=FULL with WAL journal mode.
    Strict,
    /// Relaxed fsync. redb: Durability::Eventual.
    /// SQLite: synchronous=OFF. Diagnostic-only — not durable.
    Unsafe,
}

/// Uniform façade over a transactional storage engine.
///
/// The trait excludes engine construction (each impl has its own
/// `open_*` constructors with engine-specific options including the
/// `DurabilityMode` for engines that have one). Construction is per-
/// impl because the relevant options diverge: ChiselEngine takes only
/// `cache_size`; RedbEngine and SqliteEngine additionally take
/// `DurabilityMode`.
///
/// Method ordering: transaction control first, then the five CRUD
/// operations (4 mutating + 1 read), then introspection.
pub trait Engine {
    /// Begin a new transaction. Subsequent mutations are buffered until
    /// `commit()` makes them durable.
    ///
    /// Returns `Err` if a transaction is already active or if the
    /// engine is in an error state (Chisel: poisoned; redb / SQLite:
    /// underlying I/O failure on the begin path).
    fn begin(&mut self) -> EngineResult<()>;

    /// Commit the active transaction. Makes all buffered mutations
    /// durable per the engine's current durability mode.
    ///
    /// Returns `Err` if no transaction is active, on commit-protocol
    /// I/O failure, or if the engine became poisoned mid-commit.
    fn commit(&mut self) -> EngineResult<()>;

    /// Roll back the active transaction. Discards all mutations.
    ///
    /// Returns `Err` if no transaction is active.
    fn rollback(&mut self) -> EngineResult<()>;

    /// Store a value and return a stable identifier for it. The
    /// identifier is monotonically increasing across calls within a
    /// single engine instance — Chisel handles, redb caller-generated
    /// keys, and SQLite rowids (with `INTEGER PRIMARY KEY AUTOINCREMENT`
    /// to suppress reuse) all follow this pattern.
    ///
    /// Identifier spaces do not align across different engines: the
    /// same allocation order produces different `Identifier` values
    /// from each engine's `allocate`. Cross-engine equivalence tests
    /// must track each engine's own identifier list, not assume
    /// equality across engines.
    ///
    /// Returns `Err` if no transaction is active or on engine I/O
    /// failure.
    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier>;

    /// Read a previously-allocated value by its identifier.
    ///
    /// Takes `&self` — readable inside or outside an active
    /// transaction (engines that need a separate read path open a
    /// fresh read transaction).
    ///
    /// Returns `Err` if the identifier was never allocated, was
    /// deleted, or on engine I/O failure.
    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>>;

    /// Replace the value associated with an identifier. The identifier
    /// is preserved.
    ///
    /// Returns `Err` if the identifier was never allocated, was
    /// deleted, no transaction is active, or on engine I/O failure.
    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()>;

    /// Delete a single identifier. After delete, `read(id)` returns
    /// `Err` and the identifier is permanently retired (Chisel: handle
    /// is tombstoned and never reused; redb / SQLite-with-AUTOINCREMENT:
    /// key is removed, never reused).
    ///
    /// Returns `Err` if the identifier was never allocated, was
    /// already deleted, no transaction is active, or on engine I/O
    /// failure.
    fn delete(&mut self, id: Identifier) -> EngineResult<()>;

    /// Bulk delete a slice of identifiers. Equivalent to a loop of
    /// `delete()` calls; engines may implement faster bulk paths
    /// (Chisel does not yet — see ISSUES.md I33).
    ///
    /// Returns `Err` on the first failing identifier; identifiers
    /// processed before that point remain marked for deletion in the
    /// active transaction's state.
    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()>;

    /// Current size of the engine's backing file in bytes. For
    /// in-memory backings, returns the size of the in-memory
    /// representation. For SQLite in WAL mode, includes the main
    /// database file plus `-wal` and `-shm` siblings if present.
    /// The runner reports deltas of this for the `file_size_delta`
    /// column of the diagnostic table.
    fn file_size_bytes(&self) -> EngineResult<u64>;

    /// Engine-internal counters. Returns `Ok(Some(...))` for
    /// `ChiselEngine` (where the counters live as `Chisel::counters()`)
    /// and `Ok(None)` for engines without instrumentation. Returns
    /// `Err` if the engine is poisoned (ChiselEngine surfaces
    /// `chisel::ChiselError::Poisoned` here rather than masking it
    /// as `Ok(None)`).
    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>>;

    /// Make the backing file fully self-contained on disk so a sibling
    /// `std::fs::copy` of the main file alone yields a re-openable database.
    ///
    /// Default implementation is a no-op: Chisel writes everything into
    /// the single `.db` file at commit time (shadow paging), and redb
    /// likewise. SQLite in WAL mode overrides this — committed data may
    /// live only in the `-wal` sibling between commits and explicit
    /// checkpoints; copying just the `.db` produces a file whose page
    /// metadata disagrees with the missing WAL, manifesting on reopen
    /// as "database disk image is malformed".
    ///
    /// Called at snapshot-seal points (`populate_snapshot`,
    /// `capture_aux_metrics_snapshot_restore`), NOT inside per-iteration
    /// timed regions — overrides may be O(database size).
    fn flush_for_snapshot(&mut self) -> EngineResult<()> {
        Ok(())
    }
}
