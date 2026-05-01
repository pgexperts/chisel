# Bench Runner + 270-Cell Micro Grid Implementation Plan (PR 4b)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the Runner machinery (`bench/src/runner.rs`) and bench-binary glue (`bench/benches/micro_grid.rs`) that drives PR 4a's workload data layer against the three engine impls to produce Criterion HTML for all 270 micro-grid cells, plus a JSONL side-channel capturing per-cell file-size deltas and Chisel internal counter deltas.

**Architecture:** Library-resident `runner.rs` holds the non-Criterion machinery (EngineMode, PopulatedSnapshot, apply_op, drive_workload_with_tx_granularity, AuxMetricsWriter, capture_aux_metrics_*). The bench binary `micro_grid.rs` holds the Criterion-shaped cell-runner helpers (run_snapshot_restore_cell, run_warm_read_cell, run_cold_read_cell) plus the 9 row-bench functions. Three iteration patterns: snapshot-restore (8 rows), persistent-engine warm-read (row 3), snapshot-restore-with-open-in-routine cold-read (row 4).

**Tech Stack:** Rust 2021 edition. Criterion 0.5 (in `[dev-dependencies]`), serde 1 + serde_json 1 (in `[dependencies]`). Reuses PR 4a's `Workload`/`Operation` and PR 3's three engine impls.

**Spec:** `docs/superpowers/specs/2026-05-01-chisel-bench-runner-micro-grid-design.md`

---

## Task 1: Cargo.toml additions + .gitignore for bench results

**Files:**
- Modify: `bench/Cargo.toml`
- Modify: `bench/.gitignore`

- [ ] **Step 1: Edit `bench/Cargo.toml`**

The current `[dependencies]` section is:
```toml
[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }
rand = "0.8"
rand_chacha = "0.3"
```

Add `serde` and `serde_json` to `[dependencies]`, and append `criterion` to `[dev-dependencies]`. **Do NOT add a `[[bench]]` block here** — that lands in Task 8 atomically with the bench file. (Cargo parses `[[bench]]` paths at manifest-load time; declaring a `[[bench]]` whose file doesn't exist yet causes a manifest parse error that blocks every cargo command.)

The full result:

```toml
[package]
name = "chisel-bench"
version = "0.1.0"
edition = "2021"
description = "Benchmark harness for Chisel — Engine trait abstraction, workload generators, and cross-engine comparison runners. Internal use only; not published."
publish = false

[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }
rand = "0.8"
rand_chacha = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
tempfile = "3"
criterion = "0.5"
```

- [ ] **Step 2: Edit `bench/.gitignore`**

The current contents are:
```
target/
.DS_Store
```

Append `results/` so bench output isn't committed:

```
target/
.DS_Store
results/
```

- [ ] **Step 3: Verify the bench subcrate still builds**

Run: `cd bench && cargo build`
Expected: clean build, "Finished" line. New crates `serde`, `serde_json`, `criterion`, plus their transitive deps appear in `bench/Cargo.lock`.

- [ ] **Step 4: Verify existing tests still pass**

Run: `cd bench && cargo test`
Expected: all PR 3 + PR 4a tests still pass (15 equivalence + 14 workload + 1 smoke = 30 tests).

- [ ] **Step 5: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock bench/.gitignore
git commit -m "$(cat <<'EOF'
bench: add criterion + serde deps for runner + micro_grid

criterion goes in [dev-dependencies] (only the [[bench]] target uses
it; keeps its transitive graph out of consumers). serde + serde_json in
[dependencies] because runner.rs (library code) writes the JSONL
aux-metrics file. The [[bench]] target itself lands in task 8
together with bench/benches/micro_grid.rs — declaring it now would
cause a manifest parse error blocking every cargo command.
results/ added to bench/.gitignore so bench output isn't committed.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `EngineMode` enum + lib.rs export + 3 tests

**Files:**
- Create: `bench/src/runner.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Create `bench/src/runner.rs` with the file header and EngineMode enum stub (no impl yet)**

The header explains the module's role; per project commenting standards, every file has one. Create the file with:

```rust
// Runner machinery for the micro grid. Holds the non-Criterion-dependent
// machinery (engine construction, snapshot population, workload application,
// aux-metric capture, JSONL writer); the Criterion-shaped cell-runner
// helpers live in `bench/benches/micro_grid.rs` because Criterion is in
// [dev-dependencies] and library code in src/ cannot import dev-deps.
//
// PR 6 (scenarios) will reuse most of this module — engine construction,
// populate_snapshot, apply_op, AuxMetricsWriter — for its own iteration
// shape, which is "one big workload run" rather than the per-iteration
// micro-grid cells PR 4b registers.

use crate::engine::{DurabilityMode, Engine, EngineResult, Identifier};
use crate::{ChiselEngine, RedbEngine, SqliteEngine};
use std::path::Path;

/// One of the five engine-mode columns of the micro grid.
///
/// Hides the per-engine constructor asymmetry behind `EngineMode::open`:
/// ChiselEngine takes `(path, cache_size_pages)` while RedbEngine and
/// SqliteEngine take `(path, cache_size_pages, DurabilityMode)`. The enum
/// is the single source of truth for the 5 modes the micro grid measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    ChiselStrict,
    RedbStrict,
    RedbUnsafe,
    SqliteStrict,
    SqliteUnsafe,
}
```

- [ ] **Step 2: Wire the module into `lib.rs`**

The current `bench/src/lib.rs` looks like (after PR 4a):

```rust
// Bench harness for Chisel.
//
// (header doc comment...)

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod sqlite_engine;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
```

Add `pub mod runner;` and `pub use runner::EngineMode;`. The full updated file:

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
// PRs 1–2 + PR-A + PR 3 landed the Engine trait and all three engine
// impls. PR 4a landed the Workload data layer. PR 4b (this PR) adds
// the Runner + 270-cell registration. PRs 5–7 follow.

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod runner;
pub mod sqlite_engine;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use runner::EngineMode;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
```

- [ ] **Step 3: Write three failing tests at the bottom of `bench/src/runner.rs`**

Add this `#[cfg(test)] mod tests` block:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::NamedTempFile;

    #[test]
    fn engine_mode_label_uniqueness() {
        let labels: HashSet<&'static str> = EngineMode::ALL.iter().map(|m| m.label()).collect();
        assert_eq!(labels.len(), EngineMode::ALL.len(), "labels must be distinct");
        for label in &labels {
            assert!(!label.is_empty(), "no label may be empty");
        }
    }

    #[test]
    fn engine_mode_supports_internal_counters() {
        for mode in EngineMode::ALL {
            let expected = matches!(mode, EngineMode::ChiselStrict);
            assert_eq!(mode.supports_internal_counters(), expected,
                "only ChiselStrict reports internal counters; got {mode:?}");
        }
    }

    #[test]
    fn engine_mode_open_each_mode() {
        for mode in EngineMode::ALL {
            let tf = NamedTempFile::new().unwrap();
            let mut engine = mode.open(tf.path(), 256).unwrap();
            // basic sanity: open + begin/commit a no-op tx
            engine.begin().unwrap();
            engine.commit().unwrap();
        }
    }
}
```

- [ ] **Step 4: Run tests, expect compile errors**

Run: `cd bench && cargo test runner::tests`
Expected: compile errors — `cannot find function ALL`, `cannot find method label`, `cannot find method supports_internal_counters`, `cannot find method open` on `EngineMode`.

- [ ] **Step 5: Add the `EngineMode::ALL` const + `label`, `supports_internal_counters`, `open` methods**

Add immediately after the `EngineMode` enum definition:

```rust
impl EngineMode {
    /// All five modes the micro grid measures. The order is the canonical
    /// column order in the markdown summary (PR 5).
    pub const ALL: [Self; 5] = [
        Self::ChiselStrict,
        Self::RedbStrict,
        Self::RedbUnsafe,
        Self::SqliteStrict,
        Self::SqliteUnsafe,
    ];

