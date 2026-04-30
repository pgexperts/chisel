# Chisel Bench — RedbEngine + SqliteEngine — Design

**Date:** 2026-04-30
**Status:** Design approved; implementation plan pending.
**Scope:** Add `RedbEngine` and `SqliteEngine` as `Engine` trait impls in the `bench/` subcrate. Both support strict and unsafe durability modes via a constructor parameter. Five cross-engine equivalence tests verify trait-abstraction faithfulness across all three engines (Chisel, redb, SQLite). Land two deferred items from earlier reviews: per-method doc comments on `Engine`, and `#[repr(transparent)]` on `Identifier`. PR 3 of the bench-suite series.

This spec follows on from `2026-04-25-chisel-benchmark-suite-design.md` (the overall bench-suite design) and `2026-04-25-chisel-bench-engine-foundation-design.md` (the PR-2 design that established the `Engine` trait + `ChiselEngine`).

## 1. Goals and Non-Goals

### Goals

- Add `RedbEngine` and `SqliteEngine` as `Engine` trait impls in the `bench/` subcrate. With `ChiselEngine` already in place, the bench harness gains its three-engine cross-comparison surface.
- Both engines support both durability modes (strict and unsafe) via a constructor parameter. ChiselEngine has only one mode (always-fsync) and its constructor signature stays unchanged.
- Add five scenario-style cross-engine equivalence tests covering empty / inline / just-overflow / large-overflow / delete-and-allocate boundaries. Each scenario runs as a separate named test against each engine — fifteen named tests total.
- Land two deferred items from earlier reviews: per-method doc comments on the `Engine` trait (now that we have concrete error semantics across three engines), and `#[repr(transparent)]` on `Identifier` (with the corresponding `unsafe` cleanup in ChiselEngine's `delete_many`). Also fold in audit F4: change `internal_counters` to return `EngineResult<Option<ChiselCounters>>` so a poisoned engine surfaces honestly instead of silently masquerading as "no counters available."

### Non-Goals (this PR)

- *Workload generators / Runner / Reporter / Criterion benches.* These are PR 4. PR 3 only adds engines + equivalence tests; no Criterion bench files yet.
- *Scenario tier* — YCSB-A, YCSB-B, mutation log, document store. Those are PR 6. The equivalence tests in this PR are correctness-driven, not performance-driven.
- *Markdown post-processor.* PR 5.
- *CI integration.* PR 7. PR 3's tests run via plain `cargo test` in the bench subcrate.
- *Per-leaf `delete_many` batching* (ISSUES.md I33). Still deferred.
- *mmap'd cache backing* (ISSUES.md I34). Just filed; deferred until PR 4 data informs the case.
- *Multi-table SQLite schemas.* SqliteEngine uses one table — `chisel_bench(id INTEGER PRIMARY KEY AUTOINCREMENT, value BLOB NOT NULL)` — per the handle-as-natural-identifier framing.
- *In-memory variants of RedbEngine and SqliteEngine.* ChiselEngine has `open_in_memory`; the others use file-backed via `tempfile::NamedTempFile` in tests.
- *Async or multi-thread testing.* Single-threaded throughout, matching the engine design.

## 2. Architecture — file structure, deps, shared types

### 2.1 File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/Cargo.toml` | Modify | Add `redb` (2.x) and `rusqlite` (0.31+, with `bundled` feature) as dependencies |
| `bench/src/engine.rs` | Modify | Add `DurabilityMode` enum; add `#[repr(transparent)]` to `Identifier`; add per-method doc comments to the `Engine` trait; change `internal_counters` signature to `EngineResult<Option<ChiselCounters>>` |
| `bench/src/chisel_engine.rs` | Modify | `delete_many` uses safe-slice transmute via `repr(transparent)`; `internal_counters` propagates poison via `?` |
| `bench/src/redb_engine.rs` | Create | `RedbEngine` impl |
| `bench/src/sqlite_engine.rs` | Create | `SqliteEngine` impl |
| `bench/src/lib.rs` | Modify | Add `pub mod redb_engine` / `pub mod sqlite_engine`; re-export `RedbEngine`, `SqliteEngine`, `DurabilityMode` |
| `bench/tests/equivalence.rs` | Create | Five boundary scenario tests, each instantiated per engine |

### 2.2 Dependency choices

- **`redb = "2"`** — major version 2.x. The `Engine` trait we built in PR 2 maps cleanly to redb's `Database`, `WriteTransaction`, `Table`, and `ReadTransaction` types. redb's default `Durability::Immediate` matches Chisel's fsync-per-commit; `Durability::Eventual` is the unsafe-mode counterpart.
- **`rusqlite = { version = "0.31", features = ["bundled"] }`** — the `bundled` feature compiles SQLite from source, so the build doesn't need a system `libsqlite3-dev`. Strict mode uses `PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL`; unsafe mode uses `PRAGMA synchronous=OFF`.

Both are regular `[dependencies]` (not `[dev-dependencies]`) because `RedbEngine` and `SqliteEngine` are public types in the bench crate. The plan resolves exact pinned versions.

### 2.3 `DurabilityMode` enum

In `bench/src/engine.rs`:

```rust
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
```

### 2.4 `Identifier` — repr-transparent + construction guidance

```rust
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
```

The construction-guidance paragraph addresses audit F5's "lacks direct construction guidance in doc comment." The `#[repr(transparent)]` annotation enables the ChiselEngine cleanup in §6.

### 2.5 `Engine` trait — per-method doc comments + signature change

Each of the trait's methods gets a doc comment explaining what it does, what conditions cause `Err`, and any cross-engine semantic notes. The note on `allocate` is the most important — it states explicitly that **identifier spaces don't align across engines**, so the same call sequence yields different `Identifier` values from each impl. The equivalence test in §7 relies on this; the trait's doc comments are the canonical source.

Doc comments are factual statements about the trait contract, not specific engine error types — they say "returns Err on engine I/O failure" rather than "returns rusqlite::Error::SqliteFailure or redb::Error::Io" because the trait abstracts over concrete error types.

The trait signature for `internal_counters` changes from:

```rust
fn internal_counters(&self) -> Option<ChiselCounters>;
```

to:

```rust
fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>>;
```

This addresses audit F4 ("ChiselEngine drops poison signal silently"). A poisoned engine now surfaces the error rather than masking it. RedbEngine and SqliteEngine return `Ok(None)`.

## 3. `RedbEngine` implementation

### 3.1 Crate layout

New file `bench/src/redb_engine.rs`. Public type `RedbEngine`. One module-scoped `TableDefinition<u64, &[u8]>` constant — the entire engine is a single table holding `(monotonic_u64_key, value_blob)` pairs.

### 3.2 Struct shape

```rust
pub struct RedbEngine {
    db: redb::Database,
    path: PathBuf,                          // for file_size_bytes()
    next_id: u64,                           // caller-generated monotonic key
    durability: DurabilityMode,
    active_tx: Option<redb::WriteTransaction>,
}
```

`next_id` is recovered on open by reading the table's largest key (or 0 if empty), then incremented forever within the engine's lifetime. Keys are never reused even after delete — matches Chisel's handle-stability semantic.

`active_tx` holds the in-flight `WriteTransaction` between `begin()` and `commit()`/`rollback()`. redb 2.x's `WriteTransaction` is `'static`-suitable (it holds an `Arc` to the database state internally), so storing it on the struct works without lifetime gymnastics.

### 3.3 Constructor

```rust
impl RedbEngine {
    pub fn open_file(
        path: &Path,
        cache_size_pages: usize,
        durability: DurabilityMode,
    ) -> EngineResult<Self> {
        let cache_bytes = cache_size_pages.max(1) * PAGE_SIZE;  // 8 KB pages
        let db = redb::Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?;
        let next_id = recover_next_id(&db)?;
        Ok(Self { db, path: path.to_path_buf(), next_id, durability, active_tx: None })
    }
}
```

`cache_size_pages` matches ChiselEngine's signature for harness symmetry — the harness doesn't have to know that redb's API takes bytes. `cache_bytes = pages × 8192` keeps the comparison apples-to-apples.

### 3.4 Method mapping

| `Engine` method | redb mapping |
|---|---|
| `begin()` | `db.begin_write()`; set durability per `DurabilityMode`; store on `self.active_tx` |
| `commit()` | `take(active_tx).commit()` (consumes the WriteTransaction) |
| `rollback()` | `take(active_tx).abort()` |
| `allocate(value)` | Borrow `&active_tx`; open table; `table.insert(next_id, value)`; bump `next_id` |
| `read(id)` | `&self`-shaped: if `active_tx.is_some()` use it; otherwise open a fresh `begin_read()` tx. Either way `table.get(id.0)?.ok_or(...)?.value().to_vec()` |
| `update(id, value)` | `table.insert(id.0, value)` (insert is upsert in redb) |
| `delete(id)` | `table.remove(id.0)?` |
| `delete_many(ids)` | Loop `table.remove(id.0)` for each — same pattern as ChiselEngine's pre-PR-A path |
| `file_size_bytes()` | `std::fs::metadata(&self.path)?.len()` |
| `internal_counters()` | Returns `Ok(None)` |

### 3.5 Durability mapping

```rust
let durability = match self.durability {
    DurabilityMode::Strict => redb::Durability::Immediate,
    DurabilityMode::Unsafe => redb::Durability::Eventual,
};
write_tx.set_durability(durability);
```

`Durability::Immediate` is redb's default and performs an fsync per commit. `Durability::Eventual` writes data but skips the durability barrier; commits return faster but a crash may lose the latest writes.

### 3.6 Why caller-generated monotonic keys

redb's API doesn't have a built-in auto-incrementing key — it's a generic key-value store. The harness must supply keys. By generating monotonically and never reusing, we match Chisel's identifier-stability promise (and SQLite's `AUTOINCREMENT` semantic from §4).

## 4. `SqliteEngine` implementation

### 4.1 Crate layout

New file `bench/src/sqlite_engine.rs`. Public type `SqliteEngine`. Single-table schema: `chisel_bench(id INTEGER PRIMARY KEY AUTOINCREMENT, value BLOB NOT NULL)`. The `AUTOINCREMENT` is load-bearing — it's what suppresses SQLite's default rowid-reuse-on-delete behavior, matching Chisel's handle-stability promise.

### 4.2 Struct shape

```rust
pub struct SqliteEngine {
    conn: rusqlite::Connection,
    path: PathBuf,                  // for file_size_bytes()
    durability: DurabilityMode,
    active_tx: bool,                // not Option<Transaction<'_>>: see below
}
```

**Why `active_tx: bool` and not `Option<Transaction>`.** rusqlite's `Connection::transaction()` returns `Transaction<'_>` borrowing `&mut Connection`. We cannot hold that across separate `&mut self` calls without lifetime gymnastics. The clean solution: don't use rusqlite's `Transaction` wrapper; instead run `BEGIN` / `COMMIT` / `ROLLBACK` as raw SQL via `conn.execute_batch()`. The connection stays accessible; transaction state lives in the simple `bool` flag. SQLite's `BEGIN`/`COMMIT`/`ROLLBACK` SQL is exactly what the wrapper does internally — we just skip the wrapper.

### 4.3 Constructor

```rust
impl SqliteEngine {
    pub fn open_file(
        path: &Path,
        cache_size_pages: usize,
        durability: DurabilityMode,
    ) -> EngineResult<Self> {
        let conn = rusqlite::Connection::open(path)?;

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
            )"
        )?;

        Ok(Self { conn, path: path.to_path_buf(), durability, active_tx: false })
    }
}
```

`cache_size = -<KB>` is SQLite syntax for "in KB" (positive values mean SQLite-native pages). `cache_size_pages × 8 KB` matches the harness convention.

### 4.4 Method mapping

| `Engine` method | SQLite mapping |
|---|---|
| `begin()` | `conn.execute_batch("BEGIN")`; `active_tx = true` |
| `commit()` | `conn.execute_batch("COMMIT")`; `active_tx = false` |
| `rollback()` | `conn.execute_batch("ROLLBACK")`; `active_tx = false` |
| `allocate(value)` | `INSERT INTO chisel_bench (value) VALUES (?)`; `Identifier(conn.last_insert_rowid() as u64)` |
| `read(id)` | `SELECT value FROM chisel_bench WHERE id = ?` |
| `update(id, value)` | `UPDATE chisel_bench SET value = ? WHERE id = ?`; rows-affected = 0 → `Err` |
| `delete(id)` | `DELETE FROM chisel_bench WHERE id = ?`; rows-affected = 0 → `Err` |
| `delete_many(ids)` | Prepared statement reused across iterations |
| `file_size_bytes()` | Sum of main file + `-wal` + `-shm` if present |
| `internal_counters()` | Returns `Ok(None)` |

### 4.5 Type bridging

SQLite's rowid is `i64`; our `Identifier` is `u64`. Cast via `as` is lossless in practice because rowids `AUTOINCREMENT` start at 1 and grow monotonically — bounded well below `i64::MAX`. The cast is documented as a comment in the impl, not abstracted into a helper.

### 4.6 `file_size_bytes()` — the WAL file question

SQLite in WAL mode keeps a `-wal` shadow file (recent writes pending checkpoint) and a `-shm` shared-memory index file. The honest "size on disk" includes all three. Implementation sums the three, with missing-file handled gracefully (the `-wal` and `-shm` may not exist if no writes have happened yet). The plan resolves the precise path-extension code; the principle is "include all three when present."

This is the honest answer for PR 4's `file_size_delta` column. Reporting only the main file would undercount SQLite's storage footprint — particularly visible after a write-heavy transaction before WAL checkpoint runs.

### 4.7 Why no batched `DELETE … WHERE id IN (?, …)`

Could be done with N parameters per call, but: (a) SQLite's parameter-count limit (default 999) caps batch size; (b) the loop with a prepared statement is very close in cost to a single IN query and easier to reason about; (c) consistency with `RedbEngine`'s loop pattern. If PR 4's micro-grid shows row 9 (`delete_many(1000)`) as a SQLite outlier, a future PR can switch to the IN form.

## 5. ChiselEngine adjustments

Two small changes that land alongside the trait modifications from §2.

### 5.1 `delete_many` no longer allocates

Currently:

```rust
fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
    let handles: Vec<u64> = ids.iter().map(|i| i.0).collect();
    Ok(self.db.delete_many(&handles)?)
}
```

After PR 3 (using `Identifier`'s `#[repr(transparent)]` from §2.4):

```rust
fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
    // SAFETY: Identifier is #[repr(transparent)] over u64, so a slice
    // of Identifier and a slice of u64 have identical layout. The
    // borrow ends with this call; no aliasing concern; no 'static
    // lifetime escapes.
    let handles: &[u64] = unsafe {
        std::slice::from_raw_parts(ids.as_ptr() as *const u64, ids.len())
    };
    Ok(self.db.delete_many(handles)?)
}
```

Saves one allocation per `delete_many` call. The `unsafe` is small and the SAFETY comment cites the structural reason it's correct. Resolves audit F5.

### 5.2 `internal_counters` propagates poison

ChiselEngine's impl after the trait signature change:

```rust
fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
    Ok(Some(self.db.counters()?))   // poison surfaces via ?
}
```

Resolves audit F4. RedbEngine and SqliteEngine impls return `Ok(None)`.

## 6. Cross-engine equivalence test

### 6.1 File and shape

`bench/tests/equivalence.rs` (new). Five scenario functions, each generic over `<E: Engine>`. Three named tests per scenario (one per engine concrete type) = 15 named tests total.

Each scenario asserts only that *each engine round-trips its own identifiers* — `read(id)` returns the bytes that the corresponding `allocate(value)` stored. It does not assert across engines (identifier values don't align by design — see §2.5's `Engine::allocate` doc comment).

### 6.2 The five scenarios

| # | Scenario | What it tests | Sizes / shape |
|---|----------|---------------|---------------|
| 1 | `scenario_empty_value` | 0-byte values round-trip | `b""` |
| 2 | `scenario_inline_range` | Small-and-medium values that fit inline (Chisel-side); just storage for redb/SQLite | 32 B, 256 B, 2 KB |
| 3 | `scenario_just_overflow_boundary` | Values bracketing Chisel's `MAX_INLINE_VALUE = 8162`; verifies the inline/overflow routing is invisible to the trait abstraction | 8160 (fits inline), 8200 (minimal overflow), 9000 |
| 4 | `scenario_large_overflow` | 1 MB value — stresses Chisel's overflow chain (~128 pages); just storage for the others | 1 MB |
| 5 | `scenario_delete_and_allocate` | Allocate 5, delete 3 of them mid-batch, allocate 5 more, verify all surviving identifiers still round-trip + deleted ones error on read. Validates the no-reuse identifier semantics. | 5 + 3 deletes + 5 |

### 6.3 Per-engine instantiation pattern

```rust
#[test]
fn equivalence_empty_value_chisel() {
    let mut engine = ChiselEngine::open_in_memory(64).unwrap();
    scenario_empty_value(&mut engine);
}

#[test]
fn equivalence_empty_value_redb() {
    let tmp = NamedTempFile::new().unwrap();
    let mut engine = RedbEngine::open_file(tmp.path(), 64, DurabilityMode::Strict).unwrap();
    scenario_empty_value(&mut engine);
}

#[test]
fn equivalence_empty_value_sqlite() {
    let tmp = NamedTempFile::new().unwrap();
    let mut engine = SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Strict).unwrap();
    scenario_empty_value(&mut engine);
}
```

`tempfile::NamedTempFile` is the existing dev-dependency from PR 2's smoke test. ChiselEngine uses `open_in_memory` (faster; equivalence test isn't checking durability). RedbEngine and SqliteEngine require file-backed instances; the tempfile lives long enough for the test, OS cleans up the path afterward.

### 6.4 Why 15 explicit tests rather than parameterized macros

`cargo test` output names every test individually. If `equivalence_just_overflow_boundary_sqlite` fails, the developer sees exactly which (scenario, engine) tuple broke. Test discovery and filtering (`cargo test equivalence_empty_value`) work cleanly. The verbose form is straightforward; ~120 lines of test code total is fine for a contained suite.

A `paste!`-driven macro would cut this to ~40 lines, but the readability trade-off isn't worth it for 15 tests.

### 6.5 Strict-mode-only

Each test uses `DurabilityMode::Strict` for redb and SQLite. The `Unsafe` mode is exercised in PR 4's micro-grid (it's a benchmark concern, not a correctness concern — relaxed-fsync engines should still round-trip data correctly within a process lifetime).

