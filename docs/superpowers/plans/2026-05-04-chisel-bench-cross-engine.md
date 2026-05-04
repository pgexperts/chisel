# Bench Cross-Engine Comparison Report Implementation Plan (PR 8)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the `cross-engine.md` artifact (a per-metric Chisel-vs-redb-vs-SQLite comparison rendered by the `summarize` binary alongside `summary.md` and `results.json`) plus the macOS-fsync fairness fix on `SqliteEngine` (`PRAGMA fullfsync=ON` for Strict mode, always-on).

**Architecture:** New render module at `bench/src/summary/render_cross_engine.rs` reuses the existing `ScenarioMetrics` and `Metadata` data structures — no new data-loading path. The `summarize` binary calls one more render function and writes one more file. The fairness fix adds a single `execute_batch` call to `SqliteEngine::open_file` for the Strict branch. Roughly 230 LOC total.

**Tech Stack:** Rust 2021. Pure stdlib for `format_bytes_iec`. Reuses existing `chrono`, `serde_json`, `rusqlite` deps. No new dependencies.

**Spec:** [`docs/superpowers/specs/2026-05-04-chisel-bench-cross-engine-design.md`](docs/superpowers/specs/2026-05-04-chisel-bench-cross-engine-design.md)

**Pre-commit checklist (every commit task must pass these):**
- From repo root: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt -- --check`
- From `bench/`: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check` *(spillway-rollout lesson #1 captured in CLAUDE.md: per-task gates must run `cd bench && cargo test`)*

**Worktree:** `claude/bench-cross-engine` branch (already created; spec committed at `082de92`). Implementation continues in the main repo working tree directly — no separate worktree needed for a 6-task PR.

**Project conventions:**
- No `Co-Authored-By` trailer in commits.
- No Claude-referencing text in commit messages.
- Heredoc commit messages use `<<'EOF'` (single-quoted).

---

## Task 1: Add `format_bytes_iec` helper to `format.rs`

**Goal:** Add a binary-IEC byte formatter (KiB/MiB/GiB) for unsigned values with no sign prefix. Distinct from the existing `format_bytes(bytes: i64)` which is signed and uses 1024-base "KB/MB/GB" labels — the cross-engine.md tables need positive-only values with proper IEC labels.

**Files:**
- Modify: `bench/src/summary/format.rs` (add helper + 3 tests in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Read the existing format.rs to understand the test pattern**

```bash
sed -n '/^#\[cfg(test)\]/,$p' bench/src/summary/format.rs | head -30
```

The file already has tests for `format_duration_ns`, `format_bytes` (signed), and `parse_size_to_bytes`. The new helper follows the same shape.

- [ ] **Step 2: Add the helper function**

In `bench/src/summary/format.rs`, after the existing `format_bytes` function (search for `pub fn format_bytes(`), add:

```rust
/// Format a byte count using binary IEC suffixes (B / KiB / MiB / GiB).
/// Positive values only; no sign prefix. Distinct from `format_bytes`
/// which signs the output and uses 1024-base "KB/MB/GB" labels — the
/// cross-engine comparison tables (PR 8) need IEC labels for technical
/// clarity ("MiB" is unambiguously 2^20, "MB" can mean 10^6 in some
/// contexts).
///
/// Boundary convention: switch to the next-larger unit at exactly 1024
/// of the smaller unit. So 1023 B → "1023 B", 1024 B → "1.0 KiB".
/// One decimal for KiB and MiB; two decimals for GiB (since GiB values
/// are typically 2-3 digits and the extra precision matters more).
pub fn format_bytes_iec(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes < KIB {
        format!("{} B", bytes)
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    }
}
```

- [ ] **Step 3: Add tests**

In the same file's `#[cfg(test)] mod tests` block, after the existing `parse_size_to_bytes_unknown_label_returns_none` test, add:

```rust
    #[test]
    fn format_bytes_iec_under_1k_uses_bytes() {
        assert_eq!(format_bytes_iec(0), "0 B");
        assert_eq!(format_bytes_iec(1), "1 B");
        assert_eq!(format_bytes_iec(512), "512 B");
        assert_eq!(format_bytes_iec(1023), "1023 B");
    }

    #[test]
    fn format_bytes_iec_uses_binary_suffixes_at_each_boundary() {
        // Exact boundaries — 1024 of the smaller unit becomes 1.0 of larger.
        assert_eq!(format_bytes_iec(1024), "1.0 KiB");
        assert_eq!(format_bytes_iec(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes_iec(1024_u64.pow(3)), "1.00 GiB");
    }

    #[test]
    fn format_bytes_iec_intermediate_values_round_correctly() {
        // 1.5 KiB
        assert_eq!(format_bytes_iec(1536), "1.5 KiB");
        // 100 MiB exactly
        assert_eq!(format_bytes_iec(100 * 1024 * 1024), "100.0 MiB");
        // 4.2 MiB approximately
        assert_eq!(format_bytes_iec(4_404_019), "4.2 MiB");
        // 8 GiB
        assert_eq!(format_bytes_iec(8 * 1024_u64.pow(3)), "8.00 GiB");
    }
```

- [ ] **Step 4: Verify**

```bash
cd bench && cargo test --lib summary::format::
cd bench && cargo test
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd /Users/xof/Documents/Dev/chisel && cargo test
cd /Users/xof/Documents/Dev/chisel && cargo clippy -- -D warnings
cd /Users/xof/Documents/Dev/chisel && cargo fmt -- --check
```

Expected: 3 new tests pass; total bench lib test count up by 3 from baseline.

- [ ] **Step 5: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel
git add bench/src/summary/format.rs
git commit -m "$(cat <<'EOF'
bench: add format_bytes_iec for binary-IEC byte formatting

Adds pub fn format_bytes_iec(bytes: u64) -> String to the summary
format helpers. Positive-only, no sign prefix, binary IEC suffixes
(B / KiB / MiB / GiB). Distinct from the existing format_bytes
which signs the output and uses 1024-base "KB/MB/GB" labels.

PR 8's cross-engine.md uses this helper for the file-size column;
IEC labels remove the MB/MiB ambiguity that "MB" carries in some
contexts. Boundary at exactly 1024 of the smaller unit. One decimal
for KiB and MiB; two decimals for GiB.

Three tests cover: under-1K bytes path, exact boundaries (1024,
1024 KiB, 1024 MiB), and intermediate values (1.5 KiB, ~4.2 MiB).
EOF
)"
```

---

## Task 2: SqliteEngine fairness fix — `PRAGMA fullfsync=ON` for Strict

**Goal:** Add `PRAGMA fullfsync=ON` to `SqliteEngine::open_file` for `DurabilityMode::Strict`. Always-on (no `#[cfg(target_os)]` gate); Linux ignores it, macOS uses it to call `fcntl(F_FULLFSYNC)`. After this fix, `Strict` mode produces semantically-equivalent durability across both platforms.

**Files:**
- Modify: `bench/src/sqlite_engine.rs` (add 4 lines + 1 test)

- [ ] **Step 1: Locate the existing pragma site**

```bash
grep -nE "PRAGMA synchronous|durability|Strict" bench/src/sqlite_engine.rs | head -10
```

The relevant lines in `open_file` are around lines 44-48:

```rust
let synchronous = match durability {
    DurabilityMode::Strict => "FULL",
    DurabilityMode::Unsafe => "OFF",
};
conn.execute_batch(&format!("PRAGMA synchronous = {synchronous};"))?;
```

- [ ] **Step 2: Add the fairness-fix pragma**

In `bench/src/sqlite_engine.rs`, immediately after the `conn.execute_batch(&format!("PRAGMA synchronous = {synchronous};"))?;` line (the one identified in Step 1), insert:

```rust
        // PR 8 fairness fix: on macOS, plain fsync() flushes to OS write
        // buffer but not to the disk's write cache. Chisel's sync_all uses
        // fcntl(F_FULLFSYNC) which is durable through the disk cache;
        // without the equivalent in SQLite, sqlite-strict on macOS is
        // ~3 orders of magnitude faster than chisel-strict, which is a
        // measurement artifact, not a real performance difference.
        // PRAGMA fullfsync=ON makes SQLite call F_FULLFSYNC on every
        // sync. Linux ignores the pragma (its fsync() already flushes
        // through). Strict mode only — Unsafe is the speed-over-safety
        // dial; pulling it back via fullfsync would defeat its purpose.
        if matches!(durability, DurabilityMode::Strict) {
            conn.execute_batch("PRAGMA fullfsync = ON;")?;
        }
```

- [ ] **Step 3: Add a unit test**

At the bottom of `bench/src/sqlite_engine.rs`, find the existing `#[cfg(test)] mod tests` block (or create one if it doesn't exist — search with `grep -n "cfg(test)" bench/src/sqlite_engine.rs`).

If there's no test module yet, add at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn strict_mode_sets_fullfsync_pragma() {
        let tmp = NamedTempFile::new().unwrap();
        let engine = SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Strict).unwrap();
        // Query the pragma value back. SQLite returns it as an integer:
        // 1 = ON, 0 = OFF.
        let value: i64 = engine
            .conn
            .query_row("PRAGMA fullfsync;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 1, "Strict mode must enable fullfsync");
    }

    #[test]
    fn unsafe_mode_does_not_set_fullfsync_pragma() {
        let tmp = NamedTempFile::new().unwrap();
        let engine = SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Unsafe).unwrap();
        let value: i64 = engine
            .conn
            .query_row("PRAGMA fullfsync;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 0, "Unsafe mode must NOT enable fullfsync");
    }
}
```

If a test module already exists, append the two test functions inside it.

The tests access `engine.conn` directly — verify the field is accessible from a `#[cfg(test)] mod tests` in the same file (it is, since tests can see private fields of the enclosing module). If the test module is in a separate file, you'll need a `pub(crate) fn conn(&self) -> &Connection` accessor.