    /// Stable column label used in BenchmarkIds and JSONL output.
    pub fn label(self) -> &'static str {
        match self {
            Self::ChiselStrict => "chisel-strict",
            Self::RedbStrict => "redb-strict",
            Self::RedbUnsafe => "redb-unsafe",
            Self::SqliteStrict => "sqlite-strict",
            Self::SqliteUnsafe => "sqlite-unsafe",
        }
    }

    /// True iff `Engine::internal_counters()` returns `Some(_)` for this
    /// mode. Currently only `ChiselStrict` (the other engines are
    /// black-box). The post-processor uses this to decide whether to
    /// fill the Chisel-internals appendix table for a cell.
    pub fn supports_internal_counters(self) -> bool {
        matches!(self, Self::ChiselStrict)
    }

    /// Construct a fresh engine of this mode, file-backed at `path` with
    /// the given cache budget. Hides the per-engine constructor
    /// asymmetry: ChiselEngine has no DurabilityMode parameter (always
    /// strict by design), the others do.
    pub fn open(self, path: &Path, cache_size_pages: usize) -> EngineResult<Box<dyn Engine>> {
        match self {
            Self::ChiselStrict => Ok(Box::new(ChiselEngine::open_file(path, cache_size_pages)?)),
            Self::RedbStrict => Ok(Box::new(RedbEngine::open_file(
                path, cache_size_pages, DurabilityMode::Strict,
            )?)),
            Self::RedbUnsafe => Ok(Box::new(RedbEngine::open_file(
                path, cache_size_pages, DurabilityMode::Unsafe,
            )?)),
            Self::SqliteStrict => Ok(Box::new(SqliteEngine::open_file(
                path, cache_size_pages, DurabilityMode::Strict,
            )?)),
            Self::SqliteUnsafe => Ok(Box::new(SqliteEngine::open_file(
                path, cache_size_pages, DurabilityMode::Unsafe,
            )?)),
        }
    }
}
```

- [ ] **Step 6: Run tests, expect pass**

Run: `cd bench && cargo test runner::tests`
Expected: 3 passed.

- [ ] **Step 7: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 8: Commit**

```bash
git add bench/src/runner.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: add EngineMode enum with open/label/counter helpers

EngineMode is the single source of truth for the 5 mode columns of the
micro grid, hiding the per-engine constructor asymmetry (ChiselEngine
takes no DurabilityMode; the others do). EngineMode::ALL gives the
canonical iteration order for registration loops.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Aux-metric types (ChiselCountersDelta, CellId, CellAuxMetrics) + counter_delta helper

**Files:**
- Modify: `bench/src/runner.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Add the four types and helper to `runner.rs`**

Add immediately after the `EngineMode` impl block (before `#[cfg(test)]`):

```rust
use chisel::stats::ChiselCounters;

/// Per-cell deltas of the four Chisel internal counters (master spec
/// §6.1). Reported in the JSONL aux-metrics file for cells where the
/// engine is Chisel; `None` for redb / SQLite.
///
/// Values are subtracted (after - before) and are guaranteed
/// non-negative because counters are cumulative-from-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ChiselCountersDelta {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub fsync_calls: u64,
    pub pages_allocated: u64,
}

/// Identifier of one micro-grid cell — the 3-tuple (row, mode, size).
/// Serialized via `#[serde(flatten)]` into `CellAuxMetrics` so the
/// JSONL line has top-level `row`, `mode`, `size` fields rather than
/// nested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CellId {
    pub row: &'static str,
    pub mode: &'static str,
    pub size: &'static str,
}

/// One JSONL line in `bench/results/aux_metrics.jsonl`. The full payload
/// for one cell of the micro grid: identifier + file-size delta + (for
/// Chisel) counter deltas. PR 5's post-processor reads this alongside
/// Criterion's `estimates.json` to produce the markdown summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CellAuxMetrics {
    #[serde(flatten)]
    pub cell_id: CellId,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

/// Compute the per-field delta between two ChiselCounters snapshots.
/// Returns `None` if either snapshot is `None` (i.e., for non-Chisel
/// engines whose `internal_counters()` returns `Ok(None)`).
///
/// Subtraction uses `saturating_sub` defensively even though the
/// invariant is that counters are cumulative-from-open (so after >=
/// before always). This guards against any future re-architecture that
/// would otherwise produce a debug-build panic.
pub fn counter_delta(
    before: Option<ChiselCounters>,
    after: Option<ChiselCounters>,
) -> Option<ChiselCountersDelta> {
    let (b, a) = before.zip(after)?;
    Some(ChiselCountersDelta {
        cache_hits: a.cache_hits.saturating_sub(b.cache_hits),
        cache_misses: a.cache_misses.saturating_sub(b.cache_misses),
        fsync_calls: a.fsync_calls.saturating_sub(b.fsync_calls),
        pages_allocated: a.pages_allocated.saturating_sub(b.pages_allocated),
    })
}
```

- [ ] **Step 2: Update `bench/src/lib.rs` to re-export the new types**

Replace the `pub use runner::EngineMode;` line with:

```rust
pub use runner::{CellAuxMetrics, CellId, ChiselCountersDelta, EngineMode};
```

(The `counter_delta` function stays as `runner::counter_delta` — not part of the top-level API surface; it's a building block.)

- [ ] **Step 3: Verify build**

Run: `cd bench && cargo build`
Expected: clean compile.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/runner.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: add aux-metric types (CellAuxMetrics, CellId, ChiselCountersDelta)

Plus the counter_delta helper that subtracts two ChiselCounters
snapshots into a ChiselCountersDelta. saturating_sub for defensive
subtraction even though the cumulative-from-open invariant guarantees
non-negative deltas. Types use serde::Serialize derive — PR 5's
deserializer is its own concern.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: AuxMetricsWriter + JSONL format test

**Files:**
- Modify: `bench/src/runner.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `runner.rs`:

```rust
    #[test]
    fn aux_metrics_writer_jsonl_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/aux_metrics.jsonl");
        let mut writer = AuxMetricsWriter::create(&path).unwrap();

        writer.append(&CellAuxMetrics {
            cell_id: CellId { row: "allocate-1pertx", mode: "chisel-strict", size: "32B" },
            file_size_delta_bytes: 262_144,
            counters: Some(ChiselCountersDelta {
                cache_hits: 12,
                cache_misses: 35,
                fsync_calls: 2,
                pages_allocated: 18,
            }),
        }).unwrap();

        writer.append(&CellAuxMetrics {
            cell_id: CellId { row: "allocate-1pertx", mode: "redb-strict", size: "32B" },
            file_size_delta_bytes: 196_608,
            counters: None,
        }).unwrap();

        writer.append(&CellAuxMetrics {
            cell_id: CellId { row: "delete-1pertx", mode: "chisel-strict", size: "1MB" },
            file_size_delta_bytes: -1_048_576,  // delete shrinks
            counters: Some(ChiselCountersDelta {
                cache_hits: 0, cache_misses: 1, fsync_calls: 1, pages_allocated: 0,
            }),
        }).unwrap();

        // Read the file back and verify each line is parseable JSON
        // with the expected schema.
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "exactly 3 lines for 3 appends");

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            // top-level row/mode/size from the flatten attr
            assert!(v.get("row").is_some());
            assert!(v.get("mode").is_some());
            assert!(v.get("size").is_some());
            assert!(v.get("file_size_delta_bytes").is_some());
            // counters key must be present (null or object)
            assert!(v.get("counters").is_some());
        }

        // Spot-check one line in detail.
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["row"], "allocate-1pertx");
        assert_eq!(v["mode"], "chisel-strict");
        assert_eq!(v["size"], "32B");
        assert_eq!(v["file_size_delta_bytes"], 262_144);
        assert_eq!(v["counters"]["cache_hits"], 12);

        // The non-Chisel line should have counters: null.
        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(v2["counters"].is_null());

        // The negative-delta line should serialize as a negative number.
        let v3: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(v3["file_size_delta_bytes"], -1_048_576);
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test runner::tests::aux_metrics_writer_jsonl_format`
Expected: compile error — `cannot find type AuxMetricsWriter`.

- [ ] **Step 3: Implement `AuxMetricsWriter`**

Add to `runner.rs`, immediately after the `counter_delta` function (before `#[cfg(test)]`):