### 6.6 Why no `delete_many` scenario

PR 2's `bench/tests/smoke.rs` already exercises `delete_many` through ChiselEngine and asserts post-delete reads error. PR 3 adds two more impls of `delete_many`; an analogous `scenario_delete_many` would be a sixth scenario, but `scenario_delete_and_allocate` covers the survivor-vs-deleted contract that matters most. If a redb/SQLite `delete_many` regression slipped through this gate, PR 4's micro-grid would catch it.

## 7. Build sequence

Five tasks, in order. Each task lands as one commit (signature changes + caller migration land together so the codebase compiles at every commit boundary).

| # | Task | Files | Content | LOC estimate |
|---|------|-------|---------|--------------|
| 1 | **Trait extensions + ChiselEngine adjustments** | `engine.rs` (modify), `chisel_engine.rs` (modify) | `DurabilityMode` enum, `#[repr(transparent)]` on `Identifier`, per-method doc comments on `Engine`, `internal_counters` signature change, `ChiselEngine.delete_many` uses `unsafe` slice transmute, `ChiselEngine.internal_counters` propagates poison | ~80 |
| 2 | **RedbEngine** | `redb_engine.rs` (new), `Cargo.toml` (+`redb`), `lib.rs` (re-export) | `RedbEngine` impl + `WriteTransaction`-on-struct pattern + `next_id` recovery on open + `DurabilityMode` translation | ~150 |
| 3 | **SqliteEngine** | `sqlite_engine.rs` (new), `Cargo.toml` (+`rusqlite`), `lib.rs` (re-export) | `SqliteEngine` impl + raw `BEGIN`/`COMMIT`/`ROLLBACK` SQL + `INTEGER PRIMARY KEY AUTOINCREMENT` schema + PRAGMA setup + three-file `file_size_bytes` | ~180 |
| 4 | **Equivalence tests** | `tests/equivalence.rs` (new) | 5 scenario fns generic over `<E: Engine>` + 15 named tests instantiating each scenario per engine | ~150 |
| 5 | **Final gate** | None (verification only) | `cargo test/clippy/fmt` at root + bench/; confirm tempfile cleanup behavior; confirm `cargo build` from a fresh checkout pulls redb + rusqlite cleanly | 0 |

