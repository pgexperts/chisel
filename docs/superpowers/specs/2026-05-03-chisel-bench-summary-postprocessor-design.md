# Chisel Bench — Markdown Summary Post-Processor — Design

**Date:** 2026-05-03
**Status:** Design approved; implementation plan pending.
**Scope:** Add a Rust binary `chisel-bench-summarize` (in `bench/src/bin/summarize.rs`) plus a supporting library module (`bench/src/summary/`) that reads PR 4b's bench output (Criterion `sample.json` files + `aux_metrics.jsonl`) and produces three artifacts in a timestamped output directory: a human-readable `summary.md`, a machine-readable `results.json` indexed by cell-key for PR 7's CI diff, and a `raw/` archive of the per-cell Criterion JSON (both `sample.json` and `estimates.json`) for reproducibility. PR 5 of the bench-suite series.

This spec follows on from `2026-04-25-chisel-benchmark-suite-design.md` (the overall bench-suite design, especially §7.1 on output artifacts) and `2026-05-01-chisel-bench-runner-micro-grid-design.md` (PR 4b, which produces the input data this PR consumes). PR 5 is a pure post-processor — no benches run, no engines touched.

## 1. Goals and Non-Goals

### Goals

- Land a single Rust binary `summarize` that reads `target/criterion/<row>/<mode>/<size>/sample.json` files plus `bench/results/aux_metrics.jsonl` and emits three output artifacts under `bench/results/<UTC-ISO8601>/`: `summary.md`, `results.json`, `raw/`.
- Markdown summary follows master spec §7.1 layout, adjusted for the 6-row reality of PR 4b's micro grid: header (timestamp, chisel commit, machine, durability legend), one table per micro-grid row with cells `p50 (p99)` in magnitude-adaptive units, file-size delta table, Chisel internals appendix.
- JSON results uses a flat composite-key schema (`<row>/<mode>/<size>` keys) to keep PR 7's CI diff trivially `before[k] vs after[k]`. Includes metadata (timestamp, chisel commit, machine info, cell count) for provenance.
- Raw archive copies only `estimates.json` and `sample.json` per cell (not the full Criterion HTML reports) to keep the archive ~330 KB rather than many MB.
- Library/binary split mirrors PR 4b: non-CLI logic lives under `bench/src/summary/`, the binary is a thin wrapper. PR 7's CI workflow can call the library code directly if it wants.
- 13 unit + integration tests using committed fixtures (a 2-cell synthetic Criterion tree).

### Non-Goals (this PR)

- *Scenario tier markdown.* PR 6 adds scenarios; until then the markdown's scenario section is omitted entirely (not rendered as an empty placeholder). The post-processor uses defensive JSONL deserialization so unknown future fields (e.g., a `scenario` key on aux metrics) won't break parsing.
- *True p99 from sample.json sorting.* Criterion does not natively compute a p99. We surface the upper bound of Criterion's 95% confidence interval on the mean as the closest available proxy, and document it in the markdown header. Computing a real p99 would require parsing `sample.json` and sorting; deferred.
- *CI diff binary.* PR 7's job. PR 5 just produces the `results.json` PR 7 will consume.
- *HTML output beyond Criterion's native reports.* Criterion already produces per-cell HTML at `target/criterion/<row>/<mode>/<size>/report/`. PR 5's markdown is the cross-cell pivot; HTML duplicates Criterion without benefit.
- *Variance-driven sample-size tuning recommendations.* Could be a future "warn if std-dev high" feature; not v1.
- *Multi-run comparison.* PR 5 produces one summary per run. Comparing two runs is PR 7's job.

## 2. Architecture — file structure, dependencies, library/binary split

