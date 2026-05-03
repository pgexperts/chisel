# Bench Summary Post-Processor Implementation Plan (PR 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land a Rust binary `chisel-bench-summarize` (entry point at `bench/src/bin/summarize.rs`) plus a supporting library module under `bench/src/summary/` that reads PR 4b's `target/criterion/<row>/<mode>/<size>/sample.json` files plus `bench/results/aux_metrics.jsonl` and emits three output artifacts under `bench/results/<UTC-ISO8601>/`: `summary.md`, `results.json`, and `raw/`.

**Architecture:** Library/binary split mirrors PR 4b. Pure-function helpers (formatters, percentile interpolation, size parsing) live in `summary/format.rs`. The discovery layer (filesystem walk + JSONL parse + join) lives in `summary/discover.rs`. The two renderers (`summary/render_md.rs`, `summary/render_json.rs`) consume a `Vec<Cell>` produced by discovery. Metadata gathering lives in `summary/metadata.rs`. The binary is a thin CLI wrapper around the library functions.

**Tech Stack:** Rust 2021. New deps: `chrono = "0.4"` (timestamps), `walkdir = "2"` (filesystem walk), `clap = "4"` with `derive` feature (CLI parsing), `hostname = "0.4"` (machine info). Dev-dep: `assert_cmd = "2"` (binary integration test). serde + serde_json already in deps from PR 4b.

**Spec:** `docs/superpowers/specs/2026-05-03-chisel-bench-summary-postprocessor-design.md`

---

## Task 1: Cargo.toml additions (deps + [[bin]] target)

**Files:**
- Modify: `bench/Cargo.toml`

- [ ] **Step 1: Edit `bench/Cargo.toml`**

The current `[dependencies]` section (after PR 4b) is:
```toml
[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }
rand = "0.8"
rand_chacha = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Add `chrono`, `walkdir`, `clap`, `hostname` to `[dependencies]`. Add `assert_cmd` to `[dev-dependencies]`. Add a new `[[bin]]` target. The full updated file:

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
tempfile = "3"
chrono = "0.4"
walkdir = "2"
clap = { version = "4", features = ["derive"] }
hostname = "0.4"

[dev-dependencies]
criterion = "0.5"
assert_cmd = "2"

[[bench]]
name = "micro_grid"
harness = false

# Note: [[bin]] summarize is added in Task 10 when src/bin/summarize.rs is created.
# Declaring it here would break `cargo build` because Cargo validates target sources at parse time.
```

Note `tempfile = "3"` stays in `[dependencies]` (NOT moved to dev-dependencies). PR 4b's `runner.rs` exposes `NamedTempFile` through the public type `PopulatedSnapshot`, so it's a runtime dep, not test-only.

The `[[bin]]` declaration is deferred to Task 10 to keep this commit's `cargo build` green.

- [ ] **Step 2: Verify the bench subcrate still builds**

Run: `cd bench && cargo build`
Expected: clean build, "Finished" line. New crates `chrono`, `walkdir`, `clap`, `clap_derive`, `hostname`, `assert_cmd`, plus their transitive deps appear in `bench/Cargo.lock`.

- [ ] **Step 3: Verify existing tests still pass**

Run: `cd bench && cargo test`
Expected: 22 lib tests + 15 equivalence + 1 lib smoke + 1 runner smoke = 39 tests, all passing.

- [ ] **Step 4: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock
git commit -m "$(cat <<'EOF'
bench: add chrono/walkdir/clap/hostname deps + [[bin]] summarize

Foundation for the PR 5 markdown post-processor. New runtime deps:
chrono (UTC timestamps), walkdir (Criterion tree walk), clap with
derive (CLI parsing), hostname (machine info). New dev-dep: assert_cmd
(binary integration test). The [[bin]] target points at a file that
doesn't exist yet — task 9 creates it.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `bench/src/summary/mod.rs` scaffold + `lib.rs` wire-up

**Files:**
- Create: `bench/src/summary/mod.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Create the summary module root**

Create `bench/src/summary/mod.rs` with:

```rust
// Markdown summary post-processor module. Reads PR 4b's bench output
// (target/criterion/<row>/<mode>/<size>/sample.json + bench/results/aux_metrics.jsonl)
// and produces three artifacts under bench/results/<UTC-ISO8601>/:
//
//   summary.md     — human-readable per-row tables
//   results.json   — flat composite-key schema for PR 7's CI diff
//   raw/           — archival copy of estimates.json + sample.json per cell
//
// The five submodules split by responsibility, not by output artifact:
//
//   format        — magnitude-adaptive formatters + percentile-interp helper
//   discover      — Cell/TimingStats/AuxMetrics types + filesystem walk + JSONL parse
//   metadata      — Metadata/MachineInfo + git/hostname gathering
//   render_json   — Vec<Cell> + Metadata -> serde_json::Value
//   render_md     — Vec<Cell> + Metadata -> String

pub mod discover;
pub mod format;
pub mod metadata;
pub mod render_json;
pub mod render_md;

pub use discover::{copy_raw_archive, discover_cells, AuxMetrics, Cell, TimingStats};
pub use metadata::{gather_metadata, MachineInfo, Metadata};
pub use render_json::render_json;
pub use render_md::render_markdown;
```

- [ ] **Step 2: Wire the module into `lib.rs`**

The current `bench/src/lib.rs` (after PR 4b) ends with:
```rust
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

Add `pub mod summary;` next to the other module declarations. The file becomes:

```rust
// (existing header comment...)

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod runner;
pub mod sqlite_engine;
pub mod summary;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use runner::EngineMode;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
```

(We do NOT re-export anything from `summary` at the crate root — its types are namespaced under `bench::summary::*`. Re-exports are reserved for the bench-runner public API; the post-processor is library-but-binary-driven.)

- [ ] **Step 3: Verify it compiles**