```rust
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};

/// Append-only JSONL writer for the per-cell aux-metrics output.
/// One line per cell. Truncates the file on `create()` so re-runs
/// don't accumulate stale entries; appends thereafter via `append()`.
///
/// The file is opened in buffered mode; every `append()` call ends
/// with `flush()` so a Ctrl-C mid-grid leaves the partial output
/// parseable (each completed cell is one full line).
pub struct AuxMetricsWriter {
    writer: BufWriter<File>,
}

impl AuxMetricsWriter {
    /// Open `path` for write, truncating any prior contents. Creates
    /// parent directories if missing (so the bench can write to
    /// `bench/results/aux_metrics.jsonl` without the runner having to
    /// pre-create `bench/results/`).
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self { writer: BufWriter::new(file) })
    }

    /// Append one cell's metrics as a single JSON line. Flushes after
    /// the write so partial output is parseable on interrupt.
    pub fn append(&mut self, metrics: &CellAuxMetrics) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, metrics)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}
```

- [ ] **Step 4: Update `bench/src/lib.rs` re-exports**

Replace the `pub use` line for runner with:

```rust
pub use runner::{AuxMetricsWriter, CellAuxMetrics, CellId, ChiselCountersDelta, EngineMode};
```

- [ ] **Step 5: Run, expect pass**

Run: `cd bench && cargo test runner::tests::aux_metrics_writer_jsonl_format`
Expected: 1 passed.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/runner.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: add AuxMetricsWriter for per-cell JSONL output

Truncate-on-create + append-with-flush. Each call to append() flushes
so partial output is parseable on Ctrl-C — completed cells are full
JSON lines, incomplete writes stop at line boundaries. Creates parent
directories on create() so callers can pass bench/results/...
without pre-mkdir.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `apply_op` + `drive_workload_with_tx_granularity` helpers

**Files:**
- Modify: `bench/src/runner.rs`

- [ ] **Step 1: Add the two helpers**

Add to `runner.rs`, immediately after `AuxMetricsWriter` (before `#[cfg(test)]`):

```rust
use crate::workload::{Operation, Workload};

/// Translate one Operation into the engine method call that implements
/// it, resolving `alloc_index` references through `snapshot_ids` (for
/// pre-populated records) or `new_ids` (for records allocated earlier
/// in the same iteration). Newly-allocated identifiers are pushed to
/// `new_ids` so subsequent ops in the same iteration can reference them.
///
/// The micro-grid workloads only reference one of the two id sources
/// (alloc workloads have `prepop_count == 0`; read/update/delete
/// workloads have no Allocate ops in their iteration), but a unified
/// resolver keeps this helper general for PR 6 scenarios that mix.
pub fn apply_op(
    engine: &mut dyn Engine,
    op: &Operation,
    snapshot_ids: &[u64],
    new_ids: &mut Vec<Identifier>,
) {
    let resolve = |i: usize| -> Identifier {
        if i < snapshot_ids.len() {
            Identifier(snapshot_ids[i])
        } else {
            new_ids[i - snapshot_ids.len()]
        }
    };
    match op {
        Operation::Allocate { size } => {
            let id = engine.allocate(&vec![0u8; *size]).unwrap();
            new_ids.push(id);
        }
        Operation::Read { alloc_index } => {
            engine.read(resolve(*alloc_index)).unwrap();
        }
        Operation::Update { alloc_index, size } => {
            engine.update(resolve(*alloc_index), &vec![0u8; *size]).unwrap();
        }
        Operation::Delete { alloc_index } => {
            engine.delete(resolve(*alloc_index)).unwrap();
        }
        Operation::DeleteMany { alloc_indices } => {
            let ids: Vec<Identifier> = alloc_indices.iter().map(|&i| resolve(i)).collect();
            engine.delete_many(&ids).unwrap();
        }
    }
}

/// Run the workload's ops against the engine, grouped into transactions
/// of `ops_per_tx` ops each. Used by both the snapshot-restore
/// cell-runner (in the timed routine) and `capture_aux_metrics_*` (in
/// the calibration run).
///
/// `unwrap()` on engine errors is intentional: an engine error inside a
/// timed bench iteration means the bench is broken, and panicking gives
/// a clear stack trace. A user-recoverable error path here would
/// silently corrupt timing measurements.
pub fn drive_workload_with_tx_granularity(
    engine: &mut dyn Engine,
    workload: &Workload,
    ops_per_tx: usize,
    snapshot_ids: &[u64],
) {
    let mut new_ids: Vec<Identifier> = Vec::new();
    for chunk in workload.ops.chunks(ops_per_tx) {
        engine.begin().unwrap();
        for op in chunk {
            apply_op(engine, op, snapshot_ids, &mut new_ids);
        }
        engine.commit().unwrap();
    }
}
```

- [ ] **Step 2: Verify build (no direct tests; smoke test in task 12 covers)**

Run: `cd bench && cargo build && cargo test`
Expected: clean compile, all 30+ existing tests still pass.

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 4: Commit**

```bash
git add bench/src/runner.rs
git commit -m "$(cat <<'EOF'
bench: add apply_op + drive_workload_with_tx_granularity helpers

apply_op routes Operation variants to engine methods, resolving
alloc_index through snapshot_ids ∪ new_ids. drive wraps batches of
ops_per_tx in begin/commit. Both used by the cell-runners
(in timed routines) and the aux-metrics calibration runs.
unwrap() is intentional: engine errors inside a bench iteration mean
the bench is broken, and a panic gives a stack trace immediately.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `PopulatedSnapshot` + `populate_snapshot` + 3 per-engine tests

**Files:**
- Modify: `bench/src/runner.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn populate_snapshot_chisel_basic() {
        let snap = populate_snapshot(EngineMode::ChiselStrict, 256, 100).unwrap();
        assert_eq!(snap.ids().len(), 100);
        assert!(std::fs::metadata(snap.path()).unwrap().len() > 0);
    }

    #[test]
    fn populate_snapshot_redb_basic() {
        let snap = populate_snapshot(EngineMode::RedbStrict, 256, 100).unwrap();
        assert_eq!(snap.ids().len(), 100);
        assert!(std::fs::metadata(snap.path()).unwrap().len() > 0);
    }

    #[test]
    fn populate_snapshot_sqlite_basic() {
        let snap = populate_snapshot(EngineMode::SqliteStrict, 256, 100).unwrap();
        assert_eq!(snap.ids().len(), 100);
        assert!(std::fs::metadata(snap.path()).unwrap().len() > 0);
    }
```

- [ ] **Step 2: Run, expect compile errors**

Run: `cd bench && cargo test runner::tests::populate_snapshot`
Expected: compile errors — `cannot find type PopulatedSnapshot`, `cannot find function populate_snapshot`, `no method ids` / `path`.

- [ ] **Step 3: Implement `PopulatedSnapshot` and `populate_snapshot`**

Add to `runner.rs`, immediately after `drive_workload_with_tx_granularity` (before `#[cfg(test)]`):

```rust
use tempfile::NamedTempFile;

/// A populated database file paired with its alloc-order → engine-id map.
///
/// The file is a `tempfile::NamedTempFile` so it auto-deletes on drop.
/// `ids()` returns the identifiers in allocation order — element `i` is
/// the engine identifier that the `i`-th `gen_prepopulate` Allocate
/// produced. Workloads reference records by `alloc_index` (per PR 4a's
/// contract); the cell-runners pass `ids()` through to `apply_op` to
/// resolve those indices to engine identifiers.
pub struct PopulatedSnapshot {
    file: NamedTempFile,
    ids: Vec<u64>,
}

impl PopulatedSnapshot {
    pub fn path(&self) -> &Path {
        self.file.path()
    }
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }
}

/// Build a fresh DB file populated with `prepop_count` records of `size_bytes`
/// each, capturing the engine-assigned identifiers in allocation order.
///
/// Pre-population uses one `begin/.../commit` block, not one tx per op —
/// matches scenario-style allocation more closely and is much faster
/// for large counts. Returns the snapshot ready for the cell-runners
/// to copy-and-restore from.
///
/// The returned `PopulatedSnapshot` owns the file via `NamedTempFile`;
/// callers must keep it alive until all cells using its `path()` are
/// done. (The micro-grid bench file pattern naturally does this — the
/// snapshot is created at the top of the per-(mode, size) block and
/// drops at the end, after the cell-runner returns.)
pub fn populate_snapshot(
    mode: EngineMode,
    size_bytes: usize,
    prepop_count: usize,
) -> EngineResult<PopulatedSnapshot> {
    let file = NamedTempFile::new()?;
    let mut engine = mode.open(file.path(), super::CACHE_SIZE_PAGES)?;
    let mut ids = Vec::with_capacity(prepop_count);
    let payload = vec![0u8; size_bytes];

    engine.begin()?;
    for _ in 0..prepop_count {
        let id = engine.allocate(&payload)?;
        ids.push(id.0);
    }
    engine.commit()?;
    drop(engine);  // explicit close before returning the file

    Ok(PopulatedSnapshot { file, ids })
}
```

