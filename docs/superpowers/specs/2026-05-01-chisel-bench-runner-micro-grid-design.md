# Chisel Bench — Runner + 270-Cell Micro Grid — Design

**Date:** 2026-05-01
**Status:** Design approved; implementation plan pending.
**Scope:** Add the Runner machinery (`bench/src/runner.rs`) and the bench-binary glue (`bench/benches/micro_grid.rs`) that drives the workload data layer (PR 4a) against the three engine impls (PR 3) to produce Criterion HTML for all 270 micro-grid cells, plus a side-channel JSONL file capturing per-cell file-size deltas and Chisel internal counter deltas. PR 4b of the bench-suite series — the second half of the original PR 4 in the master spec.

This spec follows on from `2026-04-25-chisel-benchmark-suite-design.md` (the overall bench-suite design) and `2026-04-30-chisel-bench-workload-data-layer-design.md` (PR 4a, which landed `Operation`/`Workload` and the six seeded generators). The original PR 4 covered both the workload data layer and the Runner / 270-cell registration in one ~600 LOC PR. PR 4a landed the data layer alone; this PR (4b) lands everything else.

## 1. Goals and Non-Goals

### Goals

- Land an `EngineMode` enum that names the 5 engine-mode columns (`ChiselStrict`, `RedbStrict`, `RedbUnsafe`, `SqliteStrict`, `SqliteUnsafe`) with a single `EngineMode::open()` method that hides the per-engine constructor asymmetry.
- Land three cell-runner functions covering the three iteration patterns the micro grid needs: snapshot-restore (8 of 9 rows), warm-read persistent-engine (row 3), cold-read snapshot-restore-with-open-in-routine (row 4).
- Land snapshot-construction machinery (`populate_snapshot`) that builds a pre-populated DB and an alloc-order → engine-identifier sidecar (`PopulatedSnapshot::ids()`), so workloads can reference records by alloc_index without engine-specific lookup.
- Land an auxiliary-metrics writer (`AuxMetricsWriter`) that appends one JSONL line per cell to `bench/results/aux_metrics.jsonl`, capturing file-size delta and (Chisel-only) internal-counter delta from one calibration iteration per cell.
- Land the bench binary `bench/benches/micro_grid.rs` that registers all 270 cells across the 9 row groups, with `Throughput::Elements(N)` set per group so per-op numbers are auto-normalized.
- Run the full grid to completion in under 60 minutes on a developer laptop (the local-only diagnostic-tier budget per master spec §3).

### Non-Goals (this PR)