Run: `cd bench && cargo build 2>&1 | tail -5`
Expected: compile errors complaining about missing modules `discover`, `format`, `metadata`, `render_json`, `render_md` (those files don't exist yet). The errors confirm `mod.rs` is wired into the crate. Tasks 3-8 create the submodule files in order.

If you see ONLY those errors and no others, the wire-up is correct. Proceed.

- [ ] **Step 4: Don't commit yet**

This task's mod.rs alone doesn't compile. We commit at the end of task 3 once `format.rs` exists and the build is green again. (Subsequent tasks each add one submodule and re-greenify the build.)

Continue to Task 3.

---

## Task 3: `bench/src/summary/format.rs` (formatters + percentile interp + parser + tests)

**Files:**
- Create: `bench/src/summary/format.rs`

- [ ] **Step 1: Write all 7 failing tests**

Create `bench/src/summary/format.rs` containing only the file header and test module:

```rust
// Pure-function helpers for the summary post-processor: magnitude-adaptive
// formatters for durations and byte counts, a numpy-style linear-interpolation
// percentile, and a parser for size labels like "32B" / "1MB" into byte counts.
//
// All functions are pure — easy to unit-test, no I/O, no allocation beyond
// the returned String/Option. format_duration_ns and format_bytes use 2
// decimal places at every magnitude switch so cells line up uniformly in
// table columns.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_ns_picks_unit_correctly() {
        // Sub-µs stays in ns
        assert_eq!(format_duration_ns(500.0), "500 ns");
        // µs range
        assert_eq!(format_duration_ns(1500.0), "1.50 µs");
        assert_eq!(format_duration_ns(45_678.0), "45.68 µs");
        // ms range
        assert_eq!(format_duration_ns(1_500_000.0), "1.50 ms");
        // s range
        assert_eq!(format_duration_ns(1_500_000_000.0), "1.50 s");
    }

    #[test]
    fn format_duration_ns_handles_boundary_values() {
        // Just under each boundary uses the smaller unit
        assert_eq!(format_duration_ns(999.0), "999 ns");
        assert_eq!(format_duration_ns(999_999.0), "1000.00 µs");
        // At-and-above the boundary uses the bigger unit
        assert_eq!(format_duration_ns(1000.0), "1.00 µs");
        assert_eq!(format_duration_ns(1_000_000.0), "1.00 ms");
        assert_eq!(format_duration_ns(1_000_000_000.0), "1.00 s");
    }

    #[test]
    fn format_bytes_picks_unit_correctly() {
        assert_eq!(format_bytes(500), "+500 B");
        assert_eq!(format_bytes(2048), "+2.0 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "+2.00 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "+2.00 GB");
    }

    #[test]
    fn format_bytes_handles_negative_and_zero() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(-1024), "-1.0 KB");
        assert_eq!(format_bytes(-2_097_152), "-2.00 MB");
    }

    #[test]
    fn parse_size_to_bytes_round_trips_known_sizes() {
        // The PR 4b SIZES table (master spec §3.2)
        assert_eq!(parse_size_to_bytes("32B"), Some(32));
        assert_eq!(parse_size_to_bytes("256B"), Some(256));
        assert_eq!(parse_size_to_bytes("2KB"), Some(2048));
        assert_eq!(parse_size_to_bytes("16KB"), Some(16_384));
        assert_eq!(parse_size_to_bytes("128KB"), Some(131_072));
        assert_eq!(parse_size_to_bytes("1MB"), Some(1_048_576));
    }

    #[test]
    fn parse_size_to_bytes_unknown_label_returns_none() {
        assert_eq!(parse_size_to_bytes("foobar"), None);
        assert_eq!(parse_size_to_bytes("32"), None);    // missing unit
        assert_eq!(parse_size_to_bytes(""), None);
    }

    #[test]
    fn percentile_linear_interp_known_values() {
        // sorted = [10, 20, 30, 40, 50], n=5 so idx = q * 4
        let sorted = [10.0, 20.0, 30.0, 40.0, 50.0];
        // p0 = sorted[0] = 10.0
        assert!((percentile_linear_interp(&sorted, 0.0).unwrap() - 10.0).abs() < 1e-9);
        // p50 = idx 2.0 = sorted[2] = 30.0
        assert!((percentile_linear_interp(&sorted, 0.50).unwrap() - 30.0).abs() < 1e-9);
        // p95 = idx 3.8 → 40 + 0.8 * (50-40) = 48.0
        assert!((percentile_linear_interp(&sorted, 0.95).unwrap() - 48.0).abs() < 1e-9);
        // p99 = idx 3.96 → 40 + 0.96 * (50-40) = 49.6
        assert!((percentile_linear_interp(&sorted, 0.99).unwrap() - 49.6).abs() < 1e-9);
        // p100 = sorted[4] = 50.0
        assert!((percentile_linear_interp(&sorted, 1.0).unwrap() - 50.0).abs() < 1e-9);
        // Empty input
        assert_eq!(percentile_linear_interp(&[], 0.5), None);
        // Single element
        assert_eq!(percentile_linear_interp(&[42.0], 0.99), Some(42.0));
    }
}
```

- [ ] **Step 2: Run, expect compile errors**

Run: `cd bench && cargo test summary::format::tests 2>&1 | tail -10`
Expected: compile errors — `cannot find function format_duration_ns`, `format_bytes`, `parse_size_to_bytes`, `percentile_linear_interp` in this scope.

- [ ] **Step 3: Implement the four functions**

Add this BEFORE the `#[cfg(test)] mod tests` block in `bench/src/summary/format.rs`:

```rust
/// Format a duration in nanoseconds as a human-readable string.
/// Auto-picks ns / µs / ms / s based on magnitude. Two decimal places
/// at every switch point so cells line up uniformly in tables.
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

/// Format a byte count (positive, negative, or zero) with sign prefix.
/// Auto-picks B / KB / MB / GB based on magnitude. 1024-base (matches
/// `ls -lh` and human intuition for filesystem sizes).
pub fn format_bytes(bytes: i64) -> String {
    let abs = bytes.unsigned_abs();
    let sign = if bytes < 0 {
        "-"
    } else if bytes > 0 {
        "+"
    } else {
        ""
    };
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

/// Parse a size label like "32B", "2KB", "1MB" into the corresponding
/// byte count. Returns None for unrecognized formats. Used by the
/// markdown renderer to sort size columns numerically rather than
/// lexically (which would put "1MB" before "32B").
pub fn parse_size_to_bytes(label: &str) -> Option<u64> {
    // Find the boundary between digits and unit
    let unit_start = label.find(|c: char| !c.is_ascii_digit())?;
    if unit_start == 0 {
        return None;   // no leading digits
    }
    let (num_str, unit) = label.split_at(unit_start);
    let num: u64 = num_str.parse().ok()?;
    let multiplier: u64 = match unit {
        "B" => 1,
        "KB" => 1_024,
        "MB" => 1_024 * 1_024,
        "GB" => 1_024 * 1_024 * 1_024,
        _ => return None,
    };
    Some(num * multiplier)
}

/// Numpy-style percentile via linear interpolation. `q` is in [0.0, 1.0]
/// (0.0 = min, 1.0 = max). `sorted` MUST be sorted ascending; behavior
/// is undefined otherwise. Returns None if `sorted` is empty.
///
/// At `q = 0.99` with `n = 100` samples (Criterion's default), the
/// percentile lands at index 99.0, which is exactly sorted[99] (the
/// max). At `q = 0.99` with `n = 5`, the percentile lands at index
/// 3.96, which is interpolated between sorted[3] and sorted[4].
pub fn percentile_linear_interp(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let idx_f = q * (n - 1) as f64;
    let lower = idx_f.floor() as usize;
    let upper = idx_f.ceil() as usize;
    if lower == upper {
        Some(sorted[lower])
    } else {
        let frac = idx_f - lower as f64;
        Some(sorted[lower] + frac * (sorted[upper] - sorted[lower]))
    }
}
```

- [ ] **Step 4: Run tests, expect 7 passing**

Run: `cd bench && cargo test summary::format::tests`
Expected: 7 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/summary/mod.rs bench/src/summary/format.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: add summary::format with formatters + percentile interp

Magnitude-adaptive formatters (format_duration_ns, format_bytes) plus
parse_size_to_bytes and percentile_linear_interp. All pure functions,
covered by 7 table-driven tests. Module scaffold (summary/mod.rs) and
lib.rs wire-up land here too — subsequent tasks fill in the other
submodules (discover, metadata, render_json, render_md).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

The build is green again as of this commit (mod.rs declares discover/metadata/render_json/render_md but the `pub use` in mod.rs references them — wait, no, `pub use` would error if the modules don't exist. We need to comment those out for now).

Actually, looking at mod.rs from Task 2 — it has `pub use discover::...` etc. which won't compile until those modules exist. The clean fix: in Task 2, declare the modules as `pub mod` but DON'T add the `pub use` re-exports. Re-exports get added in each task as their module lands.

Re-doing Task 2's mod.rs to only have `pub mod` declarations for tasks 3-8's modules WHEN THEY EXIST. So Task 2's actual `mod.rs` should be:

```rust
// (header comment as above...)

pub mod format;
```

Just `format` until task 3 lands. Then task 4 adds `pub mod discover;`, etc. Re-exports collected at the end (task 9 or final-cleanup).

Adjust Task 2 step 1 and Task 3 step 6 accordingly:

**REVISED Task 2 step 1 mod.rs:**
```rust
// (header comment as in original task 2)

pub mod format;
```

**REVISED Task 3 step 6 commit step:**
After committing format.rs, mod.rs has just `pub mod format;` and no re-exports. The full `pub use` list lands in task 9 (or could be added incrementally per task — see each task below).

For each subsequent task (4-8), add the `pub mod <name>;` line to mod.rs WHEN you create the file. This avoids broken-build commits. Track this in each task.

---

## Task 4: Test fixtures (synthetic Criterion tree + aux_metrics.jsonl)

**Files:**
- Create: `bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/sample.json`
- Create: `bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/estimates.json`
- Create: `bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B/sample.json`
- Create: `bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B/estimates.json`
- Create: `bench/tests/fixtures/criterion/corrupt/chisel-strict/32B/sample.json`
- Create: `bench/tests/fixtures/aux_metrics.jsonl`

These fixtures are committed as static test data — handcrafted with known values so percentile assertions are exact. Tasks 5+ rely on them being in place.

- [ ] **Step 1: Create the chisel-strict 32B sample.json fixture**

Make the directory and write the file:
```bash
mkdir -p bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B
```

Create `bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/sample.json`:

```json
{
  "iters": [10.0, 10.0, 10.0, 10.0, 10.0],
  "times": [10000.0, 20000.0, 30000.0, 40000.0, 50000.0]
}
```

The math: per_iter[i] = times[i] / iters[i] = [1000, 2000, 3000, 4000, 5000] ns. Sorted (already is). Percentiles via linear interp:
- p50 (idx 2.0) = 3000 ns
- p95 (idx 3.8) = 4000 + 0.8 * (5000 - 4000) = 4800 ns
- p99 (idx 3.96) = 4000 + 0.96 * (5000 - 4000) = 4960 ns

These exact values feed the discover tests in task 5.

- [ ] **Step 2: Create the chisel-strict 32B estimates.json fixture**

Create `bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/estimates.json`:

```json
{
  "mean": {
    "confidence_interval": {"confidence_level": 0.95, "lower_bound": 2500.0, "upper_bound": 3500.0},
    "point_estimate": 3000.0,
    "standard_error": 250.0
  },
  "median": {
    "confidence_interval": {"confidence_level": 0.95, "lower_bound": 2800.0, "upper_bound": 3200.0},
    "point_estimate": 3000.0,
    "standard_error": 100.0
  },
  "median_abs_dev": {
    "confidence_interval": {"confidence_level": 0.95, "lower_bound": 800.0, "upper_bound": 1200.0},
    "point_estimate": 1000.0,
    "standard_error": 50.0
  },
  "slope": null
}
```

Used only for the raw-archive copy test; never read by discover.

- [ ] **Step 3: Create the redb-strict 32B fixtures**

```bash
mkdir -p bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B
```

Create `bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B/sample.json`:

```json
{
  "iters": [5.0, 5.0, 5.0],
  "times": [5000.0, 10000.0, 15000.0]
}
```

Math: per_iter = [1000, 2000, 3000] ns sorted.
- p50 (idx 1.0) = 2000 ns
- p95 (idx 1.9) = 1000 + 0.9 * (3000 - 1000)... wait no. With sorted = [1000, 2000, 3000], n=3, idx_f = 0.95 * 2 = 1.9. lower=1, upper=2. sorted[1] + 0.9 * (sorted[2] - sorted[1]) = 2000 + 0.9 * 1000 = 2900 ns.
- p99 (idx 1.98) = 2000 + 0.98 * (3000 - 2000) = 2980 ns.

Create `bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B/estimates.json`:

```json
{
  "mean": {
    "confidence_interval": {"confidence_level": 0.95, "lower_bound": 1500.0, "upper_bound": 2500.0},
    "point_estimate": 2000.0,
    "standard_error": 200.0
  },
  "median": {
    "confidence_interval": {"confidence_level": 0.95, "lower_bound": 1800.0, "upper_bound": 2200.0},
    "point_estimate": 2000.0,
    "standard_error": 100.0
  },
  "median_abs_dev": {
    "confidence_interval": {"confidence_level": 0.95, "lower_bound": 800.0, "upper_bound": 1200.0},
    "point_estimate": 1000.0,
    "standard_error": 50.0
  },
  "slope": null
}
```

- [ ] **Step 4: Create the corrupt sample.json fixture for the malformed-input test**

```bash
mkdir -p bench/tests/fixtures/criterion/corrupt/chisel-strict/32B
```

Create `bench/tests/fixtures/criterion/corrupt/chisel-strict/32B/sample.json`:

```
this is not valid JSON
```

Just plain garbled text. Used by `discover_cells_handles_malformed_sample_json`.

- [ ] **Step 5: Create the aux_metrics.jsonl fixture**

Create `bench/tests/fixtures/aux_metrics.jsonl`:

```jsonl
{"row":"allocate-1pertx","mode":"chisel-strict","size":"32B","file_size_delta_bytes":8192,"counters":{"cache_hits":12,"cache_misses":3,"fsync_calls":2,"pages_allocated":4}}
{"row":"allocate-1pertx","mode":"redb-strict","size":"32B","file_size_delta_bytes":4096,"counters":null}
```

Two lines matching the two valid fixture cells (chisel-strict 32B and redb-strict 32B). Note the `counters: null` for the non-Chisel cell.

- [ ] **Step 6: Verify the fixture tree**

Run: `find bench/tests/fixtures -type f | sort`
Expected output:
```
bench/tests/fixtures/aux_metrics.jsonl
bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/estimates.json
bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/sample.json
bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B/estimates.json
bench/tests/fixtures/criterion/allocate-1pertx/redb-strict/32B/sample.json
bench/tests/fixtures/criterion/corrupt/chisel-strict/32B/sample.json
```

Run: `cat bench/tests/fixtures/aux_metrics.jsonl | wc -l`
Expected: 2.

Run: `python3 -c "import json,sys; [json.loads(l) for l in open('bench/tests/fixtures/aux_metrics.jsonl')]" && echo OK`
Expected: prints `OK` — both JSONL lines are valid JSON.

Run: `python3 -c "import json; print(json.load(open('bench/tests/fixtures/criterion/allocate-1pertx/chisel-strict/32B/sample.json')))" | head -1`
Expected: prints the parsed dict — the JSON is valid.

Run: `python3 -c "import json; json.load(open('bench/tests/fixtures/criterion/corrupt/chisel-strict/32B/sample.json'))" 2>&1 | head -3`
Expected: `json.decoder.JSONDecodeError` — confirms the corrupt fixture IS invalid JSON.

- [ ] **Step 7: Verify clippy and fmt still clean (no Rust source changes)**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 8: Commit**

```bash
git add bench/tests/fixtures/
git commit -m "$(cat <<'EOF'
bench: add summary post-processor test fixtures

Synthetic 2-cell Criterion tree (allocate-1pertx × {chisel-strict, redb-strict}
× 32B) with hand-crafted sample.json so percentile assertions are exact:
chisel-strict per_iter = [1000, 2000, 3000, 4000, 5000] ns; p50=3000,
p95=4800, p99=4960. redb-strict per_iter = [1000, 2000, 3000] ns;
p50=2000, p95=2900, p99=2980. Plus a corrupt/ subtree with invalid JSON
for the malformed-sample test, and a 2-line aux_metrics.jsonl matching
the two valid cells (counters non-null for chisel, null for redb).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `bench/src/summary/discover.rs` — types + discover_cells + 3 tests

**Files:**
- Create: `bench/src/summary/discover.rs`
- Modify: `bench/src/summary/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `bench/src/summary/discover.rs` with the file header and tests (no implementation yet):

```rust
// Filesystem discovery layer for the summary post-processor. Walks
// target/criterion/<row>/<mode>/<size>/sample.json files, parses
// aux_metrics.jsonl, joins them by (row, mode, size) key into Cell
// values. Cells with missing-on-one-side data carry None for the
// missing field — renderers handle the partial-state cases explicitly.

use crate::runner::ChiselCountersDelta;
use crate::summary::format::percentile_linear_interp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// One cell of the micro grid, joined across Criterion sample.json and
/// aux_metrics.jsonl. Cells where one source is missing carry None for
/// that field; both sources missing means the cell isn't in the Vec at all.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cell {
    pub row: String,
    pub mode: String,
    pub size: String,
    pub timing: Option<TimingStats>,
    pub aux: Option<AuxMetrics>,
}

/// Per-cell timing percentiles computed from Criterion's raw sample.json
/// per-iteration times. p50/p95/p99 share the same sample distribution
/// so they're mutually comparable for regression detection.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TimingStats {
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
}

/// Per-cell auxiliary metrics from aux_metrics.jsonl. counters is None
/// for non-Chisel engines (mirroring the JSONL schema from PR 4b).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AuxMetrics {
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn discover_cells_joins_criterion_with_aux() {
        let criterion_dir = fixtures_root().join("criterion");
        let aux_path = fixtures_root().join("aux_metrics.jsonl");
        let cells = discover_cells(&criterion_dir, &aux_path).unwrap();

        // The fixture has 2 valid cells (chisel-strict 32B, redb-strict 32B)
        // plus 1 corrupt cell (corrupt/chisel-strict/32B) which gets timing: None.
        // The corrupt cell is NOT in aux_metrics.jsonl, so it would have
        // aux: None too — but corrupt has timing: None AND aux: None and might
        // therefore be excluded. We expect only the 2 valid cells.
        // Actually the corrupt cell has neither side populated, so it's
        // omitted from the Vec entirely. Adjust if discover_cells decides
        // to include "completely missing" cells; the spec says no.
        assert!(cells.len() >= 2, "expected at least 2 cells, got {}", cells.len());

        let chisel_cell = cells.iter()
            .find(|c| c.row == "allocate-1pertx" && c.mode == "chisel-strict" && c.size == "32B")
            .expect("chisel-strict 32B cell missing");
        let timing = chisel_cell.timing.expect("chisel-strict 32B should have timing");
        assert!((timing.p50_ns - 3000.0).abs() < 1e-6);
        assert!((timing.p95_ns - 4800.0).abs() < 1e-6);
        assert!((timing.p99_ns - 4960.0).abs() < 1e-6);
        let aux = chisel_cell.aux.expect("chisel-strict 32B should have aux");
        assert_eq!(aux.file_size_delta_bytes, 8192);
        let counters = aux.counters.expect("chisel-strict 32B should have non-null counters");
        assert_eq!(counters.cache_hits, 12);
        assert_eq!(counters.cache_misses, 3);
        assert_eq!(counters.fsync_calls, 2);
        assert_eq!(counters.pages_allocated, 4);

        let redb_cell = cells.iter()
            .find(|c| c.row == "allocate-1pertx" && c.mode == "redb-strict" && c.size == "32B")
            .expect("redb-strict 32B cell missing");
        let timing = redb_cell.timing.expect("redb-strict 32B should have timing");
        assert!((timing.p50_ns - 2000.0).abs() < 1e-6);
        assert!((timing.p95_ns - 2900.0).abs() < 1e-6);
        assert!((timing.p99_ns - 2980.0).abs() < 1e-6);
        let aux = redb_cell.aux.expect("redb-strict 32B should have aux");
        assert_eq!(aux.file_size_delta_bytes, 4096);
        assert!(aux.counters.is_none(), "redb cells have null counters");
    }

    #[test]
    fn discover_cells_handles_missing_aux_gracefully() {
        let criterion_dir = fixtures_root().join("criterion");
        // Point at a nonexistent path
        let aux_path = fixtures_root().join("nonexistent_aux.jsonl");
        let cells = discover_cells(&criterion_dir, &aux_path).unwrap();

        assert!(cells.len() >= 2);
        for cell in &cells {
            // Every cell should have aux: None since aux file missing
            assert!(cell.aux.is_none(),
                "expected aux: None for {}/{}/{} when aux file missing",
                cell.row, cell.mode, cell.size);
        }
        // But timing should still be populated for valid cells
        let valid_cells: Vec<_> = cells.iter()
            .filter(|c| c.timing.is_some())
            .collect();
        assert!(valid_cells.len() >= 2, "expected at least 2 cells with timing");
    }

    #[test]
    fn discover_cells_handles_malformed_sample_json() {
        let criterion_dir = fixtures_root().join("criterion");
        let aux_path = fixtures_root().join("aux_metrics.jsonl");
        let cells = discover_cells(&criterion_dir, &aux_path).unwrap();

        // The corrupt fixture is at corrupt/chisel-strict/32B/sample.json
        // (parse fails). If discover_cells visits it, it should record
        // timing: None for that cell, not panic.
        let corrupt_cell = cells.iter()
            .find(|c| c.row == "corrupt" && c.mode == "chisel-strict" && c.size == "32B");

        // Either the cell is in the Vec with timing: None, or it's
        // omitted entirely (when both timing and aux are None). Both
        // behaviors are acceptable per the spec; assert NOT-PANICKED
        // by getting here, and assert the valid cells are unaffected.
        if let Some(cell) = corrupt_cell {
            assert!(cell.timing.is_none(), "corrupt cell should have timing: None");
        }

        // Valid cells must still be present and correct:
        let chisel_valid = cells.iter()
            .find(|c| c.row == "allocate-1pertx" && c.mode == "chisel-strict" && c.size == "32B")
            .expect("valid chisel-strict 32B should still discover cleanly");
        assert!(chisel_valid.timing.is_some());
    }
}
```

- [ ] **Step 2: Add `pub mod discover;` to `mod.rs`**

Edit `bench/src/summary/mod.rs`. After Task 3, it has `pub mod format;`. Add `pub mod discover;`:

```rust
// (header comment...)

pub mod discover;
pub mod format;
```

- [ ] **Step 3: Run, expect compile errors**

Run: `cd bench && cargo test summary::discover::tests 2>&1 | tail -10`
Expected: compile error — `cannot find function discover_cells in this scope`. The `Cell`, `TimingStats`, `AuxMetrics` types should resolve fine since we declared them; only the function is missing.

- [ ] **Step 4: Implement `discover_cells` (and parsing helpers)**

Add this code to `bench/src/summary/discover.rs`, BEFORE the `#[cfg(test)] mod tests` block:

```rust
/// Errors that can arise during discovery. The post-processor's CLI
/// layer surfaces these as user-facing error messages with exit code 1
/// when fatal, or warnings to stderr when non-fatal (per the failure-
/// handling matrix in spec §7.3).
#[derive(Debug)]
pub enum DiscoverError {
    /// criterion_dir doesn't exist or isn't readable.
    CriterionDirNotFound(std::path::PathBuf),
    /// criterion_dir exists but contains no sample.json leaves.
    NoCellsFound(std::path::PathBuf),
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CriterionDirNotFound(p) => write!(
                f,
                "Criterion output directory '{}' does not exist; run cargo bench --bench micro_grid first",
                p.display()
            ),
            Self::NoCellsFound(p) => write!(
                f,
                "no cells found under '{}'; the directory exists but contains no sample.json",
                p.display()
            ),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// JSONL line schema (matches PR 4b's CellAuxMetrics serde_json output).
#[derive(Deserialize)]
struct AuxLine {
    row: String,
    mode: String,
    size: String,
    file_size_delta_bytes: i64,
    counters: Option<ChiselCountersDelta>,
}

/// Criterion 0.5 sample.json schema. We only read these two arrays;
/// any additional fields Criterion adds in future versions are tolerated
/// via `#[serde(default)]` on the struct (omitted here since both fields
/// are required for any valid Criterion sample).
#[derive(Deserialize)]
struct SampleJson {
    iters: Vec<f64>,
    times: Vec<f64>,
}

/// Walk `criterion_dir` and parse `aux_metrics_path`, joining by
/// (row, mode, size) into a sorted Vec<Cell>. See module-level docs
/// for the partial-state semantics (Option<TimingStats> / Option<AuxMetrics>).
pub fn discover_cells(
    criterion_dir: &Path,
    aux_metrics_path: &Path,
) -> Result<Vec<Cell>, DiscoverError> {
    if !criterion_dir.exists() {
        return Err(DiscoverError::CriterionDirNotFound(criterion_dir.to_path_buf()));
    }

    // Step 1: load aux_metrics.jsonl into a hashmap. Missing file =>
    // empty map (every cell will have aux: None).
    let mut aux_map: HashMap<(String, String, String), AuxMetrics> = HashMap::new();
    if aux_metrics_path.exists() {
        match std::fs::read_to_string(aux_metrics_path) {
            Ok(contents) => {
                for (lineno, line) in contents.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<AuxLine>(line) {
                        Ok(entry) => {
                            aux_map.insert(
                                (entry.row, entry.mode, entry.size),
                                AuxMetrics {
                                    file_size_delta_bytes: entry.file_size_delta_bytes,
                                    counters: entry.counters,
                                },
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "warning: skipping malformed aux line {}: {}",
                                lineno + 1,
                                e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "warning: aux-metrics file '{}' could not be read ({}); cells will have no file-size or counter data",
                    aux_metrics_path.display(),
                    e
                );
            }
        }
    } else {
        eprintln!(
            "warning: aux-metrics file '{}' missing; cells will have no file-size or counter data",
            aux_metrics_path.display()
        );
    }

    // Step 2: walk criterion_dir to find sample.json leaves.
    // Each leaf is at depth 3: <row>/<mode>/<size>/sample.json.
    let mut cells: Vec<Cell> = Vec::new();
    let mut sample_paths_seen = 0usize;

    for entry in walkdir::WalkDir::new(criterion_dir)
        .min_depth(4)   // criterion_dir + row + mode + size = depth 4 for the file itself
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_name() != "sample.json" {
            continue;
        }
        sample_paths_seen += 1;

        // Walk the path back: file -> size dir -> mode dir -> row dir.
        let size_dir = entry.path().parent().unwrap();
        let mode_dir = size_dir.parent().unwrap();
        let row_dir = mode_dir.parent().unwrap();

        let size = size_dir.file_name().unwrap().to_string_lossy().into_owned();
        let mode = mode_dir.file_name().unwrap().to_string_lossy().into_owned();
        let row = row_dir.file_name().unwrap().to_string_lossy().into_owned();

        // Parse sample.json; if it fails, log and emit timing: None.
        let timing = match parse_sample_json(entry.path()) {
            Ok(Some(t)) => Some(t),
            Ok(None) => None,    // empty sample (no times)
            Err(e) => {
                eprintln!(
                    "warning: skipping malformed sample.json at {}: {}",
                    entry.path().display(),
                    e
                );
                None
            }
        };

        let aux = aux_map.remove(&(row.clone(), mode.clone(), size.clone()));

        // Only emit the cell if at least one side has data.
        if timing.is_some() || aux.is_some() {
            cells.push(Cell { row, mode, size, timing, aux });
        }
    }

    // Step 3: any aux entries we didn't pair up with a Criterion cell
    // (rare but possible) get emitted with timing: None.
    for ((row, mode, size), aux) in aux_map.into_iter() {
        cells.push(Cell {
            row,
            mode,
            size,
            timing: None,
            aux: Some(aux),
        });
    }

    if sample_paths_seen == 0 && cells.is_empty() {
        return Err(DiscoverError::NoCellsFound(criterion_dir.to_path_buf()));
    }

    // Step 4: sort by (row, mode, size) for deterministic output.
    cells.sort_by(|a, b| {
        a.row.cmp(&b.row)
            .then_with(|| a.mode.cmp(&b.mode))
            .then_with(|| a.size.cmp(&b.size))
    });

    Ok(cells)
}

fn parse_sample_json(path: &Path) -> Result<Option<TimingStats>, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(path)?;
    let parsed: SampleJson = serde_json::from_str(&contents)?;
    if parsed.iters.len() != parsed.times.len() || parsed.iters.is_empty() {
        return Ok(None);
    }
    let mut per_iter: Vec<f64> = parsed
        .times
        .iter()
        .zip(&parsed.iters)
        .map(|(t, i)| t / i)
        .collect();
    per_iter.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(Some(TimingStats {
        p50_ns: percentile_linear_interp(&per_iter, 0.50).unwrap(),
        p95_ns: percentile_linear_interp(&per_iter, 0.95).unwrap(),
        p99_ns: percentile_linear_interp(&per_iter, 0.99).unwrap(),
    }))
}
```

- [ ] **Step 5: Run tests, expect 3 passing**

Run: `cd bench && cargo test summary::discover::tests`
Expected: 3 passed.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/summary/discover.rs bench/src/summary/mod.rs
git commit -m "$(cat <<'EOF'
bench: add summary::discover with Cell types + filesystem walk

discover_cells walks target/criterion/<row>/<mode>/<size>/sample.json,
parses aux_metrics.jsonl, joins by (row, mode, size) key. Percentiles
computed from raw per-iteration times via numpy-style linear
interpolation (uses summary::format::percentile_linear_interp). Three
tests against committed fixtures cover the happy path, missing aux,
and malformed sample.json cases.

DiscoverError surfaces fatal failures (criterion dir missing, no cells
found); non-fatal cases (missing aux, malformed line) log warnings to
stderr and continue with Option<...> Nones.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `discover.rs` — `copy_raw_archive` helper

**Files:**
- Modify: `bench/src/summary/discover.rs`

The raw-archive copy is part of `discover.rs` because it's also a filesystem-traversal operation. No new test (covered indirectly by the integration smoke test in task 11).

- [ ] **Step 1: Add `copy_raw_archive` to `discover.rs`**

Append this function to `bench/src/summary/discover.rs`, BEFORE the `#[cfg(test)] mod tests` block:

```rust
/// Copy `estimates.json` and `sample.json` files from `criterion_dir`
/// into `raw_out_dir`, preserving the directory structure. Skips
/// Criterion's HTML reports, plot images, change/ subdirectories, and
/// other supporting files — those stay in target/criterion/ for live
/// browsing; the archive's job is reproducibility (so the markdown
/// numbers can be regenerated from the archive if target/ is wiped).
///
/// 165 cells × 2 small JSON files ≈ 330 KB total archive size.
pub fn copy_raw_archive(criterion_dir: &Path, raw_out_dir: &Path) -> std::io::Result<()> {
    for entry in walkdir::WalkDir::new(criterion_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name();
        if name != "estimates.json" && name != "sample.json" {
            continue;
        }
        let rel = entry.path().strip_prefix(criterion_dir).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        let dest = raw_out_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(entry.path(), &dest)?;
    }
    Ok(())
}
```

- [ ] **Step 2: Verify build**

Run: `cd bench && cargo build && cargo test summary::discover::tests`
Expected: clean build, 3 tests still passing.

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 4: Commit**

```bash
git add bench/src/summary/discover.rs
git commit -m "$(cat <<'EOF'
bench: add copy_raw_archive for the raw/ archive output

Copies estimates.json + sample.json files from target/criterion/ into
the output directory's raw/ subdirectory, preserving structure. Skips
Criterion's HTML reports + plots — the archive's purpose is data
reproducibility, not browsable diagnostics. Tested indirectly through
the integration smoke test (task 11).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `bench/src/summary/metadata.rs` (Metadata + gather_metadata)

**Files:**
- Create: `bench/src/summary/metadata.rs`
- Modify: `bench/src/summary/mod.rs`

- [ ] **Step 1: Create `metadata.rs`**

```rust
// Metadata gathering for the summary post-processor: timestamp,
// chisel commit (best-effort via `git rev-parse HEAD`), machine
// info (os/arch/hostname). Serialized into the metadata block of
// the JSON output and into the markdown header.
//
// All fields fall back to safe defaults on failure rather than
// aborting — the post-processor still produces a useful summary
// even when run in environments without git or hostname access
// (tarballed CI, containers without /etc/hostname, etc.).

use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Serialize)]
pub struct Metadata {
    pub timestamp: String,
    pub chisel_commit: String,
    pub machine: MachineInfo,
    pub post_processor_version: &'static str,
    pub criterion_dir: String,
    pub aux_metrics_path: String,
    pub cell_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineInfo {
    pub os: String,
    pub arch: String,
    pub hostname: String,
}

/// Build a Metadata struct from the current environment. Best-effort
/// for git commit (returns "unknown" if git is unavailable) and
/// hostname (returns "unknown" if the syscall fails).
pub fn gather_metadata(
    criterion_dir: &Path,
    aux_metrics_path: &Path,
    cell_count: usize,
) -> Result<Metadata, Box<dyn std::error::Error>> {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let chisel_commit = git_rev_parse_head().unwrap_or_else(|| "unknown".to_string());

    let machine = MachineInfo {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string()),
    };