Note: this references `super::CACHE_SIZE_PAGES` — which doesn't exist yet. We add it at the top of `runner.rs`:

```rust
/// Page-cache budget passed to every engine when constructing for the
/// micro grid. 256 pages × 8 KB = 2 MB ≈ 8% of the 25 MB raw payload
/// per cell, so random-access workloads will miss frequently. See spec
/// §6.1.
pub const CACHE_SIZE_PAGES: usize = 256;
```

This goes near the top of `runner.rs`, immediately after the `use` statements and before the `EngineMode` enum.

Update the `populate_snapshot` body to reference the just-added const:

```rust
let mut engine = mode.open(file.path(), CACHE_SIZE_PAGES)?;
```

(Drop the `super::` — `CACHE_SIZE_PAGES` is in the same module.)

- [ ] **Step 4: Update `bench/src/lib.rs` re-exports**

Replace the `pub use` line for runner with:

```rust
pub use runner::{
    AuxMetricsWriter, CACHE_SIZE_PAGES, CellAuxMetrics, CellId, ChiselCountersDelta,
    EngineMode, PopulatedSnapshot,
};
```

- [ ] **Step 5: Run tests, expect pass**

Run: `cd bench && cargo test runner::tests::populate_snapshot`
Expected: 3 passed.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/runner.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: add PopulatedSnapshot + populate_snapshot

Builds a fresh tempfile-backed DB with N records of size S, capturing
engine-assigned identifiers in alloc order. Tested per-engine
(Chisel, redb, SQLite) for shape (ids().len() == prepop_count) +
non-empty file. CACHE_SIZE_PAGES = 256 const lands here too — used
by populate_snapshot and (later) the cell-runners.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `capture_aux_metrics_snapshot_restore` + `capture_aux_metrics_warm_read`

**Files:**
- Modify: `bench/src/runner.rs`

- [ ] **Step 1: Add both functions**

Add to `runner.rs`, immediately after `populate_snapshot` (before `#[cfg(test)]`):

```rust
/// Capture per-cell aux metrics for the snapshot-restore-style rows
/// (8 of 9 rows: rows 1, 2, 4–9). One calibration iteration: copy
/// the snapshot, open the engine, snapshot counters + file size,
/// drive the workload, snapshot again, return deltas.
///
/// Panics on engine errors during the calibration run — same rationale
/// as `drive_workload_with_tx_granularity`: an engine error during
/// bench setup means the bench is broken.
pub fn capture_aux_metrics_snapshot_restore(
    cell_id: CellId,
    mode: EngineMode,
    snapshot_path: &Path,
    snapshot_ids: &[u64],
    workload: &Workload,
    ops_per_tx: usize,
) -> CellAuxMetrics {
    let working = NamedTempFile::new().unwrap();
    std::fs::copy(snapshot_path, working.path()).unwrap();
    let mut engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();

    let counters_before = engine.internal_counters().unwrap();
    let size_before = engine.file_size_bytes().unwrap();

    drive_workload_with_tx_granularity(&mut *engine, workload, ops_per_tx, snapshot_ids);

    let counters_after = engine.internal_counters().unwrap();
    let size_after = engine.file_size_bytes().unwrap();

    CellAuxMetrics {
        cell_id,
        file_size_delta_bytes: (size_after as i64) - (size_before as i64),
        counters: counter_delta(counters_before, counters_after),
    }
}

/// Capture per-cell aux metrics for the warm-read row (row 3 only).
/// Runs against the SAME persistent engine the cell-runner just used
/// for measurements — cache is warm, OS page cache is warm. Captures
/// counters + size around one extra read at this state, giving the
/// steady-state warm-cache counter activity (mostly cache hits).
///
/// Doing snapshot-restore-style calibration for warm-read would
/// produce *cold* counter activity, contradicting the row's name and
/// purpose. See spec §4.2.
pub fn capture_aux_metrics_warm_read(
    cell_id: CellId,
    engine: &mut dyn Engine,
    workload: &Workload,
    snapshot_ids: &[u64],
) -> CellAuxMetrics {
    let counters_before = engine.internal_counters().unwrap();
    let size_before = engine.file_size_bytes().unwrap();

    apply_op(engine, &workload.ops[0], snapshot_ids, &mut Vec::new());

    let counters_after = engine.internal_counters().unwrap();
    let size_after = engine.file_size_bytes().unwrap();

    CellAuxMetrics {
        cell_id,
        file_size_delta_bytes: (size_after as i64) - (size_before as i64),
        counters: counter_delta(counters_before, counters_after),
    }
}
```

- [ ] **Step 2: Verify build (no direct tests; smoke test in task 12 covers)**

Run: `cd bench && cargo build && cargo test`
Expected: clean compile, all existing tests still pass.

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 4: Commit**

```bash
git add bench/src/runner.rs
git commit -m "$(cat <<'EOF'
bench: add capture_aux_metrics_{snapshot_restore,warm_read}

Two calibration paths because warm-read row 3 (persistent engine)
needs counter capture against its actual warm state, not against
a fresh snapshot-restore (which would produce cold counter values
contradicting the row's purpose). Each path is a single calibration
iteration outside Criterion's measurement window.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `micro_grid.rs` scaffold (constants, `seed_for`, empty `micro_grid`, `criterion_main!`) + `[[bench]]` declaration

**Files:**
- Create: `bench/benches/micro_grid.rs`
- Modify: `bench/Cargo.toml`

- [ ] **Step 1a: Create the bench file with constants and stub**

(File MUST be created before adding the `[[bench]]` block to Cargo.toml — otherwise cargo's manifest parser rejects the manifest, blocking every cargo command. Create the file first, then add the manifest entry in Step 1b.)


Create `bench/benches/micro_grid.rs` with:

```rust
// Bench binary: the 270-cell micro grid. Iterates EngineMode::ALL × SIZES
// × the 9 row groups, registering each cell as a Criterion benchmark
// inside a per-row BenchmarkGroup with Throughput::Elements(N) for
// per-op normalization. Aux metrics (file-size delta + Chisel internal
// counter deltas) are captured per cell into bench/results/aux_metrics.jsonl.
//
// The three Criterion-shaped cell-runner helpers (run_*_cell) live here
// rather than in src/runner.rs because Criterion is in [dev-dependencies]
// and src/ code can't import dev-deps. The helpers are private.
//
// Run the full grid: `cargo bench --bench micro_grid`. Filter to one row:
// `cargo bench --bench micro_grid read-warm`.

use chisel_bench::Engine;
use chisel_bench::runner::{
    AuxMetricsWriter, CellId, EngineMode, apply_op,
    capture_aux_metrics_snapshot_restore, capture_aux_metrics_warm_read,
    drive_workload_with_tx_granularity, populate_snapshot, CACHE_SIZE_PAGES,
};
use chisel_bench::workload::{
    Workload, gen_allocate, gen_delete_many, gen_delete_random,
    gen_read_random, gen_update_random,
};
use criterion::{
    BatchSize, BenchmarkGroup, BenchmarkId, Criterion, Throughput,
    criterion_group, criterion_main, measurement::WallTime,
};
use tempfile::NamedTempFile;

