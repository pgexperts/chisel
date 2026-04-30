# Bench RedbEngine + SqliteEngine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `RedbEngine` and `SqliteEngine` as `Engine` trait impls in the `bench/` subcrate, with strict and unsafe durability modes, plus five cross-engine equivalence tests. Also lands trait extensions (per-method doc comments, `internal_counters` signature change) and `Identifier` cleanup deferred from earlier reviews.

**Architecture:** PR 3 of the bench-suite series. Two new engine impls (sibling files to `chisel_engine.rs`) wire redb 2.x and rusqlite 0.31+ to the existing `Engine` trait. A new `DurabilityMode` enum at the bench layer carries strict/unsafe configuration through engine constructors. `Identifier` becomes `#[repr(transparent)]` so ChiselEngine's `delete_many` can use a documented `unsafe` slice transmute instead of a per-call `Vec<u64>`. The `internal_counters` trait method changes shape from `Option<ChiselCounters>` to `EngineResult<Option<ChiselCounters>>` so a poisoned engine surfaces honestly.

**Tech Stack:** Rust 2021, `chisel-bench` subcrate (path-deps on `chisel`), new deps `redb = "2"` and `rusqlite = { version = "0.31", features = ["bundled"] }`.

---

## Notes for the executing engineer

- This is PR 3 of the bench-suite series. PRs 1–2 + PR-A are already on `main`. The spec for this PR is `docs/superpowers/specs/2026-04-30-chisel-bench-redb-sqlite-engines-design.md`. You don't need to read the spec — every step below contains the code/commands you need.
- Task 1 is the only "coupled" task: it lands trait signature changes alongside the ChiselEngine impl updates that satisfy them. Splitting them would leave the codebase non-compiling between commits.
- Tasks 2 and 3 each add a dependency to `bench/Cargo.toml` and a new module file. They compile cleanly on their own.
- Task 4 adds tests; nothing else.
- Task 5 is verification only.
- All work happens in `bench/`. The main `chisel` crate is not modified.
- If the executing engineer wants isolation, create a worktree before Task 1: `git worktree add .worktrees/redb-sqlite-engines -b redb-sqlite-engines`. All subsequent work happens inside that worktree.
- Convention reminder: `cargo clippy -- -D warnings` and `cargo fmt -- --check` must pass at every commit boundary. Doc-comment list-item continuations use 2-space hanging indent (`doc_overindented_list_items` clippy lint enforces).

## File Structure

| File | Touch | Owner Task | Responsibility |
|------|-------|------------|----------------|
| `bench/Cargo.toml` | Modify | 2 + 3 | Add `redb` (Task 2); add `rusqlite` (Task 3) |
| `bench/src/engine.rs` | Modify | 1 | `DurabilityMode` enum; `#[repr(transparent)]` on `Identifier`; per-method doc comments on `Engine`; `internal_counters` signature change |
| `bench/src/chisel_engine.rs` | Modify | 1 | `delete_many` uses `unsafe` transmute; `internal_counters` propagates poison via `?` |
| `bench/src/redb_engine.rs` | Create | 2 | `RedbEngine` impl |
| `bench/src/sqlite_engine.rs` | Create | 3 | `SqliteEngine` impl |
| `bench/src/lib.rs` | Modify | 1 + 2 + 3 | Add re-exports as new types appear |
| `bench/tests/equivalence.rs` | Create | 4 | Five scenarios × 3 engines = 15 named tests |

---

## Task 1: Trait extensions + ChiselEngine adjustments

This task is one logical unit because the `internal_counters` signature change ripples through `ChiselEngine`. Both files have to land together for the codebase to compile.