    Ok(Metadata {
        timestamp,
        chisel_commit,
        machine,
        post_processor_version: env!("CARGO_PKG_VERSION"),
        criterion_dir: criterion_dir.display().to_string(),
        aux_metrics_path: aux_metrics_path.display().to_string(),
        cell_count,
    })
}

/// Run `git rev-parse HEAD` from the chisel repo root (CARGO_MANIFEST_DIR
/// is bench/, so the repo root is one level up). Returns None on any
/// failure — git missing, not a git repo, command non-zero exit. The
/// post-processor surfaces this as "unknown" rather than aborting.
fn git_rev_parse_head() -> Option<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let repo_root = Path::new(manifest_dir).parent()?;

    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let sha = String::from_utf8(output.stdout).ok()?;
    Some(sha.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gather_metadata_populates_all_fields() {
        let metadata = gather_metadata(
            Path::new("target/criterion"),
            Path::new("bench/results/aux_metrics.jsonl"),
            42,
        )
        .unwrap();

        assert!(!metadata.timestamp.is_empty());
        assert!(metadata.timestamp.ends_with('Z'));
        assert!(!metadata.chisel_commit.is_empty()); // either a sha or "unknown"
        assert!(!metadata.machine.os.is_empty());
        assert!(!metadata.machine.arch.is_empty());
        assert!(!metadata.machine.hostname.is_empty());
        assert_eq!(metadata.criterion_dir, "target/criterion");
        assert_eq!(metadata.aux_metrics_path, "bench/results/aux_metrics.jsonl");
        assert_eq!(metadata.cell_count, 42);
        assert!(!metadata.post_processor_version.is_empty());
    }
}
```

- [ ] **Step 2: Add `pub mod metadata;` to `mod.rs`**

```rust
// (header comment...)