/// Six log-spaced sizes, one per Chisel internal regime (master spec §3.2).
/// `prepop_count` calibrated to ~25 MB raw payload per cell (master spec §3.4).
const SIZES: [(usize, &str, usize); 6] = [
    (32,        "32B",    800_000),
    (256,       "256B",   100_000),
    (2_048,     "2KB",    12_500),
    (16_384,    "16KB",   1_500),
    (131_072,   "128KB",  200),
    (1_048_576, "1MB",    25),
];

/// Hardcoded per-row seeds for workload determinism. Hardcoded rather
/// than derived from row names because Rust's DefaultHasher randomizes
/// per-process — derived seeds would change between invocations.
fn seed_for(row_name: &str) -> u64 {
    match row_name {
        "allocate-1pertx" => 0x4001,
        "allocate-1000pertx" => 0x4002,
        "read-warm" => 0x4003,
        "read-cold" => 0x4004,
        "update-1pertx" => 0x4005,
        "update-1000pertx" => 0x4006,
        "delete-1pertx" => 0x4007,
        "delete-1000pertx" => 0x4008,
        "delete_many" => 0x4009,
        _ => panic!("unknown row name: {row_name}"),
    }
}

fn micro_grid(c: &mut Criterion) {
    let _aux = AuxMetricsWriter::create("bench/results/aux_metrics.jsonl").unwrap();
    // Row-bench function calls land in tasks 10 and 11.
    let _ = c;
}

criterion_group!(benches, micro_grid);
criterion_main!(benches);
```

The `let _aux = ...` and `let _ = c;` are placeholders so the function compiles. Tasks 10 and 11 will replace the body with the real row-bench calls.

- [ ] **Step 1b: Add the `[[bench]]` block to `bench/Cargo.toml`**

Now that the file exists, add the `[[bench]]` declaration. Append to the end of `bench/Cargo.toml` (after the `[dev-dependencies]` block):

```toml

[[bench]]
name = "micro_grid"
harness = false
```

The `harness = false` line is required for Criterion. Without it, Cargo links the unstable `libtest` benchmark harness and you get cryptic linker errors. (This block was deferred from Task 1 because cargo's manifest parser rejects `[[bench]]` declarations whose target file doesn't exist yet — it must land together with the file.)

- [ ] **Step 2: Verify the bench target compiles**

Run: `cd bench && cargo bench --no-run 2>&1 | tail -5`
Expected: clean compile, "Finished" line. The bench binary is built but no benchmarks register.

- [ ] **Step 3: Verify the bench binary runs (does nothing useful but exits 0)**

Run: `cd bench && cargo bench --bench micro_grid -- --quick 2>&1 | tail -5`
Expected: Criterion runs, finds zero benches in the group, exits 0. The `--quick` flag is built-in (Criterion 0.5+) and short-circuits sample size for fast smoke verification.

- [ ] **Step 4: Verify `bench/results/aux_metrics.jsonl` was created (truncated empty)**

Run: `ls -la bench/results/aux_metrics.jsonl && wc -l bench/results/aux_metrics.jsonl`
Expected: file exists, 0 lines.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff. Note: clippy may flag unused imports — that's expected because the row-bench functions in tasks 10/11 will use them. Ignore via `#[allow(unused_imports)]` ONLY if clippy fires; otherwise leave as-is.

If clippy DOES fire on unused imports, add this attribute to the top of `micro_grid.rs` immediately after the file header:

```rust
#![allow(unused_imports)]   // tasks 10/11 will use these row-bench imports
```

- [ ] **Step 6: Commit**

```bash
git add bench/benches/micro_grid.rs bench/Cargo.toml
git commit -m "$(cat <<'EOF'
bench: scaffold micro_grid bench binary + [[bench]] declaration

Constants (SIZES, seed_for), imports, criterion_main! glue, plus the
[[bench]] target block in Cargo.toml (deferred from task 1 because
the manifest parser rejects [[bench]] entries with no file). The
micro_grid function body is empty — tasks 10/11 will fill in
row-bench calls. With this commit, cargo bench --bench micro_grid
succeeds and creates bench/results/aux_metrics.jsonl as an empty file.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Three cell-runner helpers in `micro_grid.rs`

**Files:**
- Modify: `bench/benches/micro_grid.rs`

- [ ] **Step 1: Add the three private cell-runner helpers**

Insert immediately after `seed_for` (before `fn micro_grid`):

```rust
/// Snapshot-restore cell-runner — used by 8 of 9 rows (allocate, cold-read,
/// update, delete, delete_many). Each iteration copies the pre-built
/// snapshot, opens a fresh engine, runs the workload's ops grouped into
/// transactions of `ops_per_tx`, then drops engine + tempfile.
fn run_snapshot_restore_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    snapshot_path: &std::path::Path,
    snapshot_ids: &[u64],
    workload: &Workload,
    ops_per_tx: usize,
) {
    group.bench_with_input(
        BenchmarkId::new(mode.label(), size_label),
        &(),
        |b, _| {
            b.iter_batched(
                || {
                    let working = NamedTempFile::new().unwrap();
                    std::fs::copy(snapshot_path, working.path()).unwrap();
                    let engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                    (engine, working)
                },
                |(mut engine, _working)| {
                    drive_workload_with_tx_granularity(
                        &mut *engine, workload, ops_per_tx, snapshot_ids,
                    );
                },
                BatchSize::PerIteration,
            );
        },
    );
}

/// Warm-read cell-runner — row 3 only. Engine is opened once per cell
/// and reused across all iterations; the cache warms naturally during
/// Criterion's warmup phase. Reads don't mutate engine-visible state,
/// so persistent engine is safe.
fn run_warm_read_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    engine: &mut dyn Engine,
    workload: &Workload,
    snapshot_ids: &[u64],
) {
    group.bench_with_input(
        BenchmarkId::new(mode.label(), size_label),
        &(),
        |b, _| {
            b.iter(|| {
                for op in &workload.ops {
                    apply_op(engine, op, snapshot_ids, &mut Vec::new());
                }
            });
        },
    );
}

/// Cold-read cell-runner — row 4 only. Engine open is INSIDE the timed
/// routine: cold means "fresh open, no values touched, first read is
/// the timed call" (master spec §5.2). File copy stays in setup.
fn run_cold_read_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    snapshot_path: &std::path::Path,
    snapshot_ids: &[u64],
    workload: &Workload,
) {
    group.bench_with_input(
        BenchmarkId::new(mode.label(), size_label),
        &(),
        |b, _| {
            b.iter_batched(
                || {
                    let working = NamedTempFile::new().unwrap();
                    std::fs::copy(snapshot_path, working.path()).unwrap();
                    working
                },
                |working| {
                    let mut engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                    apply_op(&mut *engine, &workload.ops[0], snapshot_ids, &mut Vec::new());
                },
                BatchSize::PerIteration,
            );
        },
    );
}
```

- [ ] **Step 2: Verify build**

Run: `cd bench && cargo bench --no-run 2>&1 | tail -5`
Expected: clean compile.

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff. (The functions ARE unused at this point — clippy may flag them. If it does, the `#![allow(unused_imports)]` from task 8 should be widened to `#![allow(unused_imports, dead_code)]`. Tasks 10/11 will use them.)

If clippy fires on dead_code: update the top-of-file attribute to:
```rust
#![allow(unused_imports, dead_code)]   // tasks 10/11 will exercise these
```

- [ ] **Step 4: Commit**

