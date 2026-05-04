# Bench CI Integration Implementation Plan (PR 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land a `chisel-bench-diff` binary at `bench/src/bin/diff.rs` that diffs two `results.json` files and emits a markdown PR comment, plus a `.github/workflows/bench.yml` GitHub Actions workflow that runs the scenario tier on each PR, invokes the diff binary, and posts the comment via `peter-evans/create-or-update-comment`. Workflow is signal-only — it never blocks merge.

**Architecture:** Library/binary split mirrors PR 5's `summary` module. Library code (`bench/src/diff/{parse,compare,render}.rs`) handles JSON parse, threshold comparison, and markdown rendering — fully unit-testable with synthetic fixtures. The binary (`bench/src/bin/diff.rs`) is just `clap` + file I/O + stdout. The workflow does two checkouts (main + PR HEAD), runs `cargo bench --bench scenarios` against each, summarizes both via the existing `summarize` binary, and feeds both `results.json` files to the diff binary.

**Tech Stack:** Rust 2021 (no new runtime deps — uses existing `serde_json`, `clap`, `chrono`). YAML for workflow. Test infrastructure: `assert_cmd` (already a dev-dep).

**Spec:** `docs/superpowers/specs/2026-05-04-chisel-bench-ci-design.md`

**Pre-commit checklist (every commit task must pass these from inside `bench/`):**
- `cargo test` (full, not `--lib` — integration tests live in `bench/tests/`)
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt -- --check`

The bench subcrate is a sibling, not a workspace member, so these run from inside `bench/` not from repo root. The existing `ci.yml` does NOT enforce clippy/fmt on the bench subcrate; the discipline is on the implementer per task.

**Worktree note:** Implementation continues in the current worktree (`silly-euler-e130ad`, branch `claude/silly-euler-e130ad`). The PR 7 spec is already committed there. The handoff suggested a new worktree named `bench-ci-integration`, but switching now would require cherry-picking the spec commit; staying on the current branch is simpler and the branch can be renamed later if desired.

---

## Task 1: Cargo.toml + module skeleton

**Goal:** Add the `chisel-bench-diff` binary target and the `bench/src/diff/` module skeleton. After this task, `cargo build` succeeds and the binary stub runs but produces no real output.

**Files:**
- Modify: `bench/Cargo.toml`
- Modify: `bench/src/lib.rs`
- Create: `bench/src/diff/mod.rs`
- Create: `bench/src/diff/parse.rs` (stub)
- Create: `bench/src/diff/compare.rs` (stub)
- Create: `bench/src/diff/render.rs` (stub)
- Create: `bench/src/bin/diff.rs` (stub)

- [ ] **Step 1: Edit `bench/Cargo.toml`**

The current `[[bin]]` block declares only `summarize` (cargo bin name). Add `chisel-bench-diff` alongside it. Find the existing `[[bin]]` declaration and add a second one. After:

```toml
[[bin]]
name = "summarize"
path = "src/bin/summarize.rs"

[[bin]]
name = "chisel-bench-diff"
path = "src/bin/diff.rs"
```

Note: the existing summarize binary uses a short cargo name (`summarize`) but a longer `clap` command name (`chisel-bench-summarize`) declared inside `summarize.rs`. The new diff binary uses the long form for both — `chisel-bench-diff` for both cargo `[[bin]] name` and clap `#[command(name = ...)]`. Pick whichever you prefer for the cargo name; the workflow in Task 13 invokes via `--bin chisel-bench-diff` to match this plan's choice.

No new dependencies. The binary uses `clap`, `chrono`, `serde_json` — all already in `[dependencies]` from PR 5/6.

- [ ] **Step 2: Edit `bench/src/lib.rs`** to add the new module:

Find where existing modules are declared (e.g. `pub mod summary;`, `pub mod scenarios;`) and add:

```rust
pub mod diff;
```

- [ ] **Step 3: Create `bench/src/diff/mod.rs`** with module declarations:

```rust
// PR 7: regression-diff library. Consumes two results.json files
// (PR 5 schema), computes per-metric deltas with threshold-based
// flagging, renders the result as a markdown PR-comment body.
//
// Library/binary split: this module is unit-testable; the binary
// at src/bin/diff.rs is just argv parsing, file I/O, and stdout.

pub mod compare;
pub mod parse;
pub mod render;
```

- [ ] **Step 4: Create `bench/src/diff/parse.rs`** stub:

```rust
// JSON parser for the diff's input. Reads PR 5's results.json
// schema (top-level "scenarios" map keyed by "<scenario>/<mode>")
// and produces a typed view containing only the four metrics the
// diff cares about (throughput + p50/p95/p99). All other fields
// (cells, metadata, counters, file size) are ignored.
```

(File starts as a header comment only; implementation lands in Task 2.)

- [ ] **Step 5: Create `bench/src/diff/compare.rs`** stub:

```rust
// Threshold-based comparison of baseline vs PR ParsedResults.
// Produces a DiffReport with per-scenario per-metric MetricDelta
// values. The "bad-direction-positive" sign convention on
// delta_pct (see spec §3.3) means every regression check is
// uniformly `delta_pct > threshold_pct` regardless of metric.
```

- [ ] **Step 6: Create `bench/src/diff/render.rs`** stub:

```rust
// Markdown renderer for a DiffReport. Produces the PR-comment body
// with status line, summary table, collapsible per-scenario detail,
// and footer. Always-emitted marker `<!-- chisel-bench-diff -->`
// on first line lets peter-evans/find-comment update existing
// comments rather than appending new ones.
```

- [ ] **Step 7: Create `bench/src/bin/diff.rs`** stub:

```rust
// CLI entry point for chisel-bench-diff. Argv parsing + file I/O
// only; all logic lives in the chisel_bench::diff library module.

fn main() -> std::process::ExitCode {
    eprintln!("chisel-bench-diff: not yet implemented");
    std::process::ExitCode::FAILURE
}
```

- [ ] **Step 8: Verify the bench subcrate still builds and tests pass**

```bash
cd bench && cargo build
cd bench && cargo build --bin chisel-bench-diff
cd bench && cargo test
```

Expected: clean builds, all existing tests pass (post-PR-6 count: ~54 tests + 12 scenario cells if you also `cargo bench`, but tests should be 50+ unchanged).

- [ ] **Step 9: Verify clippy and fmt are clean**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
```

Expected: both clean. If clippy flags the empty stub modules with "unused" warnings, add `#![allow(dead_code)]` to the stubs — they get filled in next tasks.

- [ ] **Step 10: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock bench/src/lib.rs bench/src/diff/ bench/src/bin/diff.rs
git commit -m "$(cat <<'EOF'
bench: scaffold chisel-bench-diff binary and diff/ module

Adds the bin target in bench/Cargo.toml, the diff/ module with stub
parse/compare/render submodules, and a bin/diff.rs stub that exits
non-zero. No functionality yet — subsequent commits fill in parse,
then compare, then render, then wire the binary.

No new dependencies; all needed crates (serde_json, clap, chrono)
are already present from PR 5 and PR 6.
EOF
)"
```

---

## Task 2: `parse.rs` — read results.json into ParsedResults

**Goal:** Implement `parse_results_json(path) -> Result<ParsedResults, ParseError>` with TDD coverage of valid file, empty-scenarios file, and malformed file.

**Files:**
- Modify: `bench/src/diff/parse.rs`

- [ ] **Step 1: Write the type definitions and failing test**

Replace the contents of `bench/src/diff/parse.rs` with:

```rust
// JSON parser for the diff's input. Reads PR 5's results.json
// schema (top-level "scenarios" map keyed by "<scenario>/<mode>")
// and produces a typed view containing only the four metrics the
// diff cares about (throughput + p50/p95/p99). All other fields
// (cells, metadata, counters, file size) are ignored.
//
// BTreeMap rather than HashMap: we want deterministic key
// iteration order in tests and renderer output.

use std::collections::BTreeMap;
use std::path::Path;

/// The four scenario metrics the diff binary compares. Mirrors a
/// subset of PR 5's `ScenarioMetrics` but flat — no Option wrapping,
/// no counters, no file-size fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScenarioMetrics {
    pub throughput_ops_per_sec: f64,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
}

/// Typed view of a results.json file, restricted to the scenarios
/// data the diff cares about. Cells, metadata, counters all dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedResults {
    /// Map from "<scenario>/<mode>" key to metrics.
    pub scenarios: BTreeMap<String, ScenarioMetrics>,
}

