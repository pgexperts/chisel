# Chisel Bench — Scenario Tier — Design

**Date:** 2026-05-03
**Status:** Design approved; implementation plan pending.
**Scope:** Add the scenario tier (master spec §4): four end-to-end YCSB-style workloads (YCSB-A, YCSB-B, mutation log, document store) running once per (scenario, strict-mode) cell = 12 cells total. Each scenario is a deterministic mixed-op workload composed from new pure-data primitives in `workload.rs` (Zipfian indices, log-normal sizes, weighted op-mix). The runner uses inline `Instant::now()` timing rather than Criterion to fit the master spec's 1-6 minute total runtime budget. PR 6 also extends PR 5's post-processor to render scenario output in `summary.md` and `results.json`.

This spec follows on from `2026-04-25-chisel-benchmark-suite-design.md` (the overall bench-suite design, especially §4 on scenarios + §7.1 on output artifacts), `2026-04-30-chisel-bench-workload-data-layer-design.md` (PR 4a — established the `Workload`/`Operation` data layer), `2026-05-01-chisel-bench-runner-micro-grid-design.md` (PR 4b — established the runner machinery this spec extends), and `2026-05-03-chisel-bench-summary-postprocessor-design.md` (PR 5 — established the post-processor this spec extends).

PR 5 deliberately deferred the "scenario summary" section of `summary.md` to PR 6 (PR 5 spec §10). That deferral is honored here: PR 6 ships the scenarios end-to-end through to rendered markdown.

## 1. Goals and Non-Goals

### Goals

- Land four scenario workload generators in a new `bench/src/scenarios.rs` module: `gen_ycsb_a`, `gen_ycsb_b`, `gen_mutation_log`, `gen_document_store`. Each is a thin composition of reusable primitives plus master-spec-§4 parameters. Each has a paired `gen_<scenario>_prepopulate` for the pre-population phase.
- Land three reusable workload primitives in `bench/src/workload.rs`: `zipfian_indices` (Zipfian-distributed access pattern with configurable θ), `lognormal_sizes` (log-normal-distributed sizes with configurable median/p99), and `mix_operations` (weighted-random op-kind composition over a pre-sampled access pattern + sizes).
- Land a `run_scenario_cell` helper in `bench/src/runner.rs` that opens a fresh engine per cell, populates the dataset untimed, runs the scenario workload with per-op `Instant::now()` instrumentation, captures aux metrics, returns a `ScenarioResult` struct.
- Land a new bench-binary target `bench/benches/scenarios.rs` (with `harness = false` and our own `main`) that iterates the 12 cells (4 scenarios × 3 strict modes) and streams `ScenarioResult` values to `bench/results/scenarios_metrics.jsonl`.
- Extend PR 5's post-processor to read `scenarios_metrics.jsonl`, render a "Scenario tier" section in `summary.md`, and add a `scenarios` top-level key to `results.json`.
- Total runtime: 1-6 minutes per master spec §4.7. Runtime budget is binding — Criterion is deliberately NOT used because its many-samples-per-bench convention would explode the budget by 10×.

### Non-Goals (this PR)

- *CI integration.* PR 7 wires `cargo bench --bench scenarios` into a GitHub Actions workflow with PR-comment posting.
- *The diff binary.* PR 7 implements the `results.json` before/after diff and regression flagging.
- *Vendored YCSB workload trace files.* Master spec §9 listed this as a deferred implementation question — we go with in-process generation. Determinism is assured by per-scenario seeds.
- *Multiple scenario seeds for variance estimation.* One run per cell is the spec's stated workflow (§4.7). Variance comes from the underlying workload's access pattern, not from measurement-noise sampling.
- *Custom Criterion HTML for scenarios.* We don't use Criterion (§3.4 below); no HTML to produce. Scenarios surface in `summary.md` only.
- *Scenario tier in the unsafe modes.* Master spec §4 explicitly limits scenarios to strict mode: "the unsafe column is a diagnostic for the micro grid; for 'real-world effects' only durable numbers belong."
- *Per-scenario raw-archive output.* The `bench/results/<timestamp>/raw/` archive PR 5 introduced contains only per-cell Criterion JSON; scenarios produce JSONL only, which is itself the raw form.

## 2. Architecture — module layout, file structure, dependencies