pub mod discover;
pub mod format;
pub mod metadata;
```

- [ ] **Step 3: Run tests, expect pass**

Run: `cd bench && cargo test summary::metadata::tests`
Expected: 1 passed.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/summary/metadata.rs bench/src/summary/mod.rs
git commit -m "$(cat <<'EOF'
bench: add summary::metadata for timestamp + commit + machine info

gather_metadata builds a Metadata struct: UTC ISO 8601 timestamp,
chisel commit via best-effort git rev-parse HEAD (returns "unknown"
on failure), machine os/arch via std::env::consts, hostname via the
hostname crate. All failures degrade gracefully so the post-processor
still produces useful output in tarballed/CI environments.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `bench/src/summary/render_json.rs` (JSON renderer + 1 test)

**Files:**
- Create: `bench/src/summary/render_json.rs`
- Modify: `bench/src/summary/mod.rs`

- [ ] **Step 1: Create `render_json.rs` with the failing test**

```rust
// JSON renderer for the summary post-processor. Produces a flat
// composite-key schema (`<row>/<mode>/<size>` keys) with explicit
// nulls for missing-on-one-side data — keeps the schema rectangular
// for PR 7's CI diff to consume without conditional-existence checks.

use crate::summary::discover::Cell;
use crate::summary::metadata::Metadata;
use serde_json::{json, Map, Value};