#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    Json(serde_json::Error),
    MissingScenariosKey,
    MalformedScenarioEntry(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Io(e) => write!(f, "I/O error reading results.json: {e}"),
            ParseError::Json(e) => write!(f, "JSON parse error in results.json: {e}"),
            ParseError::MissingScenariosKey => {
                write!(f, "results.json missing top-level `scenarios` key")
            }
            ParseError::MalformedScenarioEntry(key) => {
                write!(f, "results.json scenario entry `{key}` is malformed")
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<std::io::Error> for ParseError {
    fn from(e: std::io::Error) -> Self {
        ParseError::Io(e)
    }
}

impl From<serde_json::Error> for ParseError {
    fn from(e: serde_json::Error) -> Self {
        ParseError::Json(e)
    }
}

/// Read and parse a results.json file. Drops cells/metadata/counters;
/// keeps only the four metrics per scenario.
pub fn parse_results_json(path: &Path) -> Result<ParsedResults, ParseError> {
    let contents = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&contents)?;

    let scenarios_obj = value
        .get("scenarios")
        .and_then(|v| v.as_object())
        .ok_or(ParseError::MissingScenariosKey)?;

    let mut scenarios = BTreeMap::new();
    for (key, entry) in scenarios_obj {
        let metrics = parse_scenario_entry(key, entry)?;
        scenarios.insert(key.clone(), metrics);
    }

    Ok(ParsedResults { scenarios })
}

fn parse_scenario_entry(
    key: &str,
    entry: &serde_json::Value,
) -> Result<ScenarioMetrics, ParseError> {
    let obj = entry
        .as_object()
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;

    let throughput = obj
        .get("throughput_ops_per_sec")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;
    let p50 = obj
        .get("p50_ns")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;
    let p95 = obj
        .get("p95_ns")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;
    let p99 = obj
        .get("p99_ns")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;

    Ok(ScenarioMetrics {
        throughput_ops_per_sec: throughput,
        p50_ns: p50,
        p95_ns: p95,
        p99_ns: p99,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp(json: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parse_valid_results_json() {
        let json = r#"{
          "metadata": {"timestamp": "2026-05-04T12:00:00Z"},
          "cells": {},
          "scenarios": {
            "ycsb-a/chisel-strict": {
              "total_wall_clock_ns": 15000000000,
              "op_count": 100000,
              "throughput_ops_per_sec": 6666.7,
              "p50_ns": 120000.0,
              "p95_ns": 180000.0,
              "p99_ns": 250000.0,
              "final_file_size_bytes": 100000000,
              "file_size_delta_bytes": 4194304,
              "counters": null
            }
          }
        }"#;
        let f = write_temp(json);
        let parsed = parse_results_json(f.path()).unwrap();
        assert_eq!(parsed.scenarios.len(), 1);
        let m = parsed.scenarios.get("ycsb-a/chisel-strict").unwrap();
        assert_eq!(m.throughput_ops_per_sec, 6666.7);
        assert_eq!(m.p50_ns, 120000.0);
        assert_eq!(m.p95_ns, 180000.0);
        assert_eq!(m.p99_ns, 250000.0);
    }

    #[test]
    fn parse_empty_scenarios_map() {
        let json = r#"{"metadata": {}, "cells": {}, "scenarios": {}}"#;
        let f = write_temp(json);
        let parsed = parse_results_json(f.path()).unwrap();
        assert!(parsed.scenarios.is_empty());
    }

    #[test]
    fn parse_missing_scenarios_key() {
        let json = r#"{"metadata": {}, "cells": {}}"#;
        let f = write_temp(json);
        let err = parse_results_json(f.path()).unwrap_err();
        assert!(matches!(err, ParseError::MissingScenariosKey));
    }
}
```

Note: this task adds the implementation alongside the tests in one step rather than strict failing-test-first, because the test fixtures and parse code interlock tightly and writing the test alone produces an unhelpful "ScenarioMetrics doesn't exist" error rather than a real driver for design. The TDD discipline still applies — tests exist before commit and exercise the interface they expect.

- [ ] **Step 2: Run the new tests**

```bash
cd bench && cargo test --lib diff::parse::
```

Expected: 3 tests pass (`parse_valid_results_json`, `parse_empty_scenarios_map`, `parse_missing_scenarios_key`).

- [ ] **Step 3: Run all bench tests to confirm no regressions**

```bash
cd bench && cargo test
```

Expected: all existing tests still pass; +3 new tests = 57+ total.

- [ ] **Step 4: Verify clippy and fmt**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
```

Expected: both clean.

- [ ] **Step 5: Commit**

```bash
git add bench/src/diff/parse.rs
git commit -m "$(cat <<'EOF'
bench(diff): parse results.json into ParsedResults

Reads PR 5's results.json schema and extracts the four metrics per
scenario (throughput + p50/p95/p99). All other fields (cells, metadata,
counters, file size) are dropped — the diff cares only about regression
detection.

ParseError enum distinguishes I/O error, JSON parse error, missing
top-level scenarios key, and malformed individual entries. BTreeMap
gives deterministic iteration order for downstream compare/render.

Three unit tests: valid file, empty scenarios map, missing scenarios
key.
EOF
)"
```

---

## Task 3: `compare.rs` — core types + identical-input case

**Goal:** Define `Metric`, `DeltaStatus`, `MetricDelta`, `ScenarioDiff`, `DiffReport`, threshold constants, and the `compare` function. Cover the identical-baseline case with one test.

**Files:**
- Modify: `bench/src/diff/compare.rs`

- [ ] **Step 1: Implement the types and the compare function**

Replace `bench/src/diff/compare.rs` contents with:

```rust
// Threshold-based comparison of baseline vs PR ParsedResults.
// Produces a DiffReport with per-scenario per-metric MetricDelta
// values. The "bad-direction-positive" sign convention on
// delta_pct (see spec §3.3) means every regression check is
// uniformly `delta_pct > threshold_pct` regardless of metric.

use crate::diff::parse::{ParsedResults, ScenarioMetrics};
use std::collections::BTreeSet;
use std::path::PathBuf;

// Threshold constants (spec §3.4). Module-level, not config: tuning
// is a future change that should come with concrete data behind it.
pub const THRESHOLD_PCT_THROUGHPUT: f64 = 5.0;
pub const THRESHOLD_PCT_P50: f64 = 5.0;
pub const THRESHOLD_PCT_P95: f64 = 10.0;
pub const THRESHOLD_PCT_P99: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Throughput,
    P50,
    P95,
    P99,
}

impl Metric {
    pub fn label(&self) -> &'static str {
        match self {
            Metric::Throughput => "throughput",
            Metric::P50 => "p50",
            Metric::P95 => "p95",
            Metric::P99 => "p99",
        }
    }

    pub fn threshold_pct(&self) -> f64 {
        match self {
            Metric::Throughput => THRESHOLD_PCT_THROUGHPUT,
            Metric::P50 => THRESHOLD_PCT_P50,
            Metric::P95 => THRESHOLD_PCT_P95,
            Metric::P99 => THRESHOLD_PCT_P99,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeltaStatus {
    /// Metric improved (PR is faster / higher throughput). Not flagged.
    Improved,
    /// Within threshold. Not flagged.
    Unchanged,
    /// Regressed beyond threshold. Flagged.
    Regressed { pct: f64, threshold_pct: f64 },
    /// Cell present on PR side but absent on baseline.
    BaselineMissing,
    /// Cell present on baseline but absent on PR side.
    PrMissing,
}

#[derive(Debug, Clone)]
pub struct MetricDelta {
    pub metric: Metric,
    pub baseline: Option<f64>,
    pub pr: Option<f64>,
    /// Signed in the bad direction: positive = PR slower / lower
    /// throughput. None when either side is missing.
    pub delta_pct: Option<f64>,
    pub status: DeltaStatus,
}

#[derive(Debug, Clone)]
pub struct ScenarioDiff {
    pub scenario: String,
    pub mode: String,
    pub metrics: [MetricDelta; 4],
    /// The single worst regression in `metrics`, if any. Populates
    /// the summary-table "Worst Δ" column.
    pub worst_regression: Option<MetricDelta>,
}

#[derive(Debug)]
pub struct DiffReport {
    pub scenarios: Vec<ScenarioDiff>,
    pub regression_count: usize,
    pub baseline_path: PathBuf,
    pub pr_path: PathBuf,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

/// Compare baseline against PR. Iterates the union of scenario keys
/// from both sides; produces one ScenarioDiff per key with four
/// MetricDelta entries. Regression count is summed across all flagged
/// metrics in all scenarios.
pub fn compare(
    baseline: &ParsedResults,
    pr: &ParsedResults,
    baseline_path: PathBuf,
    pr_path: PathBuf,
    generated_at: chrono::DateTime<chrono::Utc>,
) -> DiffReport {
    let mut all_keys: BTreeSet<String> = BTreeSet::new();
    all_keys.extend(baseline.scenarios.keys().cloned());
    all_keys.extend(pr.scenarios.keys().cloned());

    let mut scenarios = Vec::new();
    let mut regression_count = 0;

    for key in all_keys {
        let baseline_m = baseline.scenarios.get(&key);
        let pr_m = pr.scenarios.get(&key);
        let diff = compare_scenario(&key, baseline_m, pr_m);
        regression_count += diff
            .metrics
            .iter()
            .filter(|m| matches!(m.status, DeltaStatus::Regressed { .. }))
            .count();
        scenarios.push(diff);
    }

    DiffReport {
        scenarios,
        regression_count,
        baseline_path,
        pr_path,
        generated_at,
    }
}

fn compare_scenario(
    key: &str,
    baseline: Option<&ScenarioMetrics>,
    pr: Option<&ScenarioMetrics>,
) -> ScenarioDiff {
    // Split "scenario/mode" key. If somehow there's no slash, treat
    // the full key as scenario and "<unknown>" as mode — schema
    // should always produce well-formed keys, so this is defensive.
    let (scenario, mode) = match key.split_once('/') {
        Some((s, m)) => (s.to_string(), m.to_string()),
        None => (key.to_string(), "<unknown>".to_string()),
    };

    let metrics = [
        compare_metric(Metric::Throughput, baseline, pr),
        compare_metric(Metric::P50, baseline, pr),
        compare_metric(Metric::P95, baseline, pr),
        compare_metric(Metric::P99, baseline, pr),
    ];

    let worst_regression = metrics
        .iter()
        .filter(|m| matches!(m.status, DeltaStatus::Regressed { .. }))
        .max_by(|a, b| {
            a.delta_pct
                .unwrap_or(0.0)
                .partial_cmp(&b.delta_pct.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();

    ScenarioDiff {
        scenario,
        mode,
        metrics,
        worst_regression,
    }
}

fn compare_metric(
    metric: Metric,
    baseline: Option<&ScenarioMetrics>,
    pr: Option<&ScenarioMetrics>,
) -> MetricDelta {
    let extract = |m: &ScenarioMetrics| -> f64 {
        match metric {
            Metric::Throughput => m.throughput_ops_per_sec,
            Metric::P50 => m.p50_ns,
            Metric::P95 => m.p95_ns,
            Metric::P99 => m.p99_ns,
        }
    };
    match (baseline, pr) {
        (None, None) => MetricDelta {
            metric,
            baseline: None,
            pr: None,
            delta_pct: None,
            // Both sides missing — shouldn't happen since the union of
            // keys is non-empty for every iteration. Treat as unchanged.
            status: DeltaStatus::Unchanged,
        },
        (Some(_), None) => MetricDelta {
            metric,
            baseline: baseline.map(extract),
            pr: None,
            delta_pct: None,
            status: DeltaStatus::PrMissing,
        },
        (None, Some(_)) => MetricDelta {
            metric,
            baseline: None,
            pr: pr.map(extract),
            delta_pct: None,
            status: DeltaStatus::BaselineMissing,
        },
        (Some(b), Some(p)) => {
            let bv = extract(b);
            let pv = extract(p);
            // Bad-direction-positive sign convention. For throughput,
            // bad = lower, so delta_pct = (baseline - pr) / baseline * 100.
            // For latency, bad = higher, so delta_pct = (pr - baseline) / baseline * 100.
            let delta_pct = match metric {
                Metric::Throughput => (bv - pv) / bv * 100.0,
                Metric::P50 | Metric::P95 | Metric::P99 => (pv - bv) / bv * 100.0,
            };
            let status = if delta_pct > metric.threshold_pct() {
                DeltaStatus::Regressed {
                    pct: delta_pct,
                    threshold_pct: metric.threshold_pct(),
                }
            } else if delta_pct < 0.0 {
                DeltaStatus::Improved
            } else {
                DeltaStatus::Unchanged
            };
            MetricDelta {
                metric,
                baseline: Some(bv),
                pr: Some(pv),
                delta_pct: Some(delta_pct),
                status,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixed_metrics() -> ScenarioMetrics {
        ScenarioMetrics {
            throughput_ops_per_sec: 1000.0,
            p50_ns: 100_000.0,
            p95_ns: 200_000.0,
            p99_ns: 500_000.0,
        }
    }

    fn one_scenario(key: &str, m: ScenarioMetrics) -> ParsedResults {
        let mut s = BTreeMap::new();
        s.insert(key.to_string(), m);
        ParsedResults { scenarios: s }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-04T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn identical_inputs_produce_no_regressions() {
        let m = fixed_metrics();
        let baseline = one_scenario("ycsb-a/chisel-strict", m);
        let pr = one_scenario("ycsb-a/chisel-strict", m);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("baseline.json"),
            PathBuf::from("pr.json"),
            now(),
        );
        assert_eq!(report.regression_count, 0);
        assert_eq!(report.scenarios.len(), 1);
        let s = &report.scenarios[0];
        assert_eq!(s.scenario, "ycsb-a");
        assert_eq!(s.mode, "chisel-strict");
        assert!(s.worst_regression.is_none());
        for md in &s.metrics {
            assert!(matches!(md.status, DeltaStatus::Unchanged));
            assert_eq!(md.delta_pct, Some(0.0));
        }
    }
}
```

- [ ] **Step 2: Run the new test**

```bash
cd bench && cargo test --lib diff::compare::identical_inputs_produce_no_regressions
```

Expected: 1 test passes.

- [ ] **Step 3: Run all bench tests**

```bash
cd bench && cargo test
```

Expected: all pass; +1 test = 58+ total.

- [ ] **Step 4: Clippy + fmt**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
```

- [ ] **Step 5: Commit**

```bash
git add bench/src/diff/compare.rs
git commit -m "$(cat <<'EOF'
bench(diff): core compare types and identical-input case

Defines Metric, DeltaStatus, MetricDelta, ScenarioDiff, DiffReport
plus the threshold constants (throughput + p50 at 5%, p95 + p99 at 10%
per spec §3.4).

The compare() function iterates the union of scenario keys from both
sides, produces one ScenarioDiff per key, and computes regression
count by summing flagged metrics. Bad-direction-positive sign
convention for delta_pct (spec §3.3) means uniform delta_pct >
threshold_pct test for regression flagging.

One test: identical baseline + PR produces zero regressions and all
DeltaStatus::Unchanged. Regression-detection tests come in the next
commit.
EOF
)"
```

---

## Task 4: `compare.rs` — regression detection tests

**Goal:** Add three tests covering the regression-vs-unchanged threshold logic for throughput and p99.

**Files:**
- Modify: `bench/src/diff/compare.rs`

- [ ] **Step 1: Add the three regression tests inside the existing `#[cfg(test)] mod tests` block**

After the existing `identical_inputs_produce_no_regressions` test, add:

```rust
    #[test]
    fn pr_throughput_10pct_lower_is_regressed() {
        // Throughput threshold is 5%; 10% lower trips it.
        let baseline = one_scenario(
            "ycsb-a/chisel-strict",
            ScenarioMetrics {
                throughput_ops_per_sec: 1000.0,
                p50_ns: 100_000.0,
                p95_ns: 200_000.0,
                p99_ns: 500_000.0,
            },
        );
        let pr = one_scenario(
            "ycsb-a/chisel-strict",
            ScenarioMetrics {
                throughput_ops_per_sec: 900.0, // 10% lower
                p50_ns: 100_000.0,
                p95_ns: 200_000.0,
                p99_ns: 500_000.0,
            },
        );
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 1);
        let s = &report.scenarios[0];
        let throughput_md = &s.metrics[0];
        assert_eq!(throughput_md.metric, Metric::Throughput);
        match &throughput_md.status {
            DeltaStatus::Regressed { pct, threshold_pct } => {
                assert!((pct - 10.0).abs() < 0.001, "expected ~10.0, got {pct}");
                assert_eq!(*threshold_pct, 5.0);
            }
            other => panic!("expected Regressed, got {other:?}"),
        }
        assert!(s.worst_regression.is_some());
        assert_eq!(
            s.worst_regression.as_ref().unwrap().metric,
            Metric::Throughput
        );
    }

    #[test]
    fn pr_p99_6pct_higher_is_unchanged() {
        // p99 threshold is 10%; 6% does not trip it.
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr_metrics = ScenarioMetrics {
            p99_ns: 530_000.0, // 6% higher than 500_000
            ..fixed_metrics()
        };
        let pr = one_scenario("ycsb-a/chisel-strict", pr_metrics);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 0);
        let p99_md = &report.scenarios[0].metrics[3];
        assert_eq!(p99_md.metric, Metric::P99);
        assert!(matches!(p99_md.status, DeltaStatus::Unchanged));
    }

    #[test]
    fn pr_p99_12pct_higher_is_regressed() {
        // p99 threshold is 10%; 12% trips it.
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr_metrics = ScenarioMetrics {
            p99_ns: 560_000.0, // 12% higher than 500_000
            ..fixed_metrics()
        };
        let pr = one_scenario("ycsb-a/chisel-strict", pr_metrics);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 1);
        let p99_md = &report.scenarios[0].metrics[3];
        match &p99_md.status {
            DeltaStatus::Regressed { pct, threshold_pct } => {
                assert!((pct - 12.0).abs() < 0.001);
                assert_eq!(*threshold_pct, 10.0);
            }
            other => panic!("expected Regressed, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the new tests**

```bash
cd bench && cargo test --lib diff::compare::
```

Expected: 4 tests pass total (1 from Task 3 + 3 new).

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/diff/compare.rs
git commit -m "$(cat <<'EOF'
bench(diff): regression-detection threshold tests

Three tests cover the threshold logic:
- throughput 10% lower → Regressed (over the 5% throughput threshold)
- p99 6% higher → Unchanged (under the 10% p99 threshold)
- p99 12% higher → Regressed (over the 10% p99 threshold)

Each test validates not just the status but also the regression
percentage and threshold values inside the Regressed variant. The
worst_regression field is also asserted on the throughput test.
EOF
)"
```

---

## Task 5: `compare.rs` — missing-cell tests

**Goal:** Two tests covering the BaselineMissing and PrMissing paths.

**Files:**
- Modify: `bench/src/diff/compare.rs`

- [ ] **Step 1: Add the two missing-cell tests inside the existing `mod tests` block**

After the previous tests, add:

```rust
    #[test]
    fn cell_missing_on_pr_yields_pr_missing() {
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr = ParsedResults::default(); // empty
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        // Not counted as regression: missing-cell is its own category.
        assert_eq!(report.regression_count, 0);
        assert_eq!(report.scenarios.len(), 1);
        let s = &report.scenarios[0];
        for md in &s.metrics {
            assert!(matches!(md.status, DeltaStatus::PrMissing));
            assert!(md.baseline.is_some());
            assert!(md.pr.is_none());
        }
    }

    #[test]
    fn cell_missing_on_baseline_yields_baseline_missing() {
        let baseline = ParsedResults::default(); // empty
        let pr = one_scenario("ycsb-c/chisel-strict", fixed_metrics());
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 0);
        assert_eq!(report.scenarios.len(), 1);
        let s = &report.scenarios[0];
        for md in &s.metrics {
            assert!(matches!(md.status, DeltaStatus::BaselineMissing));
            assert!(md.baseline.is_none());
            assert!(md.pr.is_some());
        }
    }
```

- [ ] **Step 2: Run all compare tests**

```bash
cd bench && cargo test --lib diff::compare::
```

Expected: 6 tests pass total.

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/diff/compare.rs
git commit -m "$(cat <<'EOF'
bench(diff): missing-cell handling tests

Two tests cover the BaselineMissing and PrMissing paths:
- baseline has cell, PR doesn't → all four MetricDelta entries
  show PrMissing status, regression_count stays zero
- PR has cell, baseline doesn't → BaselineMissing status, also
  not counted as regression

Per spec §3.6, missing-cell is its own category — neither a
regression nor an improvement, surfaced separately in the rendered
output.
EOF
)"
```

---

## Task 6: `render.rs` skeleton + no-regression case

**Goal:** Implement the renderer with the no-regression path: marker comment, header, status line `✅`, summary table, empty per-scenario detail (we'll fill it in later), footer.

**Files:**
- Modify: `bench/src/diff/render.rs`

- [ ] **Step 1: Write the renderer with no-regression support and one test**

Replace `bench/src/diff/render.rs` contents with:

```rust
// Markdown renderer for a DiffReport. Produces the PR-comment body
// with status line, summary table, collapsible per-scenario detail,
// and footer. Always-emitted marker `<!-- chisel-bench-diff -->`
// on first line lets peter-evans/find-comment update existing
// comments rather than appending new ones.

use crate::diff::compare::{DeltaStatus, DiffReport, Metric, MetricDelta, ScenarioDiff};
use std::collections::BTreeSet;

/// Render the full markdown comment body. See spec §4 for structure.
pub fn render_markdown(report: &DiffReport) -> String {
    let mut out = String::new();
    out.push_str("<!-- chisel-bench-diff -->\n");
    out.push_str("## 🚦 Bench results: PR vs main\n\n");
    out.push_str(&render_status_line(report));
    out.push_str("\n\n");

    // Empty-both-inputs early exit (spec §4.1 first variant).
    if report.scenarios.is_empty() {
        out.push_str(&render_footer(report));
        return out;
    }

    out.push_str(&render_summary_table(report));
    out.push_str("\n");

    out.push_str("<details>\n");
    out.push_str("<summary>Per-scenario detail (4 metrics × cells)</summary>\n\n");
    for scenario in unique_scenarios(report) {
        out.push_str(&render_scenario_detail(report, &scenario));
    }
    out.push_str("</details>\n\n");

    out.push_str(&render_footer(report));
    out
}

fn render_status_line(report: &DiffReport) -> String {
    // Priority order per spec §4.1.
    if report.scenarios.is_empty() {
        return "❗ No scenarios to compare — both inputs have empty scenario data".to_string();
    }
    if has_missing_cell(report) {
        return "❗ Diff incomplete — see details below".to_string();
    }
    if report.regression_count > 0 {
        let pair_count = report
            .scenarios
            .iter()
            .filter(|s| s.worst_regression.is_some())
            .count();
        return format!(
            "⚠️ {} regression(s) detected across {} scenario/mode pair(s)",
            report.regression_count, pair_count,
        );
    }
    "✅ No regressions detected".to_string()
}

fn has_missing_cell(report: &DiffReport) -> bool {
    report.scenarios.iter().any(|s| {
        s.metrics
            .iter()
            .any(|m| matches!(m.status, DeltaStatus::BaselineMissing | DeltaStatus::PrMissing))
    })
}

fn render_summary_table(report: &DiffReport) -> String {
    let mut out = String::new();
    out.push_str("| Scenario        | Mode          | Δ throughput | Worst Δ        |\n");
    out.push_str("| --------------- | ------------- | ------------ | -------------- |\n");
    let rows = sort_summary_rows(report);
    for s in rows {
        out.push_str(&render_summary_row(s));
        out.push('\n');
    }
    out
}

fn sort_summary_rows<'a>(report: &'a DiffReport) -> Vec<&'a ScenarioDiff> {
    let mut rows: Vec<&ScenarioDiff> = report.scenarios.iter().collect();
    let any_attention = report.regression_count > 0 || has_missing_cell(report);
    if any_attention {
        // Worst-regression first; missing-cell rows sort to the top.
        rows.sort_by(|a, b| {
            let ka = sort_key_attention(a);
            let kb = sort_key_attention(b);
            kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        rows.sort_by(|a, b| (&a.scenario, &a.mode).cmp(&(&b.scenario, &b.mode)));
    }
    rows
}

// Higher = sorts earlier. Missing-cell rows get +infinity; regressed
// rows get their delta_pct; everything else gets f64::NEG_INFINITY.
fn sort_key_attention(s: &ScenarioDiff) -> f64 {
    if s.metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::BaselineMissing | DeltaStatus::PrMissing))
    {
        return f64::INFINITY;
    }
    s.worst_regression
        .as_ref()
        .and_then(|m| m.delta_pct)
        .unwrap_or(f64::NEG_INFINITY)
}