### 2.1 File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/Cargo.toml` | Modify | Add `chrono = "0.4"`, `walkdir = "2"`, `clap = { version = "4", features = ["derive"] }`, `hostname = "0.4"` to `[dependencies]`. Add `assert_cmd = "2"` to `[dev-dependencies]`. Add `[[bin]] name = "summarize"`. |
| `bench/src/bin/summarize.rs` | Create | CLI entry point: clap parsing, top-level `run` orchestration, exit codes. ~80 LOC. |
| `bench/src/summary/mod.rs` | Create | Module root; re-exports the units below. ~20 LOC. |
| `bench/src/summary/format.rs` | Create | Magnitude-adaptive formatters (`format_duration_ns`, `format_bytes`) + `parse_size_to_bytes` size-label parser. Pure functions. ~60 LOC. |
| `bench/src/summary/discover.rs` | Create | `Cell`, `TimingStats`, `AuxMetrics` types; `discover_cells` (walks Criterion + parses JSONL); `copy_raw_archive`. ~150 LOC. |
| `bench/src/summary/render_md.rs` | Create | Markdown renderer: header, per-row tables, file-size table, internals appendix. ~150 LOC. |
| `bench/src/summary/render_json.rs` | Create | JSON renderer: composite-key cells map + metadata. ~50 LOC. |
| `bench/src/summary/metadata.rs` | Create | `Metadata`/`MachineInfo` types + `gather_metadata` (`git rev-parse HEAD`, hostname). ~60 LOC. |
| `bench/src/lib.rs` | Modify | `pub mod summary;` + re-exports of `Cell`, `Metadata`. |
| `bench/tests/fixtures/criterion/...` | Create | Synthetic 2-cell Criterion tree for tests. |
| `bench/tests/fixtures/aux_metrics.jsonl` | Create | Companion JSONL fixture (2 lines). |
| `bench/tests/summarize_smoke.rs` | Create | Integration test invoking the binary via `assert_cmd`. ~50 LOC. |

Total new code: ~570 LOC production + ~250 LOC tests.

### 2.2 Library/binary split rationale

`bench/src/bin/summarize.rs` is the user-facing entry point: argv parsing, file paths, exit codes. The library code under `bench/src/summary/` does the actual work (discovery, rendering, archive copy). This split mirrors PR 4b's runner-vs-bench-binary separation, and gives PR 7's CI workflow a programmatic entry point if it wants one (it can call `chisel_bench::summary::*` rather than shelling out to the binary).

The four library submodules (`format`, `discover`, `render_md`, `render_json`, `metadata`) split by responsibility, not by output artifact. The `Vec<Cell>` produced by `discover_cells` is the same input both renderers consume, so format-specific code stays small and testable in isolation.

### 2.3 Dependency choices

- **`chrono = "0.4"`** — UTC ISO 8601 timestamp formatting. Widely-used, small, stable. The `time` crate is an alternative; `chrono` is more familiar and the API is a single function call.
- **`walkdir = "2"`** — recursive filesystem walk for the Criterion tree and raw-archive copy. Could be hand-rolled with `std::fs::read_dir` (~30 LOC), but `walkdir` handles the recursion + symlink edge cases cleanly and is ~10 KB of dep weight.
- **`clap = "4"` with `derive`** — argv parsing for `--out`, `--criterion`, `--aux`. Could use raw `std::env::args` for ~15 LOC of manual parsing, but clap gives `--help` text + future-proofing for free.
- **`hostname = "0.4"`** — for `MachineInfo.hostname`. Small (~20 LOC of bindings); avoids a shell-out to `hostname`.
- **`assert_cmd = "2"`** (dev-dep only) — for the integration test's binary invocation. Standard pattern for testing `[[bin]]` targets.

`serde` and `serde_json` are already in `[dependencies]` from PR 4b's aux-metrics writer. No need to re-add.

### 2.4 CLI shape

```
chisel-bench-summarize 0.1
Post-process Criterion + aux-metrics output into summary.md + results.json

USAGE:
    summarize [--out <DIR>] [--criterion <DIR>] [--aux <FILE>]

OPTIONS:
    --out <DIR>         Output directory (default: bench/results/<UTC-ISO8601>/)
    --criterion <DIR>   Criterion output directory (default: target/criterion)
    --aux <FILE>        Aux-metrics JSONL (default: bench/results/aux_metrics.jsonl)
    --help              Print help
    --version           Print version
```