### 2.1 File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/Cargo.toml` | Modify | Add `rand_distr = "0.4"` to `[dependencies]`. Add `[[bench]] name = "scenarios"` with `harness = false`. |
| `bench/src/workload.rs` | Modify | Add public helpers `zipfian_indices`, `lognormal_sizes`, `mix_operations`; add public enum `OpKind`. ~150 LOC added. |
| `bench/src/scenarios.rs` | Create | `seed_for(scenario)`, eight generator functions (4 scenarios × 2 [main + prepopulate] each). ~120 LOC. |
| `bench/src/lib.rs` | Modify | `pub mod scenarios;` + re-exports of the four scenario generators and `OpKind`. |
| `bench/src/runner.rs` | Modify | Add `ScenarioResult` struct + `run_scenario_cell` function. ~80 LOC added. |
| `bench/benches/scenarios.rs` | Create | Bench-binary `main`: iterate 12 cells, call `run_scenario_cell`, stream JSONL. ~70 LOC. |
| `bench/src/summary/discover.rs` | Modify | Add `ScenarioMetrics` type + `load_scenarios_jsonl` parser. ~40 LOC added. |
| `bench/src/summary/render_json.rs` | Modify | Add `scenarios` top-level key to output. ~20 LOC added. |
| `bench/src/summary/render_md.rs` | Modify | Add `render_scenario_table` + appendix scenario rows. ~80 LOC added. |
| `bench/src/bin/summarize.rs` | Modify | Read `scenarios_metrics.jsonl`, pass through to renderers. ~20 LOC added. |
| `bench/tests/fixtures/scenarios_metrics.jsonl` | Create | 2-line synthetic fixture for renderer + smoke tests. |
| `bench/tests/summarize_smoke.rs` | Modify | Add scenarios assertions. ~20 LOC added. |

Total new code: ~500 LOC production + ~150 LOC tests. Slightly above master spec's 400 LOC estimate due to the PR 5 renderer extension being included in this PR's scope.

### 2.2 Library/binary split

Same pattern as PR 4b's micro grid:
- **Library code** (`scenarios.rs`, `workload.rs` extensions, `runner.rs::run_scenario_cell`) is unit-testable without spinning up the binary, callable programmatically by PR 7's CI workflow.
- **Bench binary** (`bench/benches/scenarios.rs`) is the orchestration layer — argv parsing (none for v1; the binary takes no flags), iteration loop, JSONL writing.

### 2.3 Why a separate `[[bench]]` for scenarios

The scenario tier is a separate Cargo `[[bench]]` target with `harness = false` (just like `micro_grid.rs`). It has its own `main()` rather than `criterion_main!`. Two reasons:

1. **Run independence.** Scenarios complete in 1-6 min; the micro grid takes 30+ min. Users frequently want to run one without the other. Two separate `[[bench]]` targets gives `cargo bench --bench scenarios` and `cargo bench --bench micro_grid` as natural CLI splits.
2. **No Criterion in scenarios.** The whole-workload measurement model (§3.4) doesn't fit Criterion's many-samples convention. Mixing into the existing `micro_grid.rs` would mean two iteration paradigms in one file.

### 2.4 Dependency choices

- **`rand_distr = "0.4"`** — pair-version of `rand 0.8` (already in deps from PR 4a). Provides `Zipf` (Zipfian distribution sampler) and `LogNormal` (log-normal distribution sampler). Small crate, no transitive surprises.

That's the only new runtime dep. `serde`/`serde_json`/`tempfile`/`chrono`/`walkdir`/`clap`/`hostname` are already present. `assert_cmd` (dev-dep) is already present from PR 5.

## 3. Workload generators

### 3.1 New primitives in `workload.rs`

Three reusable distribution helpers, plus a new `OpKind` enum used by `mix_operations`. The existing 6 micro-grid generators stay unchanged.