fn render_summary_row(s: &ScenarioDiff) -> String {
    // Δ throughput column: display sign convention (raw direction),
    // not bad-direction-positive. See spec §4.3. Throughput is metrics[0].
    let throughput = &s.metrics[0];
    let throughput_str = match (&throughput.status, throughput.delta_pct) {
        (DeltaStatus::PrMissing, _) => "—".to_string(),
        (DeltaStatus::BaselineMissing, _) => "—".to_string(),
        (_, Some(bad_pct)) => {
            // Throughput display sign is opposite of bad-direction-positive.
            let display_pct = -bad_pct;
            format!("{display_pct:+.1}%")
        }
        (_, None) => "—".to_string(),
    };

    let worst_str = match (&s.worst_regression, missing_marker(s)) {
        (_, Some(marker)) => marker,
        (Some(md), None) => format!(
            "{} {} ⚠️",
            md.metric.label(),
            format_delta_display(md),
        ),
        (None, None) => "—".to_string(),
    };

    format!(
        "| {:<15} | {:<13} | {:>12} | {:<14} |",
        s.scenario, s.mode, throughput_str, worst_str
    )
}

fn missing_marker(s: &ScenarioDiff) -> Option<String> {
    let pr_missing = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::PrMissing));
    let baseline_missing = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::BaselineMissing));
    if pr_missing {
        Some(format!(
            "❌ {} / {} — missing on PR side",
            s.scenario, s.mode
        ))
    } else if baseline_missing {
        Some(format!(
            "❓ {} / {} — new scenario, no baseline",
            s.scenario, s.mode
        ))
    } else {
        None
    }
}