```bash
git add bench/benches/micro_grid.rs
git commit -m "$(cat <<'EOF'
bench: add the three cell-runner helpers

run_snapshot_restore_cell (8 rows), run_warm_read_cell (row 3),
run_cold_read_cell (row 4). Three iteration patterns, three private
helpers — one per genuinely-different setup/routine shape. The
warm-read helper takes a persistent engine reference (caller owns it,
keeps it warm); the others take a snapshot path and copy per iteration.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Row-bench functions, batch 1 (allocate × 2, read-warm, read-cold)

**Files:**
- Modify: `bench/benches/micro_grid.rs`

- [ ] **Step 1: Add the four row-bench functions**

Insert immediately after `run_cold_read_cell` (before `fn micro_grid`):

```rust
/// Rows 1 and 2: allocate, 1-per-tx and 1000-per-tx.
/// Empty pre-populated DB; workload is `ops_per_tx` Allocate ops; cell
/// runs one tx of `ops_per_tx` ops. Throughput::Elements(ops_per_tx).
fn bench_row_allocate_n_per_tx(
    c: &mut Criterion,
    aux: &mut AuxMetricsWriter,
    group_name: &str,
    ops_per_tx: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(ops_per_tx as u64));

    for (size_bytes, size_label, _) in SIZES {
        let workload = gen_allocate(ops_per_tx, size_bytes);
        for mode in EngineMode::ALL {
            // "Empty snapshot" = a fresh tempfile with a freshly-opened-and-closed
            // engine. populate_snapshot with prepop_count=0 gives exactly that.
            let snap = populate_snapshot(mode, size_bytes, 0).unwrap();
            run_snapshot_restore_cell(
                &mut group, mode, size_label, snap.path(), snap.ids(),
                &workload, ops_per_tx,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId { row: leak_str(group_name), mode: mode.label(), size: size_label },
                mode, snap.path(), snap.ids(), &workload, ops_per_tx,
            )).unwrap();
        }
    }

    group.finish();
}

/// Row 3: read warm. Persistent engine across iterations — cache warms
/// naturally. Workload is 64 random reads (cycled per iteration).
fn bench_row_read_warm(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    let mut group = c.benchmark_group("read-warm");
    group.throughput(Throughput::Elements(64));

    for (size_bytes, size_label, prepop_count) in SIZES {
        let workload = gen_read_random(seed_for("read-warm"), prepop_count, 64);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            let mut engine = mode.open(snap.path(), CACHE_SIZE_PAGES).unwrap();
            run_warm_read_cell(
                &mut group, mode, size_label, &mut *engine, &workload, snap.ids(),
            );
            aux.append(&capture_aux_metrics_warm_read(
                CellId { row: "read-warm", mode: mode.label(), size: size_label },
                &mut *engine, &workload, snap.ids(),
            )).unwrap();
            // engine drops here, before snap drops at end of for body
        }
    }

    group.finish();
}

/// Row 4: read cold. Fresh engine opened inside the timed routine; "cold"
/// means first read after open. Workload is 1 read.
fn bench_row_read_cold(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    let mut group = c.benchmark_group("read-cold");
    group.throughput(Throughput::Elements(1));

    for (size_bytes, size_label, prepop_count) in SIZES {
        let workload = gen_read_random(seed_for("read-cold"), prepop_count, 1);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            run_cold_read_cell(
                &mut group, mode, size_label, snap.path(), snap.ids(), &workload,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId { row: "read-cold", mode: mode.label(), size: size_label },
                mode, snap.path(), snap.ids(), &workload, /*ops_per_tx*/ 1,
            )).unwrap();
        }
    }

    group.finish();
}

/// `CellId.row` is `&'static str`. The two allocate rows have group names
/// passed in dynamically as a parameter — we leak them to satisfy the
/// `'static` requirement. There are exactly 2 such leaks per process
/// (one per allocate row); negligible memory.
fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}
```

- [ ] **Step 2: Wire the four functions into `micro_grid`**

Replace the body of the `micro_grid` function (the `let _aux = ...; let _ = c;` placeholder) with:

```rust
fn micro_grid(c: &mut Criterion) {
    let mut aux = AuxMetricsWriter::create("bench/results/aux_metrics.jsonl").unwrap();

    bench_row_allocate_n_per_tx(c, &mut aux, "allocate-1pertx", 1);
    bench_row_allocate_n_per_tx(c, &mut aux, "allocate-1000pertx", 1000);
    bench_row_read_warm(c, &mut aux);
    bench_row_read_cold(c, &mut aux);
    // Tasks 11 will add the remaining 5 calls (update × 2, delete × 2, delete_many).
}
```

If `#![allow(unused_imports, dead_code)]` was added in tasks 8/9, narrow it to just `#![allow(dead_code)]` (or remove entirely if clippy is now happy — task 11 adds the remaining usages).

- [ ] **Step 3: Verify build**

Run: `cd bench && cargo bench --no-run 2>&1 | tail -5`
Expected: clean compile.

- [ ] **Step 4: Verify the bench runs end-to-end with --quick**

Run: `cd bench && cargo bench --bench micro_grid -- --quick 2>&1 | tail -20`
Expected: Criterion runs the 4 row groups (allocate-1pertx, allocate-1000pertx, read-warm, read-cold), reports per-group results. The total at the end should mention 4 × 6 sizes × 5 modes = 120 cells. `--quick` short-circuits sample size so this completes in seconds.

- [ ] **Step 5: Verify aux_metrics.jsonl has entries**

Run: `wc -l bench/results/aux_metrics.jsonl`
Expected: 120 lines (4 rows × 6 sizes × 5 modes).

Run: `head -3 bench/results/aux_metrics.jsonl | python3 -c 'import sys,json; [print(json.loads(l)) for l in sys.stdin]'`
Expected: each line parses as a JSON object with keys `row`, `mode`, `size`, `file_size_delta_bytes`, `counters`. The `chisel-strict` lines have non-null `counters`, the others have `counters: null`.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/benches/micro_grid.rs
git commit -m "$(cat <<'EOF'
bench: add row-bench functions for allocate, read-warm, read-cold

Four row-bench functions (allocate is parameterized over ops_per_tx
so 1pertx + 1000pertx share one function). At this point the bench
binary registers 120 of 270 cells and exercises all three iteration
patterns (snapshot-restore, warm-read, cold-read). aux_metrics.jsonl
populates with 120 entries after a --quick run.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Row-bench functions, batch 2 (update × 2, delete × 2, delete_many)

**Files:**
- Modify: `bench/benches/micro_grid.rs`

- [ ] **Step 1: Add three more row-bench functions**

Insert immediately after `bench_row_read_cold` (before `leak_str`):

```rust
/// Rows 5 and 6: update, 1-per-tx and 1000-per-tx.
fn bench_row_update_n_per_tx(
    c: &mut Criterion,
    aux: &mut AuxMetricsWriter,
    group_name: &str,
    ops_per_tx: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(ops_per_tx as u64));

    for (size_bytes, size_label, prepop_count) in SIZES {
        let workload = gen_update_random(
            seed_for(group_name), prepop_count, ops_per_tx, size_bytes,
        );
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            run_snapshot_restore_cell(
                &mut group, mode, size_label, snap.path(), snap.ids(),
                &workload, ops_per_tx,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId { row: leak_str(group_name), mode: mode.label(), size: size_label },
                mode, snap.path(), snap.ids(), &workload, ops_per_tx,
            )).unwrap();
        }
    }

    group.finish();
}

/// Rows 7 and 8: delete, 1-per-tx and 1000-per-tx.
fn bench_row_delete_n_per_tx(
    c: &mut Criterion,
    aux: &mut AuxMetricsWriter,
    group_name: &str,
    ops_per_tx: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(ops_per_tx as u64));

    for (size_bytes, size_label, prepop_count) in SIZES {
        // delete generator panics if count > prepop_count; assert at the
        // SIZES level. For the 1MB row, prepop_count=25 and ops_per_tx
        // can be at most 25 — but ops_per_tx is 1 or 1000. Skip cells
        // where ops_per_tx > prepop_count by clamping the workload's
        // count to prepop_count. The reported throughput stays
        // Throughput::Elements(ops_per_tx) for cross-row comparability;
        // the actual measured time scales with the (smaller) clamped count.
        let workload_count = ops_per_tx.min(prepop_count);
        if workload_count == 0 { continue; }
        let workload = gen_delete_random(seed_for(group_name), prepop_count, workload_count);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            run_snapshot_restore_cell(
                &mut group, mode, size_label, snap.path(), snap.ids(),
                &workload, ops_per_tx,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId { row: leak_str(group_name), mode: mode.label(), size: size_label },
                mode, snap.path(), snap.ids(), &workload, ops_per_tx,
            )).unwrap();
        }
    }

    group.finish();
}

/// Row 9: delete_many bulk. One DeleteMany op per iteration carrying
/// 1000 distinct ids. Throughput::Elements(1000) since one bulk call
/// deletes 1000 records.
fn bench_row_delete_many(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    let mut group = c.benchmark_group("delete_many");
    group.throughput(Throughput::Elements(1000));

    for (size_bytes, size_label, prepop_count) in SIZES {
        let batch_size = 1000.min(prepop_count);
        if batch_size == 0 { continue; }
        let workload = gen_delete_many(
            seed_for("delete_many"), prepop_count, /*batches*/ 1, batch_size,
        );
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            run_snapshot_restore_cell(
                &mut group, mode, size_label, snap.path(), snap.ids(),
                &workload, /*ops_per_tx*/ 1,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId { row: "delete_many", mode: mode.label(), size: size_label },
                mode, snap.path(), snap.ids(), &workload, 1,
            )).unwrap();
        }
    }

    group.finish();
}
```