- [ ] **Step 4: Verify**

```bash
cd bench && cargo test --lib sqlite_engine::
cd bench && cargo test
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd /Users/xof/Documents/Dev/chisel && cargo test
cd /Users/xof/Documents/Dev/chisel && cargo clippy -- -D warnings
cd /Users/xof/Documents/Dev/chisel && cargo fmt -- --check
```

Expected: 2 new tests pass.

NOTE: this test only verifies the pragma is **set**, not that the kernel actually does `F_FULLFSYNC`. The latter is platform-dependent and untestable from Rust. Acceptance gate #1 in the spec eyeballs the macOS bench output to confirm SQLite-strict slowed down by ~1000× (which is the durability semantics manifesting through the wall-clock cost of the extra disk flush).

- [ ] **Step 5: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel
git add bench/src/sqlite_engine.rs
git commit -m "$(cat <<'EOF'
bench: SqliteEngine fairness fix — PRAGMA fullfsync=ON for Strict mode

Without this, on macOS SQLite's plain fsync() only flushes to the
OS write buffer, not the disk's write cache. Chisel's sync_all uses
fcntl(F_FULLFSYNC) which is durable through the disk cache; without
the equivalent in SQLite, sqlite-strict on macOS reports throughput
~3 orders of magnitude higher than chisel-strict. That's a
measurement artifact, not a real performance difference.