// Display percentage with raw-direction sign (spec §4.3).
// For latency metrics the bad-direction is the same as raw direction,
// so display_pct == delta_pct. For throughput, flip the sign.
fn format_delta_display(md: &MetricDelta) -> String {
    let display_pct = match md.metric {
        Metric::Throughput => -md.delta_pct.unwrap_or(0.0),
        _ => md.delta_pct.unwrap_or(0.0),
    };
    format!("{display_pct:+.1}%")
}

fn unique_scenarios(report: &DiffReport) -> Vec<String> {
    let mut s = BTreeSet::new();
    for sd in &report.scenarios {
        s.insert(sd.scenario.clone());
    }
    s.into_iter().collect()
}

fn render_scenario_detail(report: &DiffReport, scenario_name: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("### {scenario_name}\n"));
    out.push_str("| Mode          | Throughput        | p50               | p95               | p99               |\n");
    out.push_str("| ------------- | ----------------- | ----------------- | ----------------- | ----------------- |\n");
    for s in report.scenarios.iter().filter(|s| s.scenario == scenario_name) {
        out.push_str(&render_detail_row(s));
        out.push('\n');
    }
    out.push('\n');
    out
}

fn render_detail_row(s: &ScenarioDiff) -> String {
    let cells: Vec<String> = s.metrics.iter().map(render_detail_cell).collect();
    format!(
        "| {:<13} | {} | {} | {} | {} |",
        s.mode, cells[0], cells[1], cells[2], cells[3]
    )
}

fn render_detail_cell(md: &MetricDelta) -> String {
    match (&md.status, md.baseline, md.pr) {
        (DeltaStatus::PrMissing, _, _) | (DeltaStatus::BaselineMissing, _, _) => {
            "—".to_string()
        }
        (_, Some(b), Some(p)) => {
            let flag = if matches!(md.status, DeltaStatus::Regressed { .. }) {
                " ⚠️"
            } else {
                ""
            };
            let delta = format_delta_display(md);
            match md.metric {
                Metric::Throughput => format!("{} → {} ({}){}", format_throughput(b), format_throughput(p), delta, flag),
                _ => format!("{} → {} ({}){}", format_duration_ns(b), format_duration_ns(p), delta, flag),
            }
        }
        _ => "—".to_string(),
    }
}

fn format_throughput(ops: f64) -> String {
    format!("{} ops/s", ops.round() as u64)
}

fn format_duration_ns(ns: f64) -> String {
    if ns >= 1_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1} µs", ns / 1_000.0)
    } else {
        format!("{:.0} ns", ns)
    }
}