```rust
/// Tag for the four mutating-or-reading operation kinds, used by
/// mix_operations to pick which Operation variant to emit. DeleteMany
/// is intentionally absent — scenarios use the single Delete variant
/// per master spec §4.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind { Allocate, Read, Update, Delete }

/// Zipfian-distributed random indices into [0, prepop_count). Theta
/// controls skew: 0.0 = uniform, 0.99 = YCSB default (heavy skew,
/// ~75% of accesses to ~10% of records).
///
/// Returns Vec<usize> of length `count`. Uses rand_distr::Zipf.
/// Deterministic: same (seed, count, prepop_count, theta) → same Vec.
pub fn zipfian_indices(
    seed: u64,
    count: usize,
    prepop_count: usize,
    theta: f64,
) -> Vec<usize>;

/// Log-normal-distributed sizes in bytes. `median_bytes` and
/// `p99_bytes` parameterize the distribution: log-normal has shape
/// (mu, sigma); we solve for mu/sigma such that exp(mu) = median
/// and exp(mu + 2.326 * sigma) ≈ p99 (z-score 2.326 for the 99th
/// percentile of a standard normal).
///
/// Returns Vec<usize> of length `n`. Sizes clamped to [16, 4_194_304]
/// (16B floor, 4MB ceiling) to avoid pathological outliers from the
/// log-normal tail. ~0.001% of unbounded log-normal samples land
/// at multi-GB sizes; clamping accepts a tiny bias in exchange for
/// stable wall-clock measurements.
pub fn lognormal_sizes(
    seed: u64,
    n: usize,
    median_bytes: usize,
    p99_bytes: usize,
) -> Vec<usize>;

/// Compose a mixed-op sequence from per-op-kind probabilities.
/// Decouples op-kind sampling from access-pattern + size sampling
/// — each op gets a fresh sample from the appropriate input slice.
///
/// `op_specs` is a slice of (OpKind, weight) pairs; weights are
/// normalized internally. `access_pattern` provides alloc_index
/// values for Read/Update/Delete ops (consumed in order). `sizes`
/// provides byte counts for Allocate/Update ops (consumed in order).
///
/// The function consumes one access_pattern entry and/or one sizes
/// entry per op based on op kind:
///   - Allocate → consumes one size
///   - Read → consumes one access_pattern entry
///   - Update → consumes one access_pattern entry AND one size
///   - Delete → consumes one access_pattern entry
///
/// Caller must provide enough entries; otherwise mix_operations
/// panics with a clear message. The scenario generators precompute
/// the right counts based on the op-mix probabilities and op-count.
pub fn mix_operations(
    seed: u64,
    count: usize,
    op_specs: &[(OpKind, f64)],
    access_pattern: &[usize],
    sizes: &[usize],
) -> Vec<Operation>;
```

### 3.2 Determinism contract

Each helper is a pure function: deterministic in `(seed, params)`. Same testability story as PR 4a's existing generators. ChaCha8Rng is the underlying PRNG, matching PR 4a's choice.

### 3.3 Scenario-specific generators in `scenarios.rs`

Each scenario is a thin composition: call the primitives, build a `Workload`. Spec §4.1-4.4 fully specifies the parameters; the generators are deterministic given those parameters.

```rust
use crate::workload::*;

/// Per-scenario seeds. Hardcoded rather than hashed because Rust's
/// DefaultHasher randomizes per-process — derived seeds would change
/// between invocations.
pub fn seed_for(scenario: &str) -> u64 {
    match scenario {
        "ycsb-a" => 0x6001,
        "ycsb-b" => 0x6002,
        "mutation-log" => 0x6003,
        "document-store" => 0x6004,
        _ => panic!("unknown scenario: {scenario}"),
    }
}

/// S1: YCSB-A — 100K records × 1KB pre-pop, 100K ops 50/50 read/update,
/// Zipfian θ=0.99 (heavy skew, ~75% of accesses to ~10% of records).
/// Master spec §4.1.
pub fn gen_ycsb_a(seed: u64) -> Workload;
pub fn gen_ycsb_a_prepopulate(seed: u64) -> Workload;

/// S2: YCSB-B — same setup as S1, mix is 95% read / 5% update.
/// Master spec §4.2.
pub fn gen_ycsb_b(seed: u64) -> Workload;
pub fn gen_ycsb_b_prepopulate(seed: u64) -> Workload;

/// S3: Mutation Log — 10K records, sizes uniform [64B, 4KB], 100K ops
/// 25%/25%/25%/25% allocate/read/update/delete, uniform random.
/// Master spec §4.3.
pub fn gen_mutation_log(seed: u64) -> Workload;
pub fn gen_mutation_log_prepopulate(seed: u64) -> Workload;

/// S4: Document Store — 10K records, log-normal sizes (median 4KB,
/// p99 ≈ 1MB), 50K ops 70%/20%/10% read/allocate/update, Zipfian θ=0.7.
/// Master spec §4.4.
pub fn gen_document_store(seed: u64) -> Workload;
pub fn gen_document_store_prepopulate(seed: u64) -> Workload;
```