Defaults match PR 4b's runner output paths. Overrides exist for testing and unusual setups.

## 3. Data model and discovery

### 3.1 Core types in `bench/src/summary/discover.rs`

```rust
/// One cell of the micro grid, joined across Criterion and aux_metrics.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Cell {
    pub row: String,             // "allocate-1pertx"
    pub mode: String,            // "chisel-strict"
    pub size: String,            // "32B"
    pub timing: Option<TimingStats>,    // None if sample.json missing/corrupt
    pub aux: Option<AuxMetrics>,        // None if cell missing from aux_metrics.jsonl
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct TimingStats {
    pub p50_ns: f64,    // 50th percentile of sample distribution (per §3.3)
    pub p95_ns: f64,    // 95th percentile of sample distribution
    pub p99_ns: f64,    // 99th percentile of sample distribution
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct AuxMetrics {
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,    // None for non-Chisel
}
```

`ChiselCountersDelta` is reused from `bench::runner` (PR 4b). `Cell` carries `Option<TimingStats>` and `Option<AuxMetrics>` rather than requiring both — three real cases need handling:

- Both present: normal cell.
- Timing present, aux missing: aux_metrics.jsonl truncated mid-write or bench crashed between Criterion's last sample and `aux.append`.
- Aux present, timing missing: rare; possible if Criterion's sample.json got corrupted (especially relevant given the SQLite WAL flake we hit in PR 4b — a partial write path could in principle land aux without sample data).

Renderers handle each case: missing data shows as `—` in markdown and `null` in JSON.

### 3.2 Discovery algorithm

```rust
pub fn discover_cells(
    criterion_dir: &Path,
    aux_metrics_path: &Path,
) -> Result<Vec<Cell>, DiscoverError>;
```

Implementation steps:

1. **Load aux_metrics.jsonl** into `HashMap<(String, String, String), AuxMetrics>` keyed by `(row, mode, size)`. Single pass; bad lines logged to stderr but don't abort the whole load.
2. **Walk `criterion_dir`** with `walkdir::WalkDir::new(criterion_dir).max_depth(3).min_depth(3)`. Each leaf at depth 3 is a `<row>/<mode>/<size>` directory. For each leaf, read `sample.json` (compute p50, p95, p99 from the raw distribution per §3.3) into `TimingStats`. We do not read `estimates.json` for the timing fields — `sample.json` is sufficient and authoritative.
3. **Join** by `(row, mode, size)` key. Cells with either source produce a `Cell`; missing-on-one-side entries get `None` for the missing field.
4. **Sort** the result by `(row, mode, size)` so output is deterministic.

### 3.3 Criterion JSON parsing — what we read