**Total estimate: ~560 lines.** Spec says ~400; the extra ~160 is the trait extensions + ChiselEngine adjustments, which the spec didn't account for explicitly.

### 7.1 Minimal viable point: Task 3

Tasks 1–3 give us all three engine impls compiling and the `Engine` trait fully documented. Task 5's gates pass with just per-engine internal correctness (PR 2's smoke test still runs). Task 4 adds the cross-engine equivalence verification on top — important, but the foundation is meaningful without it.

### 7.2 Rollback paths

- Task 1 fails: trait stays at PR-2 shape; Tasks 2+ blocked because they reference `DurabilityMode` and the new `internal_counters` signature.
- Task 2 fails: revert removes `redb` dep + the new file. Trait extensions from Task 1 are unaffected.
- Task 3 fails: revert removes `rusqlite` dep + the new file. Tasks 1+2 unaffected.
- Task 4 fails: revert removes the test file. Engines all still work.
- Task 5 fails: per-task revert based on which gate broke.

### 7.3 Out of scope (will not appear in any task)

- Workload generators / Runner / Reporter (PR 4)
- Criterion bench files (PR 4)
- The four scenario-tier benchmarks: YCSB-A, YCSB-B, mutation log, document store (PR 6)
- Markdown post-processor (PR 5)
- CI workflow (PR 7)
- Per-leaf `delete_many` batching (I33)
- mmap'd cache backing (I34)