The pre-populate generators emit Allocate ops sized per the scenario's distribution (fixed 1KB for YCSB, uniform for mutation log, log-normal for document store). The pre-populate Workload runs first (untimed); the scenario Workload runs second (timed).

### 3.4 Why no Criterion for the scenario runner

Master spec §4.7 sets the runtime budget: "4 scenarios × 3 strict engines = 12 scenario runs. Target ~5-30s per run, ~1-6 minutes total."

Criterion's default convention is many-samples-per-bench for variance estimation. For a 100K-op scenario at ~5-30s per run, Criterion with `sample_size(10)` (Criterion's minimum) produces 12 cells × 10 samples × 5-30s = 10-60 minutes — 10× the spec's budget.

The variance Criterion's machinery is designed to manage (measurement-noise jitter) is not the variance scenarios actually exhibit (workload-driven, access-pattern-skew). 100K ops in a single run is already self-averaging; running it 100 times to "get statistics" measures the wrong thing.

The runner uses inline `Instant::now()` timing instead. Per-op timings collected during the run let us compute p50/p95/p99 from the per-op distribution within a single 100K-op execution — same statistical power as Criterion's bootstrap-of-means but at 1/10th the runtime cost.

## 4. Scenario runner

### 4.1 `ScenarioResult` struct

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScenarioResult {
    pub scenario: String,                         // "ycsb-a"
    pub mode: String,                             // "chisel-strict"
    pub total_wall_clock_ns: u64,                 // total run time
    pub op_count: usize,                          // number of timed ops
    pub throughput_ops_per_sec: f64,              // op_count / (total / 1e9)
    pub p50_ns: f64,                              // from per-op timings
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub final_file_size_bytes: u64,
    pub file_size_delta_bytes: i64,               // final - after-prepop
    pub counters: Option<ChiselCountersDelta>,    // None for non-Chisel
}
```

### 4.2 `run_scenario_cell` function

Lives in `bench/src/runner.rs` alongside the existing micro-grid cell-runners. Reuses `apply_op` and `EngineMode::open` from PR 4b's existing runner module.

Pseudocode:

```rust
pub fn run_scenario_cell(
    mode: EngineMode,
    scenario_name: &str,
    prepopulate_workload: &Workload,
    scenario_workload: &Workload,
) -> ScenarioResult {
    let working = NamedTempFile::new().unwrap();
    let mut engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();

    // Pre-population phase (untimed).
    let mut snapshot_ids = Vec::with_capacity(prepopulate_workload.ops.len());
    drive_workload_with_tx_granularity(
        &mut *engine, prepopulate_workload, /* ops_per_tx */ 1000, &[],
    );
    // After prepopulate, capture the post-prepop file size + counter snapshot
    // as the "before" state for delta computation.
    let size_after_prepop = engine.file_size_bytes().unwrap();
    let counters_before = engine.internal_counters().unwrap();

    // Snapshot the populated identifier list (alloc_index → engine id).
    // [details depend on how prepopulate captures ids; reuse PopulatedSnapshot
    // pattern or accept that prepopulate_workload allocates and we track ids
    // inline]

    // Timed phase: run scenario_workload with per-op Instant::now().
    let mut per_op_ns: Vec<u64> = Vec::with_capacity(scenario_workload.ops.len());
    let mut new_ids: Vec<Identifier> = Vec::new();
    let total_start = Instant::now();
    for op in &scenario_workload.ops {
        let op_start = Instant::now();
        // Each mutating op gets its own tx; reads are bare.
        if op_is_mutating(op) {
            engine.begin().unwrap();
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids);
            engine.commit().unwrap();
        } else {
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids);
        }
        per_op_ns.push(op_start.elapsed().as_nanos() as u64);
    }
    let total_wall_clock = total_start.elapsed();

    // Post-run aux metrics.
    let counters_after = engine.internal_counters().unwrap();
    let size_after = engine.file_size_bytes().unwrap();

    // Compute percentiles from per-op distribution.
    let mut sorted: Vec<f64> = per_op_ns.iter().map(|&n| n as f64).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let p50 = percentile_linear_interp(&sorted, 0.50).unwrap();
    let p95 = percentile_linear_interp(&sorted, 0.95).unwrap();
    let p99 = percentile_linear_interp(&sorted, 0.99).unwrap();

    let total_ns = total_wall_clock.as_nanos() as u64;
    let op_count = scenario_workload.ops.len();

    ScenarioResult {
        scenario: scenario_name.to_string(),
        mode: mode.label().to_string(),
        total_wall_clock_ns: total_ns,
        op_count,
        throughput_ops_per_sec: op_count as f64 / (total_ns as f64 / 1e9),
        p50_ns: p50,
        p95_ns: p95,
        p99_ns: p99,
        final_file_size_bytes: size_after,
        file_size_delta_bytes: size_after as i64 - size_after_prepop as i64,
        counters: counter_delta(counters_before, counters_after),
    }
}
```

Engine drops at end of function; tempfile is auto-deleted. The `per_op_ns` Vec is dropped at end of function — only the percentile statistics are retained. For 100K ops × 8 bytes = ~800KB transient memory.

### 4.3 Transactions in scenarios

Spec §4 doesn't specify transaction boundaries within a scenario. The runner wraps each mutating op in a single-op tx (begin → op → commit). Reads happen outside any tx, matching engine conventions.

This means the per-op timings include the per-tx commit cost — the right thing to measure for a YCSB-style workload, where clients care about the latency of "make this update durable."

The mutating-op detection is a small helper in `runner.rs`:
```rust
fn op_is_mutating(op: &Operation) -> bool {
    matches!(op,
        Operation::Allocate { .. }
            | Operation::Update { .. }
            | Operation::Delete { .. }
            | Operation::DeleteMany { .. })
}
```

### 4.4 Pre-population identifier tracking

The micro-grid `populate_snapshot` captures alloc-order identifiers via the `PopulatedSnapshot` struct. Scenarios use the same pattern but inline: `run_scenario_cell` calls `drive_workload_with_tx_granularity` for the prepopulate phase and accumulates the resulting identifiers into a local `Vec<u64>` (extended from the existing `apply_op` helper that already pushes to `new_ids` on Allocate).

Alternative considered: have `gen_*_prepopulate` produce the prepopulate workload separately and run it via the existing `populate_snapshot` machinery to get a `PopulatedSnapshot`. Rejected because scenarios open a fresh engine per cell and don't need the snapshot file — running prepopulate inline against the same engine is simpler.

## 5. Bench binary (`bench/benches/scenarios.rs`)

```rust
// CLI orchestration for the scenario tier. Iterates the 12 cells
// (4 scenarios × 3 strict modes), calls run_scenario_cell for each,
// streams results to bench/results/scenarios_metrics.jsonl.

