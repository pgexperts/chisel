# Chisel Bench — CI Integration — Design

**Date:** 2026-05-04
**Status:** Design approved; implementation plan pending.
**Scope:** Add a GitHub Actions workflow that runs the scenario tier on every PR, diffs the PR's results against `main`, and posts a sticky regression-report comment. Add a new `chisel-bench-diff` binary in `bench/` that consumes two `results.json` files and emits the markdown comment. The workflow is signal-only — it never blocks merge.

This spec follows on from `2026-04-25-chisel-benchmark-suite-design.md` (the master bench-suite design, especially §7 on output and CI integration), `2026-05-03-chisel-bench-summary-postprocessor-design.md` (PR 5 — established the `results.json` schema this spec consumes), and `2026-05-03-chisel-bench-scenario-tier-design.md` (PR 6 — established the `cargo bench --bench scenarios` invocation and `bench/results/scenarios_metrics.jsonl` output that this spec drives).

The handoff at `docs/superpowers/handoffs/2026-05-04-pr7-ci-integration.md` recorded the seven open questions that this spec resolves.

## 1. Goals and Non-Goals

### Goals

- Land a new `chisel-bench-diff` binary at `bench/src/bin/diff.rs`. CLI: two `--baseline` / `--pr` flags pointing at `results.json` files, emits a markdown report to stdout, exits 0 on success. Pure render-and-print: no GitHub API, no networking, unit-testable with synthetic JSON fixtures.
- Land a new GitHub Actions workflow at `.github/workflows/bench.yml`, triggered on `pull_request` against `main`, pinned to `ubuntu-latest`, that runs `cargo bench --bench scenarios` twice (once on `main`, once on PR HEAD), invokes `chisel-bench-diff`, and posts the result as a sticky PR comment via `peter-evans/create-or-update-comment`.
- Detect performance regressions per the master spec's "5% over baseline" rule, refined to four metrics with two thresholds: throughput and p50 at 5%, p95 and p99 at 10%. Flag worse-direction only.
- Comment is sticky: subsequent pushes update the existing comment via a marker string `<!-- chisel-bench-diff -->` rather than appending new comments.
- The workflow never blocks merge. The comment is signal, not gate.
- Lessons from PR 6 codified into the acceptance gate: PR 7 itself is the first end-to-end test case (workflows defined in a PR run on that PR's own pushes).

### Non-Goals (this PR)

- *Persistent baseline storage.* No artifact/cache of `main`'s results across PRs. Each PR pays the cost of running both sides. The master spec called this out as a "future optimization"; this PR does not bite that off. (Resolves handoff §5 question 3.)
- *Cross-engine fairness.* Chisel's `sync_all` calls `F_FULLFSYNC` on macOS while SQLite's default `fsync()` does not — but this workflow runs on Linux, where the asymmetry doesn't apply. The macOS-fairness fix (`PRAGMA fullfsync=ON` on `SqliteEngine`) is PR 8's problem, not this PR's.
- *Micro-grid runs in CI.* Master spec §7.3 explicit: only the scenario tier runs in CI.
- *Unsafe-durability columns in CI.* Master spec §7.3 same.
- *Chisel-internal counter columns in the regression diff.* Used to investigate *why* a regression happened during local triage, not to detect it. The summarize binary still includes them in `results.json`; the diff binary ignores them.
- *Fork-PR comment posting.* Documented as a known limitation in the workflow file. Using `pull_request_target` to elevate the token would compile and run untrusted PR code with write privileges — a real security risk for a workflow that executes arbitrary Rust. Same-repo PRs (the common case for Chisel today) work fine.
- *Configurable thresholds.* Threshold values live as module-level constants in `compare.rs`. Tuning them is a future change with concrete data behind it.
- *Adding clippy/fmt enforcement to CI for the `bench/` subcrate.* Currently `ci.yml`'s clippy and fmt jobs run against the root `chisel` crate only (the bench subcrate is a sibling, not a workspace member). Adding bench-subcrate lint jobs to `ci.yml` is reasonable but is scope creep for this PR. PR 7's commits will be lint-clean by author discipline; CI enforcement is a follow-up.

## 2. Architecture — file structure, dependency graph, LOC estimate

### 2.1 File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/Cargo.toml` | Modify | Add `[[bin]] name = "chisel-bench-diff"` target pointing at `src/bin/diff.rs`. No new dependencies. |
| `bench/src/bin/diff.rs` | Create | Binary entry: argv parsing (clap), file reads, call into library, print to stdout. ~50 LOC. |
| `bench/src/diff/mod.rs` | Create | Module root. Re-exports `parse`, `compare`, `render` types. ~10 LOC. |
| `bench/src/diff/parse.rs` | Create | `parse_results_json(path: &Path) -> Result<ParsedResults, ParseError>`. Produces a typed view of the `scenarios` map. ~60 LOC + tests. |
| `bench/src/diff/compare.rs` | Create | `compare(baseline: &ParsedResults, pr: &ParsedResults) -> DiffReport`. Produces `Vec<ScenarioDiff>` with per-metric deltas and flag status. ~100 LOC + tests. |
| `bench/src/diff/render.rs` | Create | `render_markdown(report: &DiffReport) -> String`. Produces the comment body. ~120 LOC + tests. |
| `bench/src/lib.rs` | Modify | `pub mod diff;`. ~1 LOC. |
| `bench/tests/diff_smoke.rs` | Create | End-to-end smoke test using `assert_cmd` against fixture files. ~50 LOC. |
| `bench/tests/fixtures/diff/baseline.json` | Create | Synthetic 12-cell results.json. |
| `bench/tests/fixtures/diff/pr_no_regression.json` | Create | Same numbers ±1%. |
| `bench/tests/fixtures/diff/pr_with_regression.json` | Create | Two flagged cells (one throughput, one p99). |
| `bench/tests/fixtures/diff/pr_missing_cell.json` | Create | 11-cell file. |
| `bench/tests/fixtures/diff/pr_new_scenario.json` | Create | 13-cell file. |
| `bench/tests/fixtures/diff/expected_diff_no_regression.md` | Create | Expected stdout for the no-regression smoke test. |
| `bench/tests/fixtures/diff/expected_diff_with_regression.md` | Create | Expected stdout for the regression smoke test. |
| `.github/workflows/bench.yml` | Create | The workflow. ~80 LOC YAML. |

**Total new code:** ~250 production Rust + ~150 test code + ~80 YAML = roughly 500 LOC across the PR. This is above the master spec's "~80 yaml" estimate because the diff binary's size wasn't sized in the master spec.

### 2.2 Module dependency graph

```
bench/src/bin/diff.rs           [binary entry — argv + stdout]
   │
   └─► bench/src/diff/mod.rs    [public API surface]
         ├─► parse.rs           parse results.json → ParsedResults
         ├─► compare.rs         (ParsedResults, ParsedResults) → DiffReport
         └─► render.rs          DiffReport → String (markdown)
```

The library/binary split mirrors PR 5's `summary` module. The library code is callable without invoking the binary harness; the binary is just argv parsing, file I/O, and stdout printing. Testing happens at the library level for all the interesting logic (parse correctness, comparison thresholds, rendering details) and at the binary level only for end-to-end smoke (does the binary actually print sensible markdown when invoked).

### 2.3 No changes to existing modules

The `summary` library, `summarize` binary, `runner.rs`, `scenarios.rs`, and all other pre-existing bench code are unmodified. PR 7 is purely additive.

The `summarize` binary's CLI already supports `--scenarios <path>` and `--out <path>` (added in PR 6), which is exactly what the workflow needs. No changes required there.

### 2.4 Dependency choices

No new runtime dependencies. The diff binary uses `serde_json` (already in deps), `clap` (already in deps for the `summarize` binary), and `chrono` (already in deps). Test code adds `assert_cmd` usage but that crate is already a dev-dependency from PR 5.

## 3. The diff binary

### 3.1 CLI

```
chisel-bench-diff --baseline <PATH> --pr <PATH> [--scenarios-only]
```

- `--baseline <PATH>`: required. Path to `results.json` from the baseline (typically `main`).
- `--pr <PATH>`: required. Path to `results.json` from the PR HEAD.
- `--scenarios-only`: optional, currently a no-op. Reserved for when micro-grid diffing is added in a future PR. Including it now means the workflow YAML doesn't need to change when that future PR lands.
- Output: markdown to stdout. No file output.
- Exit code: 0 on success, 1 only on argument parse failure, unreadable input files, or malformed JSON. Missing scenarios in the data, regressions, or empty-but-valid inputs all exit 0 — those are diff content, not diff failure.

### 3.2 Data model

In `bench/src/diff/compare.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Throughput,
    P50,
    P95,
    P99,
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
    /// Signed in the bad direction: positive = PR slower / lower throughput.
    /// None when either side is missing.
    pub delta_pct: Option<f64>,
    pub status: DeltaStatus,
}

#[derive(Debug, Clone)]
pub struct ScenarioDiff {
    pub scenario: String,
    pub mode: String,
    pub metrics: [MetricDelta; 4],
    /// The single worst regression in `metrics`, if any. Used to populate
    /// the summary table's "Worst Δ" column.
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
```

### 3.3 Sign convention

`delta_pct` is always *signed in the bad direction*: positive means PR is worse than baseline. For throughput, this means `pr < baseline` produces positive `delta_pct`; for latency percentiles, `pr > baseline` produces positive `delta_pct`. The bad-direction normalization happens at `MetricDelta` construction time, so every downstream consumer (renderer, summary table builder, sort-worst-first) uses the same `delta_pct > threshold_pct` test regardless of metric type.

### 3.4 Threshold constants

In `bench/src/diff/compare.rs`:

```rust
const THRESHOLD_PCT_THROUGHPUT: f64 = 5.0;
const THRESHOLD_PCT_P50:        f64 = 5.0;
const THRESHOLD_PCT_P95:        f64 = 10.0;
const THRESHOLD_PCT_P99:        f64 = 10.0;
```

Module-level constants, not config. Rationale:
- Tail-latency regressions (p99) are exactly the kind of thing that ships unnoticed and bites later — Chisel's commit path is fsync-bound and any new `fsync()` site shows up in tail latency before throughput. The wider threshold on p95/p99 is the noise-tolerance compromise: ~5% on tail percentiles is below the per-cell noise floor on a CI runner with the scenario tier's sample sizes.
- No absolute-time floor in v1. The scenarios all run 50K–100K ops, so per-op times are well above the noise floor where 5% on, say, 20 µs (= 1 µs absolute delta) might be ambiguous. If experience surfaces noise on the `chisel-strict` p99 column for very short scenarios, an absolute floor can be added in a follow-up.

### 3.5 No regressions in better-direction deltas

Improvements (PR is faster than baseline) are not flagged. The summary table's "Worst Δ" column shows `—` when no metric regressed, even if the PR shows large speedups. The reasoning: a regression report is for catching things that warrant attention; speedups are a known good outcome that doesn't.

### 3.6 Missing-cell handling

Per handoff §5 question 5: missing cells produce a clear failure row in the report and the binary exits 0.

- *Cell present on baseline, absent on PR* → row marked `❌ <scenario> / <mode> — missing on PR side`. Worst-Δ column shows the same status.
- *Cell present on PR, absent on baseline* (e.g., PR adds a new scenario) → row marked `❓ <scenario> / <mode> — new scenario, no baseline`. Not flagged as regression.
- *Cell absent on both sides* → not in the report at all.

A scenario removed in PR (present on baseline, absent on PR for *all* its modes) shows three `❌` rows — one per mode. This is acceptable: it surfaces the change clearly to a reviewer.

## 4. Markdown output structure

Single render path. The diff binary always emits:

```markdown
<!-- chisel-bench-diff -->
## 🚦 Bench results: PR vs main

<status line: see §4.1>

| Scenario        | Mode          | Δ throughput | Worst Δ        |
| --------------- | ------------- | ------------ | -------------- |
| <one row per scenario/mode pair, sorted per §4.2>

<details>
<summary>Per-scenario detail (4 metrics × 12 cells)</summary>

### <scenario name>
| Mode          | Throughput        | p50               | p95               | p99               |
| ------------- | ----------------- | ----------------- | ----------------- | ----------------- |
| chisel-strict | <value → value (Δ%)>  | ...               | ...               | ...               |
| <repeat per mode>

<repeat per scenario>

</details>

<sub>
Generated by chisel-bench-diff at <ISO8601 UTC timestamp>.
Compares PR HEAD against main. Never blocks merge — signal, not gate.
Thresholds: throughput 5%, p50 5%, p95 10%, p99 10%.
</sub>
```

### 4.1 Status-line variants

One of, in priority order (first matching condition wins):
- `❗ No scenarios to compare — both inputs have empty scenario data` — when both baseline and PR scenario maps are empty. (Highest priority: nothing else makes sense to render after this header; the rest of the report degenerates.)
- `❗ Diff incomplete — see details below` — when any cell has `BaselineMissing` or `PrMissing` status, regardless of regression count. (Missing cells take precedence over the regression header to avoid silent data loss being hidden by an `✅`.)
- `⚠️ N regression(s) detected across M scenario/mode pair(s)` — when `regression_count > 0`. N counts individual flagged metrics; M counts unique scenario/mode pairs containing at least one flagged metric.
- `✅ No regressions detected` — when `regression_count == 0` and no missing cells. (Lowest priority; default green case.)

### 4.2 Summary-table sort order

- If `regression_count > 0` or there are missing cells: sort worst-regression-first by `worst_regression.delta_pct` descending. Missing-cell rows sort to the top.
- Otherwise: alphabetical by `(scenario, mode)`.

### 4.3 Per-cell rendering in the detail tables

Each metric cell shows: `<baseline> → <pr> (<signed-delta>%)`, with `⚠️` appended when the metric is flagged. Time values use auto-magnitude formatting (ns → µs → ms based on magnitude); throughput is integer ops/sec. Missing-cell metrics show `—` instead of a number.

**Display sign convention** (distinct from the internal `delta_pct` convention in §3.3): the user-facing percentage uses *raw value direction*, not bad-direction-positive. For throughput, a drop from 6500 → 6300 displays as `-3.1%` (raw value decreased). For latency, a rise from 130 µs → 132 µs displays as `+1.5%` (raw value increased). This matches the reader's intuition that "− means down, + means up." The renderer flips the sign on `delta_pct` for the throughput metric only (since `delta_pct` is internally bad-direction-positive); for latency metrics, internal and display conventions match.

The auto-magnitude helper for time formatting is small enough (~15 LOC) to inline in `render.rs`; not extracted into a shared helper. PR 5's `summary/format.rs` has similar logic — duplicating ~15 LOC across two modules is preferable to taking a cross-module dependency for trivial formatting.

### 4.4 Marker comment

The literal string `<!-- chisel-bench-diff -->` is the first line of every output. Two purposes:
1. `peter-evans/find-comment` keys on this body-substring to find an existing comment to update on subsequent PR pushes (rather than appending a new comment per push).
2. Versioning signal: if PR 8+ change the comment format incompatibly, this marker can become `<!-- chisel-bench-diff v2 -->` and old comments coexist with new ones during the transition.

## 5. GitHub Actions workflow

### 5.1 File: `.github/workflows/bench.yml`

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

### 5.2 Key choices

1. **Single sequential job, not parallel-per-side.** Same runner = same hardware = same noise floor. Fairness over wall-clock. ~10–25 min total expected on the bench tier.
2. **`actions/checkout@v4` twice** with different `path:` values. Cargo manifest dirs are independent (each has its own `bench/Cargo.toml`), and `Swatinem/rust-cache@v2` `workspaces:` accepts multiple paths.
3. **`--scenarios results/scenarios_metrics.jsonl`** explicit path. Cargo runs binaries with `cwd = package root`, so `bench/results/...` (the summarize binary's CLI default) resolves to `bench/bench/results/...` from inside `bench/` — wrong. The relative path from inside the package is `results/scenarios_metrics.jsonl`.
4. **`cat` the diff before posting.** Diagnostic for the workflow log — if comment-post fails, the diff content is still captured in the run.
5. **No `if: always()` on the comment step.** If bench fails, the workflow goes red on its own. Don't try to be helpful when broken.
6. **`timeout-minutes: 45`** safety net, well above the ~25-min worst-case expected.
7. **Pinned action major versions** (`@v4`, `@v3`) per the existing convention in `ci.yml` and `wheels.yml`. SHA-pinning is a security-supply-chain decision that should be made repo-wide if at all.
8. **Two `peter-evans` actions chained.** `find-comment` locates by marker substring, `create-or-update-comment` creates or updates based on whether an ID was found. Idiomatic and well-documented; the alternative (`marocchino/sticky-pull-request-comment`) is also good but is one more third-party action to vet.

### 5.3 Why output paths use absolute `/tmp` paths

Both `--out /tmp/main-out` and `--out /tmp/pr-out` are absolute paths, isolated from the bench subcrate's tree. This means the summarize binary writes to a known, non-conflicting location regardless of which checkout invoked it. The diff binary then reads from the same `/tmp` paths.

Workflow runs are fresh runners with fresh `/tmp`, so collision with prior runs is impossible.

### 5.4 Why `cargo bench` in two separate steps rather than parallelized

Same-runner serialization (§5.2 point 1) precludes running them in parallel on the same machine. Could theoretically split into two jobs on two runners, but: (a) two-runner parallelism doubles the runner-minutes cost, (b) different hardware noise compromises the comparison, (c) requires artifact upload/download to ferry `results.json` between jobs, adding complexity. The serial cost of ~10-25 min is acceptable.

## 6. Testing strategy

Three tiers, mirroring PR 6's hard-won lesson that unit tests don't catch end-to-end issues.

### 6.1 Unit tests

In `#[cfg(test)] mod tests` blocks within each `diff/` module.

**`parse.rs` tests** (~3 tests):
- Parse a valid `results.json` fixture into `ParsedResults`.
- Parse `results.json` with `scenarios: {}` → `ParsedResults` with empty scenario map.
- Parse a malformed `results.json` (missing top-level `scenarios` key) → returns a clear `ParseError`.

**`compare.rs` tests** (~6 tests):
- Identical baseline + PR → all `MetricDelta::status == Unchanged`.
- PR throughput 10% lower (over the 5% throughput threshold) → `Regressed { pct: 10.0, threshold_pct: 5.0 }`.
- PR p99 6% higher (under the 10% p99 threshold) → `Unchanged`.
- PR p99 12% higher (over the 10% p99 threshold) → `Regressed`.
- Cell present on baseline, absent on PR → `MetricDelta::status == PrMissing` for all four metrics.
- Cell absent on baseline, present on PR → `MetricDelta::status == BaselineMissing` for all four metrics.

**`render.rs` tests** (~5 tests):
- Empty `DiffReport` (no scenarios on either side) → renders `❗ No scenarios to compare — both inputs have empty scenario data` per §4.1's first variant.
- No-regression `DiffReport` → output starts with `<!-- chisel-bench-diff -->`, contains `✅ No regressions detected`.
- Regression `DiffReport` → output contains `⚠️`, the worst-Δ column has the right value with the right emoji.
- Missing-cell `DiffReport` → output contains `❗ Diff incomplete` and the `❌ ... — missing on PR side` row.
- New-scenario `DiffReport` → output contains the `❓ ... — new scenario, no baseline` row.

**Total in-module tests:** ~14.

### 6.2 Integration smoke (`bench/tests/diff_smoke.rs`)

~2 tests using `assert_cmd`:
- Run `chisel-bench-diff --baseline fixtures/baseline.json --pr fixtures/pr_no_regression.json`. Snapshot-compare stdout against `fixtures/expected_diff_no_regression.md`.
- Same with the regression fixture.

Snapshot comparison normalizes the timestamp line (replace `Generated by chisel-bench-diff at <timestamp>` with a placeholder before comparing). All other lines are byte-exact matches.

**Test fixtures** (under `bench/tests/fixtures/diff/`):
- `baseline.json` — 12-cell synthetic results.json
- `pr_no_regression.json` — same numbers ±1%
- `pr_with_regression.json` — two flagged cells (one throughput regression, one p99 regression)
- `pr_missing_cell.json` — 11-cell file
- `pr_new_scenario.json` — 13-cell file
- `expected_diff_no_regression.md` — expected stdout
- `expected_diff_with_regression.md` — expected stdout

### 6.3 Workflow YAML lint (one-time, manual)

Run `actionlint .github/workflows/bench.yml` locally before committing. Fix any warnings. This catches typos in step `id`, action `with:` keys, action versions, and shell-syntax issues that GitHub's UI surfaces only at first PR push.

This is a one-time author check, not a CI gate — adding actionlint to ci.yml is out of scope for PR 7.

### 6.4 What's deliberately not unit-tested

- *clippy/fmt enforcement on the bench subcrate by ci.yml.* Out of scope; flagged as a follow-up.
- *Behavior under fork PRs.* Documented; not exercised. Anyone wanting to validate it would need to fork the repo, which is itself the whole point of the limitation.
- *Behavior when `cargo bench` panics partway.* The summarize binary can produce a partial `results.json`. Diff against a complete baseline would surface as missing-cell rows. Covered by the missing-cell unit tests; no separate integration test for the partial-bench-failure path.
- *SHA-pinning of third-party actions.* Out of scope; major-version pinning is the existing convention.

## 7. End-to-end acceptance gate

PR 6 taught that synthetic unit tests don't catch end-to-end issues. PR 7's acceptance gate has two checks beyond unit tests, one of which is the PR itself.

### 7.1 PR 7 is its own first test case

When PR 7 is pushed to GitHub, the `bench.yml` workflow will trigger because GitHub Actions evaluates workflow files from the PR's own branch on `pull_request` triggers. Expected outcome:

- Workflow goes green within 25 min.
- A comment appears on PR 7 with `✅ No regressions detected` (PR 7 doesn't change Chisel's commit path, so no scenario should regress).
- The comment HTML source contains the marker `<!-- chisel-bench-diff -->`.
- A second push to PR 7 *updates* the same comment (no second comment appended).

**This validates:** workflow YAML correctness, two-checkout works, summarize integration works, diff binary builds and runs in CI, comment posting works, marker-based update works.

If the workflow fails on PR 7 itself, the failure mode determines next steps:
- Build/clippy/fmt failure → fix in additional PR 7 commits.
- Bench timeout → investigate; may need to bump `timeout-minutes` or reduce scenario sizes (but the latter affects the master spec, requiring its own discussion).
- Comment-post failure → most likely a permissions issue; check `permissions: pull-requests: write`.

### 7.2 Manual regression-flag verification (post-merge)

After PR 7 merges, open a follow-up throwaway PR titled `[DO NOT MERGE] PR 7 verification — inject regression`. Add `std::thread::sleep(Duration::from_micros(100))` in a hot Chisel path (`PageCache::get` is a good candidate — every read goes through it). Push, observe:

- Comment shows `⚠️` header.
- Worst-Δ column on at least one row shows `p99 +<value>% ⚠️` or similar.
- Per-scenario detail block contains the regression breakdown.
- Close PR without merging; delete the branch.

This is a manual, post-merge step recorded as the final task of the PR 7 implementation plan. Not blocking PR 7 merge itself — the unit tests of `compare.rs` cover the regression-detection logic, so the deliberate-regression PR is a confidence check rather than a gate.

## 8. Hard constraints

Carried forward from the handoff §6 unchanged. Restated here so this spec is self-contained.

- **Runner must be Linux** (`ubuntu-latest`), not macOS. Reason: macOS's `F_FULLFSYNC` makes Chisel's commit path fsync-bound at ~5–10 ms/commit while default-`fsync()` SQLite runs ~3 orders of magnitude faster. On Linux the asymmetry is gone. CI on macOS would produce numbers useless for regression detection.
- **Never block merge** on the regression report.
- **Only the scenario tier** runs in CI.
- **Don't touch the `results.json` schema.** PR 5 consumers depend on it. The diff binary consumes the existing schema as-is.
- **No `Co-Authored-By` trailer** in commits. No Claude-referencing text in commit messages, license files, docs, etc. (project-wide convention from the user's CLAUDE.md).
- **No commits with secrets.** The new workflow YAML uses `${{ secrets.GITHUB_TOKEN }}` properly; no hard-coded tokens.

## 9. Build sequence

Single PR (PR 7 of the bench-suite series). One worktree; one branch. Implementation plan will break this into ~10–15 tasks of 50-200 LOC each, in dependency order:

1. `bench/Cargo.toml` — add `[[bin]] chisel-bench-diff` target.
2. `bench/src/diff/mod.rs` + `bench/src/lib.rs` — module skeleton.
3. `bench/src/diff/parse.rs` + tests.
4. `bench/src/diff/compare.rs` + tests.
5. `bench/src/diff/render.rs` + tests (no-regression case).
6. `bench/src/diff/render.rs` extensions (regression, missing-cell, new-scenario cases) + tests.
7. `bench/src/bin/diff.rs` — binary entry.
8. `bench/tests/fixtures/diff/*.json` — synthetic fixtures.
9. `bench/tests/fixtures/diff/expected_*.md` — expected stdout snapshots.
10. `bench/tests/diff_smoke.rs` — end-to-end smoke.
11. `.github/workflows/bench.yml` — the workflow.
12. (Manual, no commit) — actionlint the workflow.
13. (After PR push) — verify workflow runs green on PR 7 itself with the expected comment.
14. (Post-merge) — manual deliberate-regression PR for §7.2.

The implementation plan will further subdivide where appropriate. PR 6's plan had 16 tasks for ~600 LOC; PR 7 should be similar or smaller.

## 10. Open implementation-phase questions

These are deliberately deferred to the implementation plan:

- Exact `clap` derive shape vs builder pattern for the diff binary's CLI. Convention in this repo is derive (matches `summarize`); no expected change.
- Whether `assert_cmd` snapshots use `pretty_assertions` for nicer diffs on failure. Convention in PR 5's smoke test is plain `assert_eq!`; no expected change.
- Specific magnitude thresholds for the auto-magnitude time formatting helper in `render.rs`. PR 5's `summary/format.rs` has the same logic and can serve as reference.
- Exact wording of error messages from `parse.rs` (e.g., "missing top-level `scenarios` key" vs "invalid results.json format"). Just-good-enough is fine; revisit if user-visible.

These are implementation detail that does not affect the design contract. The plan resolves them.

## 11. Lessons from PR 6 codified into this design

These appear as design decisions above, but worth restating for the next session that picks this up:

1. **The acceptance gate runs end-to-end, not just unit tests.** §7.1 makes PR 7 its own test case; §7.2 mandates a deliberate-regression follow-up PR.
2. **Spec runtime estimates cite the actual CI target, not local-dev timings.** §5 budget says "~10–25 min" with explicit `ubuntu-latest` qualifier. Anyone running this on macOS will see vastly worse numbers per the macOS-fsync caveat in §8.
3. **Allocate slack in the task list for unplanned fixes.** PR 6 surfaced three latent bugs from one task. The PR 7 implementation plan should have ~15% headroom in its task count for similar surprises.
4. **No `Co-Authored-By` trailer; no Claude-referencing text.** §8 carries this forward.
