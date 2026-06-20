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
use crate::summary::format::percentile_linear_interp;
use crate::{ChiselEngine, RedbEngine, SqliteEngine};
use std::path::Path;
use std::time::Instant;

/// Page-cache budget passed to every engine when constructing for the
/// micro grid. 256 pages × 8 KB = 2 MB ≈ 8% of the 25 MB raw payload
/// per cell, so random-access workloads will miss frequently. See spec
/// §6.1.
pub const CACHE_SIZE_PAGES: usize = 256;

/// One of the five engine-mode columns of the micro grid.
///
/// Hides the per-engine constructor asymmetry behind `EngineMode::open`:
/// ChiselEngine takes `(path, cache_size_pages)` while RedbEngine and
/// SqliteEngine take `(path, cache_size_pages, DurabilityMode)`. The enum
/// is the single source of truth for the 5 modes the micro grid measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    ChiselStrict,
    /// Chisel against its `Vec<u8>`-backed in-memory PageIo — same engine,
    /// no file, no fsync. Exists so the scenario tier can measure Chisel's
    /// pure CPU/memory cost; the `chisel-mem` − `chisel-strict` delta on the
    /// same scenario is a direct read of the fsync/file-I/O tax (I92).
    ChiselMemory,
    RedbStrict,
    RedbUnsafe,
    SqliteStrict,
    SqliteUnsafe,
}

impl EngineMode {
    /// The five file-backed modes the micro grid measures, in canonical
    /// markdown-summary column order (PR 5). `ChiselMemory` is deliberately
    /// excluded: the snapshot-restore and cold-read rows seed an engine by
    /// `fs::copy`-ing a populated file, which a `Vec<u8>`-backed engine cannot
    /// observe. In-memory is exercised only by the scenario tier
    /// (`scenarios.rs`), which pre-populates in-process. (I92)
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
            Self::ChiselMemory => "chisel-mem",
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
        matches!(self, Self::ChiselStrict | Self::ChiselMemory)
    }

    /// Construct a fresh engine of this mode, file-backed at `path` with
    /// the given cache budget. Hides the per-engine constructor
    /// asymmetry: ChiselEngine has no DurabilityMode parameter (always
    /// strict by design), the others do.
    pub fn open(self, path: &Path, cache_size_pages: usize) -> EngineResult<Box<dyn Engine>> {
        match self {
            Self::ChiselStrict => Ok(Box::new(ChiselEngine::open_file(path, cache_size_pages)?)),
            // I92: in-memory backing ignores `path` — there is no file. Exercised
            // only by the scenario tier, which pre-populates in-process; any
            // tempfile the caller opens alongside is simply unused.
            Self::ChiselMemory => Ok(Box::new(ChiselEngine::open_in_memory(cache_size_pages)?)),
            Self::RedbStrict => Ok(Box::new(RedbEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Strict,
            )?)),
            Self::RedbUnsafe => Ok(Box::new(RedbEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Unsafe,
            )?)),
            Self::SqliteStrict => Ok(Box::new(SqliteEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Strict,
            )?)),
            Self::SqliteUnsafe => Ok(Box::new(SqliteEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Unsafe,
            )?)),
        }
    }
}

use chisel::ChiselCounters;