/// Render a Vec<Cell> + Metadata into the results.json document.
/// Output schema:
///
///   {
///     "metadata": { ... metadata fields ... },
///     "cells": {
///       "<row>/<mode>/<size>": {
///         "p50_ns": ..., "p95_ns": ..., "p99_ns": ...,
///         "file_size_delta_bytes": ..., "counters": ...
///       },
///       ...
///     }
///   }
///
/// Missing data is explicit `null`, not omitted — keeps the schema
/// rectangular for diff tooling.
pub fn render_json(cells: &[Cell], metadata: &Metadata) -> Value {
    let mut cells_map = Map::new();
    for cell in cells {
        let key = format!("{}/{}/{}", cell.row, cell.mode, cell.size);
        cells_map.insert(key, render_cell_json(cell));
    }
    json!({
        "metadata": metadata,
        "cells": cells_map,
    })
}

fn render_cell_json(cell: &Cell) -> Value {
    json!({
        "p50_ns": cell.timing.map(|t| t.p50_ns),
        "p95_ns": cell.timing.map(|t| t.p95_ns),
        "p99_ns": cell.timing.map(|t| t.p99_ns),
        "file_size_delta_bytes": cell.aux.map(|a| a.file_size_delta_bytes),
        "counters": cell.aux.and_then(|a| a.counters),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ChiselCountersDelta;
    use crate::summary::discover::{AuxMetrics, TimingStats};
    use crate::summary::metadata::MachineInfo;

    fn fixture_metadata() -> Metadata {
        Metadata {
            timestamp: "2026-05-03T13:22:15Z".to_string(),
            chisel_commit: "abc123".to_string(),
            machine: MachineInfo {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                hostname: "test-host".to_string(),
            },
            post_processor_version: "0.1.0",
            criterion_dir: "target/criterion".to_string(),
            aux_metrics_path: "bench/results/aux_metrics.jsonl".to_string(),
            cell_count: 2,
        }
    }

    #[test]
    fn render_json_schema_round_trips() {
        let cells = vec![
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "chisel-strict".to_string(),
                size: "32B".to_string(),
                timing: Some(TimingStats { p50_ns: 1234.5, p95_ns: 1567.8, p99_ns: 1890.2 }),
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 8192,
                    counters: Some(ChiselCountersDelta {
                        cache_hits: 12, cache_misses: 3, fsync_calls: 2, pages_allocated: 4,
                    }),
                }),
            },
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "redb-strict".to_string(),
                size: "32B".to_string(),
                timing: None,
                aux: Some(AuxMetrics { file_size_delta_bytes: 4096, counters: None }),
            },
        ];

        let value = render_json(&cells, &fixture_metadata());

        // Top-level keys
        assert_eq!(value["metadata"]["cell_count"], 2);
        assert!(value["cells"].is_object());

        // First cell
        let c1 = &value["cells"]["allocate-1pertx/chisel-strict/32B"];
        assert_eq!(c1["p50_ns"], 1234.5);
        assert_eq!(c1["p95_ns"], 1567.8);
        assert_eq!(c1["p99_ns"], 1890.2);
        assert_eq!(c1["file_size_delta_bytes"], 8192);
        assert_eq!(c1["counters"]["cache_hits"], 12);

        // Second cell — timing: None should serialize as null
        let c2 = &value["cells"]["allocate-1pertx/redb-strict/32B"];
        assert!(c2["p50_ns"].is_null());
        assert!(c2["p95_ns"].is_null());
        assert!(c2["p99_ns"].is_null());
        assert_eq!(c2["file_size_delta_bytes"], 4096);
        assert!(c2["counters"].is_null());

        // Round-trip via to_string + parse
        let s = serde_json::to_string(&value).unwrap();
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["cells"]["allocate-1pertx/chisel-strict/32B"]["p50_ns"], 1234.5);
    }
}
```

- [ ] **Step 2: Add `pub mod render_json;` to `mod.rs`**

```rust
// (header...)