use chisel_bench::runner::{run_scenario_cell, EngineMode};
use chisel_bench::scenarios::{
    gen_document_store, gen_document_store_prepopulate, gen_mutation_log,
    gen_mutation_log_prepopulate, gen_ycsb_a, gen_ycsb_a_prepopulate,
    gen_ycsb_b, gen_ycsb_b_prepopulate, seed_for,
};
use chisel_bench::workload::Workload;
use std::io::Write;

const STRICT_MODES: &[EngineMode] = &[
    EngineMode::ChiselStrict,
    EngineMode::RedbStrict,
    EngineMode::SqliteStrict,
];

fn main() -> std::io::Result<()> {
    let out_path = "bench/results/scenarios_metrics.jsonl";
    if let Some(p) = std::path::Path::new(out_path).parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut writer = std::fs::File::create(out_path)?;

    for (scenario_name, prepop, workload) in build_scenarios() {
        for &mode in STRICT_MODES {
            eprintln!("running {scenario_name} on {} ...", mode.label());
            let result = run_scenario_cell(mode, scenario_name, &prepop, &workload);
            serde_json::to_writer(&mut writer, &result)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            eprintln!(
                "  total {:.2}s, p50 {} ns, p99 {} ns, throughput {:.1} ops/s",
                result.total_wall_clock_ns as f64 / 1e9,
                result.p50_ns, result.p99_ns, result.throughput_ops_per_sec
            );
        }
    }
    Ok(())
}