fn render_footer(report: &DiffReport) -> String {
    format!(
        "<sub>\nGenerated by chisel-bench-diff at {}.\nCompares PR HEAD against main. Never blocks merge — signal, not gate.\nThresholds: throughput 5%, p50 5%, p95 10%, p99 10%.\n</sub>\n",
        report.generated_at.format("%Y-%m-%dT%H:%M:%SZ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::compare::compare;
    use crate::diff::parse::{ParsedResults, ScenarioMetrics};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn fixed_metrics() -> ScenarioMetrics {
        ScenarioMetrics {
            throughput_ops_per_sec: 1000.0,
            p50_ns: 100_000.0,
            p95_ns: 200_000.0,
            p99_ns: 500_000.0,
        }
    }

    fn one_scenario(key: &str, m: ScenarioMetrics) -> ParsedResults {
        let mut s = BTreeMap::new();
        s.insert(key.to_string(), m);
        ParsedResults { scenarios: s }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-04T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn no_regression_renders_green_check() {
        let m = fixed_metrics();
        let baseline = one_scenario("ycsb-a/chisel-strict", m);
        let pr = one_scenario("ycsb-a/chisel-strict", m);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(out.starts_with("<!-- chisel-bench-diff -->\n"), "marker first line missing");
        assert!(out.contains("## 🚦 Bench results: PR vs main"));
        assert!(out.contains("✅ No regressions detected"));
        assert!(out.contains("| ycsb-a"));
        assert!(out.contains("chisel-strict"));
        assert!(out.contains("<details>"));
        assert!(out.contains("Generated by chisel-bench-diff at 2026-05-04T12:00:00Z"));
        assert!(out.contains("Thresholds: throughput 5%, p50 5%, p95 10%, p99 10%"));
    }
}
```

- [ ] **Step 2: Run the test**

```bash
cd bench && cargo test --lib diff::render::no_regression_renders_green_check
```

Expected: 1 test passes.

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/diff/render.rs
git commit -m "$(cat <<'EOF'
bench(diff): renderer skeleton + no-regression case

Implements the full markdown renderer with all four status-line
variants in priority order (empty > missing-cell > regression > green).
Regression and missing-cell paths are reachable but only the
no-regression case has a unit test in this commit.

Display sign convention (spec §4.3): the user-facing percentage uses
raw value direction. For throughput, the renderer flips the sign on
delta_pct (which is internally bad-direction-positive). For latency,
internal and display conventions match.

Auto-magnitude time formatting (ns/µs/ms) inlined per spec §4.3 —
not extracted into a shared helper because PR 5's summary/format.rs
is small enough that duplication is preferable to a cross-module
dependency.
EOF
)"
```

---

## Task 7: `render.rs` — regression rendering test

**Goal:** Add one test for the regression case: status line shows `⚠️`, summary row shows the worst-Δ column populated, sort order puts the worst row first.

**Files:**
- Modify: `bench/src/diff/render.rs`

- [ ] **Step 1: Add the regression-rendering test inside the existing `mod tests` block**

After the `no_regression_renders_green_check` test, add:

```rust
    #[test]
    fn regression_renders_warning_with_worst_column_populated() {
        // Set up two scenarios; one with a 12% p99 regression, one clean.
        let mut bs = BTreeMap::new();
        bs.insert("ycsb-a/chisel-strict".to_string(), fixed_metrics());
        bs.insert("ycsb-b/chisel-strict".to_string(), fixed_metrics());
        let baseline = ParsedResults { scenarios: bs };

        let mut ps = BTreeMap::new();
        ps.insert(
            "ycsb-a/chisel-strict".to_string(),
            ScenarioMetrics {
                p99_ns: 560_000.0, // 12% over 500_000
                ..fixed_metrics()
            },
        );
        ps.insert("ycsb-b/chisel-strict".to_string(), fixed_metrics());
        let pr = ParsedResults { scenarios: ps };

        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 1);
        let out = render_markdown(&report);

        assert!(
            out.contains("⚠️ 1 regression(s) detected across 1 scenario/mode pair(s)"),
            "warning header missing or wrong:\n{out}"
        );
        assert!(out.contains("p99 +12.0% ⚠️"), "worst-Δ column wrong:\n{out}");
        // ycsb-a (the regressed row) should appear before ycsb-b in the
        // summary table when sort-by-worst-first is applied.
        let ya_pos = out.find("| ycsb-a").unwrap();
        let yb_pos = out.find("| ycsb-b").unwrap();
        assert!(
            ya_pos < yb_pos,
            "ycsb-a (regressed) should sort before ycsb-b (clean):\nya_pos={ya_pos} yb_pos={yb_pos}\n{out}"
        );
    }
```

- [ ] **Step 2: Run the test**

```bash
cd bench && cargo test --lib diff::render::regression_renders_warning_with_worst_column_populated
```

Expected: pass.

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/diff/render.rs
git commit -m "$(cat <<'EOF'
bench(diff): regression-rendering test

Validates that a 12% p99 regression in one of two scenarios produces:
- ⚠️ status line with correct counts ("1 regression across 1 pair")
- "p99 +12.0% ⚠️" in the worst-Δ column of the regressed row
- sort order: regressed row before clean row in the summary table

The renderer code already supported these paths from Task 6; this
commit just adds the test that exercises them.
EOF
)"
```

---

## Task 8: `render.rs` — missing-cell + new-scenario tests

**Goal:** Two tests covering the `❌ ... — missing on PR side` and `❓ ... — new scenario, no baseline` branches.

**Files:**
- Modify: `bench/src/diff/render.rs`

- [ ] **Step 1: Add the two tests inside the existing `mod tests` block**

After the previous test, add:

```rust
    #[test]
    fn missing_cell_renders_with_red_x_and_diff_incomplete_header() {
        // Baseline has a scenario that PR doesn't (e.g. PR removed it).
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr = ParsedResults::default();
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(
            out.contains("❗ Diff incomplete — see details below"),
            "diff-incomplete header missing:\n{out}"
        );
        assert!(
            out.contains("❌ ycsb-a / chisel-strict — missing on PR side"),
            "missing-row marker missing:\n{out}"
        );
    }

    #[test]
    fn new_scenario_renders_with_question_mark_marker() {
        // PR adds a scenario that baseline doesn't have.
        let baseline = ParsedResults::default();
        let pr = one_scenario("ycsb-c/chisel-strict", fixed_metrics());
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(
            out.contains("❗ Diff incomplete — see details below"),
            "diff-incomplete header missing:\n{out}"
        );
        assert!(
            out.contains("❓ ycsb-c / chisel-strict — new scenario, no baseline"),
            "new-scenario marker missing:\n{out}"
        );
    }
```

- [ ] **Step 2: Run the tests**

```bash
cd bench && cargo test --lib diff::render::
```

Expected: 4 tests in render module pass total.

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/diff/render.rs
git commit -m "$(cat <<'EOF'
bench(diff): missing-cell and new-scenario rendering tests

Two tests cover the special-case rows in the summary table:
- baseline-only scenario (PR removed it) → ❌ row, diff-incomplete
  header takes precedence over green-check
- PR-only scenario (PR added it) → ❓ row, also flagged as
  diff-incomplete because the baseline can't be measured

Both cases share the "❗ Diff incomplete" header per spec §4.1's
priority order — missing cells take precedence over a clean
no-regression result so silent data loss isn't hidden by a green
check.
EOF
)"
```

---

## Task 9: `render.rs` — empty-both-inputs test

**Goal:** One test covering the early-return path when both baseline and PR have empty scenario maps.

**Files:**
- Modify: `bench/src/diff/render.rs`

- [ ] **Step 1: Add the test inside the existing `mod tests` block**

After the previous tests, add:

```rust
    #[test]
    fn empty_both_inputs_renders_no_scenarios_message() {
        let baseline = ParsedResults::default();
        let pr = ParsedResults::default();
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(out.starts_with("<!-- chisel-bench-diff -->\n"));
        assert!(
            out.contains("❗ No scenarios to compare — both inputs have empty scenario data"),
            "empty-inputs header missing:\n{out}"
        );
        // Footer is still present.
        assert!(out.contains("Generated by chisel-bench-diff"));
        // No summary table or per-scenario detail in the output.
        assert!(
            !out.contains("| Scenario"),
            "summary table should be absent for empty input:\n{out}"
        );
        assert!(
            !out.contains("<details>"),
            "details block should be absent for empty input:\n{out}"
        );
    }
```

- [ ] **Step 2: Run the test**

```bash
cd bench && cargo test --lib diff::render::empty_both_inputs_renders_no_scenarios_message
```

Expected: pass.

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/diff/render.rs
git commit -m "$(cat <<'EOF'
bench(diff): empty-both-inputs rendering test

Covers spec §4.1's first status-line variant: when both baseline and
PR have empty scenario maps, render the "No scenarios to compare"
header and skip the summary table and per-scenario detail entirely
(the early-return path).

Footer is still emitted so the comment retains the marker comment
and provenance info.
EOF
)"
```

---

## Task 10: `bin/diff.rs` — binary entry point

**Goal:** Implement the `chisel-bench-diff` binary using clap, calling into the library, printing to stdout.

**Files:**
- Modify: `bench/src/bin/diff.rs`

- [ ] **Step 1: Replace the stub with the real binary**

Replace `bench/src/bin/diff.rs` contents with:

```rust
// CLI entry point for chisel-bench-diff. Argv parsing + file I/O
// only; all logic lives in the chisel_bench::diff library module.
//
// Output: markdown to stdout, intended for capture by the bench
// workflow and posting as a PR comment via peter-evans/create-or-
// update-comment with the marker `<!-- chisel-bench-diff -->`.

use chisel_bench::diff::{compare, parse, render};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chisel-bench-diff", version)]
#[command(about = "Diff two results.json files and render a PR-comment markdown report")]
struct Cli {
    /// Path to the baseline results.json (typically from main).
    #[arg(long)]
    baseline: PathBuf,

    /// Path to the PR HEAD results.json.
    #[arg(long)]
    pr: PathBuf,

    /// Reserved for when micro-grid diffing is added in a future PR.
    /// Currently a no-op; included so the workflow YAML doesn't need
    /// to change when that future PR lands.
    #[arg(long, default_value_t = false)]
    #[allow(dead_code)]
    scenarios_only: bool,
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
    let baseline = parse::parse_results_json(&cli.baseline)?;
    let pr = parse::parse_results_json(&cli.pr)?;
    let report = compare::compare(
        &baseline,
        &pr,
        cli.baseline.clone(),
        cli.pr.clone(),
        chrono::Utc::now(),
    );
    let md = render::render_markdown(&report);
    print!("{md}");
    Ok(())
}
```

- [ ] **Step 2: Verify the binary builds and runs end-to-end with library-fixture data**

Quick smoke from the shell — write a minimal valid results.json to /tmp and run the binary:

```bash
cat > /tmp/empty-results.json <<'EOF'
{"metadata": {}, "cells": {}, "scenarios": {}}
EOF
cd bench && cargo run --bin chisel-bench-diff -- \
  --baseline /tmp/empty-results.json --pr /tmp/empty-results.json
```

Expected stdout starts with `<!-- chisel-bench-diff -->` and contains the empty-inputs header `❗ No scenarios to compare`.

- [ ] **Step 3: Clippy + fmt + full tests**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd bench && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add bench/src/bin/diff.rs
git commit -m "$(cat <<'EOF'
bench(diff): binary entry point for chisel-bench-diff

clap-derived CLI with --baseline, --pr, and a reserved no-op
--scenarios-only flag (spec §3.1). The binary parses both files,
calls compare() with the current UTC time as generated_at, renders
markdown, and prints to stdout.

Errors during parse or read produce exit-code 1 with stderr message;
all valid-input outcomes (regression, no regression, missing cell,
empty input) exit 0 — those are diff content, not diff failure
(spec §3.1).
EOF
)"
```

---

## Task 11: JSON test fixtures

**Goal:** Create five synthetic `results.json` fixture files under `bench/tests/fixtures/diff/`. These are consumed by the integration smoke test in Task 12 and serve as documentation of the schema the diff binary handles.

**Files:**
- Create: `bench/tests/fixtures/diff/baseline.json`
- Create: `bench/tests/fixtures/diff/pr_no_regression.json`
- Create: `bench/tests/fixtures/diff/pr_with_regression.json`
- Create: `bench/tests/fixtures/diff/pr_missing_cell.json`
- Create: `bench/tests/fixtures/diff/pr_new_scenario.json`

The fixtures use a 4-cell mini-grid (one scenario × two modes vs another scenario × two modes — keeps fixtures small and readable). Real PR 6 output would have 12 cells; for diffing logic this is irrelevant.

- [ ] **Step 1: Create `bench/tests/fixtures/diff/baseline.json`**

```json
{
  "metadata": {
    "timestamp": "2026-05-04T12:00:00Z",
    "chisel_commit": "abc123",
    "machine": {"os": "linux", "arch": "x86_64", "hostname": "ci-runner"},
    "post_processor_version": "0.1.0",
    "criterion_dir": "target/criterion",
    "aux_metrics_path": "bench/results/aux_metrics.jsonl",
    "cell_count": 0
  },
  "cells": {},
  "scenarios": {
    "ycsb-a/chisel-strict": {
      "total_wall_clock_ns": 15000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 6666.7,
      "p50_ns": 120000.0,
      "p95_ns": 180000.0,
      "p99_ns": 250000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 4194304,
      "counters": null
    },
    "ycsb-a/redb-strict": {
      "total_wall_clock_ns": 19000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 5263.2,
      "p50_ns": 145000.0,
      "p95_ns": 200000.0,
      "p99_ns": 320000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 5000000,
      "counters": null
    },
    "ycsb-b/chisel-strict": {
      "total_wall_clock_ns": 12000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 8333.3,
      "p50_ns": 100000.0,
      "p95_ns": 150000.0,
      "p99_ns": 220000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 1048576,
      "counters": null
    },
    "ycsb-b/redb-strict": {
      "total_wall_clock_ns": 14000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 7142.9,
      "p50_ns": 115000.0,
      "p95_ns": 170000.0,
      "p99_ns": 280000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 1500000,
      "counters": null
    }
  }
}
```

- [ ] **Step 2: Create `bench/tests/fixtures/diff/pr_no_regression.json`**

Identical-except-noise variant. Same schema, ±1% on values. Use baseline-times-1.005 for an example that's clearly within all thresholds:

```json
{
  "metadata": {
    "timestamp": "2026-05-04T12:30:00Z",
    "chisel_commit": "def456",
    "machine": {"os": "linux", "arch": "x86_64", "hostname": "ci-runner"},
    "post_processor_version": "0.1.0",
    "criterion_dir": "target/criterion",
    "aux_metrics_path": "bench/results/aux_metrics.jsonl",
    "cell_count": 0
  },
  "cells": {},
  "scenarios": {
    "ycsb-a/chisel-strict": {
      "total_wall_clock_ns": 15075000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 6633.4,
      "p50_ns": 120600.0,
      "p95_ns": 180900.0,
      "p99_ns": 251250.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 4194304,
      "counters": null
    },
    "ycsb-a/redb-strict": {
      "total_wall_clock_ns": 19095000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 5236.9,
      "p50_ns": 145725.0,
      "p95_ns": 201000.0,
      "p99_ns": 321600.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 5000000,
      "counters": null
    },
    "ycsb-b/chisel-strict": {
      "total_wall_clock_ns": 12060000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 8291.6,
      "p50_ns": 100500.0,
      "p95_ns": 150750.0,
      "p99_ns": 221100.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 1048576,
      "counters": null
    },
    "ycsb-b/redb-strict": {
      "total_wall_clock_ns": 14070000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 7107.2,
      "p50_ns": 115575.0,
      "p95_ns": 170850.0,
      "p99_ns": 281400.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 1500000,
      "counters": null
    }
  }
}
```

- [ ] **Step 3: Create `bench/tests/fixtures/diff/pr_with_regression.json`**

Two flagged cells: ycsb-a/chisel-strict throughput drops 10% (over the 5% threshold), ycsb-b/chisel-strict p99 rises 15% (over the 10% threshold). The other two cells stay clean:

```json
{
  "metadata": {
    "timestamp": "2026-05-04T13:00:00Z",
    "chisel_commit": "ghi789",
    "machine": {"os": "linux", "arch": "x86_64", "hostname": "ci-runner"},
    "post_processor_version": "0.1.0",
    "criterion_dir": "target/criterion",
    "aux_metrics_path": "bench/results/aux_metrics.jsonl",
    "cell_count": 0
  },
  "cells": {},
  "scenarios": {
    "ycsb-a/chisel-strict": {
      "total_wall_clock_ns": 16666666666,
      "op_count": 100000,
      "throughput_ops_per_sec": 6000.0,
      "p50_ns": 120000.0,
      "p95_ns": 180000.0,
      "p99_ns": 250000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 4194304,
      "counters": null
    },
    "ycsb-a/redb-strict": {
      "total_wall_clock_ns": 19000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 5263.2,
      "p50_ns": 145000.0,
      "p95_ns": 200000.0,
      "p99_ns": 320000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 5000000,
      "counters": null
    },
    "ycsb-b/chisel-strict": {
      "total_wall_clock_ns": 12000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 8333.3,
      "p50_ns": 100000.0,
      "p95_ns": 150000.0,
      "p99_ns": 253000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 1048576,
      "counters": null
    },
    "ycsb-b/redb-strict": {
      "total_wall_clock_ns": 14000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 7142.9,
      "p50_ns": 115000.0,
      "p95_ns": 170000.0,
      "p99_ns": 280000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 1500000,
      "counters": null
    }
  }
}
```

- [ ] **Step 4: Create `bench/tests/fixtures/diff/pr_missing_cell.json`**

Same as baseline but with `ycsb-a/chisel-strict` removed (3 cells total):

```json
{
  "metadata": {
    "timestamp": "2026-05-04T13:30:00Z",
    "chisel_commit": "jkl012",
    "machine": {"os": "linux", "arch": "x86_64", "hostname": "ci-runner"},
    "post_processor_version": "0.1.0",
    "criterion_dir": "target/criterion",
    "aux_metrics_path": "bench/results/aux_metrics.jsonl",
    "cell_count": 0
  },
  "cells": {},
  "scenarios": {
    "ycsb-a/redb-strict": {
      "total_wall_clock_ns": 19000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 5263.2,
      "p50_ns": 145000.0,
      "p95_ns": 200000.0,
      "p99_ns": 320000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 5000000,
      "counters": null
    },
    "ycsb-b/chisel-strict": {
      "total_wall_clock_ns": 12000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 8333.3,
      "p50_ns": 100000.0,
      "p95_ns": 150000.0,
      "p99_ns": 220000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 1048576,
      "counters": null
    },
    "ycsb-b/redb-strict": {
      "total_wall_clock_ns": 14000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 7142.9,
      "p50_ns": 115000.0,
      "p95_ns": 170000.0,
      "p99_ns": 280000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 1500000,
      "counters": null
    }
  }
}
```

- [ ] **Step 5: Create `bench/tests/fixtures/diff/pr_new_scenario.json`**

Same as baseline but with an extra `ycsb-c/chisel-strict` cell (5 cells total):

```json
{
  "metadata": {
    "timestamp": "2026-05-04T14:00:00Z",
    "chisel_commit": "mno345",
    "machine": {"os": "linux", "arch": "x86_64", "hostname": "ci-runner"},
    "post_processor_version": "0.1.0",
    "criterion_dir": "target/criterion",
    "aux_metrics_path": "bench/results/aux_metrics.jsonl",
    "cell_count": 0
  },
  "cells": {},
  "scenarios": {
    "ycsb-a/chisel-strict": {
      "total_wall_clock_ns": 15000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 6666.7,
      "p50_ns": 120000.0,
      "p95_ns": 180000.0,
      "p99_ns": 250000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 4194304,
      "counters": null
    },
    "ycsb-a/redb-strict": {
      "total_wall_clock_ns": 19000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 5263.2,
      "p50_ns": 145000.0,
      "p95_ns": 200000.0,
      "p99_ns": 320000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 5000000,
      "counters": null
    },
    "ycsb-b/chisel-strict": {
      "total_wall_clock_ns": 12000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 8333.3,
      "p50_ns": 100000.0,
      "p95_ns": 150000.0,
      "p99_ns": 220000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 1048576,
      "counters": null
    },
    "ycsb-b/redb-strict": {
      "total_wall_clock_ns": 14000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 7142.9,
      "p50_ns": 115000.0,
      "p95_ns": 170000.0,
      "p99_ns": 280000.0,
      "final_file_size_bytes": 110000000,
      "file_size_delta_bytes": 1500000,
      "counters": null
    },
    "ycsb-c/chisel-strict": {
      "total_wall_clock_ns": 13000000000,
      "op_count": 100000,
      "throughput_ops_per_sec": 7692.3,
      "p50_ns": 110000.0,
      "p95_ns": 160000.0,
      "p99_ns": 230000.0,
      "final_file_size_bytes": 100000000,
      "file_size_delta_bytes": 2097152,
      "counters": null
    }
  }
}
```

- [ ] **Step 6: Sanity-check the fixtures parse cleanly**

Quick shell smoke — feed each fixture into the diff binary against itself; expect exit 0 and well-formed output:

```bash
cd bench
for f in tests/fixtures/diff/baseline.json tests/fixtures/diff/pr_no_regression.json tests/fixtures/diff/pr_with_regression.json tests/fixtures/diff/pr_missing_cell.json tests/fixtures/diff/pr_new_scenario.json; do
  echo "==== $f ===="
  cargo run --quiet --bin chisel-bench-diff -- --baseline "$f" --pr "$f" | head -5
done
```

Expected: each prints `<!-- chisel-bench-diff -->` as its first line and `## 🚦 Bench results: PR vs main` shortly after. No errors.

- [ ] **Step 7: Commit**

```bash
git add bench/tests/fixtures/diff/
git commit -m "$(cat <<'EOF'
bench(diff): synthetic results.json fixtures for diff testing

Five fixtures under bench/tests/fixtures/diff/:
- baseline.json — 4-cell mini grid (ycsb-a + ycsb-b × chisel + redb)
- pr_no_regression.json — same numbers ±0.5%
- pr_with_regression.json — ycsb-a/chisel throughput -10%, ycsb-b/chisel
  p99 +15% (both flagged), other cells unchanged
- pr_missing_cell.json — 3 cells, ycsb-a/chisel removed
- pr_new_scenario.json — 5 cells, adds ycsb-c/chisel

The mini grid keeps fixtures readable; real PR 6 output has 12 cells
but the diff logic is grid-size-independent. Consumed by the
integration smoke test in the next commit.
EOF
)"
```

---

## Task 12: Integration smoke test + expected MD fixtures

**Goal:** Two `assert_cmd`-driven integration tests using the JSON fixtures. Each compares stdout against an expected markdown fixture (with the timestamp line normalized).

**Files:**
- Create: `bench/tests/fixtures/diff/expected_diff_no_regression.md`
- Create: `bench/tests/fixtures/diff/expected_diff_with_regression.md`
- Create: `bench/tests/diff_smoke.rs`

- [ ] **Step 1: Create `bench/tests/fixtures/diff/expected_diff_no_regression.md`**

This is the exact expected stdout for `chisel-bench-diff --baseline baseline.json --pr pr_no_regression.json`, with the timestamp line containing `<TIMESTAMP>` as a placeholder for normalization. Generate it by running the binary first, then capture and edit:

```bash
cd bench
cargo run --quiet --bin chisel-bench-diff -- \
  --baseline tests/fixtures/diff/baseline.json \
  --pr tests/fixtures/diff/pr_no_regression.json \
  > /tmp/expected_no_reg.md
# Edit /tmp/expected_no_reg.md to replace the timestamp with <TIMESTAMP>:
sed -i.bak 's|Generated by chisel-bench-diff at [^.]*\.|Generated by chisel-bench-diff at <TIMESTAMP>.|' /tmp/expected_no_reg.md
cp /tmp/expected_no_reg.md tests/fixtures/diff/expected_diff_no_regression.md
```

(macOS sed needs `-i.bak`; on GNU sed, plain `-i` works. The .bak file can be removed.)

Verify the file looks right (starts with marker, has ✅ status line, ends with footer including `<TIMESTAMP>`).

- [ ] **Step 2: Create `bench/tests/fixtures/diff/expected_diff_with_regression.md`**

Same approach for the regression fixture:

```bash
cd bench
cargo run --quiet --bin chisel-bench-diff -- \
  --baseline tests/fixtures/diff/baseline.json \
  --pr tests/fixtures/diff/pr_with_regression.json \
  > /tmp/expected_reg.md
sed -i.bak 's|Generated by chisel-bench-diff at [^.]*\.|Generated by chisel-bench-diff at <TIMESTAMP>.|' /tmp/expected_reg.md
cp /tmp/expected_reg.md tests/fixtures/diff/expected_diff_with_regression.md
```

Verify it starts with marker and contains `⚠️ 2 regression(s) detected across 2 scenario/mode pair(s)`.

- [ ] **Step 3: Create `bench/tests/diff_smoke.rs`**

```rust
// Integration smoke for chisel-bench-diff. Runs the binary against
// committed JSON fixtures and snapshot-compares stdout to expected
// markdown files. The "Generated by chisel-bench-diff at <ts>"
// timestamp line is normalized to a placeholder before comparison.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn normalize_timestamp(s: &str) -> String {
    // Pattern: "Generated by chisel-bench-diff at YYYY-MM-DDTHH:MM:SSZ."
    // Replace with: "Generated by chisel-bench-diff at <TIMESTAMP>."
    let re = regex::Regex::new(r"Generated by chisel-bench-diff at [^.]+\.").unwrap();
    re.replace_all(s, "Generated by chisel-bench-diff at <TIMESTAMP>.")
        .into_owned()
}

fn run_diff(baseline: &str, pr: &str) -> String {
    let output = Command::cargo_bin("chisel-bench-diff")
        .unwrap()
        .args(["--baseline", baseline, "--pr", pr])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

fn read_expected(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn no_regression_diff_matches_snapshot() {
    let actual = run_diff(
        "tests/fixtures/diff/baseline.json",
        "tests/fixtures/diff/pr_no_regression.json",
    );
    let actual_norm = normalize_timestamp(&actual);
    let expected = read_expected("tests/fixtures/diff/expected_diff_no_regression.md");
    assert_eq!(actual_norm, expected, "snapshot mismatch — see diff above");
}

#[test]
fn with_regression_diff_matches_snapshot() {
    let actual = run_diff(
        "tests/fixtures/diff/baseline.json",
        "tests/fixtures/diff/pr_with_regression.json",
    );
    let actual_norm = normalize_timestamp(&actual);
    let expected = read_expected("tests/fixtures/diff/expected_diff_with_regression.md");
    assert_eq!(actual_norm, expected, "snapshot mismatch — see diff above");
}

#[test]
fn binary_runs_against_all_fixtures_without_error() {
    // Smoke check: every fixture-against-self produces a successful
    // exit and non-empty stdout starting with the marker. Catches
    // unicode / formatting issues even when no expected snapshot
    // exists for that pair.
    let fixtures = [
        "baseline.json",
        "pr_no_regression.json",
        "pr_with_regression.json",
        "pr_missing_cell.json",
        "pr_new_scenario.json",
    ];
    for name in fixtures {
        let path_str = format!("tests/fixtures/diff/{name}");
        assert!(Path::new(&path_str).exists(), "fixture missing: {path_str}");
        let out = run_diff(&path_str, &path_str);
        assert!(
            out.starts_with("<!-- chisel-bench-diff -->\n"),
            "fixture {name} self-diff doesn't start with marker"
        );
    }
}
```

- [ ] **Step 4: Add `regex` to `[dev-dependencies]` in `bench/Cargo.toml`**

The smoke test uses `regex` for timestamp normalization. Add to the existing `[dev-dependencies]` section:

```toml
[dev-dependencies]
# ... existing dev-deps ...
regex = "1"
```

(If `regex` is already in dev-dependencies from PR 5 or earlier, skip this step.)

- [ ] **Step 5: Run the smoke tests**

```bash
cd bench && cargo test --test diff_smoke
```

Expected: all 3 smoke tests pass.

If snapshot tests fail (likely on first attempt — table formatting often surprises), inspect the diff:

```bash
cd bench
cargo run --quiet --bin chisel-bench-diff -- \
  --baseline tests/fixtures/diff/baseline.json \
  --pr tests/fixtures/diff/pr_no_regression.json \
  > /tmp/actual_no_reg.md
diff /tmp/actual_no_reg.md tests/fixtures/diff/expected_diff_no_regression.md
```

If the diff is just whitespace or a missed timestamp normalization, fix the expected fixture; if it's a renderer bug, fix `render.rs`.

- [ ] **Step 6: Run all bench tests**

```bash
cd bench && cargo test
```

Expected: all pass.

- [ ] **Step 7: Clippy + fmt**

```bash
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
```

- [ ] **Step 8: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock bench/tests/fixtures/diff/expected_diff_*.md bench/tests/diff_smoke.rs
git commit -m "$(cat <<'EOF'
bench(diff): integration smoke test + expected MD snapshots

Three integration tests in bench/tests/diff_smoke.rs:
- no-regression snapshot: baseline + pr_no_regression → expected MD
- with-regression snapshot: baseline + pr_with_regression → expected MD
- self-diff smoke for all five fixtures: confirms each parses and
  renders without error, output starts with marker

Snapshot comparison normalizes the timestamp line via regex (replaces
"Generated by chisel-bench-diff at <ts>." with "<TIMESTAMP>"). All
other characters are byte-exact matches.

Adds regex to dev-dependencies (used only by diff_smoke.rs).
EOF
)"
```

---

## Task 13: GitHub Actions workflow

**Goal:** Create `.github/workflows/bench.yml` per spec §5.1.

**Files:**
- Create: `.github/workflows/bench.yml`

- [ ] **Step 1: Create `.github/workflows/bench.yml`**

```yaml
name: Bench