- *Markdown summary post-processing.* PR 5 reads `aux_metrics.jsonl` + Criterion's `estimates.json` and emits `summary.md` and `results.json`.
- *Scenario tier.* PR 6 covers YCSB-A/B, mutation log, and document store. PR 4b's Runner is structured to be reusable for them; the scenarios themselves are out of scope.
- *Zipfian / log-normal generators.* Those land with PR 6 once concrete scenarios pin the requirements.
- *CI integration.* PR 7. The micro grid is local-only per master spec §7.3 (~30 min runtime on `ubuntu-latest` is too expensive for per-PR runs).
- *Per-row sample-size tuning beyond hitting the 60-minute budget.* Master spec §5.3 says use Criterion's defaults until variance forces a tune. We tune only enough to hit the budget; real per-row variance work is a follow-up.
- *Cache-size sweeps.* Single `CACHE_SIZE_PAGES = 256` for v1. Multiple cache sizes per cell would multiply the grid by 3-5×; not worth it for the diagnostic tier.
- *Filesystem-aware copy optimization.* `std::fs::copy` for snapshot restore. APFS `clonefile` and Linux reflinks are happy accidents; we don't bake them in.
- *Re-execution determinism beyond what Criterion provides.* The workload is deterministic per-seed (per PR 4a's contract), but Criterion's adaptive iter-count + sampling means total run time and exact iteration count vary across runs.

## 2. Architecture — module layout and file structure

### 2.1 File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/Cargo.toml` | Modify | Add `serde`, `serde_json` to `[dependencies]`. Add `criterion = "0.5"` to `[dev-dependencies]`. Add `[[bench]] name = "micro_grid"` with `harness = false`. |
| `bench/src/runner.rs` | Create | `EngineMode` enum, `PopulatedSnapshot`, `populate_snapshot`, `AuxMetricsWriter`, `CellAuxMetrics`, `CellId`, `ChiselCountersDelta`, `apply_op`, `drive_workload_with_tx_granularity`, `capture_aux_metrics_snapshot_restore`, `capture_aux_metrics_warm_read`, `counter_delta`. No Criterion dependency. ~280 LOC. |
| `bench/src/lib.rs` | Modify | `pub mod runner;` + re-exports of `EngineMode`, `PopulatedSnapshot`, `AuxMetricsWriter`, `CellAuxMetrics`, `CellId`. |
| `bench/benches/micro_grid.rs` | Create | Top-level `micro_grid` function, the 9 row-bench functions, three private cell-runner helpers (`run_snapshot_restore_cell`, `run_warm_read_cell`, `run_cold_read_cell` — Criterion-shaped, hence here not in `runner.rs`), `criterion_main!`. ~220 LOC. |
| `bench/tests/runner_smoke.rs` | Create | One end-to-end smoke test that runs a single cell via `run_snapshot_restore_cell` against a minimal `Criterion::default()`. ~40 LOC. |

`runner.rs` is library code and reusable by PR 6's scenarios. `micro_grid.rs` is bench-binary glue and contains the iteration loops.

### 2.2 Library separation rationale

The non-Criterion-dependent machinery (engine construction, snapshot population, workload application, aux-metric capture, JSONL writer) is library-resident in `runner.rs`. PR 6's four scenarios will drive workloads against engines with the same snapshot-restore + engine-construction + aux-metric capture machinery; if it lived under `benches/`, PR 6 couldn't import it. Library placement also makes this code unit-testable without spinning up Criterion.

The three cell-runner helpers (`run_snapshot_restore_cell`, `run_warm_read_cell`, `run_cold_read_cell`) live in `bench/benches/micro_grid.rs` as private helpers because they take `&mut BenchmarkGroup` — a Criterion type, and Criterion is in `[dev-dependencies]`. PR 6's scenarios have a different iteration shape (one big workload run, not per-iteration cells) and won't reuse these helpers anyway.

### 2.3 Dependency choices

- **`criterion = "0.5"`** in `[dev-dependencies]` — only consumed by `[[bench]]` targets. Putting it in `[dependencies]` would force its transitive graph (rayon, plotters, tinytemplate) onto every consumer of `chisel-bench`.
- **`serde = { version = "1", features = ["derive"] }`** and **`serde_json = "1"`** in `[dependencies]` — used by `runner.rs` (library code) for the JSONL aux-metric output. PR 5's post-processor will derive its own `Deserialize`; PR 4b only needs `Serialize`.
- **`harness = false`** on the `[[bench]]` target — required for Criterion. Without it, Cargo links the unstable `libtest` benchmark harness and you get cryptic errors. Most common Cargo footgun for first-time Criterion users.

## 3. Iteration patterns — the three cell-runners

### 3.1 Pre-population strategy: snapshot-and-restore

Each cell measures against a pre-populated dataset of ~25 MB raw payload (per master spec §3.4). Population cost per cell is paid once. Per-iteration state reset uses snapshot-and-restore: copy a pre-built snapshot file to a working path before each timed iteration, open the engine on the copy, run the timed work, drop both the engine and the working file.

Strategy A (snapshot-restore) was chosen over persistent-engine-with-drift (which would conflate "cost of operation" with "cost of operation against accumulated state") and over per-iteration-pre-warm (which is infeasible — scanning 800K records before each iteration adds ~4 seconds × thousands of iterations = hours).

The single carve-out is row 3 (warm reads), where snapshot-restore would erase the warm/cold semantic distinction with row 4. For row 3, the engine persists across all iterations of a cell; the cache warms naturally during Criterion's warmup phase. See §3.3.

### 3.2 Snapshot-restore pattern (rows 1, 2, 4–9)

Used by 8 of 9 rows: allocate (1, 2), cold-read (4), update (5, 6), delete (7, 8), delete_many (9).

```rust
group.bench_with_input(
    BenchmarkId::new(mode.label(), size_label),
    &(),
    |b, _| {
        b.iter_batched(
            || {
                // Setup (untimed).
                let working = NamedTempFile::new().unwrap();
                std::fs::copy(snapshot_path, working.path()).unwrap();
                let engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                (engine, working)
            },
            |(mut engine, _working)| {
                // Routine (timed).
                drive_workload_with_tx_granularity(&mut *engine, workload, ops_per_tx, snapshot_ids);
            },
            BatchSize::PerIteration,
        );
    },
);
```

Key invariants:

- **`BatchSize::PerIteration`** is required, not `SmallInput`. Criterion may otherwise share setup state across iterations, breaking the "fresh state per measurement" contract.
- **`_working` lives across the routine closure.** `NamedTempFile` deletes its file on drop; binding it to `_working` (in the routine's tuple destructure) keeps the file alive until the routine returns.
- **The engine is `Box<dyn Engine>`** because `EngineMode::open` returns one. The `&mut *engine` deref reaches the trait method receivers.
- **`snapshot_ids: &[u64]`** is the alloc-order → engine-identifier map captured at populate time. The routine uses it to translate `Operation::Read { alloc_index }` into the engine's actual identifier.

### 3.3 Warm-read pattern (row 3 only)

```rust
let mut engine = mode.open(populated_path, CACHE_SIZE_PAGES).unwrap();
group.bench_with_input(
    BenchmarkId::new(mode.label(), size_label),
    &(),
    |b, _| {
        b.iter(|| {
            // No setup; engine is persistent. Cache warms during Criterion's warmup phase.
            for op in &workload.ops {
                apply_op(&mut *engine, op, ids, &mut new_ids);
            }
        });
    },
);
```

Key invariants:

- **`b.iter` not `b.iter_batched`** — no per-iteration setup means no setup closure.
- **Engine is opened ONCE per cell**, declared outside the closure, captured by mutable reference. Because Criterion's closures are `FnMut`, this works only because reads don't mutate engine-visible state.
- **No file copy per iteration.** The whole point of the warm-read carve-out: pay the snapshot-load cost once and let the cache build up naturally.
- **Workload `count = 64`** (see §5.3) — gives access-pattern variance without smearing tail latency.
- **Aux metrics are captured against THIS engine, not via snapshot-restore calibration.** After Criterion's measurement loop returns, the engine is still open and warm. Capturing counter deltas around one additional read at this point gives steady-state warm-cache counter activity — which is what row 3 should report. See §4.2.

### 3.4 Cold-read pattern (row 4)

```rust
group.bench_with_input(
    BenchmarkId::new(mode.label(), size_label),
    &(),
    |b, _| {
        b.iter_batched(
            || {
                // Setup (untimed): file copy only. Engine open is in the routine.
                let working = NamedTempFile::new().unwrap();
                std::fs::copy(snapshot_path, working.path()).unwrap();
                working
            },
            |working| {
                // Routine (TIMED): open + read.
                let mut engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                let op = &workload.ops[0];
                apply_op(&mut *engine, op, snapshot_ids, &mut Vec::new());
            },
            BatchSize::PerIteration,
        );
    },
);
```

Key invariants:

- **`mode.open(...)` is INSIDE the routine.** That's the defining property per master spec §5.2: "Cold means *Chisel-LRU cold*: the engine is freshly opened, no values touched, the first read of each measurement sample is the timed call."
- **Workload `count = 1`.** Cold means *first* read after open. Subsequent reads would warm the engine within a single iteration.
- **File copy stays in setup** — only the open + read is timed.

### 3.5 Shared helpers

Two helpers in `runner.rs` are shared across the three cell-runners:

```rust
fn drive_workload_with_tx_granularity(
    engine: &mut dyn Engine,
    workload: &Workload,
    ops_per_tx: usize,
    snapshot_ids: &[u64],
) {
    let mut new_ids: Vec<Identifier> = Vec::with_capacity(workload.ops.len());
    for chunk in workload.ops.chunks(ops_per_tx) {
        engine.begin().unwrap();
        for op in chunk {
            apply_op(engine, op, snapshot_ids, &mut new_ids);
        }
        engine.commit().unwrap();
    }
}

fn apply_op(
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
```

The `resolve` closure handles the case where a workload references both pre-populated identifiers (in `snapshot_ids`) and newly-allocated identifiers from earlier in the same iteration (in `new_ids`). The micro grid's workloads only reference one or the other (allocate workloads have empty `prepop_count`; read/update/delete workloads have `count = 1` or `1000` and don't allocate within a single iteration), but a unified `resolve` keeps the helper general for PR 6 scenarios that mix.

### 3.6 The pre-populated identifier map

Workloads reference records by `alloc_index` (per PR 4a's contract), which is an integer position in allocation order. At iteration time, the engine has been pre-populated, but the engine-assigned identifiers (Chisel handles, redb keys, SQLite rowids) need to be known.

`populate_snapshot` captures these during populate by recording the `Identifier` returned from each `engine.allocate()` call. The captured `Vec<u64>` becomes `PopulatedSnapshot::ids()`. The cell-runner functions take `snapshot_ids: &[u64]` and pass it through to `apply_op`'s `resolve`.

The alternative — extending the Engine trait with `iter_in_alloc_order()` and re-deriving identifiers from the engine after each snapshot-restore — would make the trait engine-aware in a way that's awkward for redb/SQLite (whose key spaces aren't structurally "alloc order" without convention). The capture-during-populate approach is engine-agnostic.

## 4. Auxiliary metrics capture

### 4.1 What we capture per cell

Criterion natively captures wall-clock time. Two additional per-cell metrics from master spec §6.1 need a side channel:

| Metric | Source | Applies to |
|--------|--------|-----------|
| File-size delta (bytes) | `engine.file_size_bytes()` before and after | All 270 cells |
| Chisel counter deltas (`cache_hits`, `cache_misses`, `fsync_calls`, `pages_allocated`) | `engine.internal_counters()` | 54 Chisel cells (1 mode × 6 sizes × 9 rows) |

### 4.2 The calibration-run approach

We capture aux metrics from one *calibration* iteration outside Criterion's measurement window. Calibration is structurally identical to a regular iteration but isn't part of any Criterion sample. The capture path differs between the snapshot-restore rows and the warm-read row, because the engine state they're characterizing differs.

**Snapshot-restore rows (1, 2, 4–9):** the calibration is structurally identical to a regular timed iteration — fresh open from snapshot, run the workload, capture before/after.

```rust
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
```

**Warm-read row (3):** the calibration runs against the SAME persistent engine that just finished measurement. The cache is warm, the OS page cache is warm — exactly the state the row is supposed to characterize. Capturing one read against this state gives steady-state warm-cache counter activity (mostly cache hits).

```rust
pub fn capture_aux_metrics_warm_read(
    cell_id: CellId,
    engine: &mut dyn Engine,
    workload: &Workload,
    snapshot_ids: &[u64],
) -> CellAuxMetrics {
    let counters_before = engine.internal_counters().unwrap();
    let size_before = engine.file_size_bytes().unwrap();

    // One read; mirrors what a single timed iteration would do.
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

Doing snapshot-restore-style calibration for row 3 would produce *cold* counter activity (every cache lookup a miss, every read a disk fetch), which contradicts the row's name and purpose. The persistent-engine calibration captures what "warm read" actually means.

Calibration runs *after* Criterion's measurement, not before. Two reasons:

- **Pre-calibration would prime the OS page cache**, making Criterion's first samples slightly faster than later ones — measurable drift that contaminates the timing distribution.
- **Post-calibration captures the cell in a representative state.** Once Criterion's last sample finishes, the OS cache is in whatever state the workload produces in steady operation; calibration measures from that point.

The `counter_delta(before, after)` helper handles the per-field subtraction and the `Option` zipping (returns `None` if either side is `None`, i.e., for non-Chisel engines).

### 4.3 Why one calibration, not Criterion-internal capture

We deliberately don't capture aux metrics inside Criterion's `iter_batched` routine. Two reasons:

- **Criterion's iteration count is adaptive.** Counters captured at end-of-sample reflect "all iterations in this sample's batch" — divided by iter count to per-iter. The arithmetic is doable but error-prone, and a single calibration run produces the same per-iteration delta directly.
- **File-size delta has no per-iteration meaning across many iterations.** Over 100 iterations of "begin + 1000 allocates + commit," file size grows monotonically; "per-iteration delta" is just the slope of the growth curve, which is what calibration measures.

### 4.4 Output format

Single file: `bench/results/aux_metrics.jsonl`. One line per cell:

```jsonl
{"row":"allocate-1pertx","mode":"chisel-strict","size":"32B","file_size_delta_bytes":262144,"counters":{"cache_hits":12,"cache_misses":35,"fsync_calls":2,"pages_allocated":18}}
{"row":"allocate-1pertx","mode":"redb-strict","size":"32B","file_size_delta_bytes":196608,"counters":null}
```

Format choices:

- **JSONL, not JSON array** — append-only writes during the bench run; partial output is parseable if the bench is interrupted (e.g., Ctrl-C after 100 of 270 cells).
- **`row`/`mode`/`size` as separate fields**, not a concatenated key — PR 5's pivot logic doesn't have to slug-parse, and the file is grep-able by hand.
- **`counters: null` for non-Chisel** rather than omitting the key — schema-stable, easier for PR 5 to pattern-match.

The bench file truncates the file at the start of each run via `AuxMetricsWriter::create()`. Re-runs don't accumulate stale entries.

### 4.5 The `CellAuxMetrics` and `CellId` types

```rust
#[derive(serde::Serialize)]
pub struct CellAuxMetrics {
    #[serde(flatten)]
    pub cell_id: CellId,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,    // serialized as `counters: null` for non-Chisel
}

#[derive(serde::Serialize, Clone, Copy)]
pub struct CellId {
    pub row: &'static str,
    pub mode: &'static str,    // EngineMode::label()
    pub size: &'static str,    // SIZES table label
}

#[derive(serde::Serialize, Clone, Copy)]
pub struct ChiselCountersDelta {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub fsync_calls: u64,
    pub pages_allocated: u64,
}
```

`#[serde(flatten)]` on `cell_id` produces the `row`/`mode`/`size` fields at the top level of each JSONL line, matching the example in §4.4.

## 5. Bench file layout & registration

### 5.1 Top-level `micro_grid` function

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

criterion_group!(benches, micro_grid);
criterion_main!(benches);
```

### 5.2 Single bench binary

One `[[bench]] name = "micro_grid"` in `Cargo.toml`, not one per row. Filtering by row is provided by Criterion's built-in filter syntax: `cargo bench --bench micro_grid read-warm` runs only the read-warm group. The alternative — separate `[[bench]]` targets per row — would mean 9 bench binaries to maintain and 9 separate Criterion runs for full-grid execution.

### 5.3 The `SIZES` constant

```rust
const SIZES: [(usize, &str, usize); 6] = [
    //  bytes,   label,   prepop_count   (~25 MB raw payload per spec §3.4)
    (32,        "32B",    800_000),
    (256,       "256B",   100_000),
    (2_048,     "2KB",    12_500),
    (16_384,    "16KB",   1_500),
    (131_072,   "128KB",  200),
    (1_048_576, "1MB",    25),
];
```

Lives at the top of `micro_grid.rs`. The `prepop_count` values match master spec §3.4 exactly; the constant is a single source of truth for the (size, label, count) triplet.

### 5.4 Workload counts per cell

| Row | `count` arg to generator | Why |
|-----|--------------------------|-----|
| allocate-1pertx | 1 | one Allocate per timed iteration |
| allocate-1000pertx | 1000 | thousand Allocates batched into one tx per iteration |
| read-warm | 64 | enough access-pattern variance per iteration without smearing tail latency (see §6 below) |
| read-cold | 1 | cold = first read after open, by definition |
| update-1pertx | 1 | one Update per iteration |
| update-1000pertx | 1000 | thousand Updates per iteration |
| delete-1pertx | 1 | one Delete per iteration |
| delete-1000pertx | 1000 | thousand Deletes per iteration |
| delete_many | `gen_delete_many(seed, prepop, batches=1, batch_size=1000)` | one DeleteMany op carrying 1000 ids |

### 5.5 Throughput::Elements per row group

Set at the row-group level (`group.throughput(Throughput::Elements(N))`):

| Row group | N |
|-----------|---|
| allocate-1pertx, read-cold, update-1pertx, delete-1pertx | 1 |
| allocate-1000pertx, update-1000pertx, delete-1000pertx | 1000 |
| read-warm | 64 (matches workload count) |
| delete_many | 1000 (one DeleteMany op = 1000 deletions) |

Criterion auto-normalizes per-element throughput in its HTML and JSON. PR 5's post-processor reads the per-element values, not raw sample times.

### 5.6 Per-cell snapshot construction

Each `(mode, size)` pair gets its own pre-populated snapshot built once per row by `populate_snapshot`. For rows with `prepop_count > 0` (reads, updates, deletes, delete_many), the snapshot contains the population. For allocate rows, the "snapshot" is an empty DB (built once per `(mode, size)` to amortize the engine-create cost, even though it's small).

The bench file pattern:

```rust
for (size_bytes, size_label, prepop_count) in SIZES {
    for mode in EngineMode::ALL {
        let snapshot = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
        run_*_cell(&mut group, mode, size_label, snapshot.path(), snapshot.ids(), &workload);
        aux.append(capture_aux_metrics(/* ... */)).unwrap();
        // snapshot drops here; tempfile is deleted.
    }
}
```

## 6. Defaults and constants

### 6.1 `CACHE_SIZE_PAGES = 256`

All three engines receive `cache_size_pages = 256` (= 2 MB at 8 KB pages). This matches Chisel's default cache size; redb and SQLite use the same number for their `cache_size_pages` parameter (each engine maps it to its native unit internally).

Rationale: a uniform cache size across modes makes cross-engine comparisons fair. 256 pages × 8 KB = 2 MB caches ~8% of a 25 MB dataset, so random-access workloads will miss frequently — that's the realistic case the bench wants to surface.

Future work could sweep cache sizes, but multiple cache values per cell would multiply the grid by 3-5× — out of scope for the diagnostic tier.

### 6.2 Sample size policy

Default to Criterion's defaults (`sample_size = 100`, `warmup ≈ 3s`, `measurement_time ≈ 5s`) in PR 4b. Per master spec §5.3, "We keep these defaults unless variance turns out to be unacceptable for the regression-detection use case, at which point we tune."

PR 4b's acceptance criterion #6 caps total run time at 60 minutes on a developer laptop. If defaults exceed that — likely for the slow rows (1 MB × write rows = ~25 records × 1000 allocates of 1 MB each per iter, then 100 samples) — sample sizes for the offending rows get tuned within this PR before merge. We don't ship something that takes 6 hours to run.

### 6.3 Hardcoded row seeds

```rust
fn seed_for(row_name: &str) -> u64 {
    match row_name {
        "allocate-1pertx"     => 0x4001,
        "allocate-1000pertx"  => 0x4002,
        "read-warm"           => 0x4003,
        "read-cold"           => 0x4004,
        "update-1pertx"       => 0x4005,
        "update-1000pertx"    => 0x4006,
        "delete-1pertx"       => 0x4007,
        "delete-1000pertx"    => 0x4008,
        "delete_many"         => 0x4009,
        _ => panic!("unknown row name: {row_name}"),
    }
}
```

Hardcoded, not derived. `std::hash::DefaultHasher` randomizes its initial state per-process (HashDoS mitigation), so it can't produce stable hashes across runs without explicit seeding. Adding `ahash` or `FxHash` for hashing 9 known-ahead-of-time strings is overkill; the literal table is iron-clad and easy to audit.

## 7. Tests and acceptance criteria

### 7.1 Test coverage

Pure-function tests in `bench/src/runner.rs` (inline `#[cfg(test)] mod tests`):

| # | Test | What it verifies |
|---|------|------------------|
| 1 | `engine_mode_label_uniqueness` | Each `EngineMode::label()` returns a non-empty string; all 5 labels are distinct |
| 2 | `engine_mode_supports_internal_counters` | Only `ChiselStrict` returns `true` |
| 3 | `engine_mode_open_each_mode` | `EngineMode::open` succeeds for each variant against a tempfile path |
| 4 | `populate_snapshot_chisel_basic` | Populate 100 records of 256 B; confirm `ids().len() == 100` and the file is non-empty |
| 5 | `populate_snapshot_redb_basic` | Same against RedbEngine |
| 6 | `populate_snapshot_sqlite_basic` | Same against SqliteEngine |
| 7 | `aux_metrics_writer_jsonl_format` | Write 3 cells (one Chisel, one redb, one with an i64 negative delta), reload as text, confirm 3 lines, each parses as JSON with the expected keys including `counters: null` for non-Chisel |

End-to-end smoke in `bench/tests/runner_smoke.rs`:

| # | Test | What it verifies |
|---|------|------------------|
| 8 | `smoke_run_one_snapshot_restore_cell` | Run `run_snapshot_restore_cell` once with `Criterion::default().sample_size(10)` against a 256B/100-record allocate workload, assert the call returns without panicking |

Total new tests: 8 (7 unit + 1 end-to-end smoke).

### 7.2 Acceptance criteria

PR 4b is mergeable when:

1. `cargo build -p chisel-bench` and `cargo test -p chisel-bench` pass on macOS and Linux.
2. `cargo clippy -p chisel-bench --all-targets -- -D warnings` is clean.
3. `cargo fmt -- --check` is clean across touched files.
4. The 8 new tests in §7.1 pass.
5. `cargo bench --bench micro_grid -- --quick` completes without error, exercising at least one cell from each of the 9 row groups.
6. `cargo bench --bench micro_grid` runs to completion in under 60 minutes on a developer laptop. If it doesn't, sample sizes for slow rows get tuned in this PR before merge.
7. After a full-grid run, `bench/results/aux_metrics.jsonl` contains exactly 270 lines, each valid JSON with the expected schema (top-level `row`, `mode`, `size`, `file_size_delta_bytes`, `counters`).
8. After a full-grid run, `target/criterion/<row>/<mode>/<size>/estimates.json` exists for all 270 cells.
9. `bench/src/runner.rs` and `bench/benches/micro_grid.rs` follow project commenting standards — file headers explain role, doc comments explain choices not mechanics.

### 7.3 What PR 4b does NOT include

Deferred to PR 5 (markdown post-processor):
- Reading `aux_metrics.jsonl` and combining with Criterion JSON.
- Producing `summary.md` and `results.json`.

Deferred to PR 6 (scenarios):
- The four YCSB-style scenarios.
- Zipfian access patterns, log-normal size distributions, mixed-op workload generators.

Deferred to PR 7 (CI):
- `.github/workflows/bench.yml` and PR comment posting.

Out of scope entirely:
- Per-row sample-size tuning beyond what's needed to hit the 60-minute budget.
- Cache-size sweeps.
- Filesystem-aware copy optimization.

## 8. Build sequence relationship

PR 4b is the second half of the original PR 4 in the master spec. The series after this PR lands becomes:

| # | PR | Status |
|---|----|--------|
| 1 | Instrumentation precursor | Landed |
| 2 | `bench/` subcrate + Engine trait + ChiselEngine | Landed |
| 3 | RedbEngine + SqliteEngine + equivalence tests | Landed |
| 4a | Workload data layer | Landed |
| **4b** | **Runner + 270-cell registration** | **This PR** |
| 5 | Markdown summary post-processor | Pending |
| 6 | Scenario tier | Pending |
| 7 | CI workflow | Pending |
| 8 | Cross-engine relative-performance tests (addendum) | Pending — own spec/plan |

CLAUDE.md should be updated when PR 4b merges to reflect the bench-suite series progress.

### 8.1 Rollback

PR 4b fails review or is reverted: PR 4a's workload data layer remains, all earlier PRs continue to function, no one downstream depends on this code yet. Easy revert.

## 9. Open implementation-phase questions

These are deferred to the implementation plan:

- The exact constant value for the `serde` and `criterion` versions (likely `serde = "1"`, `serde_json = "1"`, `criterion = "0.5"` — current stable lines).
- Whether `populate_snapshot` writes the `.ids` sidecar to disk or just keeps it in memory (`PopulatedSnapshot::ids()` as `&[u64]`). In-memory is simpler and we don't currently need cross-process reuse.
- Specific Criterion `BatchSize` choice (`PerIteration` is the default; the plan confirms).
- Whether `seed_for` lives in `runner.rs` or `micro_grid.rs`. Probably the bench file, since it's only used by the registration loops.
- Doc-comment style for the cell-runner functions (one-line vs multi-paragraph). The plan picks a uniform style.
- The exact set of integration tests in `bench/tests/runner_smoke.rs` — one cell or one per pattern. The plan resolves.

These are implementation details that do not affect the design contract.