/// Per-cell deltas of the four Chisel internal counters (master spec
/// §6.1). Reported in the JSONL aux-metrics file for cells where the
/// engine is Chisel; `None` for redb / SQLite.
///
/// Values are subtracted (after - before) and are guaranteed
/// non-negative because counters are cumulative-from-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pub counters: Option<ChiselCountersDelta>, // serialized as `counters: null` for non-Chisel
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
        Ok(Self {
            writer: BufWriter::new(file),
        })
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
            // I90: black_box both ends of the timed read. Without the output
            // fence the returned Vec is never observed, so the optimizer is
            // free to elide the read body and the redb/sqlite adapters'
            // `to_vec()` copies — which silently zeroes the very work the
            // read rows measure. This is latent today (the `&mut dyn Engine`
            // virtual call blocks devirtualization) but becomes live under the
            // I91 LTO profile, which is exactly why I90 must land first. The
            // input fence keeps the id's address arithmetic inside the measured
            // region. Every timed read path (scenario tier, warm/cold micro-grid
            // rows, drive_workload_with_tx_granularity) funnels through here, so
            // this one site covers them all.
            let value = engine
                .read(std::hint::black_box(resolve(*alloc_index)))
                .unwrap();
            std::hint::black_box(value);
        }
        Operation::Update { alloc_index, size } => {
            engine
                .update(resolve(*alloc_index), &vec![0u8; *size])
                .unwrap();
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

use tempfile::NamedTempFile;

/// A populated database file paired with its alloc-order → engine-id map.
///
/// The file is a `tempfile::NamedTempFile` so it auto-deletes on drop.
/// `ids()` returns the identifiers in allocation order — element `i` is
/// the engine identifier that the `i`-th prepopulating Allocate
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

/// Per-tx byte budget for snapshot population. Chunked at this size to
/// amortize fsync cost during prepop (one commit per chunk rather than
/// one per record). Bench engines now run with the spillway enabled at
/// the production-default scale (1024 × cache budget), matching
/// SQLite's temp-file overflow and redb's on-disk B-tree pages, so
/// there is no strict cache-page ceiling on transaction size. 1 MiB
/// sits between the original 4 MiB (excessive, heavy allocation
/// churn per chunk) and the former 128 KiB (tuned for the now-removed
/// strict cap), giving a good fsync amortization ratio without
/// burdening the handle-table COW pressure per chunk.
/// Internal to `populate_snapshot`; the row-bench skip threshold in
/// micro_grid.rs is a different concern (workload-time, not setup).
const POPULATE_TX_BUDGET_BYTES: usize = 1024 * 1024;

/// Per-tx record-count cap for snapshot population, applied alongside
/// the byte budget. Each Chisel allocate() COWs a fresh root page for
/// the handle-table radix tree on every insert (even back-to-back
/// inserts produce distinct dirty pages — see `insert_recursive` in
/// handle_table.rs). At depth 1 that is two new dirty pages per insert
/// (interior + leaf), so 1024 inserts already produce 2048 dirty
/// pages — the hard ceiling. 500 records per chunk leaves headroom
/// for the data-page and freemap COW that runs alongside.
const POPULATE_TX_MAX_RECORDS: usize = 500;

/// Build a fresh DB file populated with `prepop_count` records of `size_bytes`
/// each, capturing the engine-assigned identifiers in allocation order.
///
/// Pre-population is chunked into multiple `begin/.../commit` blocks of at
/// most `POPULATE_TX_BUDGET_BYTES` worth of payload each to amortize fsync
/// cost. With the spillway enabled (production-default: 1024 × cache budget),
/// there is no hard cache-page ceiling on transaction size — chunking here
/// is purely for fsync amortization, not cache survival. The snapshot
/// content is identical regardless of chunk size (same records, same ids
/// in the same allocation order — only internal tx granularity changes).
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
    let mut engine = mode.open(file.path(), CACHE_SIZE_PAGES)?;
    let mut ids = Vec::with_capacity(prepop_count);
    let payload = vec![0u8; size_bytes];

    // Chunk so each tx stays under both per-tx caps: byte budget
    // (amortizes fsync cost for any record size) and record count
    // (tiny records that fit many per chunk by bytes but accumulate
    // handle-table COW pressure by count).
    let chunk_size =
        (POPULATE_TX_BUDGET_BYTES / size_bytes.max(1)).clamp(1, POPULATE_TX_MAX_RECORDS);
    let mut remaining = prepop_count;
    while remaining > 0 {
        let this_chunk = remaining.min(chunk_size);
        engine.begin()?;
        for _ in 0..this_chunk {
            let id = engine.allocate(&payload)?;
            ids.push(id.0);
        }
        engine.commit()?;
        remaining -= this_chunk;
    }
    // Force engines that maintain sibling journal files (SQLite WAL) to
    // flush them into the main DB so a sibling file-copy of `file.path()`
    // alone produces a re-openable database. No-op for Chisel and redb.
    engine.flush_for_snapshot()?;
    drop(engine); // explicit close before returning the file

    Ok(PopulatedSnapshot { file, ids })
}

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

    // Match populate_snapshot: ensure sibling journal files (SQLite WAL)
    // are folded into the main DB before we read file size or hand the
    // working file off. file_size_bytes already includes -wal/-shm, so
    // this primarily affects re-openability symmetry, not the captured
    // delta — but keeps the post-workload state honestly self-contained.
    engine.flush_for_snapshot().unwrap();

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