# Triggers on PRs to main. Posts a regression-report comment on the PR.
# Never blocks merge — signal, not gate.
#
# NOTE on fork PRs: this workflow uses `pull_request` (not
# `pull_request_target`), so `${{ secrets.GITHUB_TOKEN }}` is read-only
# for fork PRs and the comment-post step will fail gracefully. This is
# intentional: `pull_request_target` would run untrusted PR code with
# elevated token privileges, a real security risk for a workflow that
# compiles and runs arbitrary Rust. Fork-PR comment posting is not
# supported in v1.
on:
  pull_request:
    branches: [main]

# Cancel in-flight bench runs when a new commit is pushed to the same
# PR. Bench takes ~10-25 min; stale runs aren't worth waiting for.
concurrency:
  group: bench-${{ github.event.pull_request.number }}
  cancel-in-progress: true

jobs:
  bench:
    runs-on: ubuntu-latest
    timeout-minutes: 45
    permissions:
      contents: read
      pull-requests: write   # for posting the PR comment

    steps:
      - name: Checkout PR HEAD (default workspace)
        uses: actions/checkout@v4

      - name: Checkout main (sibling directory)
        uses: actions/checkout@v4
        with:
          ref: main
          path: main-checkout

      - uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo build
        uses: Swatinem/rust-cache@v2
        with:
          workspaces: |
            bench
            main-checkout/bench

      # Build everything we need up front so build failures surface
      # before the long bench runs.
      - name: Build bench (PR)
        working-directory: bench
        run: cargo build --release --bench scenarios --bin summarize --bin chisel-bench-diff

      - name: Build bench (main)
        working-directory: main-checkout/bench
        run: cargo build --release --bench scenarios --bin summarize

      - name: Run scenarios on main
        working-directory: main-checkout/bench
        run: cargo bench --bench scenarios

      - name: Summarize main results
        working-directory: main-checkout/bench
        run: |
          cargo run --release --bin summarize -- \
            --scenarios results/scenarios_metrics.jsonl \
            --out /tmp/main-out

      - name: Run scenarios on PR
        working-directory: bench
        run: cargo bench --bench scenarios

      - name: Summarize PR results
        working-directory: bench
        run: |
          cargo run --release --bin summarize -- \
            --scenarios results/scenarios_metrics.jsonl \
            --out /tmp/pr-out

      - name: Generate diff
        working-directory: bench
        run: |
          cargo run --release --bin chisel-bench-diff -- \
            --baseline /tmp/main-out/results.json \
            --pr /tmp/pr-out/results.json \
            > /tmp/diff-comment.md
          echo "----- Diff comment preview -----"
          cat /tmp/diff-comment.md

      - name: Find existing bench comment
        uses: peter-evans/find-comment@v3
        id: fc
        with:
          issue-number: ${{ github.event.pull_request.number }}
          comment-author: 'github-actions[bot]'
          body-includes: '<!-- chisel-bench-diff -->'

      - name: Create or update PR comment
        uses: peter-evans/create-or-update-comment@v4
        with:
          comment-id: ${{ steps.fc.outputs.comment-id }}
          issue-number: ${{ github.event.pull_request.number }}
          body-path: /tmp/diff-comment.md
          edit-mode: replace