PRAGMA fullfsync=ON makes SQLite call F_FULLFSYNC on every sync.
Linux ignores the pragma (its fsync() already flushes through), so
this is always-on without a #[cfg(target_os)] gate. Strict mode
only — Unsafe is the speed-over-safety dial; pulling it back via
fullfsync would defeat its purpose.

Two tests assert the pragma reads back as 1 in Strict and 0 in
Unsafe. The tests confirm the pragma is set; they cannot test that
the kernel actually does F_FULLFSYNC (platform-dependent, untestable
from Rust). The acceptance check is eyeballing macOS bench numbers:
sqlite-strict throughput should drop ~1000x after this fix.

Retroactive impact: existing macOS bench numbers for sqlite-strict
will be much slower after this PR. PR 7's bench workflow runs on
Linux where the pragma is a no-op, so the workflow's PR-vs-main
diff comment will show no change for sqlite-strict cells across
the PR 8 boundary. macOS-local bench runs that compare across the
boundary will see the correct-but-large delta — that's the fix
working, not a regression.
EOF
)"
```

---

## Task 3: Create `render_cross_engine.rs` module + happy-path tests

**Goal:** Create the new render module with `render_cross_engine_markdown(scenarios: &[ScenarioMetrics], metadata: &Metadata) -> String`. Includes the document header, scenarios description list, three per-metric tables (throughput, p99, file size), and methodology footer. Tests cover the happy path (12-cell input renders all expected sections) plus edge cases (empty input, missing-engine cells).

**Files:**
- Create: `bench/src/summary/render_cross_engine.rs`

- [ ] **Step 1: Create the file with the full implementation**

Create `bench/src/summary/render_cross_engine.rs` with the following content:

```rust
// Cross-engine comparison report renderer (PR 8). Produces a markdown
// document suitable for the README and 1.0 release notes. Three tables:
// throughput, p99 latency, file size — scenarios as rows, the three
// strict-mode engines as columns. Absolute numbers; no ratios.
//
// Reads the same Vec<ScenarioMetrics> the existing render_md consumes,
// so adding this renderer requires zero changes to the discover/load
// path. PR 5/6's data structures are unchanged.

use crate::summary::discover::ScenarioMetrics;
use crate::summary::format::{format_bytes_iec, format_duration_ns};
use crate::summary::metadata::Metadata;

/// Render the cross-engine comparison markdown document. See spec
/// `docs/superpowers/specs/2026-05-04-chisel-bench-cross-engine-design.md`
/// for the document structure.
///
/// `scenarios` is filtered for the three strict-mode engines:
/// chisel-strict, redb-strict, sqlite-strict. Other modes (e.g.
/// future *-unsafe variants if they're ever added to the scenario
/// tier) are ignored — the cross-engine report is strict-only per
/// the master spec.
pub fn render_cross_engine_markdown(
    scenarios: &[ScenarioMetrics],
    metadata: &Metadata,
) -> String {
    let mut out = String::new();
    out.push_str("# Chisel Bench: Cross-engine comparison\n\n");
    out.push_str(&render_header(metadata));
    out.push_str("\n");

    // Empty-input early exit per spec §3.2.
    if scenarios.is_empty() {
        out.push_str(
            "No scenario data available — run `cargo bench --bench scenarios` first.\n",
        );
        return out;
    }

    out.push_str(&render_scenarios_section());
    out.push_str("\n");
    out.push_str(&render_throughput_table(scenarios));
    out.push_str("\n");
    out.push_str(&render_p99_table(scenarios));
    out.push_str("\n");
    out.push_str(&render_file_size_table(scenarios));
    out.push_str("\n");
    out.push_str(&render_methodology_footer());
    out
}

fn render_header(metadata: &Metadata) -> String {
    format!(
        "Generated by chisel-bench-summarize at {}.\n\
         Machine: {} {} {}; Chisel commit {}.\n\
         \n\
         Three engines, all in their **Strict durability mode** (every commit\n\
         fsynced through the disk's write cache). On macOS, `SqliteEngine` uses\n\
         `PRAGMA fullfsync=ON` so its fsync semantics match Chisel's `sync_all`\n\
         (`fcntl(F_FULLFSYNC)`); on Linux the pragma is a no-op.\n\
         \n\
         Cache size: 256 pages (2 MiB) for all three engines.\n",
        metadata.timestamp,
        metadata.machine.os,
        metadata.machine.arch,
        metadata.machine.hostname,
        metadata.chisel_commit,
    )
}

fn render_scenarios_section() -> &'static str {
    "## Scenarios\n\
     \n\
     - **YCSB-A** — 50/50 read/update mix, Zipfian access (θ=0.99). 100K records × 1 KiB.\n\
     - **YCSB-B** — 95/5 read-heavy, Zipfian (θ=0.99). Same dataset as YCSB-A.\n\
     - **Mutation Log** — 25/25/25/25 allocate/read/update/delete mix, uniform random\n  \
       access. 10K records, sizes uniform in [64 B, 4 KiB].\n\
     - **Document Store** — 70/20/10 read/allocate/update mix, Zipfian (θ=0.7),\n  \
       log-normal value sizes (median 4 KiB, p99 ≈ 1 MiB). 10K records.\n"
}

/// Scenarios in the master-spec / build_scenarios order.
const SCENARIO_ORDER: &[&str] = &["ycsb-a", "ycsb-b", "mutation-log", "document-store"];

/// Engine modes, left-to-right in the table columns. Chisel first
/// because it's the subject of the comparison.
const ENGINE_MODES: &[&str] = &["chisel-strict", "redb-strict", "sqlite-strict"];

/// Table column headers (display labels) for the three engines.
const ENGINE_LABELS: &[&str] = &["Chisel", "redb", "SQLite"];