pub mod discover;
pub mod format;
pub mod metadata;
pub mod render_json;
```

- [ ] **Step 3: Run tests, expect pass**

Run: `cd bench && cargo test summary::render_json::tests`
Expected: 1 passed.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/summary/render_json.rs bench/src/summary/mod.rs
git commit -m "$(cat <<'EOF'
bench: add summary::render_json for results.json output

Flat composite-key schema (`<row>/<mode>/<size>` keys) with explicit
nulls for missing-on-one-side data. Test asserts schema shape, value
types, and that None fields serialize as JSON null. Round-trips through
serde_json.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `bench/src/summary/render_md.rs` (markdown renderer + 2 tests)

**Files:**
- Create: `bench/src/summary/render_md.rs`
- Modify: `bench/src/summary/mod.rs`

- [ ] **Step 1: Create `render_md.rs` with failing tests**

```rust
// Markdown renderer for the summary post-processor. Produces a single
// String from a Vec<Cell> + Metadata. Document structure follows
// master spec §7.1 + spec §4.1 of this PR.

use crate::summary::discover::Cell;
use crate::summary::format::{format_bytes, format_duration_ns, parse_size_to_bytes};
use crate::summary::metadata::Metadata;
use std::collections::BTreeSet;
use std::fmt::Write;

const MODE_ORDER: &[&str] = &[
    "chisel-strict",
    "redb-strict",
    "redb-unsafe",
    "sqlite-strict",
    "sqlite-unsafe",
];

/// Render a Vec<Cell> + Metadata into a complete markdown summary
/// (header + per-row tables + file-size delta table + Chisel internals
/// appendix + footer).
pub fn render_markdown(cells: &[Cell], metadata: &Metadata) -> String {
    let mut out = String::new();
    render_header(&mut out, metadata);
    render_durability_legend(&mut out);
    render_disclaimer(&mut out);
    render_micro_grid(&mut out, cells);
    render_file_size_table(&mut out, cells);
    render_chisel_internals_appendix(&mut out, cells);
    render_footer(&mut out, metadata);
    out
}

fn render_header(out: &mut String, m: &Metadata) {
    let _ = writeln!(out, "# Chisel Benchmark Summary\n");
    let _ = writeln!(out, "**Generated:** {}", m.timestamp);
    let _ = writeln!(out, "**Chisel commit:** {}", m.chisel_commit);
    let _ = writeln!(
        out,
        "**Machine:** {} {} — hostname: {}",
        m.machine.os, m.machine.arch, m.machine.hostname
    );
    let _ = writeln!(out, "**Cells:** {}", m.cell_count);
    let _ = writeln!(out);
}

fn render_durability_legend(out: &mut String) {
    let _ = writeln!(out, "## Durability mode legend\n");
    let _ = writeln!(out, "- `chisel-strict` — Chisel native (always fsync; no unsafe mode by design)");
    let _ = writeln!(out, "- `redb-strict` — redb with `Durability::Immediate`");
    let _ = writeln!(out, "- `redb-unsafe` — redb with `Durability::Eventual` (diagnostic only — not durable)");
    let _ = writeln!(out, "- `sqlite-strict` — SQLite with `synchronous=FULL` and WAL journaling");
    let _ = writeln!(out, "- `sqlite-unsafe` — SQLite with `synchronous=OFF` (diagnostic only — not durable)");
    let _ = writeln!(out);
}

fn render_disclaimer(out: &mut String) {
    let _ = writeln!(out, "## Method\n");
    let _ = writeln!(out, "Wall-clock cells show `p50 (p99)` in magnitude-adaptive units. Percentiles are computed directly from Criterion's raw `sample.json` per-iteration times via numpy-style linear interpolation; all three percentiles share the same sample distribution. With Criterion's default ~100 samples per cell, p99 has appreciable statistical uncertainty — for tight tail bounds, consult Criterion's per-cell HTML report under `target/criterion/<row>/<mode>/<size>/report/`.");
    let _ = writeln!(out);
}

fn render_micro_grid(out: &mut String, cells: &[Cell]) {
    let _ = writeln!(out, "## Micro grid\n");

    // Collect distinct (row, sizes_in_row) for ordering. Group cells by row.
    let rows: Vec<String> = {
        let mut seen = BTreeSet::new();
        for c in cells { seen.insert(c.row.clone()); }
        seen.into_iter().collect()
    };

    for row in &rows {
        let row_cells: Vec<&Cell> = cells.iter().filter(|c| &c.row == row).collect();
        if row_cells.is_empty() { continue; }

        // Determine size column ordering (numeric, ascending).
        let mut sizes: Vec<String> = {
            let mut seen = BTreeSet::new();
            for c in &row_cells { seen.insert(c.size.clone()); }
            seen.into_iter().collect()
        };
        sizes.sort_by_key(|s| parse_size_to_bytes(s).unwrap_or(u64::MAX));

        let _ = writeln!(out, "### `{}`\n", row);
        // Header row
        let _ = write!(out, "| mode |");
        for size in &sizes {
            let _ = write!(out, " {} |", size);
        }
        let _ = writeln!(out);
        // Separator row
        let _ = write!(out, "|------|");
        for _ in &sizes {
            let _ = write!(out, "-----|");
        }
        let _ = writeln!(out);
        // Data rows (one per mode in canonical order)
        for mode in MODE_ORDER {
            let _ = write!(out, "| {} |", mode);
            for size in &sizes {
                let cell_opt = row_cells.iter()
                    .find(|c| c.mode == *mode && &c.size == size);
                let cell_str = match cell_opt.and_then(|c| c.timing.as_ref()) {
                    Some(t) => format!("{} ({})", format_duration_ns(t.p50_ns), format_duration_ns(t.p99_ns)),
                    None => "—".to_string(),
                };
                let _ = write!(out, " {} |", cell_str);
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out);
    }
}

fn render_file_size_table(out: &mut String, cells: &[Cell]) {
    let _ = writeln!(out, "## File-size delta\n");
    let _ = writeln!(out, "Captured per-cell from one calibration iteration (post-Criterion measurement). Negative values indicate the operation shrunk the file. Magnitude-adaptive units (1024-base).\n");

    // Collect distinct sizes from all cells (numeric ascending)
    let mut sizes: Vec<String> = {
        let mut seen = BTreeSet::new();
        for c in cells { seen.insert(c.size.clone()); }
        seen.into_iter().collect()
    };
    sizes.sort_by_key(|s| parse_size_to_bytes(s).unwrap_or(u64::MAX));

    // Header
    let _ = write!(out, "| row | mode |");
    for size in &sizes {
        let _ = write!(out, " {} |", size);
    }
    let _ = writeln!(out);
    let _ = write!(out, "|-----|------|");
    for _ in &sizes {
        let _ = write!(out, "-----|");
    }
    let _ = writeln!(out);

    // Group by (row, mode), in canonical order (row alphabetical, mode by MODE_ORDER)
    let mut row_set: BTreeSet<String> = BTreeSet::new();
    for c in cells { row_set.insert(c.row.clone()); }

    for row in &row_set {
        for mode in MODE_ORDER {
            // Only emit row if any cell exists for this (row, mode)
            let has_any = cells.iter().any(|c| &c.row == row && c.mode == *mode);
            if !has_any { continue; }
            let _ = write!(out, "| {} | {} |", row, mode);
            for size in &sizes {
                let cell_opt = cells.iter().find(|c| &c.row == row && c.mode == *mode && &c.size == size);
                let s = match cell_opt.and_then(|c| c.aux.as_ref()) {
                    Some(a) => format_bytes(a.file_size_delta_bytes),
                    None => "—".to_string(),
                };
                let _ = write!(out, " {} |", s);
            }
            let _ = writeln!(out);
        }
    }
    let _ = writeln!(out);
}

fn render_chisel_internals_appendix(out: &mut String, cells: &[Cell]) {
    let _ = writeln!(out, "## Chisel internals appendix\n");
    let _ = writeln!(out, "Counter deltas for cells where `engine_mode = chisel-strict`. One row per cell (row × size); columns are the four counters from `Chisel::counters()`.\n");
    let _ = writeln!(out, "| row | size | cache_hits | cache_misses | fsync_calls | pages_allocated |");
    let _ = writeln!(out, "|-----|------|------------|--------------|-------------|-----------------|");

    let mut chisel_cells: Vec<&Cell> = cells.iter()
        .filter(|c| c.mode == "chisel-strict")
        .collect();
    chisel_cells.sort_by(|a, b| {
        a.row.cmp(&b.row)
            .then_with(|| {
                let asize = parse_size_to_bytes(&a.size).unwrap_or(u64::MAX);
                let bsize = parse_size_to_bytes(&b.size).unwrap_or(u64::MAX);
                asize.cmp(&bsize)
            })
    });

    for cell in chisel_cells {
        let counters = cell.aux.and_then(|a| a.counters);
        let (h, m, f, p) = match counters {
            Some(c) => (c.cache_hits.to_string(), c.cache_misses.to_string(), c.fsync_calls.to_string(), c.pages_allocated.to_string()),
            None => ("—".to_string(), "—".to_string(), "—".to_string(), "—".to_string()),
        };
        let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} |", cell.row, cell.size, h, m, f, p);
    }
    let _ = writeln!(out);
}