**Files:**
- Modify: `bench/src/engine.rs`
- Modify: `bench/src/chisel_engine.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Confirm baseline — full test suite green**

Run from the worktree root:
```bash
cargo test
cd bench && cargo test && cd ..
```
Expected: all tests pass at both levels. If anything is red, stop and escalate.

- [ ] **Step 2: Replace the contents of `bench/src/engine.rs`**

Write the file with this exact content:

```rust
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
}
```

- [ ] **Step 3: Replace the contents of `bench/src/chisel_engine.rs`**

Write the file with this exact content:

```rust
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
        let handles: &[u64] = unsafe {
            std::slice::from_raw_parts(ids.as_ptr() as *const u64, ids.len())
        };
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
```

- [ ] **Step 4: Update `bench/src/lib.rs` to re-export `DurabilityMode`**

Replace the existing re-export line with the expanded version. Find:

```rust
pub use engine::{Engine, EngineResult, Identifier};
```

Replace with:

```rust
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
```

- [ ] **Step 5: Run cargo build from `bench/`, expect clean compile**

```bash
cd bench && cargo build && cd ..
```
Expected: clean build. If it fails, the most likely cause is a missing import or a stale signature reference.

- [ ] **Step 6: Run cargo test from root + `bench/`, expect all pass**

```bash
cargo test
cd bench && cargo test && cd ..
```
Expected: all existing tests still pass — no test changes in this task, and no behavior changes user-visible. The Chisel core suite at root and the smoke test in `bench/` should be green.

- [ ] **Step 7: Run clippy and fmt at both levels, expect clean**

```bash
cargo clippy -- -D warnings
cd bench && cargo clippy --tests -- -D warnings && cd ..
cargo fmt -- --check
cd bench && cargo fmt -- --check && cd ..
```
Expected: silent at all four. Watch for `doc_overindented_list_items` on the new doc comments — list-item continuations under `///` use 2-space hanging indent.

- [ ] **Step 8: Commit**

```bash
git add bench/src/engine.rs bench/src/chisel_engine.rs bench/src/lib.rs
git commit -m "bench: trait extensions + ChiselEngine adjustments

- DurabilityMode enum at the bench layer
- #[repr(transparent)] on Identifier with construction-guidance comment
- Per-method doc comments on Engine trait
- internal_counters signature change: Option -> EngineResult<Option>
  (audit F4 — poisoned engine surfaces honestly via Err)
- ChiselEngine.delete_many uses unsafe slice transmute
  (audit F5 — saves per-call Vec<u64> allocation)
- ChiselEngine.internal_counters propagates poison via ?

PR 3 of the bench-suite series, Task 1 of 5."
```

---

## Task 2: RedbEngine

**Files:**
- Modify: `bench/Cargo.toml`
- Create: `bench/src/redb_engine.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Add `redb` to `bench/Cargo.toml`**

Open `bench/Cargo.toml`. Find the `[dependencies]` section. After the existing `chisel = { path = ".." }` line, add:

```toml
redb = "2"
```

The result should look like:

```toml
[dependencies]
chisel = { path = ".." }
redb = "2"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create `bench/src/redb_engine.rs`**

Write the file with this exact content:

```rust
// RedbEngine — Engine trait impl backed by redb.
//
// Schema: a single table mapping caller-generated monotonic u64 keys
// to byte-blob values. The harness owns the key-allocation policy
// (next_id starts from max_existing_key + 1 on open, increments
// monotonically, never reuses) so that identifier semantics match
// Chisel's "handles never reused after delete" promise — see the
// Engine::allocate doc comment.
//
// Transaction state lives on the struct as Option<WriteTransaction>.
// redb 2.x's WriteTransaction is 'static-suitable (it holds Arc to
// internal state) so storing it directly works without lifetime
// gymnastics.

use crate::engine::{DurabilityMode, Engine, EngineResult, Identifier};
use chisel::stats::ChiselCounters;
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("chisel_bench");

const PAGE_SIZE: usize = 8192;

pub struct RedbEngine {
    db: Database,
    path: PathBuf,
    next_id: u64,
    durability: DurabilityMode,
    active_tx: Option<redb::WriteTransaction>,
}

impl RedbEngine {
    /// Open or create a file-backed redb database.
    ///
    /// `cache_size_pages` matches the harness convention: pages of
    /// 8 KB. redb's API takes bytes; we multiply.
    pub fn open_file(
        path: &Path,
        cache_size_pages: usize,
        durability: DurabilityMode,
    ) -> EngineResult<Self> {
        let cache_bytes = cache_size_pages.max(1) * PAGE_SIZE;
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?;
        let next_id = recover_next_id(&db)?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
            next_id,
            durability,
            active_tx: None,
        })
    }
}

/// Find the largest existing key + 1, or 0 if the table is empty or
/// missing. Called once at open time to seed the monotonic key
/// allocator. Cost: one read transaction, one table iter-back.
fn recover_next_id(db: &Database) -> EngineResult<u64> {
    let read_tx = db.begin_read()?;
    match read_tx.open_table(TABLE) {
        Ok(table) => {
            // last() returns the largest key under redb's u64 ordering
            // (big-endian byte order, which matches numeric order).
            match table.last()? {
                Some((key, _)) => Ok(key.value() + 1),
                None => Ok(0),
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

impl Engine for RedbEngine {
    fn begin(&mut self) -> EngineResult<()> {
        if self.active_tx.is_some() {
            return Err("transaction already active".into());
        }
        let mut tx = self.db.begin_write()?;
        let durability = match self.durability {
            DurabilityMode::Strict => Durability::Immediate,
            DurabilityMode::Unsafe => Durability::Eventual,
        };
        tx.set_durability(durability);
        self.active_tx = Some(tx);
        Ok(())
    }

    fn commit(&mut self) -> EngineResult<()> {
        let tx = self
            .active_tx
            .take()
            .ok_or("no active transaction")?;
        tx.commit()?;
        Ok(())
    }

    fn rollback(&mut self) -> EngineResult<()> {
        let tx = self
            .active_tx
            .take()
            .ok_or("no active transaction")?;
        tx.abort()?;
        Ok(())
    }

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier> {
        let id = self.next_id;
        {
            let tx = self
                .active_tx
                .as_ref()
                .ok_or("no active transaction")?;
            let mut table = tx.open_table(TABLE)?;
            table.insert(id, value)?;
        }
        self.next_id += 1;
        Ok(Identifier(id))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        if let Some(tx) = &self.active_tx {
            let table = tx.open_table(TABLE)?;
            let value = table
                .get(id.0)?
                .ok_or("identifier not found")?;
            Ok(value.value().to_vec())
        } else {
            let read_tx = self.db.begin_read()?;
            let table = read_tx.open_table(TABLE)?;
            let value = table
                .get(id.0)?
                .ok_or("identifier not found")?;
            Ok(value.value().to_vec())
        }
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        let tx = self
            .active_tx
            .as_ref()
            .ok_or("no active transaction")?;
        let mut table = tx.open_table(TABLE)?;
        // redb's insert is upsert; we don't need a separate update path.
        // Verify the key exists first to match the trait's semantic
        // (update on a non-existent identifier returns Err).
        if table.get(id.0)?.is_none() {
            return Err("identifier not found".into());
        }
        table.insert(id.0, value)?;
        Ok(())
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        let tx = self
            .active_tx
            .as_ref()
            .ok_or("no active transaction")?;
        let mut table = tx.open_table(TABLE)?;
        let removed = table.remove(id.0)?;
        if removed.is_none() {
            return Err("identifier not found".into());
        }
        Ok(())
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        let tx = self
            .active_tx
            .as_ref()
            .ok_or("no active transaction")?;
        let mut table = tx.open_table(TABLE)?;
        for id in ids {
            let removed = table.remove(id.0)?;
            if removed.is_none() {
                return Err("identifier not found".into());
            }
        }
        Ok(())
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
        Ok(None)
    }
}
```