fn render_throughput_table(scenarios: &[ScenarioMetrics]) -> String {
    let mut out = String::new();
    out.push_str("## Throughput (ops/sec, higher is better)\n\n");
    out.push_str("| Scenario        | Chisel  | redb    | SQLite  |\n");
    out.push_str("| --------------- | ------- | ------- | ------- |\n");
    for &scenario in SCENARIO_ORDER {
        out.push_str(&format!("| {:<15} ", scenario));
        for &mode in ENGINE_MODES {
            let cell = lookup_scenario(scenarios, scenario, mode)
                .map(|m| format!("{:>7}", m.throughput_ops_per_sec.round() as u64))
                .unwrap_or_else(|| format!("{:>7}", "—"));
            out.push_str(&format!("| {} ", cell));
        }
        out.push_str("|\n");
    }
    out
}

fn render_p99_table(scenarios: &[ScenarioMetrics]) -> String {
    let mut out = String::new();
    out.push_str("## p99 latency per op (lower is better)\n\n");
    out.push_str("| Scenario        | Chisel    | redb      | SQLite    |\n");
    out.push_str("| --------------- | --------- | --------- | --------- |\n");
    for &scenario in SCENARIO_ORDER {
        out.push_str(&format!("| {:<15} ", scenario));
        for &mode in ENGINE_MODES {
            let cell = lookup_scenario(scenarios, scenario, mode)
                .map(|m| format_duration_ns(m.p99_ns))
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!("| {:<9} ", cell));
        }
        out.push_str("|\n");
    }
    out
}

fn render_file_size_table(scenarios: &[ScenarioMetrics]) -> String {
    let mut out = String::new();
    out.push_str("## File size after workload (smaller is better)\n\n");
    out.push_str("| Scenario        | Chisel       | redb         | SQLite       |\n");
    out.push_str("| --------------- | ------------ | ------------ | ------------ |\n");
    for &scenario in SCENARIO_ORDER {
        out.push_str(&format!("| {:<15} ", scenario));
        for &mode in ENGINE_MODES {
            let cell = lookup_scenario(scenarios, scenario, mode)
                .map(|m| format_bytes_iec(m.final_file_size_bytes))
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!("| {:<12} ", cell));
        }
        out.push_str("|\n");
    }
    out
}

fn render_methodology_footer() -> &'static str {
    // Path from bench/results/<UTC>/cross-engine.md to the master spec
    // is three hops: out of UTC dir → bench/results → bench/ → repo root,
    // then docs/superpowers/specs/...
    "---\n\
     \n\
     Methodology: each cell is the result of a single end-to-end run of the\n\
     named scenario against the engine in strict durability mode. See\n\
     [`summary.md`](summary.md) in the same directory for the full per-cell\n\
     detail (p50, p95, total wall clock, file-size delta, and Chisel-internal\n\
     counter snapshots) and [the master bench spec](../../../docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md)\n\
     for workload definitions. Each engine takes a single fsync per commit\n\
     through the disk write cache; numbers depend on the platform's storage\n\
     stack and are not portable across machine classes.\n"
}