- [ ] **Step 2: Wire all 9 row functions into `micro_grid`**

Replace the body of `micro_grid` with all 9 calls:

```rust
fn micro_grid(c: &mut Criterion) {
    let mut aux = AuxMetricsWriter::create("bench/results/aux_metrics.jsonl").unwrap();

    bench_row_allocate_n_per_tx(c, &mut aux, "allocate-1pertx", 1);
    bench_row_allocate_n_per_tx(c, &mut aux, "allocate-1000pertx", 1000);
    bench_row_read_warm(c, &mut aux);
    bench_row_read_cold(c, &mut aux);
    bench_row_update_n_per_tx(c, &mut aux, "update-1pertx", 1);
    bench_row_update_n_per_tx(c, &mut aux, "update-1000pertx", 1000);
    bench_row_delete_n_per_tx(c, &mut aux, "delete-1pertx", 1);
    bench_row_delete_n_per_tx(c, &mut aux, "delete-1000pertx", 1000);
    bench_row_delete_many(c, &mut aux);
}
```

Remove any remaining `#![allow(...)]` attributes — everything is now used.

- [ ] **Step 3: Verify build**

Run: `cd bench && cargo bench --no-run 2>&1 | tail -5`
Expected: clean compile.

- [ ] **Step 4: Verify the bench runs end-to-end with --quick**

Run: `cd bench && cargo bench --bench micro_grid -- --quick 2>&1 | tail -10`
Expected: all 9 row groups run; total ~270 cells (some delete cells may be skipped at 1MB size due to clamping). `--quick` finishes in under a minute.

- [ ] **Step 5: Verify aux_metrics.jsonl has all entries**