We read one file per cell: `sample.json`. Criterion does not store throughput in any per-cell JSON file (it computes throughput at HTML-render time from `Throughput::Elements(N)` set per row group in PR 4b's bench file), so PR 5 does not surface a `throughput_per_sec` field — PR 7's CI diff can compute it from `p50_ns` + a hardcoded N-per-row table if it wants. This keeps PR 5 decoupled from PR 4b's per-row N values.

**`sample.json`** (Criterion 0.5) is the raw measurement data:

```json
{
  "iters": [10.0, 10.0, 10.0, ...],
  "times": [12340.5, 15678.2, 12450.1, ...]
}
```

Each `times[i]` is the wall-clock time for a batch of `iters[i]` iterations. Per-iteration time = `times[i] / iters[i]`. We compute all three percentiles from the per-iteration times:

```rust
fn compute_percentiles(times: &[f64], iters: &[f64]) -> (f64, f64, f64) {
    let mut per_iter: Vec<f64> = times.iter().zip(iters)
        .map(|(t, i)| t / i)
        .collect();
    per_iter.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        percentile_linear_interp(&per_iter, 0.50),
        percentile_linear_interp(&per_iter, 0.95),
        percentile_linear_interp(&per_iter, 0.99),
    )
}
```

`percentile_linear_interp` follows numpy's default: `idx_f = q * (n - 1)`, then linear interpolation between `sorted[floor(idx_f)]` and `sorted[ceil(idx_f)]`. This gives well-defined values even when the percentile index lands between samples — important for the Criterion default of 100 samples where `p99` is between the 99th and 100th sorted element.

Computing all three percentiles from the same sample distribution makes them mutually comparable: a regression that shifts `p50` and `p99` together has a clearer signal than one where p50 comes from `median.point_estimate` (Criterion's bootstrap-related computation) and p99 comes from a CI proxy. Trade-off: we lose any benefit from Criterion's bootstrap-stabilized median estimate, but for the regression-detection use case (compare two distributions), self-consistent samples beat heterogeneous estimators.

**`estimates.json`** is not read for the rendered output. The raw archive (§7.4) still copies it alongside `sample.json` for forensic / reproducibility purposes — Criterion's mean-and-slope statistical estimates are useful when a reader is debugging an unexpected percentile result and wants to see what Criterion's own bootstrap analysis said.

**Honest disclosure** in the markdown header: the percentiles are computed from Criterion's raw samples (typically 100 per cell at default config). At small sample counts, p99 has high statistical uncertainty — readers wanting tighter tail bounds use Criterion's per-cell HTML report which shows the full distribution.

### 3.4 Pre-populated identifier map analogue

Not relevant for PR 5 — we don't open engines, we just read files.

## 4. Markdown rendering

### 4.1 Document structure

The markdown is composed top-to-bottom:

1. **H1 title** — `# Chisel Benchmark Summary`.
2. **Header block** — bolded key/value lines for timestamp, chisel commit, machine info, cell count (with skip count if any).
3. **Durability mode legend** — bulleted list explaining the five modes.
4. **Wall-clock unit and percentile disclaimer** — one paragraph noting magnitude-adaptive units and the percentile computation: p50, p95, and p99 are computed directly from Criterion's raw `sample.json` per-iteration times via numpy-style linear interpolation. Three percentiles share the same sample distribution so they're mutually comparable. With Criterion's default ~100 samples per cell, p99 has appreciable statistical uncertainty — readers wanting tight tail bounds consult Criterion's per-cell HTML report which shows the full distribution.
5. **Micro grid section** (`## Micro grid`) — one H3 subsection per row group with a 5-row × 6-column table. Cell format: `<p50> (<p99>)` in magnitude-adaptive units. Missing cells: `—`.
6. **File-size delta section** (`## File-size delta`) — single table; rows = (row, mode) pairs, columns = sizes. Cell format: `<signed_byte_delta>` in magnitude-adaptive bytes (B/KB/MB).
7. **Chisel internals appendix** (`## Chisel internals appendix`) — single table; rows = (row, size) pairs filtered to `chisel-strict`, columns = `cache_hits`, `cache_misses`, `fsync_calls`, `pages_allocated`.
8. **Notes section** — bulleted list of skipped cells with reasons; bench-host filesystem caveat (APFS reflink vs ext4 byte-copy).
9. **Footer** — generator version + chisel commit.

### 4.2 Magnitude-adaptive formatters in `bench/src/summary/format.rs`

```rust
/// Auto-pick ns/µs/ms/s based on magnitude. Two decimal places at every
/// switch point so cells line up uniformly in tables.
pub fn format_duration_ns(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{:.0} ns", ns)
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

/// Sign-prefixed bytes formatter (1024-base, matches `ls -lh`).
pub fn format_bytes(bytes: i64) -> String {
    let abs = bytes.unsigned_abs();
    let sign = if bytes < 0 { "-" } else if bytes > 0 { "+" } else { "" };
    if abs < 1_024 {
        format!("{}{} B", sign, abs)
    } else if abs < 1_024 * 1_024 {
        format!("{}{:.1} KB", sign, abs as f64 / 1_024.0)
    } else if abs < 1_024 * 1_024 * 1_024 {
        format!("{}{:.2} MB", sign, abs as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{}{:.2} GB", sign, abs as f64 / 1_024.0_f64.powi(3))
    }
}
```

The `:.2` precision at every magnitude boundary gives consistent column widths (`1.20 µs` lines up under `145 µs` more readably than `1.2 µs` would).

### 4.3 Size-label sorting

Lexical sort puts `"1MB"` before `"32B"` — wrong for size ordering. The renderer parses each size label to a byte count via:

```rust
pub fn parse_size_to_bytes(label: &str) -> Option<u64>;   // "32B" → 32, "2KB" → 2048, "1MB" → 1_048_576
```

Then sorts numerically. Decouples the renderer from PR 4b's specific SIZES table — future additions like `512B` or `64MB` work without changes.

Mode ordering is hardcoded as the canonical list `["chisel-strict", "redb-strict", "redb-unsafe", "sqlite-strict", "sqlite-unsafe"]` (matches master spec §3.3 column order). Modes are a closed enumeration; sizes are open.

## 5. JSON rendering

### 5.1 Schema

Flat composite-key map with metadata block:

```json
{
  "metadata": {
    "timestamp": "2026-05-03T13:22:15Z",
    "chisel_commit": "3254500",
    "machine": {
      "os": "macos",
      "arch": "aarch64",
      "hostname": "chrispowerbook"
    },
    "post_processor_version": "0.1",
    "criterion_dir": "target/criterion",
    "aux_metrics_path": "bench/results/aux_metrics.jsonl",
    "cell_count": 165
  },
  "cells": {
    "allocate-1pertx/chisel-strict/32B": {
      "p50_ns": 1234.5,
      "p95_ns": 1567.8,
      "p99_ns": 1890.2,
      "file_size_delta_bytes": 8192,
      "counters": { "cache_hits": 0, "cache_misses": 1, "fsync_calls": 2, "pages_allocated": 4 }
    },
    "allocate-1pertx/redb-strict/32B": {
      "p50_ns": 1456.2,
      "p95_ns": 1789.4,
      "p99_ns": 2012.8,
      "file_size_delta_bytes": 4096,
      "counters": null
    }
  }
}
```

Key design points:

- **Composite key string `<row>/<mode>/<size>`** matches Criterion's directory layout 1:1 — same join key on disk and in JSON. Grep-friendly for debugging.
- **Missing data is explicit `null`**, not omitted — keeps the schema rectangular for PR 7's diff to do `before["k"] vs after["k"]` without conditional-existence checks.
- **No `throughput_per_sec` field.** Criterion does not store throughput in any per-cell JSON file (per §3.3); computing it would require hardcoding PR 4b's per-row N table in the post-processor, which is the kind of cross-PR coupling we avoid. Consumers (PR 7's diff) can compute throughput from `p50_ns + N` if they want.
- **`counters: null` for non-Chisel modes** mirrors the JSONL aux-metrics format exactly.
- **`metadata.machine` is an object**, not a single string — easier to filter in CI ("only flag regressions on linux-aarch64 hosts").

### 5.2 Implementation

```rust
pub fn render_json(cells: &[Cell], metadata: &Metadata) -> serde_json::Value {
    let mut cells_map = serde_json::Map::new();
    for cell in cells {
        let key = format!("{}/{}/{}", cell.row, cell.mode, cell.size);
        cells_map.insert(key, render_cell_json(cell));
    }
    serde_json::json!({ "metadata": metadata, "cells": cells_map })
}

fn render_cell_json(cell: &Cell) -> serde_json::Value {
    serde_json::json!({
        "p50_ns": cell.timing.map(|t| t.p50_ns),
        "p95_ns": cell.timing.map(|t| t.p95_ns),
        "p99_ns": cell.timing.map(|t| t.p99_ns),
        "file_size_delta_bytes": cell.aux.map(|a| a.file_size_delta_bytes),
        "counters": cell.aux.and_then(|a| a.counters),
    })
}
```

Two thin functions, fully covered by a single unit test asserting the schema round-trips through serde correctly.

## 6. Metadata gathering

### 6.1 Types in `bench/src/summary/metadata.rs`

```rust
#[derive(Clone, Debug, serde::Serialize)]
pub struct Metadata {
    pub timestamp: String,                       // RFC 3339 UTC, no offset suffix (`Z`)
    pub chisel_commit: String,                   // `git rev-parse HEAD` or `"unknown"` on fail
    pub machine: MachineInfo,
    pub post_processor_version: &'static str,    // env!("CARGO_PKG_VERSION")
    pub criterion_dir: String,                   // resolved path string
    pub aux_metrics_path: String,
    pub cell_count: usize,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MachineInfo {
    pub os: String,         // std::env::consts::OS — "macos", "linux"
    pub arch: String,       // std::env::consts::ARCH — "aarch64", "x86_64"
    pub hostname: String,   // hostname crate
}
```

### 6.2 Gathering function

`gather_metadata(criterion_dir, aux_metrics_path, cell_count) -> Result<Metadata, ...>`:

1. **`timestamp`**: `chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")` — RFC 3339 with colons. (The output *directory name* uses a hyphenated variant in §7.1 for filesystem safety on Windows; this is the metadata field, which goes in JSON and follows the standard.)
2. **`chisel_commit`**: shell out to `git rev-parse HEAD`, `current_dir` set to the chisel repo root (`CARGO_MANIFEST_DIR/..`). If the command fails or returns non-zero, set to `"unknown"` and continue silently. Common in tarballed/CI contexts where git isn't available.
3. **`machine.os`**: `std::env::consts::OS`.
4. **`machine.arch`**: `std::env::consts::ARCH`.
5. **`machine.hostname`**: `hostname::get()?`.
6. **`post_processor_version`**: `env!("CARGO_PKG_VERSION")` from `bench/Cargo.toml`.

## 7. Output directory and CLI orchestration

### 7.1 Output directory naming

Default: `bench/results/<UTC-ISO8601-with-hyphens>/` — e.g., `bench/results/2026-05-03T13-22-15Z/`. Hyphenated colons because macOS/Windows can't use `:` in paths. Lexically sortable (matches chronological).

User can override via `--out <DIR>`; the override is taken literally with no transformations.

### 7.2 Top-level orchestration in `bench/src/bin/summarize.rs`

```rust
fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let cells = chisel_bench::summary::discover_cells(&cli.criterion, &cli.aux)?;
    if cells.is_empty() {
        return Err("no cells discovered — did you run cargo bench --bench micro_grid?".into());
    }

    let out_dir = cli.out.unwrap_or_else(|| {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
        PathBuf::from(format!("bench/results/{ts}"))
    });
    std::fs::create_dir_all(&out_dir)?;

    let metadata = chisel_bench::summary::gather_metadata(&cli.criterion, &cli.aux, cells.len())?;
    let md = chisel_bench::summary::render_markdown(&cells, &metadata);
    let json = chisel_bench::summary::render_json(&cells, &metadata);

    std::fs::write(out_dir.join("summary.md"), md)?;
    std::fs::write(out_dir.join("results.json"), serde_json::to_string_pretty(&json)?)?;
    chisel_bench::summary::copy_raw_archive(&cli.criterion, &out_dir.join("raw"))?;

    println!("Wrote {} cells to {}", cells.len(), out_dir.display());
    Ok(())
}
```

The CLI is thin — most work is in the library functions.

### 7.3 Failure handling matrix

| Failure mode | Behavior |
|--------------|----------|
| `criterion_dir` doesn't exist | Exit 1, message: `error: Criterion output directory '<path>' does not exist; run cargo bench --bench micro_grid first` |
| `criterion_dir` exists but has no sample.json files | Exit 1, message: `error: no cells found under '<path>'; the directory exists but contains no sample.json` |
| `aux_metrics_path` missing | Warn to stderr: `warning: aux-metrics file '<path>' missing; cells will have no file-size or counter data`. Continue with `aux: None` for every cell. |
| `aux_metrics_path` exists but malformed lines | Warn per bad line to stderr: `warning: skipping malformed aux line N: <error>`. Continue. |
| Individual `sample.json` malformed | Warn to stderr: `warning: skipping malformed sample.json at <path>: <error>`. Set that cell's `timing: None`. |
| `git rev-parse HEAD` fails | Set `chisel_commit = "unknown"`. Continue silently. |
| Output directory creation fails | Exit 1 with the underlying I/O error. |
| Write of summary.md or results.json fails | Exit 1. |

Principle: refuse-to-run only when there's nothing to render. Warn-and-continue when partial rendering still produces a useful artifact.

### 7.4 Raw archive copy

`copy_raw_archive(criterion_dir, raw_out_dir)` walks `criterion_dir` and copies only `estimates.json` and `sample.json` files (preserving directory structure). Skips `report/`, `change/`, plots, etc. — those stay in `target/criterion/` for live browsing; the archive's job is reproducibility (so the markdown numbers can be regenerated from the archive if `target/` is wiped). 165 cells × 2 small JSON files ≈ 330 KB total.

## 8. Tests

### 8.1 Unit tests

**In `format.rs`** (~6 tests):
1. `format_duration_ns_picks_unit_correctly` — table-driven cases at 500, 1500, 1.5M, 1.5B ns.
2. `format_duration_ns_handles_boundary_values` — 999 ns, 1000 ns, 999_999 ns, 1_000_000 ns.
3. `format_bytes_picks_unit_correctly` — 500, 2048, 2M, 2G.
4. `format_bytes_handles_negative` — -1024 → "-1.0 KB", 0 → "0 B", +8192 → "+8.0 KB".
5. `parse_size_to_bytes_round_trips_known_sizes` — 32B → 32, 256B → 256, 2KB → 2048, 16KB → 16384, 128KB → 131072, 1MB → 1048576.
6. `parse_size_to_bytes_unknown_label_returns_none` — `"foobar"` → `None`.

**In `discover.rs`** (~3 tests):
7. `discover_cells_joins_criterion_with_aux` — uses fixtures; asserts cell vec has expected (row, mode, size) keys with correctly populated `timing` and `aux`.
8. `discover_cells_handles_missing_aux_gracefully` — `aux_metrics_path` points to nonexistent file; cells have `aux: None`.
9. `discover_cells_handles_malformed_sample_json` — fixture has one corrupt sample.json; that cell has `timing: None`, others unaffected.

**In `render_json.rs` and `render_md.rs`** (~3 tests):
10. `render_json_schema_round_trips` — render against 2-cell fixture, parse the output, assert key set + value types match the schema.
11. `render_markdown_includes_required_sections` — render against fixture, regex-check H1, durability legend, micro grid section header, file-size delta section header, internals appendix header.
12. `render_markdown_skipped_cells_render_as_dash` — fixture with `timing: None`; assert that cell appears as `—` in markdown.

### 8.2 Integration test

**In `bench/tests/summarize_smoke.rs`** (~1 test):
13. `summarize_smoke_runs_against_fixtures` — invoke the binary via `assert_cmd::Command::cargo_bin("summarize")` against the fixtures directory + `--out <tmpdir>`. Assert exit code 0, output directory created, summary.md + results.json + raw/ exist with sane file sizes (>0 bytes, <100 KB markdown, etc.).

Total: 13 new tests, ~250 LOC of test code.

### 8.3 Test fixtures

Committed under `bench/tests/fixtures/`:

```
bench/tests/fixtures/
├── criterion/
│   ├── allocate-1pertx/
│   │   ├── chisel-strict/32B/{estimates.json, sample.json}
│   │   └── redb-strict/32B/{estimates.json, sample.json}
│   └── corrupt/                            # for the malformed-sample test
│       └── chisel-strict/32B/sample.json    # invalid JSON
└── aux_metrics.jsonl                       # 2 lines matching the 2 valid cells
```

The fixture data is hand-crafted with known values so test assertions are exact (`p50_ns == 1234.5`, etc.) rather than range-based.

## 9. Acceptance criteria

PR 5 is mergeable when:

1. `cargo build -p chisel-bench` and `cargo test -p chisel-bench` pass on macOS and Linux.
2. `cargo clippy -p chisel-bench --all-targets -- -D warnings` is clean.
3. `cargo fmt -- --check` is clean across touched files.
4. The 13 new tests in §8.1 / §8.2 pass.
5. `cargo run -p chisel-bench --bin summarize -- --help` produces clap help text matching §2.4.
6. Running the binary against PR 4b's actual bench output produces:
   - `bench/results/<timestamp>/summary.md` with all six row-group tables present
   - `bench/results/<timestamp>/results.json` with `cell_count` matching aux_metrics line count
   - `bench/results/<timestamp>/raw/<row>/<mode>/<size>/{estimates.json,sample.json}` for every cell
7. `jq '.metadata, .cells | type' results.json` reports `"object", "object"`.
8. `jq '.cells | keys | length' results.json` matches the cell count from PR 4b's bench run (165 nominal).

## 10. What PR 5 does NOT include

Deferred to PR 6 (scenarios):
- Scenario summary table in markdown.
- Scenario-tier cell entries in `aux_metrics.jsonl` parsing (defensive deserialization tolerates unknown fields).

Deferred to PR 7 (CI):
- The diff binary that consumes two `results.json` files and produces a regression report.
- GitHub Actions workflow for posting bench-diff comments on PRs.

Out of scope entirely:
- HTML output beyond Criterion's native reports.
- Variance-driven sample-size tuning recommendations.
- Multi-run history / trend analysis.
- Bootstrap-stabilized percentile estimates. We compute percentiles directly from the sample distribution; Criterion has bootstrap machinery for the mean but not for arbitrary percentiles, and rolling our own would multiply scope.

## 11. Build sequence relationship

PR 5 is the fifth PR in the bench-suite series. The series state after this PR lands becomes:

| # | PR | Status |
|---|----|--------|
| 1 | Instrumentation precursor | Landed |
| 2 | `bench/` subcrate + Engine trait + ChiselEngine | Landed |
| 3 | RedbEngine + SqliteEngine + equivalence tests | Landed |
| 4a | Workload data layer | Landed |
| 4b | Runner + 6-row Criterion micro grid | Landed |
| **5** | **Markdown summary post-processor** | **This PR** |
| 6 | Scenario tier | Pending |
| 7 | CI workflow | Pending |
| 8 | Cross-engine relative-performance tests (addendum) | Pending — own spec/plan |

CLAUDE.md gets a one-paragraph update on PR 5 merge to reflect post-processor availability.

### 11.1 Rollback

PR 5 fails review or is reverted: PR 4b's bench harness still works (produces `target/criterion/` + `aux_metrics.jsonl`); users just don't get the markdown summary or `results.json`. PR 6 and 7 cannot start without PR 5's `results.json` schema, but neither is blocked by a 5-revert — they can either re-spec independently or wait.

## 12. Open implementation-phase questions

These are deferred to the implementation plan:

- Whether the test fixtures' `sample.json` files (and the companion `estimates.json` files for the raw-archive copy test) should be hand-crafted minimal valid JSON or copied from a real Criterion run. The plan resolves; hand-crafted is cleaner for known assertions (we control the per-iteration times exactly, so the percentile assertions are exact) but less faithful to Criterion's actual output shape.
- Whether `assert_cmd` is the right integration-test framework or if `std::process::Command` suffices. The plan picks the simplest form.
- Specific clap subcommand structure (none for v1; `--out` etc. as flags).

These are implementation details that do not affect the design contract.