fn build_scenarios() -> Vec<(&'static str, Workload, Workload)> {
    let s1 = seed_for("ycsb-a");
    let s2 = seed_for("ycsb-b");
    let s3 = seed_for("mutation-log");
    let s4 = seed_for("document-store");
    vec![
        ("ycsb-a",         gen_ycsb_a_prepopulate(s1),         gen_ycsb_a(s1)),
        ("ycsb-b",         gen_ycsb_b_prepopulate(s2),         gen_ycsb_b(s2)),
        ("mutation-log",   gen_mutation_log_prepopulate(s3),   gen_mutation_log(s3)),
        ("document-store", gen_document_store_prepopulate(s4), gen_document_store(s4)),
    ]
}
```

The binary streams output to JSONL — flushing after each cell. Crash-resilient: a Ctrl-C mid-run leaves N completed cells parseable.

## 6. Output schema

### 6.1 `scenarios_metrics.jsonl` format

One JSON object per line, one line per (scenario, mode) cell. Mirrors `aux_metrics.jsonl` schema decisions (top-level identifying fields, explicit `null` for missing data, JSONL for streaming).

```jsonl
{"scenario":"ycsb-a","mode":"chisel-strict","total_wall_clock_ns":15234567890,"op_count":100000,"throughput_ops_per_sec":6566.4,"p50_ns":120000.0,"p95_ns":180000.0,"p99_ns":250000.0,"final_file_size_bytes":104857600,"file_size_delta_bytes":4194304,"counters":{"cache_hits":99000,"cache_misses":1000,"fsync_calls":100000,"pages_allocated":12500}}
{"scenario":"ycsb-a","mode":"redb-strict","total_wall_clock_ns":...,"counters":null}
... (12 lines total: 4 scenarios × 3 modes)
```

Schema choices:
- **`scenario` and `mode` as separate fields** (not concatenated). Same as `row`/`mode`/`size` in aux_metrics.jsonl — avoids slug-parsing in the post-processor.
- **Both `final_file_size_bytes` AND `file_size_delta_bytes`**. Master spec §4.5 calls for "final file size"; the delta is more useful for cross-mode comparisons. Surface both.
- **`counters: null` for non-Chisel modes** — consistent with the aux_metrics.jsonl convention.
- **`p50/p95/p99` as `f64`** — same type as in micro-grid percentiles; renderer treats them uniformly.

### 6.2 `results.json` extension

PR 5's results.json has two top-level keys: `metadata` and `cells`. PR 6 adds a third: `scenarios`.

```json
{
  "metadata": {
    "timestamp": "...",
    "chisel_commit": "...",
    "machine": {...},
    "post_processor_version": "0.1.0",
    "criterion_dir": "...",
    "aux_metrics_path": "...",
    "scenarios_metrics_path": "bench/results/scenarios_metrics.jsonl",
    "cell_count": 165,
    "scenario_count": 12
  },
  "cells": { "<row>/<mode>/<size>": { ... }, ... },
  "scenarios": {
    "ycsb-a/chisel-strict": {
      "total_wall_clock_ns": 15234567890,
      "op_count": 100000,
      "throughput_ops_per_sec": 6566.4,
      "p50_ns": 120000.0,
      "p95_ns": 180000.0,
      "p99_ns": 250000.0,
      "final_file_size_bytes": 104857600,
      "file_size_delta_bytes": 4194304,
      "counters": { ... }
    },
    "ycsb-a/redb-strict": { ..., "counters": null },
    "ycsb-a/sqlite-strict": { ..., "counters": null },
    "ycsb-b/chisel-strict": { ... },
    ...
  }
}
```

Composite-key form `<scenario>/<mode>` for `scenarios` (parallel to `<row>/<mode>/<size>` for `cells`). PR 7's CI diff treats both objects uniformly — iterate keys, compare per-key values, flag deltas exceeding the regression threshold.

Two new metadata fields:
- **`scenarios_metrics_path`** — the path the post-processor read scenarios from.
- **`scenario_count`** — usually 12, but could be partial after a Ctrl-C'd run.

The existing `metadata.cell_count` field stays counting micro-grid cells only.

### 6.3 Markdown summary extension

PR 5's `summary.md` has 4 sections after the header: Method, Micro grid, File-size delta, Chisel internals appendix. PR 6 inserts a new section between Method and Micro grid:

```markdown
## Scenario tier

End-to-end YCSB-style workloads. Each scenario runs once per strict
durability mode (chisel-strict, redb-strict, sqlite-strict). Per-op
timings collected inline via `Instant::now()` before/after each op;
percentiles computed from the full distribution (no Criterion
sampling — see PR 6 spec §3.4).