### 7.4 Estimated calendar time

1–2 weeks of focused effort. Tasks are sequentially dependent; can't parallelize within the PR. Independent of unrelated work though — none of these tasks touch the main `chisel` crate.

## 8. Open Implementation-Phase Questions

These are deliberately deferred to the implementation plan:

- Exact pinned versions of `redb` and `rusqlite`. Spec says "2.x" / "0.31+"; the plan picks specific minor versions and locks them.
- Detailed shape of redb's `recover_next_id`. Likely `tx.open_table(TABLE)?.iter()?.next_back()` to get the largest existing key, plus 1; or 0 if the table is empty / has never been opened. The plan resolves the exact form (including whether to use a separate metadata table for `next_id` to avoid the iter-back overhead on open).
- The exact `file_size_bytes` path-extension dance for SQLite's `-wal` and `-shm` files. The plan picks a pattern (e.g. `path.with_extension("db-wal")` if path ends in `.db`; otherwise `path.to_string() + "-wal"`) and standardizes on it.
- Whether to expose a future `RedbEngine::open_in_memory` / `SqliteEngine::open_in_memory` for symmetry with ChiselEngine. Out of scope for PR 3 (tests use tempfiles); plan can flag it.
- Whether to add `paste!` or similar macro support to the equivalence test. Spec recommends explicit 15 tests; plan can revisit if review feedback prefers compactness.

These don't affect the design contract; the plan resolves them.