fn lookup_scenario<'a>(
    scenarios: &'a [ScenarioMetrics],
    scenario_name: &str,
    mode: &str,
) -> Option<&'a ScenarioMetrics> {
    scenarios
        .iter()
        .find(|s| s.scenario == scenario_name && s.mode == mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ChiselCountersDelta;
    use crate::summary::metadata::MachineInfo;

    fn fixture_metadata() -> Metadata {
        Metadata {
            timestamp: "2026-05-04T12:00:00Z".to_string(),
            chisel_commit: "abc123de".to_string(),
            machine: MachineInfo {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                hostname: "test-host".to_string(),
            },
            post_processor_version: "0.1.0",
            criterion_dir: "target/criterion".to_string(),
            aux_metrics_path: "bench/results/aux_metrics.jsonl".to_string(),
            cell_count: 0,
        }
    }

    fn fixture_scenario(scenario: &str, mode: &str, throughput: f64, p99_ns: f64, size: u64) -> ScenarioMetrics {
        ScenarioMetrics {
            scenario: scenario.to_string(),
            mode: mode.to_string(),
            total_wall_clock_ns: 15_000_000_000,
            op_count: 100_000,
            throughput_ops_per_sec: throughput,
            p50_ns: 100_000.0,
            p95_ns: 200_000.0,
            p99_ns,
            final_file_size_bytes: size,
            file_size_delta_bytes: 0,
            counters: if mode == "chisel-strict" {
                Some(ChiselCountersDelta {
                    cache_hits: 0,
                    cache_misses: 0,
                    fsync_calls: 0,
                    pages_allocated: 0,
                })
            } else {
                None
            },
        }
    }

    fn full_fixture() -> Vec<ScenarioMetrics> {
        let mut v = Vec::new();
        for scenario in SCENARIO_ORDER {
            for (mode, throughput) in [
                ("chisel-strict", 6500.0),
                ("redb-strict", 5500.0),
                ("sqlite-strict", 8000.0),
            ] {
                v.push(fixture_scenario(scenario, mode, throughput, 250_000.0, 100 * 1024 * 1024));
            }
        }
        v
    }

    #[test]
    fn render_full_fixture_includes_all_tables() {
        let scenarios = full_fixture();
        let out = render_cross_engine_markdown(&scenarios, &fixture_metadata());

        assert!(out.starts_with("# Chisel Bench: Cross-engine comparison\n"),
            "missing top-level header:\n{out}");
        assert!(out.contains("## Throughput (ops/sec, higher is better)"),
            "missing throughput section:\n{out}");
        assert!(out.contains("## p99 latency per op (lower is better)"),
            "missing p99 section:\n{out}");
        assert!(out.contains("## File size after workload (smaller is better)"),
            "missing file size section:\n{out}");

        // All four scenarios appear.
        for scenario in SCENARIO_ORDER {
            assert!(out.contains(scenario), "missing scenario {scenario}:\n{out}");
        }
        // All three engine column labels appear.
        for label in ENGINE_LABELS {
            assert!(out.contains(label), "missing engine {label}:\n{out}");
        }
    }

    #[test]
    fn render_empty_scenarios_emits_placeholder() {
        let out = render_cross_engine_markdown(&[], &fixture_metadata());
        assert!(out.contains("No scenario data available"),
            "empty input should emit placeholder:\n{out}");
        // Tables MUST NOT appear.
        assert!(!out.contains("## Throughput"),
            "empty input should not emit throughput table:\n{out}");
        assert!(!out.contains("## p99 latency"),
            "empty input should not emit p99 table:\n{out}");
        assert!(!out.contains("## File size"),
            "empty input should not emit file size table:\n{out}");
    }

    #[test]
    fn render_single_scenario_one_row_per_table() {
        let scenarios = vec![
            fixture_scenario("ycsb-a", "chisel-strict", 6500.0, 250_000.0, 100 * 1024 * 1024),
            fixture_scenario("ycsb-a", "redb-strict", 5500.0, 320_000.0, 110 * 1024 * 1024),
            fixture_scenario("ycsb-a", "sqlite-strict", 8000.0, 195_000.0, 95 * 1024 * 1024),
        ];
        let out = render_cross_engine_markdown(&scenarios, &fixture_metadata());

        // ycsb-a row appears in each table — count occurrences after the
        // table separator pattern.
        let ycsb_a_data_rows = out.matches("| ycsb-a").count();
        assert_eq!(ycsb_a_data_rows, 3, "expected 3 data rows (one per table) for ycsb-a:\n{out}");
        // Other scenarios still appear in column headers / leftmost cells
        // but as "—" — actually they appear as rows because all 4 scenarios
        // are always rendered. Verify the missing scenarios show em-dash.
        assert!(out.contains("ycsb-b"), "ycsb-b row should still appear:\n{out}");
        assert!(out.contains("—"), "missing data should show em-dash:\n{out}");
    }

    #[test]
    fn render_missing_engine_renders_em_dash() {
        // Only chisel-strict data for ycsb-a; redb and sqlite cells must be em-dash.
        let scenarios = vec![
            fixture_scenario("ycsb-a", "chisel-strict", 6500.0, 250_000.0, 100 * 1024 * 1024),
        ];
        let out = render_cross_engine_markdown(&scenarios, &fixture_metadata());
        // Chisel's value should be present in throughput.
        assert!(out.contains("6500"), "chisel throughput value missing:\n{out}");
        // The em-dash must appear (other engines).
        assert!(out.contains("—"), "missing-engine cells should render em-dash:\n{out}");
    }

    #[test]
    fn render_includes_methodology_footer_and_summary_link() {
        let scenarios = full_fixture();
        let out = render_cross_engine_markdown(&scenarios, &fixture_metadata());
        assert!(out.contains("Methodology:"), "methodology footer missing:\n{out}");
        assert!(out.contains("[`summary.md`](summary.md)"),
            "summary.md cross-link missing:\n{out}");
        assert!(out.contains("master bench spec"),
            "master spec link missing:\n{out}");
        assert!(out.contains("../../../docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md"),
            "master spec relative path wrong:\n{out}");
    }
}
```

- [ ] **Step 2: Verify the file compiles standalone**

Even though the new module isn't yet exported (Task 4 wires it into mod.rs), `cargo check` should still see it via the file-discovery — actually no, Rust requires explicit `mod` declarations. Add the module declaration now to keep the build green:

In `bench/src/summary/mod.rs`, find the existing module declarations (around line 17):

```rust
pub mod discover;
pub mod format;
pub mod metadata;
pub mod render_json;
pub mod render_md;
```

Add (alphabetically):

```rust
pub mod discover;
pub mod format;
pub mod metadata;
pub mod render_cross_engine;
pub mod render_json;
pub mod render_md;
```

Don't add the `pub use` re-export yet — that comes in Task 4 when we wire summarize.rs.

- [ ] **Step 3: Verify**

```bash
cd bench && cargo build
cd bench && cargo test --lib summary::render_cross_engine::
cd bench && cargo test
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd /Users/xof/Documents/Dev/chisel && cargo test
cd /Users/xof/Documents/Dev/chisel && cargo clippy -- -D warnings
cd /Users/xof/Documents/Dev/chisel && cargo fmt -- --check
```

Expected: 5 new tests pass.

- [ ] **Step 4: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel
git add bench/src/summary/mod.rs bench/src/summary/render_cross_engine.rs
git commit -m "$(cat <<'EOF'
bench: add render_cross_engine module for PR 8 comparison report

New module bench/src/summary/render_cross_engine.rs produces the
cross-engine.md markdown document — three per-metric tables
(throughput, p99 latency, file size) showing Chisel vs redb vs
SQLite for the four scenario tier workloads. Absolute numbers; no
ratios. Header includes generation timestamp, machine info, and
the strict-mode + macOS-fullfsync explanation; footer links to
summary.md and the master bench spec.

Reuses the existing ScenarioMetrics and Metadata data structures
that PR 5/6 already populate — no changes to the discover/load path.
The module is self-contained: 5 tests cover the happy path, empty
input (placeholder rather than empty tables), single-scenario
input (other scenarios render as em-dash rows), missing-engine
cells (em-dash placeholder), and the methodology footer / summary.md
cross-link / master spec relative path.

mod.rs declares the new module; the wire-up to summarize binary is
deferred to Task 4 of the plan to keep this commit focused on the
render module itself.
EOF
)"
```