| scenario | mode | throughput | p50 | p95 | p99 | total | final size |
|----------|------|-----------:|----:|----:|----:|------:|-----------:|
| ycsb-a | chisel-strict | 6566 ops/s | 120 µs | 180 µs | 250 µs | 15.23 s | 100.00 MB |
| ycsb-a | redb-strict   | ... |
| ycsb-a | sqlite-strict | ... |
| ycsb-b | chisel-strict | ... |
... (12 rows)
```

The Chisel internals appendix gets a parallel scenario-tier subsection (the 4 chisel-strict scenarios get their counter deltas listed). Same `# row | size | counters...` table layout as the micro-grid appendix, with `scenario` substituted for `row` and no size column.

The "Scenario tier" section comes before "Micro grid" because scenarios are the headline regression-detection numbers. Micro grid is diagnostic. A reader reads top-down for "did anything regress?" — answered in the scenario table — and drills down to micro grid only when a scenario shifts suspiciously.

Time and byte values in scenario rows use the magnitude-adaptive formatters from PR 5's `summary::format`. Throughput is shown as integer ops/sec.

### 6.4 Renderer extension data flow

PR 5's `discover_cells` is augmented with a parallel `load_scenarios_jsonl(path) -> Vec<ScenarioMetrics>` that parses the new file. The CLI binary calls both `discover_cells` and `load_scenarios_jsonl`, passes both Vecs through to the renderers.

PR 5's `Cell` struct stays unchanged. New `ScenarioMetrics` struct mirrors the JSONL schema:

```rust
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScenarioMetrics {
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
```

Renderer signatures gain a `scenarios: &[ScenarioMetrics]` parameter:

```rust
pub fn render_json(
    cells: &[Cell],
    scenarios: &[ScenarioMetrics],
    metadata: &Metadata,
) -> Value;

pub fn render_markdown(
    cells: &[Cell],
    scenarios: &[ScenarioMetrics],
    metadata: &Metadata,
) -> String;
```

Empty `scenarios` slice = the renderer omits the Scenario tier section + emits `"scenarios": {}` in JSON. This keeps PR 5's existing tests passing (they don't pass scenarios; renderer handles empty gracefully).

## 7. Tests, acceptance criteria, deferred items

### 7.1 Unit tests

**In `workload.rs`** (new tests, ~6 cases):
1. `zipfian_indices_determinism` — same args → same Vec.
2. `zipfian_indices_distribution` — at θ=0.99 with 10K samples over [0, 100), ~75% of samples land in the top decile (smoke check on Zipfian skew).
3. `lognormal_sizes_determinism` — same args → same Vec.
4. `lognormal_sizes_clamps_outliers` — 10K samples; assert all in [16, 4_194_304].
5. `mix_operations_op_proportions` — 50/50 op-mix over 10K ops produces ~5000 of each ±200 (binomial slack).
6. `mix_operations_uses_provided_inputs` — emitted ops' alloc_indices come from access_pattern, sizes from sizes slice.