- [ ] **Step 3: Update `bench/src/lib.rs` to add the new module + re-export**

Replace the contents of `bench/src/lib.rs` with:

```rust
// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PRs 1–2 + PR-A landed the Engine trait and ChiselEngine. PR 3
// (this PR) adds RedbEngine and SqliteEngine impls. Subsequent PRs
// add the workload + runner + micro grid (PR 4), the reporter
// (PR 5), scenarios (PR 6), and CI integration (PR 7).

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
```

- [ ] **Step 4: Run cargo build from `bench/`**

```bash
cd bench && cargo build && cd ..
```
Expected: clean build. The new redb dependency downloads and compiles on first run (may take 30–90 seconds).

- [ ] **Step 5: Run cargo test from `bench/`, expect existing tests pass**

```bash
cd bench && cargo test && cd ..
```
Expected: PR 2's smoke test still passes. No new tests in this task; that's Task 4.

- [ ] **Step 6: Run clippy + fmt from `bench/`**

```bash
cd bench && cargo clippy --tests -- -D warnings && cd ..
cd bench && cargo fmt -- --check && cd ..
```
Expected: silent.

- [ ] **Step 7: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock bench/src/redb_engine.rs bench/src/lib.rs
git commit -m "bench: add RedbEngine

Single-table u64 -> &[u8] schema, caller-generated monotonic keys
recovered from max-existing-key on open, WriteTransaction stored on
the struct between begin/commit. DurabilityMode::Strict maps to
redb::Durability::Immediate (its default); Unsafe maps to Eventual.

PR 3 of the bench-suite series, Task 2 of 5."
```

---

## Task 3: SqliteEngine

**Files:**
- Modify: `bench/Cargo.toml`
- Create: `bench/src/sqlite_engine.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Add `rusqlite` to `bench/Cargo.toml`**

Open `bench/Cargo.toml`. Find the `[dependencies]` section. After the `redb` line added in Task 2, add:

```toml
rusqlite = { version = "0.31", features = ["bundled"] }
```

The result should look like:

```toml
[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }

[dev-dependencies]
tempfile = "3"
```

The `bundled` feature compiles SQLite from source so the build doesn't depend on a system `libsqlite3-dev`.

- [ ] **Step 2: Create `bench/src/sqlite_engine.rs`**

Write the file with this exact content:

```rust
// SqliteEngine — Engine trait impl backed by rusqlite.
//
// Schema: chisel_bench(id INTEGER PRIMARY KEY AUTOINCREMENT, value BLOB).
// AUTOINCREMENT is load-bearing — it suppresses SQLite's default
// rowid-reuse-on-delete behavior, matching Chisel's handle-stability
// promise (see Engine::allocate doc comment).
//
// We don't use rusqlite's Transaction wrapper because it borrows
// &mut Connection — can't hold across separate &mut self calls
// without lifetime gymnastics. Instead we run BEGIN/COMMIT/ROLLBACK
// as raw SQL via execute_batch; transaction state is a simple bool.

use crate::engine::{DurabilityMode, Engine, EngineResult, Identifier};
use chisel::stats::ChiselCounters;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct SqliteEngine {
    conn: Connection,
    path: PathBuf,
    #[allow(dead_code)] // currently unused but preserved for symmetry with RedbEngine
    durability: DurabilityMode,
    active_tx: bool,
}

impl SqliteEngine {
    /// Open or create a file-backed SQLite database.
    ///
    /// `cache_size_pages` matches the harness convention: pages of
    /// 8 KB. SQLite's PRAGMA cache_size = -<KB> takes KB; we multiply.
    pub fn open_file(
        path: &Path,
        cache_size_pages: usize,
        durability: DurabilityMode,
    ) -> EngineResult<Self> {
        let conn = Connection::open(path)?;

        let cache_kb = cache_size_pages.max(1) * 8;
        conn.execute_batch(&format!(
            "PRAGMA cache_size = -{cache_kb}; \
             PRAGMA journal_mode = WAL;"
        ))?;

        let synchronous = match durability {
            DurabilityMode::Strict => "FULL",
            DurabilityMode::Unsafe => "OFF",
        };
        conn.execute_batch(&format!("PRAGMA synchronous = {synchronous};"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chisel_bench ( \
                id    INTEGER PRIMARY KEY AUTOINCREMENT, \
                value BLOB    NOT NULL \
            )",
        )?;

        Ok(Self {
            conn,
            path: path.to_path_buf(),
            durability,
            active_tx: false,
        })
    }
}

impl Engine for SqliteEngine {
    fn begin(&mut self) -> EngineResult<()> {
        if self.active_tx {
            return Err("transaction already active".into());
        }
        self.conn.execute_batch("BEGIN")?;
        self.active_tx = true;
        Ok(())
    }

    fn commit(&mut self) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        self.conn.execute_batch("COMMIT")?;
        self.active_tx = false;
        Ok(())
    }

    fn rollback(&mut self) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        self.conn.execute_batch("ROLLBACK")?;
        self.active_tx = false;
        Ok(())
    }

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier> {
        self.conn.execute(
            "INSERT INTO chisel_bench (value) VALUES (?)",
            rusqlite::params![value],
        )?;
        // SQLite's rowid is i64 native; AUTOINCREMENT keeps it
        // positive and growing, well below i64::MAX in practice.
        let rowid = self.conn.last_insert_rowid();
        Ok(Identifier(rowid as u64))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        let row: Vec<u8> = self.conn.query_row(
            "SELECT value FROM chisel_bench WHERE id = ?",
            rusqlite::params![id.0 as i64],
            |row| row.get(0),
        )?;
        Ok(row)
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        let n = self.conn.execute(
            "UPDATE chisel_bench SET value = ? WHERE id = ?",
            rusqlite::params![value, id.0 as i64],
        )?;
        if n == 0 {
            return Err("identifier not found".into());
        }
        Ok(())
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        let n = self.conn.execute(
            "DELETE FROM chisel_bench WHERE id = ?",
            rusqlite::params![id.0 as i64],
        )?;
        if n == 0 {
            return Err("identifier not found".into());
        }
        Ok(())
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM chisel_bench WHERE id = ?")?;
        for id in ids {
            let n = stmt.execute(rusqlite::params![id.0 as i64])?;
            if n == 0 {
                return Err("identifier not found".into());
            }
        }
        Ok(())
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        // SQLite in WAL mode keeps a -wal journal and -shm shared-memory
        // index alongside the main file. Honest "size on disk" sums all
        // three when present.
        let mut total = std::fs::metadata(&self.path)?.len();
        for suffix in ["-wal", "-shm"] {
            let mut sibling = self.path.clone().into_os_string();
            sibling.push(suffix);
            if let Ok(m) = std::fs::metadata(&sibling) {
                total += m.len();
            }
        }
        Ok(total)
    }

    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
        Ok(None)
    }
}
```

- [ ] **Step 3: Update `bench/src/lib.rs` to add the sqlite module + re-export**

Replace the contents of `bench/src/lib.rs` with:

```rust
// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PRs 1–2 + PR-A + PR 3 (this PR) land the Engine trait and all
// three engine impls. Subsequent PRs add the workload + runner +
// micro grid (PR 4), the reporter (PR 5), scenarios (PR 6), and
// CI integration (PR 7).

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod sqlite_engine;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use sqlite_engine::SqliteEngine;
```

- [ ] **Step 4: Run cargo build from `bench/`**

```bash
cd bench && cargo build && cd ..
```
Expected: clean build. The rusqlite dependency with `bundled` feature compiles SQLite from C source on first run (may take 1–3 minutes).

- [ ] **Step 5: Run cargo test from `bench/`, expect existing tests pass**