---

## Task 4: Wire `cross-engine.md` into the `summarize` binary

**Goal:** Add the cross-engine render call + write to the summarize binary so every bench post-processing run produces all three artifacts (summary.md, results.json, cross-engine.md). Add the new file path to the success-message console output.

**Files:**
- Modify: `bench/src/summary/mod.rs` (add `pub use`)
- Modify: `bench/src/bin/summarize.rs` (add render call + write + console line)

- [ ] **Step 1: Add the public re-export to mod.rs**

In `bench/src/summary/mod.rs`, find the existing `pub use` block:

```rust
pub use render_json::render_json;
pub use render_md::render_markdown;
```

Add the new re-export alphabetically:

```rust
pub use render_cross_engine::render_cross_engine_markdown;
pub use render_json::render_json;
pub use render_md::render_markdown;
```

- [ ] **Step 2: Wire into summarize.rs**

In `bench/src/bin/summarize.rs`, find the imports at the top (search for `use chisel_bench::summary`):

```rust
use chisel_bench::summary::{
    copy_raw_archive, discover_cells, gather_metadata, load_scenarios_jsonl, render_json,
    render_markdown, DiscoverError,
};
```

Add `render_cross_engine_markdown` to the import list:

```rust
use chisel_bench::summary::{
    copy_raw_archive, discover_cells, gather_metadata, load_scenarios_jsonl,
    render_cross_engine_markdown, render_json, render_markdown, DiscoverError,
};
```

Then find the existing render section (around the bottom of the `run` function, after `let json = render_json(...)`):

```rust
    // 4. Render markdown + JSON.
    let md = render_markdown(&cells, &scenarios, &metadata);
    let json = render_json(&cells, &scenarios, &metadata);

    // 5. Write output artifacts.
    std::fs::write(out_dir.join("summary.md"), &md)?;
    std::fs::write(
        out_dir.join("results.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    copy_raw_archive(&cli.criterion, &out_dir.join("raw"))?;
```

Update both blocks. The render block becomes:

```rust
    // 4. Render markdown + JSON + cross-engine.
    let md = render_markdown(&cells, &scenarios, &metadata);
    let json = render_json(&cells, &scenarios, &metadata);
    let cross_engine_md = render_cross_engine_markdown(&scenarios, &metadata);
```

The write block becomes:

```rust
    // 5. Write output artifacts.
    std::fs::write(out_dir.join("summary.md"), &md)?;
    std::fs::write(
        out_dir.join("results.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    std::fs::write(out_dir.join("cross-engine.md"), &cross_engine_md)?;
    copy_raw_archive(&cli.criterion, &out_dir.join("raw"))?;
```

Then find the success-message section (after `copy_raw_archive`):

```rust
    println!(
        "  - summary.md  ({} bytes)",
        std::fs::metadata(out_dir.join("summary.md"))?.len()
    );
    println!(
        "  - results.json ({} bytes)",
        std::fs::metadata(out_dir.join("results.json"))?.len()
    );
    println!("  - raw/ (Criterion estimates.json + sample.json archive)");
```

Add a line for cross-engine.md between `results.json` and `raw/`:

```rust
    println!(
        "  - summary.md  ({} bytes)",
        std::fs::metadata(out_dir.join("summary.md"))?.len()
    );
    println!(
        "  - results.json ({} bytes)",
        std::fs::metadata(out_dir.join("results.json"))?.len()
    );
    println!(
        "  - cross-engine.md ({} bytes)",
        std::fs::metadata(out_dir.join("cross-engine.md"))?.len()
    );
    println!("  - raw/ (Criterion estimates.json + sample.json archive)");
```

- [ ] **Step 3: Verify**

```bash
cd bench && cargo build --bin summarize
cd bench && cargo test
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd /Users/xof/Documents/Dev/chisel && cargo test
cd /Users/xof/Documents/Dev/chisel && cargo clippy -- -D warnings
cd /Users/xof/Documents/Dev/chisel && cargo fmt -- --check
```

Expected: clean build, all existing tests still pass.

- [ ] **Step 4: Spot-check by running the binary against the existing scenario fixture**

If `bench/tests/fixtures/scenarios_metrics.jsonl` exists from PR 6, you can do a smoke run:

```bash
cd bench
cargo run --bin summarize -- \
    --scenarios tests/fixtures/scenarios_metrics.jsonl \
    --criterion target/criterion \
    --out /tmp/cross-engine-smoke
```

Expected output ends with three "- summary.md / results.json / cross-engine.md / raw/" lines. Then `cat /tmp/cross-engine-smoke/cross-engine.md | head -30` should show the document with header + scenarios section + start of the throughput table.

If the fixture doesn't exist, this manual check is optional — Task 5's smoke test exercises the same path.

- [ ] **Step 5: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel
git add bench/src/summary/mod.rs bench/src/bin/summarize.rs
git commit -m "$(cat <<'EOF'
bench: summarize binary writes cross-engine.md alongside summary.md

After this commit, every cargo run --bin summarize produces three
markdown / json artifacts in the output directory:

  - summary.md         — per-cell diagnostic detail (PR 5)
  - results.json       — flat composite-key schema for the CI diff (PR 5)
  - cross-engine.md    — Chisel vs redb vs SQLite headline numbers (PR 8)

cross-engine.md is unconditional — no CLI flag toggles it.
Consumers who don't care about it just ignore the file.