fn render_footer(out: &mut String, m: &Metadata) {
    let _ = writeln!(out, "---\n");
    let _ = writeln!(out, "*Generated by `chisel-bench-summarize` {} against Chisel commit `{}`.*", m.post_processor_version, m.chisel_commit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ChiselCountersDelta;
    use crate::summary::discover::{AuxMetrics, TimingStats};
    use crate::summary::metadata::MachineInfo;

    fn fixture_metadata() -> Metadata {
        Metadata {
            timestamp: "2026-05-03T13:22:15Z".to_string(),
            chisel_commit: "abc123".to_string(),
            machine: MachineInfo { os: "macos".to_string(), arch: "aarch64".to_string(), hostname: "test".to_string() },
            post_processor_version: "0.1.0",
            criterion_dir: "x".to_string(),
            aux_metrics_path: "y".to_string(),
            cell_count: 2,
        }
    }

    fn fixture_cells() -> Vec<Cell> {
        vec![
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "chisel-strict".to_string(),
                size: "32B".to_string(),
                timing: Some(TimingStats { p50_ns: 3000.0, p95_ns: 4800.0, p99_ns: 4960.0 }),
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 8192,
                    counters: Some(ChiselCountersDelta {
                        cache_hits: 12, cache_misses: 3, fsync_calls: 2, pages_allocated: 4,
                    }),
                }),
            },
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "redb-strict".to_string(),
                size: "32B".to_string(),
                timing: None,
                aux: Some(AuxMetrics { file_size_delta_bytes: 4096, counters: None }),
            },
        ]
    }

    #[test]
    fn render_markdown_includes_required_sections() {
        let md = render_markdown(&fixture_cells(), &fixture_metadata());
        assert!(md.contains("# Chisel Benchmark Summary"), "missing H1");
        assert!(md.contains("## Durability mode legend"), "missing legend");
        assert!(md.contains("## Method"), "missing method/disclaimer section");
        assert!(md.contains("## Micro grid"), "missing micro grid header");
        assert!(md.contains("### `allocate-1pertx`"), "missing allocate-1pertx subsection");
        assert!(md.contains("## File-size delta"), "missing file-size delta header");
        assert!(md.contains("## Chisel internals appendix"), "missing appendix header");
        assert!(md.contains("chisel-strict"), "missing chisel-strict mode");
        assert!(md.contains("redb-strict"), "missing redb-strict mode");
        assert!(md.contains("3000 ns"), "missing chisel p50 cell value");
        assert!(md.contains("+8.0 KB") || md.contains("+8192 B"), "missing chisel file-size delta");
        assert!(md.contains("abc123"), "missing chisel commit reference");
    }

    #[test]
    fn render_markdown_skipped_cells_render_as_dash() {
        let md = render_markdown(&fixture_cells(), &fixture_metadata());
        // The redb-strict cell has timing: None → must show as —
        // (The em-dash is the U+2014 character).
        assert!(md.contains("—"), "missing em-dash for skipped cell");
        // Specifically, the redb-strict row should have an em-dash
        // somewhere in its row — find the row by line:
        let redb_line = md.lines()
            .find(|l| l.starts_with("| redb-strict |"))
            .expect("redb-strict line should exist in micro grid");
        assert!(redb_line.contains("—"), "redb-strict row should contain em-dash for missing timing");
    }
}
```

- [ ] **Step 2: Add `pub mod render_md;` to `mod.rs`**

```rust
// (header...)

pub mod discover;
pub mod format;
pub mod metadata;
pub mod render_json;
pub mod render_md;
```

- [ ] **Step 3: Run tests, expect 2 passing**

Run: `cd bench && cargo test summary::render_md::tests`
Expected: 2 passed.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/summary/render_md.rs bench/src/summary/mod.rs
git commit -m "$(cat <<'EOF'
bench: add summary::render_md for summary.md output

Renders header + durability legend + method disclaimer + micro grid
(one H3 subsection per row, modes as table rows, sizes numerically
sorted as columns) + file-size delta table + Chisel internals appendix
+ footer. Two tests: required sections present, em-dash for cells
with missing timing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `bench/src/bin/summarize.rs` (CLI binary) + add [[bin]] target + finalize re-exports

**Files:**
- Create: `bench/src/bin/summarize.rs`
- Modify: `bench/Cargo.toml`
- Modify: `bench/src/summary/mod.rs`

- [ ] **Step 0: Add the [[bin]] declaration to Cargo.toml**

Task 1 deferred this declaration because the source file didn't exist yet. Now we add it. After the existing `[[bench]]` block in `bench/Cargo.toml` (and AFTER the comment placeholder Task 1 left there), add:

```toml
[[bin]]
name = "summarize"
path = "src/bin/summarize.rs"
```

Replace the comment placeholder Task 1 left:
```toml
# Note: [[bin]] summarize is added in Task 10 when src/bin/summarize.rs is created.
# Declaring it here would break `cargo build` because Cargo validates target sources at parse time.
```

— with the actual `[[bin]]` block above. (You'll create the source file in Step 2; the order between Step 0 and Step 2 doesn't matter for build correctness as long as both are present before any `cargo build` runs.)

- [ ] **Step 1: Add re-exports to `mod.rs`**

After task 9, `mod.rs` has the five `pub mod` lines. Now add the `pub use` re-exports for the public API:

```rust
// (header comment...)

pub mod discover;
pub mod format;
pub mod metadata;
pub mod render_json;
pub mod render_md;

pub use discover::{copy_raw_archive, discover_cells, AuxMetrics, Cell, TimingStats};
pub use metadata::{gather_metadata, MachineInfo, Metadata};
pub use render_json::render_json;
pub use render_md::render_markdown;
```

- [ ] **Step 2: Create `bench/src/bin/summarize.rs`**

```rust
// CLI entry point for the chisel-bench-summarize post-processor.
// Reads PR 4b's bench output (Criterion sample.json + aux_metrics.jsonl)
// and emits summary.md + results.json + raw/ under bench/results/<UTC>/.
//
// All logic lives in the chisel_bench::summary library module; this
// file is just argv parsing, error printing, and exit codes.

use chisel_bench::summary::{
    copy_raw_archive, discover_cells, gather_metadata, render_json, render_markdown,
};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chisel-bench-summarize", version)]
#[command(about = "Post-process Criterion + aux-metrics output into summary.md + results.json")]
struct Cli {
    /// Output directory (default: bench/results/<UTC-ISO8601>/)
    #[arg(long)]
    out: Option<PathBuf>,

    /// Criterion output directory.
    #[arg(long, default_value = "target/criterion")]
    criterion: PathBuf,

    /// Aux-metrics JSONL produced by the bench harness.
    #[arg(long, default_value = "bench/results/aux_metrics.jsonl")]
    aux: PathBuf,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Discover cells.
    let cells = discover_cells(&cli.criterion, &cli.aux)?;
    if cells.is_empty() {
        return Err("no cells discovered — did you run cargo bench --bench micro_grid?".into());
    }

    // 2. Resolve output directory.
    let out_dir = cli.out.unwrap_or_else(|| {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
        PathBuf::from(format!("bench/results/{ts}"))
    });
    std::fs::create_dir_all(&out_dir)?;

    // 3. Gather metadata.
    let metadata = gather_metadata(&cli.criterion, &cli.aux, cells.len())?;

    // 4. Render markdown + JSON.
    let md = render_markdown(&cells, &metadata);
    let json = render_json(&cells, &metadata);