```bash
cd bench && cargo test && cd ..
```
Expected: PR 2's smoke test still passes. No new tests yet.

- [ ] **Step 6: Run clippy + fmt from `bench/`**

```bash
cd bench && cargo clippy --tests -- -D warnings && cd ..
cd bench && cargo fmt -- --check && cd ..
```
Expected: silent.

- [ ] **Step 7: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock bench/src/sqlite_engine.rs bench/src/lib.rs
git commit -m "bench: add SqliteEngine

Single-table chisel_bench(id INTEGER PRIMARY KEY AUTOINCREMENT,
value BLOB) schema. AUTOINCREMENT suppresses rowid-reuse-on-delete
to match Chisel's handle-stability promise. Raw BEGIN/COMMIT/ROLLBACK
SQL avoids rusqlite's Transaction-wrapper lifetime gymnastics.
DurabilityMode::Strict uses synchronous=FULL+WAL; Unsafe uses
synchronous=OFF.

file_size_bytes sums main + -wal + -shm files when present, for
honest reporting of SQLite's storage footprint.

PR 3 of the bench-suite series, Task 3 of 5."
```

---

## Task 4: Cross-engine equivalence tests

**Files:**
- Create: `bench/tests/equivalence.rs`

- [ ] **Step 1: Create `bench/tests/equivalence.rs`**

Write the file with this exact content:

```rust
// Cross-engine equivalence tests. Five scenarios × three engines =
// fifteen named tests. Each scenario asserts that an engine round-
// trips its own identifiers — read(allocate(v).id) returns v. We do
// not assert across engines (identifier values don't align by design,
// per the Engine::allocate doc comment).

use chisel_bench::{
    ChiselEngine, DurabilityMode, Engine, RedbEngine, SqliteEngine,
};
use tempfile::NamedTempFile;

// === Scenarios — generic over Engine ===

fn scenario_empty_value<E: Engine>(engine: &mut E) {
    engine.begin().expect("begin");
    let id = engine.allocate(b"").expect("allocate empty");
    engine.commit().expect("commit");
    assert_eq!(
        engine.read(id).expect("read empty"),
        b"",
        "empty-value round-trip failed",
    );
}

fn scenario_inline_range<E: Engine>(engine: &mut E) {
    let sizes = [32usize, 256, 2048];
    let values: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&n| (0..n).map(|i| (i % 251) as u8).collect())
        .collect();

    engine.begin().expect("begin");
    let ids: Vec<_> = values
        .iter()
        .map(|v| engine.allocate(v).expect("allocate inline"))
        .collect();
    engine.commit().expect("commit");

    for (id, expected) in ids.iter().zip(values.iter()) {
        assert_eq!(
            &engine.read(*id).expect("read inline"),
            expected,
            "inline-range round-trip failed for size {}",
            expected.len(),
        );
    }
}

fn scenario_just_overflow_boundary<E: Engine>(engine: &mut E) {
    // Sizes bracket Chisel's MAX_INLINE_VALUE = 8162. 8160 fits inline;
    // 8200 and 9000 spill to overflow. For redb / SQLite this is just
    // storage of the same byte ranges.
    let sizes = [8160usize, 8200, 9000];
    let values: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&n| (0..n).map(|i| (i % 251) as u8).collect())
        .collect();

    engine.begin().expect("begin");
    let ids: Vec<_> = values
        .iter()
        .map(|v| engine.allocate(v).expect("allocate boundary"))
        .collect();
    engine.commit().expect("commit");

    for (id, expected) in ids.iter().zip(values.iter()) {
        assert_eq!(
            &engine.read(*id).expect("read boundary"),
            expected,
            "just-overflow-boundary round-trip failed for size {}",
            expected.len(),
        );
    }
}