/// Result of running one (scenario, mode) cell. Captures everything
/// the post-processor will surface in the markdown + JSON.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub mode: String,
    pub total_wall_clock_ns: u64,
    pub op_count: usize,
    pub throughput_ops_per_sec: f64,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub final_file_size_bytes: u64,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

/// Helper: returns true iff applying this op to an engine requires
/// a transaction (Allocate / Update / Delete / DeleteMany). Reads
/// happen outside any explicit tx.
fn op_is_mutating(op: &Operation) -> bool {
    matches!(
        op,
        Operation::Allocate { .. }
            | Operation::Update { .. }
            | Operation::Delete { .. }
            | Operation::DeleteMany { .. }
    )
}

/// Run one scenario cell end-to-end:
///   1. Open a fresh engine in `mode` at a tempfile path
///   2. Run `prepopulate_workload` untimed (each Allocate in its own tx
///      for simplicity; pre-pop time is excluded from the measurement)
///   3. Snapshot file size + counters (the "before" state for deltas)
///   4. Start wall-clock timer
///   5. Iterate `scenario_workload.ops`, calling apply_op on each;
///      mutating ops get wrapped in a single-op tx (begin/commit);
///      reads happen bare. Per-op `Instant::now()` captures latency.
///   6. Stop wall-clock timer
///   7. Snapshot file size + counters (the "after" state)
///   8. Compute percentiles from per-op timings via percentile_linear_interp
///
/// Engine drops at end of function; tempfile is auto-deleted.
pub fn run_scenario_cell(
    mode: EngineMode,
    scenario_name: &str,
    prepopulate_workload: &Workload,
    scenario_workload: &Workload,
) -> ScenarioResult {
    let working = tempfile::NamedTempFile::new().expect("create tempfile");
    let mut engine = mode
        .open(working.path(), CACHE_SIZE_PAGES)
        .expect("open engine");

    // Pre-population phase (untimed). Batches ops into chunked
    // transactions sized by POPULATE_TX_BUDGET_BYTES / POPULATE_TX_MAX_RECORDS
    // to amortize fsync cost — single-op-per-tx prepop is fsync-bound and
    // blows the runtime budget at 100K-record scale (an Allocate-each-tx
    // loop on chisel-strict is ~12 min for 100K records on macOS APFS).
    // Mirror of PR 4b's `populate_snapshot` strategy, generalized via a
    // running byte accumulator since scenario prepop has heterogeneous op
    // sizes (mutation-log uses uniform [64, 4096]; document-store uses
    // lognormal). Reads/Deletes accumulate zero bytes since they don't
    // dirty new payload pages. With the spillway enabled, chunking here
    // is purely for fsync amortization — no strict cache ceiling applies.
    let mut snapshot_ids: Vec<u64> = Vec::with_capacity(prepopulate_workload.ops.len());
    let mut new_ids_during_prepop: Vec<Identifier> = Vec::new();
    let mut i = 0;
    while i < prepopulate_workload.ops.len() {
        engine.begin().expect("begin prepop tx");
        let mut accumulated_bytes: usize = 0;
        let mut accumulated_records: usize = 0;
        while i < prepopulate_workload.ops.len()
            && accumulated_records < POPULATE_TX_MAX_RECORDS
            && accumulated_bytes < POPULATE_TX_BUDGET_BYTES
        {
            let op = &prepopulate_workload.ops[i];
            let cost = match op {
                Operation::Allocate { size } => *size,
                Operation::Update { size, .. } => *size,
                Operation::Read { .. }
                | Operation::Delete { .. }
                | Operation::DeleteMany { .. } => 0,
            };
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids_during_prepop);
            accumulated_bytes += cost;
            accumulated_records += 1;
            i += 1;
        }
        engine.commit().expect("commit prepop tx");
    }
    // Move new_ids_during_prepop into snapshot_ids for the timed phase.
    snapshot_ids.extend(new_ids_during_prepop.iter().map(|id| id.0));

    // Capture "before" state right after prepopulate.
    let counters_before = engine.internal_counters().expect("counters before");
    let size_after_prepop = engine.file_size_bytes().expect("file size before");

    // Timed phase: run scenario_workload with per-op Instant::now().
    let mut per_op_ns: Vec<u64> = Vec::with_capacity(scenario_workload.ops.len());
    let mut new_ids: Vec<Identifier> = Vec::new();
    let total_start = Instant::now();
    for op in &scenario_workload.ops {
        let op_start = Instant::now();
        if op_is_mutating(op) {
            engine.begin().expect("begin tx");
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids);
            engine.commit().expect("commit tx");
        } else {
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids);
        }
        per_op_ns.push(op_start.elapsed().as_nanos() as u64);
    }
    let total_wall_clock = total_start.elapsed();

    // Capture "after" state.
    let counters_after = engine.internal_counters().expect("counters after");
    let size_after = engine.file_size_bytes().expect("file size after");

    // Compute percentiles from per-op distribution.
    let mut sorted: Vec<f64> = per_op_ns.iter().map(|&n| n as f64).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile_linear_interp(&sorted, 0.50).unwrap_or(0.0);
    let p95 = percentile_linear_interp(&sorted, 0.95).unwrap_or(0.0);
    let p99 = percentile_linear_interp(&sorted, 0.99).unwrap_or(0.0);

    let total_ns = total_wall_clock.as_nanos() as u64;
    let op_count = scenario_workload.ops.len();
    let throughput = if total_ns == 0 {
        0.0
    } else {
        op_count as f64 / (total_ns as f64 / 1e9)
    };

    ScenarioResult {
        scenario: scenario_name.to_string(),
        mode: mode.label().to_string(),
        total_wall_clock_ns: total_ns,
        op_count,
        throughput_ops_per_sec: throughput,
        p50_ns: p50,
        p95_ns: p95,
        p99_ns: p99,
        final_file_size_bytes: size_after,
        file_size_delta_bytes: size_after as i64 - size_after_prepop as i64,
        counters: counter_delta(counters_before, counters_after),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::NamedTempFile;

    #[test]
    fn engine_mode_label_uniqueness() {
        let labels: HashSet<&'static str> = EngineMode::ALL.iter().map(|m| m.label()).collect();
        assert_eq!(
            labels.len(),
            EngineMode::ALL.len(),
            "labels must be distinct"
        );
        for label in &labels {
            assert!(!label.is_empty(), "no label may be empty");
        }
    }

    #[test]
    fn engine_mode_supports_internal_counters() {
        for mode in EngineMode::ALL {
            let expected = matches!(mode, EngineMode::ChiselStrict);
            assert_eq!(
                mode.supports_internal_counters(),
                expected,
                "only ChiselStrict reports internal counters; got {mode:?}"
            );
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

    #[test]
    fn populate_snapshot_chisel_large_size_chunks() {
        // 1500 × 16 KiB = 24 MiB raw payload. Chunked populate must
        // succeed regardless of cache size; with the spillway enabled
        // overflow spills to disk rather than returning CacheFull.
        let snap = populate_snapshot(EngineMode::ChiselStrict, 16_384, 1500).unwrap();
        assert_eq!(snap.ids().len(), 1500);
    }

    #[test]
    fn aux_metrics_writer_jsonl_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/aux_metrics.jsonl");
        let mut writer = AuxMetricsWriter::create(&path).unwrap();

        writer
            .append(&CellAuxMetrics {
                cell_id: CellId {
                    row: "allocate-1pertx",
                    mode: "chisel-strict",
                    size: "32B",
                },
                file_size_delta_bytes: 262_144,
                counters: Some(ChiselCountersDelta {
                    cache_hits: 12,
                    cache_misses: 35,
                    fsync_calls: 2,
                    pages_allocated: 18,
                }),
            })
            .unwrap();

        writer
            .append(&CellAuxMetrics {
                cell_id: CellId {
                    row: "allocate-1pertx",
                    mode: "redb-strict",
                    size: "32B",
                },
                file_size_delta_bytes: 196_608,
                counters: None,
            })
            .unwrap();

        writer
            .append(&CellAuxMetrics {
                cell_id: CellId {
                    row: "delete-1pertx",
                    mode: "chisel-strict",
                    size: "1MB",
                },
                file_size_delta_bytes: -1_048_576, // delete shrinks
                counters: Some(ChiselCountersDelta {
                    cache_hits: 0,
                    cache_misses: 1,
                    fsync_calls: 1,
                    pages_allocated: 0,
                }),
            })
            .unwrap();

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

    #[test]
    fn run_scenario_cell_chisel_smoke() {
        // Small synthetic scenario: 50 prepop, 100 timed ops.
        let prepop = Workload {
            name: "test-prepop".to_string(),
            seed: 0x9001,
            prepop_count: 0,
            ops: (0..50).map(|_| Operation::Allocate { size: 256 }).collect(),
        };
        let workload = Workload {
            name: "test-scenario".to_string(),
            seed: 0x9001,
            prepop_count: 50,
            ops: (0..100)
                .map(|i| Operation::Read {
                    alloc_index: i % 50,
                })
                .collect(),
        };
        let result = run_scenario_cell(EngineMode::ChiselStrict, "smoke", &prepop, &workload);
        assert_eq!(result.scenario, "smoke");
        assert_eq!(result.mode, "chisel-strict");
        assert_eq!(result.op_count, 100);
        assert!(result.total_wall_clock_ns > 0);
        assert!(result.throughput_ops_per_sec > 0.0);
        // p50 <= p95 <= p99 (sanity, percentile ordering)
        assert!(result.p50_ns <= result.p95_ns);
        assert!(result.p95_ns <= result.p99_ns);
        // ChiselStrict produces non-null counters
        assert!(result.counters.is_some());
    }

    #[test]
    fn run_scenario_cell_returns_counters_only_for_chisel() {
        let prepop = Workload {
            name: "test-prepop".to_string(),
            seed: 0x9002,
            prepop_count: 0,
            ops: (0..50).map(|_| Operation::Allocate { size: 256 }).collect(),
        };
        let workload = Workload {
            name: "test-scenario".to_string(),
            seed: 0x9002,
            prepop_count: 50,
            ops: (0..100)
                .map(|i| Operation::Read {
                    alloc_index: i % 50,
                })
                .collect(),
        };
        let result = run_scenario_cell(EngineMode::RedbStrict, "smoke", &prepop, &workload);
        assert!(
            result.counters.is_none(),
            "redb cells produce counters: None"
        );
    }

    #[test]
    fn run_scenario_cell_in_memory_mode_is_wired_and_does_work() {
        // I92: verify the chisel-mem mode is correctly wired into the scenario
        // runner and does real engine work.
        //
        // Subtlety worth pinning (it cost a wrong first assertion): the
        // in-memory backend does NOT report fsync_calls == 0. `fsync_calls`
        // counts *protocol-level* fsync operations (3 per commit), incremented
        // regardless of backing; on the Vec<u8> backing those are no-op
        // syscalls. So for an identical workload the counters are identical to
        // chisel-strict — which is the point: with the work held equal, the
        // chisel-mem − chisel-strict *wall-clock* delta is a clean read of the
        // fsync/file-I/O tax. We assert wiring + real work, never timing
        // (timing assertions are flaky).
        let prepop = Workload {
            name: "mem-prepop".to_string(),
            seed: 0x9003,
            prepop_count: 0,
            ops: (0..50).map(|_| Operation::Allocate { size: 256 }).collect(),
        };
        let workload = Workload {
            name: "mem-scenario".to_string(),
            seed: 0x9003,
            prepop_count: 50,
            ops: (0..20).map(|_| Operation::Allocate { size: 256 }).collect(),
        };
        let result = run_scenario_cell(EngineMode::ChiselMemory, "smoke", &prepop, &workload);
        assert_eq!(result.mode, "chisel-mem");
        assert_eq!(result.op_count, 20);
        let counters = result
            .counters
            .expect("in-memory chisel still reports internal counters");
        assert!(
            counters.pages_allocated > 0,
            "timed allocates must dirty pages"
        );
        // Commits executed (3 protocol fsyncs each) even though the Vec backing
        // makes them no-op syscalls — confirms the timed mutating phase ran.
        assert!(counters.fsync_calls > 0);
    }
}