Run: `wc -l bench/results/aux_metrics.jsonl`
Expected: 240 lines, NOT 270. The cells where `ops_per_tx * size_bytes > 8 MB` are skipped to avoid overflowing Chisel's per-tx cache ceiling (256 pages × 8x hard ceiling = 16 MB; we leave 50% headroom for COW overhead). This affects:
- allocate-1000pertx at 16KB, 128KB, 1MB (3 sizes × 5 modes = 15 cells skipped, applied in Task 10's `bench_row_allocate_n_per_tx`)
- update-1000pertx at 16KB, 128KB, 1MB (15 cells skipped, applied in this task's `bench_row_update_n_per_tx`)
Total skipped: 30. Total emitted: 240.

Implement the skip via the same idiom in both row functions: a `cell_fits_in_cache` helper inside the row function, e.g.,
```rust
const TX_BUDGET_BYTES: usize = 8 * 1024 * 1024;
if ops_per_tx * size_bytes > TX_BUDGET_BYTES { continue; }
```

Run: `cut -d',' -f1 bench/results/aux_metrics.jsonl | sort | uniq -c`
Expected per-row counts: allocate-1pertx 30, allocate-1000pertx 15, read-warm 30, read-cold 30, update-1pertx 30, update-1000pertx 15, delete-1pertx 30, delete-1000pertx 30, delete_many 30. Total 240.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/benches/micro_grid.rs
git commit -m "$(cat <<'EOF'
bench: add row-bench functions for update, delete, delete_many

Three more row-bench functions complete the 9-row grid: update (×2 via
ops_per_tx), delete (×2), and delete_many (single bulk op). delete and
delete_many clamp their count to prepop_count so the 1MB row (prepop=25)
doesn't violate gen_delete_random's count <= prepop_count assertion;
Throughput::Elements stays at the unclamped ops_per_tx for cross-row
comparability. After this, --quick runs all 270 cells.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: End-to-end smoke test in `tests/runner_smoke.rs`

**Files:**
- Create: `bench/tests/runner_smoke.rs`

- [ ] **Step 1: Create the smoke test**

Create `bench/tests/runner_smoke.rs` with:

```rust
// Integration smoke test for the runner machinery. Confirms one cell
// runs end-to-end against a minimal Criterion::default() — exercises
// the snapshot-restore cell-runner, populate_snapshot, apply_op, and
// drive_workload_with_tx_granularity in a single test.
//
// This is NOT a real benchmark — it runs at sample_size = 10, in
// well under a second. Its purpose is to catch dumb mistakes
// (off-by-one in chunking, wrong snapshot path handling, panics on
// engine error, etc.) without paying full bench-grid cost.

use chisel_bench::runner::{
    capture_aux_metrics_snapshot_restore, drive_workload_with_tx_granularity,
    populate_snapshot, AuxMetricsWriter, CellId, EngineMode, CACHE_SIZE_PAGES,
};
use chisel_bench::workload::gen_allocate;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use tempfile::NamedTempFile;

#[test]
fn smoke_run_one_snapshot_restore_cell() {
    // Build a Criterion configured for the smallest practical run.
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(std::time::Duration::from_millis(50))
        .measurement_time(std::time::Duration::from_millis(200));

    // Empty snapshot for an allocate workload.
    let snap = populate_snapshot(EngineMode::ChiselStrict, 256, 0).unwrap();
    let workload = gen_allocate(/*count*/ 4, /*size*/ 256);

    {
        let mut group = c.benchmark_group("smoke");
        group.throughput(Throughput::Elements(4));
        let snapshot_path = snap.path().to_path_buf();
        let snapshot_ids = snap.ids().to_vec();
        let workload_ref = &workload;
        group.bench_with_input(
            BenchmarkId::new("chisel-strict", "256B"),
            &(),
            |b, _| {
                b.iter_batched(
                    || {
                        let working = NamedTempFile::new().unwrap();
                        std::fs::copy(&snapshot_path, working.path()).unwrap();
                        let engine = EngineMode::ChiselStrict
                            .open(working.path(), CACHE_SIZE_PAGES)
                            .unwrap();
                        (engine, working)
                    },
                    |(mut engine, _working)| {
                        drive_workload_with_tx_granularity(
                            &mut *engine, workload_ref, /*ops_per_tx*/ 1, &snapshot_ids,
                        );
                    },
                    BatchSize::PerIteration,
                );
            },
        );
        group.finish();
    }

    // Aux-metrics path: write to a tempfile, capture one cell, verify
    // we got a parseable line.
    let dir = tempfile::tempdir().unwrap();
    let aux_path = dir.path().join("aux.jsonl");
    let mut aux = AuxMetricsWriter::create(&aux_path).unwrap();
    aux.append(&capture_aux_metrics_snapshot_restore(
        CellId { row: "smoke", mode: "chisel-strict", size: "256B" },
        EngineMode::ChiselStrict, snap.path(), snap.ids(), &workload, /*ops_per_tx*/ 1,
    )).unwrap();
    drop(aux);  // flush

    let contents = std::fs::read_to_string(&aux_path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["row"], "smoke");
    assert_eq!(v["mode"], "chisel-strict");
    assert!(v["counters"].is_object(), "ChiselStrict should produce non-null counters");
}
```

This needs `serde_json` to read back the JSONL. Add to `bench/Cargo.toml`'s `[dev-dependencies]` if not already present:

Looking at task 1's Cargo.toml: `serde_json` is in `[dependencies]` already (used by runner.rs). Integration tests in `tests/` see both `[dependencies]` and `[dev-dependencies]`, so no Cargo.toml change is needed here.

- [ ] **Step 2: Run the test**

Run: `cd bench && cargo test --test runner_smoke`
Expected: 1 passed in well under a second.

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 4: Commit**

```bash
git add bench/tests/runner_smoke.rs
git commit -m "$(cat <<'EOF'
bench: add end-to-end smoke test for runner machinery

One cell runs against Criterion::default() with sample_size=10,
exercising the snapshot-restore cell-runner inline (not via the
private helper in micro_grid.rs, which integration tests can't reach)
plus populate_snapshot, apply_op, drive_workload_with_tx_granularity,
capture_aux_metrics_snapshot_restore, and AuxMetricsWriter. Catches
dumb mistakes without paying full grid cost.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Final acceptance verification

**Files:**
- Read-only checks across the bench subcrate.

- [ ] **Step 1: Verify all unit tests pass**

Run: `cd bench && cargo test 2>&1 | tail -10`
Expected: all tests pass. Counts should be: 7 new runner unit tests + 14 PR 4a workload tests + 15 PR 3 equivalence tests + 1 prior smoke + 1 new smoke = 38 tests. No failures.

- [ ] **Step 2: Verify the engine-agnostic invariant for `runner.rs`**

The rule: `bench/src/runner.rs` may import from `crate::engine`, `crate::workload`, `crate::ChiselEngine`, etc., but should NOT import directly from external engine crates `chisel`, `redb`, `rusqlite` — those go through the trait abstraction.

Run: `grep -nE "^use (chisel|redb|rusqlite)" bench/src/runner.rs`
Expected: one match — `use chisel::stats::ChiselCounters;` at the top of the file (needed for `ChiselCountersDelta`'s field-by-field subtraction). This is acceptable: it's a type re-export, not a usage of the engine API. No `use redb::*` or `use rusqlite::*` should appear.

If `grep` finds anything beyond `use chisel::stats::ChiselCounters;`, the abstraction has leaked.

- [ ] **Step 3: Run clippy with deny-warnings**

Run: `cd bench && cargo clippy --all-targets -- -D warnings`
Expected: no output (clippy clean across all targets — library, tests, and the bench binary).

- [ ] **Step 4: Run `cargo fmt --check`**

Run: `cargo fmt -- --check` (from the repo root)
Expected: no diff.

- [ ] **Step 5: Run the full bench --quick**

Run: `cd bench && cargo bench --bench micro_grid -- --quick 2>&1 | tail -20`
Expected: all 9 row groups exercise; total run time well under a minute (--quick uses sample_size=10).

- [ ] **Step 6: Verify aux_metrics.jsonl has 240 lines after a --quick run**

(NOT 270 — see Task 10/11 explanation. Cells where `ops_per_tx * size_bytes > 8 MB` are skipped to avoid Chisel's CacheFull at large 1000-per-tx writes. Skipped: 30 cells across allocate-1000pertx and update-1000pertx at 16KB/128KB/1MB sizes.)

Run: `wc -l bench/results/aux_metrics.jsonl`
Expected: 270 lines. Each row's bench_row_* function emits one line per (mode, size) pair via `aux.append`, regardless of `--quick` vs full run.

If the count is anything other than 270, debug:
- `awk -F'"row":"' '{print $2}' bench/results/aux_metrics.jsonl | awk -F'"' '{print $1}' | sort | uniq -c`
- Each of the 9 rows should show 30 lines (6 sizes × 5 modes).

- [ ] **Step 7: Verify Criterion produced output for each cell**

Run: `find target/criterion -name 'estimates.json' | wc -l`
Expected: 270 (one per cell). Note Criterion may also produce some aggregate `estimates.json` files at the group level (which would push the count higher than 270); the important check is at minimum 270.

- [ ] **Step 8: Spot-check that the directory structure matches the spec's pivot expectation**

Run: `ls target/criterion/ | head`
Expected: directories `allocate-1000pertx`, `allocate-1pertx`, `delete-1000pertx`, `delete-1pertx`, `delete_many`, `read-cold`, `read-warm`, `update-1000pertx`, `update-1pertx`. 9 row directories.

Run: `ls target/criterion/read-warm/`
Expected: 5 mode subdirectories — `chisel-strict`, `redb-strict`, `redb-unsafe`, `sqlite-strict`, `sqlite-unsafe`.

Run: `ls target/criterion/read-warm/chisel-strict/`
Expected: 6 size subdirectories — `1MB`, `128KB`, `16KB`, `2KB`, `256B`, `32B`.

Run: `ls target/criterion/read-warm/chisel-strict/32B/`
Expected: contains `estimates.json`, `sample.json`, `report/`, etc. — Criterion's standard per-bench artifacts.

- [ ] **Step 9: Verify time budget — full (non-quick) bench under 60 minutes**

Run: `time cd bench && cargo bench --bench micro_grid 2>&1 | tail -5`
Expected: full run completes in under 60 minutes (acceptance criterion #6).

If the full run exceeds 60 minutes:
- Identify the slow rows from the wall-clock output
- Tune `sample_size` (Criterion default 100) downward for those rows via `group.sample_size(N)` in the offending row-bench function
- Recommended starting point for the 1MB write rows: `group.sample_size(20)` — 5× speedup
- Add a comment in the modified row-bench function explaining the tune
- Commit the tune as `bench: tune sample_size for <row> to fit 60-min budget`
- Re-run the full bench to confirm the budget is met

If under 60 minutes, no tune commit is needed.

- [ ] **Step 10: Verify spec acceptance criteria**

Cross-check spec §7.2 acceptance criteria 1-9:

1. ✓ `cargo build -p chisel-bench` and `cargo test -p chisel-bench` pass — verified in Step 1.
2. ✓ `cargo clippy -p chisel-bench --all-targets -- -D warnings` clean — verified in Step 3.
3. ✓ `cargo fmt -- --check` clean — verified in Step 4.
4. ✓ The 8 new tests in spec §7.1 pass — verified in Step 1 (7 unit + 1 smoke = 8).
5. ✓ `cargo bench --bench micro_grid -- --quick` completes — verified in Step 5.
6. ✓ Full bench under 60 minutes — verified in Step 9.
7. ✓ `aux_metrics.jsonl` has 240 lines (270 nominal cells minus 30 cells where `ops_per_tx * size_bytes > 8 MB` exceeds Chisel's cache ceiling) with the right schema — verified in Step 6.
8. ✓ `target/criterion/<row>/<mode>/<size>/estimates.json` exists for all 270 cells — verified in Steps 7-8.
9. ✓ Project commenting standards — verified by visual inspection of the modified files (each file has a header explaining its role; doc comments explain choices not mechanics).

- [ ] **Step 11: No commit needed if all checks pass**

If steps 1-10 all pass with the existing 12 task commits, do nothing — Task 12's commit was the last code-bearing commit.

If Step 9 produced a sample_size tune, that commit was already made there — no additional commit needed.

The plan is complete.

---

## Final state after all tasks

- `bench/Cargo.toml` has criterion (dev-dep), serde, serde_json deps + `[[bench]] micro_grid` target.
- `bench/.gitignore` ignores `results/`.
- `bench/src/runner.rs` contains: `EngineMode`, `PopulatedSnapshot`, `populate_snapshot`, `AuxMetricsWriter`, `CellAuxMetrics`, `CellId`, `ChiselCountersDelta`, `apply_op`, `drive_workload_with_tx_granularity`, `capture_aux_metrics_snapshot_restore`, `capture_aux_metrics_warm_read`, `counter_delta`, `CACHE_SIZE_PAGES`. ~280 LOC. 7 unit tests.
- `bench/src/lib.rs` re-exports the new public types.
- `bench/benches/micro_grid.rs` registers all 270 cells across 9 row groups. ~220 LOC.
- `bench/tests/runner_smoke.rs` is a 1-cell end-to-end smoke test. ~50 LOC.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check` all clean.
- `cargo bench --bench micro_grid` runs the full grid in under 60 minutes.
- `bench/results/aux_metrics.jsonl` has 270 lines of per-cell metrics.
- `target/criterion/<row>/<mode>/<size>/estimates.json` exists for all 270 cells.
- 12 commits authored (or 13 with a sample-size tune).

PR 5 (markdown post-processor) can now begin: it consumes Criterion's `estimates.json` per cell and the JSONL aux-metrics file, pivots into the per-row tables described in master spec §7.1, and emits `summary.md` and `results.json`.