**In `scenarios.rs`** (new module's tests, ~3 cases):
7. `gen_ycsb_a_shape` — workload has name="ycsb-a", op_count=100_000, prepop_count=100_000, op-mix ~50/50 read/update.
8. `gen_mutation_log_shape` — op_count=100_000, op-mix ~25/25/25/25, sizes uniform [64, 4096].
9. `gen_document_store_shape` — op_count=50_000, op-mix ~70/20/10 read/alloc/update, lognormal sizes within [16, 4_194_304].

### 7.2 Runner-level tests

**In `bench/src/runner.rs`** (~2 cases):
10. `run_scenario_cell_chisel_smoke` — small scenario (5K ops) against ChiselStrict; assert ScenarioResult populated, p50 < p99 ≤ p95-or-near-p99 (sanity), throughput > 0.
11. `run_scenario_cell_returns_counters_only_for_chisel` — same scenario against RedbStrict; assert `counters: None`.

### 7.3 Renderer tests

**In `bench/src/summary/`** (~2 cases):
12. `render_json_includes_scenarios_top_level` — render with both cells and scenarios; assert `"scenarios"` is an object with N entries.
13. `render_markdown_includes_scenario_table` — render with scenarios; assert `"## Scenario tier"` section is present and contains expected rows.

### 7.4 Integration smoke

**Extends `bench/tests/summarize_smoke.rs`** (~1 new assertion):
14. `summarize_smoke_with_scenarios_jsonl` — invoke `summarize` against fixtures including `scenarios_metrics.jsonl`; verify summary.md has the scenario section and results.json has the `scenarios` top-level key.

Total: 14 new tests on top of PR 5's existing 54.

### 7.5 Test fixtures

New committed file: `bench/tests/fixtures/scenarios_metrics.jsonl` — 2 lines (1 chisel-strict, 1 redb-strict) with hand-crafted known values for assertion.

Existing `criterion/` and `aux_metrics.jsonl` fixtures stay unchanged.

### 7.6 Acceptance criteria

PR 6 is mergeable when:

1. `cargo build -p chisel-bench` and `cargo test -p chisel-bench` pass on macOS and Linux.
2. `cargo clippy -p chisel-bench --all-targets -- -D warnings` is clean.
3. `cargo fmt -- --check` is clean across touched files.
4. The 14 new tests in §7.1-§7.4 pass.
5. `cargo bench --bench scenarios` runs to completion in **under 10 minutes** on a developer laptop. (Spec target is 1-6 min; 10 min ceiling allows for variance and slow CI hardware.)
6. After running scenarios + running summarize: `summary.md` contains a "Scenario tier" section with 12 rows; `results.json` has a `scenarios` top-level key with 12 entries; `bench/results/scenarios_metrics.jsonl` has exactly 12 lines.
7. Project commenting standards held — file headers, doc comments explain choices not mechanics.

### 7.7 What PR 6 does NOT include

Deferred to PR 7 (CI):
- The CI workflow that runs `cargo bench --bench scenarios` on each PR.
- The diff binary that compares two `results.json` files and posts a regression report.
- GitHub Actions configuration for PR-comment posting.

Out of scope entirely:
- Vendored YCSB workload trace files.
- Multiple scenario seeds for variance estimation (one run per cell is the spec).
- Custom Criterion HTML for scenarios (no Criterion in scenarios).
- Scenario tier in unsafe modes.
- Per-scenario raw archive output.

## 8. Build sequence relationship

PR 6 is the sixth PR in the bench-suite series. The series state after this PR lands:

| # | PR | Status |
|---|----|--------|
| 1 | Instrumentation precursor | Landed |
| 2 | `bench/` subcrate + Engine trait + ChiselEngine | Landed |
| 3 | RedbEngine + SqliteEngine + equivalence tests | Landed |
| 4a | Workload data layer | Landed |
| 4b | Runner + 6-row Criterion micro grid | Landed |
| 5 | Markdown summary post-processor | Landed |
| **6** | **Scenario tier** | **This PR** |
| 7 | CI workflow + regression-diff binary | Pending |
| 8 | Cross-engine relative-performance tests (addendum) | Pending — own spec/plan |

CLAUDE.md gets a one-paragraph update on PR 6 merge to reflect scenario tier availability.

### 8.1 Rollback

PR 6 fails review or is reverted: the micro grid + post-processor (PRs 4b + 5) still works exactly as before. Users still get markdown summaries from `cargo bench --bench micro_grid && cargo run --bin summarize`; just no scenario section. PR 7 cannot start without PR 6's `scenarios` shape in results.json, but PR 7 is gated on PR 6 anyway.

## 9. Open implementation-phase questions

These are deferred to the implementation plan:

- Whether `mix_operations` should use `rand_distr::WeightedIndex` for op-kind selection or roll its own cumulative-weight sampler. Performance-equivalent at 100K ops; `WeightedIndex` is more idiomatic.
- Exact lognormal mu/sigma derivation from (median, p99). Closed-form: `mu = ln(median)`, `sigma = (ln(p99) - mu) / 2.326`. Plan picks the explicit formula.
- Whether `run_scenario_cell` takes prepopulate as a `Workload` parameter or runs a closure that knows how to populate. Going with `Workload` parameter for consistency with the existing micro-grid pattern.
- Specific JSONL flush cadence — we flush after each cell (1 per cell), but could flush after each line if scenarios ever get extended to multi-line per cell. Per cell is the simplest reasonable choice.
- Whether the `op_is_mutating` helper lives in `runner.rs` (where it's used) or `workload.rs` (where Operation lives). Going with runner.rs since it's a runner concern.

These are implementation details that do not affect the design contract.