Updates the success-message console output to list cross-engine.md
between results.json and raw/. mod.rs gains the pub use re-export
for render_cross_engine_markdown.
EOF
)"
```

---

## Task 5: Extend `summarize_smoke.rs` with cross-engine.md assertions

**Goal:** The existing integration smoke test (`bench/tests/summarize_smoke.rs`) already runs the summarize binary against the scenarios fixture and asserts on the output directory contents. Add three assertions confirming cross-engine.md exists, has the expected first line, and contains the engine column names + scenario names.

**Files:**
- Modify: `bench/tests/summarize_smoke.rs`

- [ ] **Step 1: Read the existing smoke test to find the assertion site**

```bash
cat bench/tests/summarize_smoke.rs | head -100
```

The existing test runs the binary, then asserts the output directory contains `summary.md`, `results.json`, and `raw/`. Find the block that does this — typically a series of `assert!(out_dir.join("...").exists())` calls.

- [ ] **Step 2: Add cross-engine.md assertions**

Append the following assertions immediately after the existing `summary.md` / `results.json` / `raw/` assertions in the smoke test body (the function name is likely `summarize_writes_expected_artifacts` or similar — match whatever's there):

```rust
    // PR 8: cross-engine.md is unconditionally produced alongside the others.
    let cross_engine_path = out_dir.join("cross-engine.md");
    assert!(
        cross_engine_path.exists(),
        "cross-engine.md should be written to {}",
        out_dir.display()
    );
    let cross_engine_content = std::fs::read_to_string(&cross_engine_path).unwrap();
    assert!(
        cross_engine_content.starts_with("# Chisel Bench: Cross-engine comparison\n"),
        "cross-engine.md first line wrong:\n{cross_engine_content}"
    );
    // All three engine column labels and all four scenario names should appear.
    for label in ["Chisel", "redb", "SQLite"] {
        assert!(
            cross_engine_content.contains(label),
            "cross-engine.md missing engine label {label}"
        );
    }
    for scenario in ["ycsb-a", "ycsb-b", "mutation-log", "document-store"] {
        assert!(
            cross_engine_content.contains(scenario),
            "cross-engine.md missing scenario {scenario}"
        );
    }
```

If the fixture used by the smoke test only has a subset of scenarios (e.g., only ycsb-a), the per-scenario `contains` assertions still pass because every scenario name appears in the rows of the table (cells are em-dash for missing ones, but the row label is present). Re-read the fixture content if you're not sure:

```bash
cat bench/tests/fixtures/scenarios_metrics.jsonl 2>/dev/null | head -5
```

If the assertion fails because the fixture lacks the engine columns or the scenario rows, adjust by relaxing the per-engine / per-scenario expectations to "at least one engine label" / "at least one scenario name." But the rendered output ALWAYS includes all four scenario row labels (the `SCENARIO_ORDER` constant is iterated unconditionally), so the strict assertion should pass regardless of fixture coverage.

- [ ] **Step 3: Verify**

```bash
cd bench && cargo test --test summarize_smoke
cd bench && cargo test
cd bench && cargo clippy --all-targets -- -D warnings
cd bench && cargo fmt -- --check
cd /Users/xof/Documents/Dev/chisel && cargo test
cd /Users/xof/Documents/Dev/chisel && cargo clippy -- -D warnings
cd /Users/xof/Documents/Dev/chisel && cargo fmt -- --check
```

Expected: smoke test passes with the new assertions.

- [ ] **Step 4: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel
git add bench/tests/summarize_smoke.rs
git commit -m "$(cat <<'EOF'
bench: extend summarize smoke test with cross-engine.md assertions

The existing summarize_smoke test runs the binary end-to-end against
a scenarios fixture and asserts on the output directory contents.
Three new assertions:

1. cross-engine.md exists in the output dir.
2. Its first line is "# Chisel Bench: Cross-engine comparison".
3. It contains all three engine column labels (Chisel / redb /
   SQLite) and all four scenario names (ycsb-a / ycsb-b /
   mutation-log / document-store).

Every scenario name appears in the rendered output regardless of
fixture coverage because SCENARIO_ORDER iterates unconditionally —
missing-cell rows render as em-dash but the row label is present.
This makes the per-scenario assertions stable against fixture
content drift.
EOF
)"
```

---

## Task 6: Pre-merge verification + push + open PR

**Goal:** Run the full check matrix on the merged branch, push, open PR. Per-task and final reviews are done; this is the one-time pre-push gate.

- [ ] **Step 1: Full check matrix from repo root**

```bash
cd /Users/xof/Documents/Dev/chisel
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: clean. Test count up by 10 from the spillway-rollout baseline (3 format_bytes_iec + 2 sqlite fullfsync + 5 render_cross_engine; the smoke-test extension reuses one existing test).

- [ ] **Step 2: From `bench/`**

```bash
cd /Users/xof/Documents/Dev/chisel/bench
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

This is the per-task lesson from spillway: `cd bench && cargo test` MUST be in the pre-push gate. Bench is a sibling crate, not a workspace member, so root-level `cargo test` doesn't include it.

- [ ] **Step 3: Confirm no Claude trailers in commit log**

```bash
cd /Users/xof/Documents/Dev/chisel
git log --oneline main..HEAD
git log main..HEAD | grep -iE "co-authored-by|claude" | head || echo "clean"
```

Expected: "clean" (the only matches should be the literal `CLAUDE.md` filename in commit message bodies, not Claude attribution).

- [ ] **Step 4: Confirm scope**

```bash
cd /Users/xof/Documents/Dev/chisel
git diff --stat main..HEAD
```

Expected: only files under `bench/src/summary/`, `bench/src/sqlite_engine.rs`, `bench/src/bin/summarize.rs`, `bench/tests/summarize_smoke.rs`, and `docs/superpowers/specs/`/`docs/superpowers/plans/`. No other files.

- [ ] **Step 5: Push and open PR**