```

**Note:** The workflow invokes `--bin summarize` (matching PR 5's `[[bin]] name = "summarize"` in `bench/Cargo.toml`) and `--bin chisel-bench-diff` (matching this plan's Task 1 declaration). If you change the cargo `[[bin]] name` for the diff binary in Task 1, update this workflow accordingly.

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/bench.yml
git commit -m "$(cat <<'EOF'
ci: add bench.yml workflow for PR regression reports

Triggers on PR open/push against main, runs cargo bench --bench
scenarios on both main and PR HEAD, summarizes both, diffs them,
posts a sticky PR comment via peter-evans/create-or-update-comment.
The comment is keyed on a marker substring so subsequent pushes
update rather than append.

Workflow runs on ubuntu-latest only — Linux fsync semantics keep
chisel-strict and sqlite-strict cells comparable. macOS runners
would produce numbers useless for regression detection
(chisel uses F_FULLFSYNC, default sqlite fsync doesn't).

concurrency group with cancel-in-progress kills stale runs on
new pushes. Fork-PR comment posting is documented as unsupported
(pull_request, not pull_request_target, for security).

Workflow never blocks merge — signal, not gate.
EOF
)"
```

---

## Task 14: actionlint validation (manual, no commit)

**Goal:** Run actionlint locally before pushing, fix any warnings.