fn scenario_large_overflow<E: Engine>(engine: &mut E) {
    let value: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();

    engine.begin().expect("begin");
    let id = engine.allocate(&value).expect("allocate 1 MB");
    engine.commit().expect("commit");

    assert_eq!(
        engine.read(id).expect("read 1 MB"),
        value,
        "large-overflow round-trip failed",
    );
}

fn scenario_delete_and_allocate<E: Engine>(engine: &mut E) {
    // Allocate 5 in one tx.
    engine.begin().expect("begin");
    let initial: Vec<_> = (0..5)
        .map(|i| {
            engine
                .allocate(format!("v{i}").as_bytes())
                .expect("allocate initial")
        })
        .collect();
    engine.commit().expect("commit");

    // Delete 3 of them, allocate 5 more.
    engine.begin().expect("begin");
    engine.delete(initial[1]).expect("delete 1");
    engine.delete(initial[2]).expect("delete 2");
    engine.delete(initial[3]).expect("delete 3");
    let added: Vec<_> = (5..10)
        .map(|i| {
            engine
                .allocate(format!("v{i}").as_bytes())
                .expect("allocate added")
        })
        .collect();
    engine.commit().expect("commit");

    // Surviving from initial: 0 and 4.
    assert_eq!(engine.read(initial[0]).expect("read survivor 0"), b"v0");
    assert_eq!(engine.read(initial[4]).expect("read survivor 4"), b"v4");

    // All added values readable.
    for (id, i) in added.iter().zip(5..10) {
        assert_eq!(
            engine.read(*id).expect("read added"),
            format!("v{i}").as_bytes(),
        );
    }

    // Deleted identifiers must error on read.
    assert!(
        engine.read(initial[1]).is_err(),
        "deleted identifier 1 must not be readable",
    );
    assert!(
        engine.read(initial[2]).is_err(),
        "deleted identifier 2 must not be readable",
    );
    assert!(
        engine.read(initial[3]).is_err(),
        "deleted identifier 3 must not be readable",
    );
}

// === Per-engine constructors ===

fn make_chisel() -> ChiselEngine {
    ChiselEngine::open_in_memory(64).expect("open chisel")
}

fn make_redb() -> (RedbEngine, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("tempfile");
    // redb's Database::create wants a non-existent or empty file; the
    // tempfile is created empty, redb treats that as "create new".
    let engine = RedbEngine::open_file(tmp.path(), 64, DurabilityMode::Strict)
        .expect("open redb");
    (engine, tmp)
}

fn make_sqlite() -> (SqliteEngine, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let engine =
        SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Strict)
            .expect("open sqlite");
    (engine, tmp)
}

// === Per-engine, per-scenario named tests (5 × 3 = 15) ===

#[test]
fn equivalence_empty_value_chisel() {
    let mut e = make_chisel();
    scenario_empty_value(&mut e);
}

#[test]
fn equivalence_empty_value_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_empty_value(&mut e);
}

#[test]
fn equivalence_empty_value_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_empty_value(&mut e);
}

#[test]
fn equivalence_inline_range_chisel() {
    let mut e = make_chisel();
    scenario_inline_range(&mut e);
}

#[test]
fn equivalence_inline_range_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_inline_range(&mut e);
}

#[test]
fn equivalence_inline_range_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_inline_range(&mut e);
}

#[test]
fn equivalence_just_overflow_boundary_chisel() {
    let mut e = make_chisel();
    scenario_just_overflow_boundary(&mut e);
}

#[test]
fn equivalence_just_overflow_boundary_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_just_overflow_boundary(&mut e);
}

#[test]
fn equivalence_just_overflow_boundary_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_just_overflow_boundary(&mut e);
}

#[test]
fn equivalence_large_overflow_chisel() {
    let mut e = make_chisel();
    scenario_large_overflow(&mut e);
}

#[test]
fn equivalence_large_overflow_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_large_overflow(&mut e);
}

#[test]
fn equivalence_large_overflow_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_large_overflow(&mut e);
}