    // 5. Write output artifacts.
    std::fs::write(out_dir.join("summary.md"), &md)?;
    std::fs::write(
        out_dir.join("results.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    copy_raw_archive(&cli.criterion, &out_dir.join("raw"))?;

    // 6. Tell user where to find it.
    println!("Wrote {} cells to {}", cells.len(), out_dir.display());
    println!("  - summary.md  ({} bytes)", std::fs::metadata(out_dir.join("summary.md"))?.len());
    println!("  - results.json ({} bytes)", std::fs::metadata(out_dir.join("results.json"))?.len());
    println!("  - raw/ (Criterion estimates.json + sample.json archive)");

    Ok(())
}
```

- [ ] **Step 3: Verify the binary compiles + `--help` works**

Run: `cd bench && cargo build --bin summarize 2>&1 | tail -5`
Expected: clean compile.

Run: `cd bench && cargo run --bin summarize -- --help 2>&1 | tail -20`
Expected: clap-formatted help text including the three flags (`--out`, `--criterion`, `--aux`) and `--help`/`--version`.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/bin/summarize.rs bench/src/summary/mod.rs
git commit -m "$(cat <<'EOF'
bench: add summarize CLI binary + finalize summary module re-exports

Thin clap-driven CLI: parse --out/--criterion/--aux, run the discover
→ gather_metadata → render_{md,json} → copy_raw_archive pipeline, write
the three artifacts, print a brief summary line. Default output goes
to bench/results/<UTC-ISO8601>/. mod.rs gets the public re-exports
for the binary and any future programmatic consumers.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Integration smoke test in `bench/tests/summarize_smoke.rs`

**Files:**
- Create: `bench/tests/summarize_smoke.rs`

- [ ] **Step 1: Create the integration smoke test**

```rust
// Integration smoke test: invoke the summarize binary against the
// committed fixtures and verify the three output artifacts are produced
// with sensible sizes + structure. Catches end-to-end wiring bugs that
// pass unit tests but break the binary.

use assert_cmd::Command;
use std::path::PathBuf;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn summarize_smoke_runs_against_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("out");

    let mut cmd = Command::cargo_bin("summarize").unwrap();
    cmd.arg("--out").arg(&out_dir);
    cmd.arg("--criterion").arg(fixtures_root().join("criterion"));
    cmd.arg("--aux").arg(fixtures_root().join("aux_metrics.jsonl"));

    let output = cmd.output().expect("failed to run binary");
    assert!(
        output.status.success(),
        "summarize exited non-zero. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the three artifacts.
    let md_path = out_dir.join("summary.md");
    let json_path = out_dir.join("results.json");
    let raw_dir = out_dir.join("raw");
    assert!(md_path.exists(), "summary.md missing");
    assert!(json_path.exists(), "results.json missing");
    assert!(raw_dir.is_dir(), "raw/ directory missing");

    // Sanity-check sizes.
    let md_size = std::fs::metadata(&md_path).unwrap().len();
    assert!(md_size > 200, "summary.md too small ({} bytes)", md_size);
    assert!(md_size < 100_000, "summary.md unexpectedly large ({} bytes)", md_size);

    let json_size = std::fs::metadata(&json_path).unwrap().len();
    assert!(json_size > 100, "results.json too small ({} bytes)", json_size);

    // Verify results.json parses + has expected structure.
    let json_content = std::fs::read_to_string(&json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
    assert!(parsed["metadata"].is_object());
    assert!(parsed["cells"].is_object());
    let cells_obj = parsed["cells"].as_object().unwrap();
    assert!(cells_obj.len() >= 2, "expected at least 2 cells in fixture output, got {}", cells_obj.len());

    // Verify the raw/ archive copied at least the chisel-strict 32B sample + estimates.
    let chisel_raw = raw_dir
        .join("allocate-1pertx")
        .join("chisel-strict")
        .join("32B");
    assert!(chisel_raw.join("sample.json").exists(), "raw chisel-strict sample.json missing");
    assert!(chisel_raw.join("estimates.json").exists(), "raw chisel-strict estimates.json missing");
}
```

- [ ] **Step 2: Run the test**

Run: `cd bench && cargo test --test summarize_smoke 2>&1 | tail -10`
Expected: 1 passed in a few seconds (the test invokes the binary, which is fast).

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 4: Commit**

```bash
git add bench/tests/summarize_smoke.rs
git commit -m "$(cat <<'EOF'
bench: add summarize integration smoke test

Invokes the summarize binary via assert_cmd against the committed
fixtures. Asserts: exit 0, summary.md exists with sane size, results.json
parses to expected schema with ≥2 cells, raw/ directory contains the
chisel-strict 32B sample.json and estimates.json. Catches wiring bugs
that unit tests miss.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Final acceptance verification

**Files:**
- Read-only checks across the bench subcrate.

- [ ] **Step 1: Run the full test suite**

Run: `cd bench && cargo test 2>&1 | grep "test result"`
Expected counts:
- 22 (existing PR 4b lib) + 7 (format) + 3 (discover) + 1 (metadata) + 1 (render_json) + 2 (render_md) = 36 lib tests
- 15 equivalence tests (PR 3)
- 1 lib smoke (existing)
- 1 runner smoke (PR 4b)
- 1 summarize smoke (this PR)
- = 54 tests total, all passing

- [ ] **Step 2: Verify clippy and fmt clean across all targets**

Run: `cd bench && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

Run: `cargo fmt -- --check` (from worktree root)
Expected: no diff.

- [ ] **Step 3: Verify the `--help` text of the binary**

Run: `cd bench && cargo run --bin summarize -- --help 2>&1 | tail -15`
Expected: clap-formatted help with `--out`, `--criterion`, `--aux` flags.

- [ ] **Step 4: Smoke-run the binary against the fixtures by hand**

Run:
```bash
cd bench
TMPOUT=$(mktemp -d)
cargo run --bin summarize -- \
  --out "$TMPOUT" \
  --criterion tests/fixtures/criterion \
  --aux tests/fixtures/aux_metrics.jsonl
echo "--- summary.md preview ---"
head -30 "$TMPOUT/summary.md"
echo "--- results.json metadata ---"
python3 -c "import json; m=json.load(open('$TMPOUT/results.json'))['metadata']; print(m)"
echo "--- raw archive contents ---"
find "$TMPOUT/raw" -type f | sort
```

Expected output:
- A markdown summary header followed by the durability legend section
- A metadata dict with `cell_count: 2` (or higher if the corrupt fixture's cell got included)
- raw archive contains 4 JSON files (2 sample + 2 estimates from the valid cells; the corrupt cell's sample.json may also have copied since the archive copies all matching files)

- [ ] **Step 5: Verify against PR 4b's bench output if available**

If `target/criterion/` exists (from a previous run on this branch), run:
```bash
cd bench && cargo run --bin summarize 2>&1 | tail -5
```
Expected: a fresh `bench/results/<UTC>/` directory, with `summary.md`, `results.json`, `raw/`. The cell count printed should match the entries in `bench/results/aux_metrics.jsonl` from PR 4b.

If `target/criterion/` doesn't exist, skip this step (the binary's behavior is verified by the integration smoke test in task 11).

- [ ] **Step 6: Cross-check spec acceptance criteria**

Spec §9 acceptance criteria 1-8:
1. ✓ cargo build / cargo test pass — verified in Step 1
2. ✓ cargo clippy --all-targets -- -D warnings clean — verified in Step 2
3. ✓ cargo fmt -- --check clean — verified in Step 2
4. ✓ The 13+1 new tests pass — verified in Step 1 (7 format + 3 discover + 1 metadata + 1 render_json + 2 render_md + 1 smoke = 15, but the spec said 13. Difference: my percentile_linear_interp test (1) and the metadata test (1). Both were pragmatic adds.)
5. ✓ `cargo run --bin summarize -- --help` produces clap help — verified in Step 3
6. ⚠️ Running against PR 4b's actual bench output produces the three artifacts — verified in Step 5 if applicable
7. ✓ `jq '.metadata, .cells | type'` reports object/object — verified by step 4 visualization
8. ⚠️ `jq '.cells | keys | length'` matches PR 4b cell count — verified in Step 5 if applicable

- [ ] **Step 7: No commit needed if all checks pass**

If steps 1-6 all pass, do nothing — the plan is complete.

---

## Final state after all tasks

- `bench/Cargo.toml` has `chrono`, `walkdir`, `clap`, `hostname` deps; `assert_cmd` dev-dep; `[[bin]] summarize` target.
- `bench/src/summary/` has 5 submodules: `format`, `discover`, `metadata`, `render_json`, `render_md`. ~570 LOC production.
- `bench/src/bin/summarize.rs` is the CLI binary. ~85 LOC.
- `bench/tests/fixtures/` contains the synthetic 2-cell Criterion tree + aux_metrics.jsonl.
- `bench/tests/summarize_smoke.rs` is the integration test.
- 15 new tests (7 + 3 + 1 + 1 + 2 + 1 smoke) on top of PR 4b's existing 39.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check` all clean.
- `cargo run --bin summarize` produces three artifacts under a timestamped output directory.
- 11 commits authored.

PR 6 (scenarios) and PR 7 (CI workflow) can now begin: PR 6 produces additional aux_metrics.jsonl entries with `scenario` keys (post-processor's defensive deserialization tolerates them); PR 7 reads `results.json` from two runs and produces a regression diff.