- [ ] **Step 1: Install actionlint if not already present**

```bash
# macOS:
brew install actionlint
# Linux:
# Download from https://github.com/rhysd/actionlint/releases or use go:
# go install github.com/rhysd/actionlint/cmd/actionlint@latest
```

- [ ] **Step 2: Run against the new workflow**

```bash
actionlint .github/workflows/bench.yml
```

Expected: no output (clean). Common warnings if not clean:
- "shellcheck reported issue" — shell-syntax in `run:` blocks; usually fixable inline.
- "could not find action XXX" — action versioning typo.
- "duplicate step id" — typo'd step `id`.

- [ ] **Step 3: If actionlint found issues, fix them in the workflow file and commit**

```bash
git add .github/workflows/bench.yml
git commit -m "ci: actionlint fixes for bench.yml"
```

If clean, no commit.

---

## Task 15: Pre-push verification checklist (manual, no commit)

**Goal:** Final local verification before pushing the branch and opening the PR.

- [ ] **Step 1: From repo root, run the full Rust check matrix**

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: all clean. (These cover the root `chisel` crate; the bench subcrate has its own check below.)

- [ ] **Step 2: From `bench/`, run the bench-subcrate check matrix**

```bash
cd bench
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

Expected: all clean. Test count should be roughly the pre-PR-7 baseline + ~16 new tests (3 parse + 6 compare + 5 render + 3 smoke).

- [ ] **Step 3: From `python/`, sanity-check the python subcrate hasn't been disturbed**

```bash
cd python
cargo build
```

Expected: clean. (No need to run pytest unless you're worried; PR 7 doesn't touch python.)

- [ ] **Step 4: Inspect git log to confirm no Claude-referencing trailers**

```bash
git log --oneline main..HEAD
git log main..HEAD | grep -i 'co-authored-by\|claude' | head
```

Expected: no `Co-Authored-By` lines and no Claude references in commit messages. Project convention.

- [ ] **Step 5: Inspect git diff for any accidental changes outside intended files**

```bash
git diff --stat main..HEAD
```

Expected: only `bench/Cargo.toml`, `bench/Cargo.lock`, `bench/src/diff/`, `bench/src/bin/diff.rs`, `bench/src/lib.rs`, `bench/tests/fixtures/diff/`, `bench/tests/diff_smoke.rs`, `.github/workflows/bench.yml`, `docs/superpowers/specs/2026-05-04-chisel-bench-ci-design.md`, `docs/superpowers/plans/2026-05-04-chisel-bench-ci.md` should appear. Anything else should be investigated.

---

## Task 16: PR 7 self-test on GitHub (post-push, manual, no commit)

**Goal:** Push the branch, open PR 7, observe the workflow runs against PR 7 itself.

- [ ] **Step 1: Push the branch and open PR 7**

```bash
git push -u origin claude/silly-euler-e130ad
gh pr create --title "PR 7: Bench CI integration" --body "$(cat <<'EOF'
## Summary
- Adds `chisel-bench-diff` binary that diffs two `results.json` files and emits a markdown PR comment
- Adds `.github/workflows/bench.yml` that runs the scenario tier on each PR, diffs against main, posts the comment
- Workflow is signal-only — never blocks merge

Resolves the seven open questions from `docs/superpowers/handoffs/2026-05-04-pr7-ci-integration.md`.

Spec: `docs/superpowers/specs/2026-05-04-chisel-bench-ci-design.md`
Plan: `docs/superpowers/plans/2026-05-04-chisel-bench-ci.md`

## Test plan
- [x] Unit tests for parse, compare, render (16 tests added)
- [x] Integration smoke tests against synthetic fixtures (3 tests added)
- [x] actionlint clean on bench.yml
- [ ] **PR 7 self-test:** the workflow defined in this PR runs on this PR's own pushes (GitHub Actions evaluates workflow files from the PR's branch on `pull_request` triggers). Expected outcome: green run, `✅ No regressions detected` comment with `<!-- chisel-bench-diff -->` marker.
- [ ] **Post-merge:** open a throwaway `[DO NOT MERGE]` PR with an injected `std::thread::sleep` in `PageCache::get`, observe the regression flag fires.
EOF
)"
```

- [ ] **Step 2: Watch the workflow run and verify acceptance criteria**

```bash
gh pr view --web   # opens the PR in browser
gh run list --workflow=bench.yml --limit 5   # see the latest bench runs
gh run watch <run-id>   # stream the logs of a specific run
```

Expected outcome on PR 7:
- Workflow goes green within ~25 minutes.
- A comment appears on PR 7 saying `✅ No regressions detected`.
- View the comment HTML source (right-click on GitHub UI; or use `gh pr view --json comments`); confirm `<!-- chisel-bench-diff -->` is present.
- Push a no-op commit to PR 7 (e.g., a typo fix in the spec); confirm the comment is *updated*, not duplicated.

If the workflow fails:
- **Build/clippy/fmt failure** → fix in additional commits to the PR.
- **Bench timeout** → investigate; bump `timeout-minutes` if reasonable; if scenarios are inherently slower than expected on the GitHub runner, that's a master-spec discussion (out of scope here).
- **Comment-post failure** → most likely a permissions issue; verify `permissions: pull-requests: write` is in `bench.yml`.

- [ ] **Step 3: After the self-test passes, mark PR 7 ready and merge per project convention**

The user's typical merge workflow is `superpowers:finishing-a-development-branch` option 1 (local merge). Follow that.

---

## Task 17: Post-merge deliberate-regression test (manual, no commit on this branch)

**Goal:** After PR 7 merges, validate the regression-flag path on a throwaway PR. This is a manual confidence check, not a gate on PR 7's merge.

- [ ] **Step 1: From a fresh worktree off main, create a throwaway branch**

```bash
git checkout main && git pull
git checkout -b throwaway/pr7-verify-regression
```

- [ ] **Step 2: Inject a deliberate regression**

Edit `src/page_cache.rs`. Find `PageCache::get` (the read path used by every page access). Add inside the function body:

```rust
std::thread::sleep(std::time::Duration::from_micros(100));
```

This adds ~100µs to every page read — well above the 5%-throughput / 10%-p99 thresholds for any scenario.

- [ ] **Step 3: Commit and push**

```bash
git add src/page_cache.rs
git commit -m "throwaway: inject 100µs sleep in PageCache::get"
git push -u origin throwaway/pr7-verify-regression
gh pr create --title "[DO NOT MERGE] PR 7 verification — inject regression" --body "Throwaway PR to validate that the bench workflow flags injected regressions."
```

- [ ] **Step 4: Observe the workflow run**

Expected outcome:
- Workflow goes green (workflow itself succeeds).
- Comment shows `⚠️ N regression(s) detected across M scenario/mode pair(s)` header.
- Worst-Δ column on at least one chisel-strict row shows `p99 +XX% ⚠️` or similar.
- Per-scenario detail block contains the regression breakdown.

- [ ] **Step 5: Close PR without merging; delete branch**

```bash
gh pr close --delete-branch
```

If you don't see the regression flag, the diff binary or workflow has a bug — investigate and file a follow-up.