#[test]
fn equivalence_delete_and_allocate_chisel() {
    let mut e = make_chisel();
    scenario_delete_and_allocate(&mut e);
}

#[test]
fn equivalence_delete_and_allocate_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_delete_and_allocate(&mut e);
}

#[test]
fn equivalence_delete_and_allocate_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_delete_and_allocate(&mut e);
}
```

- [ ] **Step 2: Run the equivalence tests**

```bash
cd bench && cargo test --test equivalence && cd ..
```
Expected: 15 tests pass. If any fail:
- The most likely failures are in `RedbEngine::recover_next_id` (key ordering issue) or `SqliteEngine` rowid casting (very rare).
- A failure on `equivalence_delete_and_allocate_sqlite` specifically would indicate the AUTOINCREMENT setup didn't take — verify the `CREATE TABLE` statement in `SqliteEngine::open_file` uses `INTEGER PRIMARY KEY AUTOINCREMENT`.
- Borrow-checker compile errors in `recover_next_id` are also possible if redb's API differs from expectations; the plan-supplied code targets redb 2.x.

- [ ] **Step 3: Run the full bench test suite, expect no regression**

```bash
cd bench && cargo test && cd ..
```
Expected: 16 tests pass total (PR 2's smoke test + the 15 new equivalence tests).

- [ ] **Step 4: Run clippy + fmt from `bench/`**

```bash
cd bench && cargo clippy --tests -- -D warnings && cd ..
cd bench && cargo fmt -- --check && cd ..
```
Expected: silent.

- [ ] **Step 5: Commit**

```bash
git add bench/tests/equivalence.rs
git commit -m "bench: cross-engine equivalence tests

Five scenarios (empty value, inline range, just-overflow boundary,
large overflow, delete-and-allocate) × three engines (Chisel, redb,
SQLite) = 15 named tests. Each scenario asserts that an engine
round-trips its own identifiers — read(allocate(v).id) returns v.
Identifier values don't align across engines by design (per the
Engine::allocate doc comment), so we do not assert across engines.

PR 3 of the bench-suite series, Task 4 of 5."
```

---

## Task 5: Final gate

Verification only. No code changes.

- [ ] **Step 1: Full Rust suite at root, no regression**

```bash
cargo test
```
Expected: every existing test still passes. The bench subcrate's tests are not run from root because each crate is independent.

- [ ] **Step 2: Bench subcrate test suite**

```bash
cd bench && cargo test && cd ..
```
Expected: 16 tests pass (1 smoke from PR 2 + 15 equivalence from Task 4).

- [ ] **Step 3: Clippy at root**

```bash
cargo clippy -- -D warnings
```
Expected: clean.

- [ ] **Step 4: Clippy in bench**

```bash
cd bench && cargo clippy --tests -- -D warnings && cd ..
```
Expected: clean.

- [ ] **Step 5: Fmt at root**

```bash
cargo fmt -- --check
```
Expected: clean.

- [ ] **Step 6: Fmt in bench**

```bash
cd bench && cargo fmt -- --check && cd ..
```
Expected: clean.

- [ ] **Step 7: Confirm git state**

```bash
git status
git log --oneline -6
```
Expected: working tree clean. The four task commits in order: Task 4, Task 3, Task 2, Task 1, then before that the spec + plan commits (or whatever was on the branch before this PR's work began).

If anything is unexpected, do NOT proceed; report.

---

## Done

PR 3 is complete when all five tasks above are done and gates 1–7 of Task 5 pass. The next step (out of scope for this plan) is `superpowers:finishing-a-development-branch` to merge to main.

PR 4 (workload generators + Runner + the micro grid) follows. It will use the Engine trait + DurabilityMode + Identifier shapes that PR 3 finalized. None of the Engine impls in PR 3 will be modified by PR 4 except possibly to add a method if the runner needs one — and that would be a contained extension, not a redesign.