```bash
cd /Users/xof/Documents/Dev/chisel
git push -u origin claude/bench-cross-engine
gh pr create --title "Bench cross-engine comparison report + macOS fsync fairness fix" --body "$(cat <<'EOF'
## Summary

PR 8 of the bench-suite series — the final item.

- Adds `cross-engine.md` artifact: a per-metric Chisel vs redb vs SQLite comparison rendered by `summarize` alongside `summary.md` and `results.json`. Three tables (throughput, p99 latency, file size); absolute numbers; no ratios. Suitable for the README and 1.0 release notes.
- Adds the macOS-fsync fairness fix: `SqliteEngine::open_file` sets `PRAGMA fullfsync=ON` for `DurabilityMode::Strict`, always-on (no `#[cfg(target_os)]` gate). Linux ignores it; macOS uses `fcntl(F_FULLFSYNC)` matching Chisel's `sync_all`. After this fix, `Strict` is semantically equivalent durability across platforms.

Resolves the seven brainstorm questions from `docs/superpowers/specs/2026-05-04-chisel-bench-cross-engine-design.md`.

## Test plan
- [x] 3 new unit tests in `format.rs` for `format_bytes_iec` (under-1K bytes, exact boundaries, intermediate values)
- [x] 2 new unit tests in `sqlite_engine.rs` for `PRAGMA fullfsync` (Strict sets it, Unsafe doesn't)
- [x] 5 new unit tests in `render_cross_engine.rs` (full fixture, empty input → placeholder, single scenario, missing engine → em-dash, methodology footer + links)
- [x] Existing `summarize_smoke.rs` extended with 3 assertions (cross-engine.md exists, expected first line, expected engine + scenario labels)
- [ ] **Manual acceptance gate (post-merge):** macOS bench run shows sqlite-strict throughput dropped ~1000× to within ~5× of chisel-strict (the fairness fix took effect at the kernel level)
- [ ] **Manual acceptance gate (next PR):** the PR 7 bench workflow's PR-vs-main diff comment shows no change for sqlite-strict cells (Linux ignores the pragma)
- [ ] **Manual acceptance gate (one-time):** open `cross-engine.md` in a markdown renderer; tables align, header is informative, methodology footer is sufficient

Spec: `docs/superpowers/specs/2026-05-04-chisel-bench-cross-engine-design.md`
Plan: `docs/superpowers/plans/2026-05-04-chisel-bench-cross-engine.md`
EOF
)"
```

The PR 7 bench workflow will run on this PR. Expected: green, with no significant cross-engine.md regressions on Linux (the fairness fix is a no-op there).

---

## Self-review checklist

Run after writing all tasks. Fix issues inline; no need to re-review.

1. **Spec coverage:**
   - §1 Goals (cross-engine.md, fairness fix, format_bytes_iec, summarize wiring): Tasks 1, 2, 3, 4
   - §1 Non-Goals (no CI regression detection, no README integration, no micro-grid, no unsafe, no per-scenario tables, no ratios, no counters): all enforced by the render module's narrow design (it doesn't even take cells/microgrid as input — it consumes scenarios only)
   - §2 Architecture (new render path on existing data): Task 3 (module) + Task 4 (binary wire-up)
   - §3 Output format (header, scenarios section, three tables, footer, formatting specifics, missing-cell handling, empty-input handling): Task 3
   - §4 SqliteEngine fairness fix (PRAGMA fullfsync=ON, always-on, Strict-only): Task 2
   - §5 File structure: matches the task list exactly
   - §6 Testing (in-module unit tests + smoke extension + manual acceptance gates): Tasks 1+2+3 (unit) + 5 (smoke) + 6 (manual gates documented in PR description)
   - §7 Hard constraints (no new deps, always-on pragma, Unsafe untouched, no micro-grid, unconditional cross-engine.md, no Claude trailers, all checks clean): all enforced by task contents
   - §8 Open implementation-phase questions (byte format thresholds, sqlite test query style, methodology link path, hardcoded vs Metadata cache size): Task 1 picks 1024-base + decimals; Task 2 uses query_row idiom; Task 3 hardcodes the 3-hop relative path with a comment explaining the count; Task 3 hardcodes "256 pages (2 MiB)" in the header (forward-compat note: extract from Metadata if cache size becomes variable)

2. **Placeholder scan:** searched for TBD/TODO/FIXME — none found except a self-referential mention in this checklist.

3. **Type consistency:**
   - `format_bytes_iec(bytes: u64) -> String` (Task 1) — used in Task 3's file-size renderer
   - `render_cross_engine_markdown(scenarios: &[ScenarioMetrics], metadata: &Metadata) -> String` (Task 3) — used in Task 4's summarize.rs wiring
   - `ScenarioMetrics` struct fields — `scenario`, `mode`, `throughput_ops_per_sec`, `p99_ns`, `final_file_size_bytes` — all confirmed present in the existing `bench/src/summary/discover.rs::ScenarioMetrics`
   - `Metadata` struct — `timestamp`, `chisel_commit`, `machine` (with `os`/`arch`/`hostname`) — confirmed present in the existing `bench/src/summary/metadata.rs`
   - `SCENARIO_ORDER` and `ENGINE_MODES` constants are private to the render module, used only there
   - `DurabilityMode::Strict` / `::Unsafe` (Task 2) — existing public API

4. **Integration tests not blocked:** the smoke test extension (Task 5) doesn't need new fixtures — the existing scenarios_metrics.jsonl fixture covers the assertions because all four scenario rows are rendered unconditionally regardless of fixture coverage.

---

That's the plan. Tasks 1-5 are commit-producing; Task 6 is the pre-merge gate + push + PR creation. Total estimated production code: ~150 LOC + ~80 LOC of unit + smoke tests = ~230 LOC across the PR.
