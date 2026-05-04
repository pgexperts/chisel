# Chisel Bench — Cross-Engine Comparison Report — Design

**Date:** 2026-05-04
**Status:** Approved; implementation plan pending.
**Scope:** Add a `cross-engine.md` artifact to the bench post-processor (PR 5's `summarize` binary) that presents headline cross-engine numbers (Chisel vs redb vs SQLite) suitable for the README and 1.0 release notes. Add the macOS-fsync fairness fix to `SqliteEngine` (`PRAGMA fullfsync=ON` on Strict mode) so SQLite's durability semantics actually match the other engines on macOS. PR 8 of the bench-suite series; final item.

This spec is an addendum to the master bench-suite design at `docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`, which sketched PR 8 only as "Produce headline cross-engine numbers as a byproduct of the above, suitable for the README and 1.0 release notes" (Goals §1) and deferred the macOS-fsync fairness fix to PR 8 once PR 6 surfaced the issue.

The handoff context: as of PR 7 + the spillway feature, the bench-suite series has produced the data-collection infrastructure (Engine trait, workloads, scenario tier) and the regression-detection workflow. PR 8 doesn't add new measurement infrastructure — it adds a presentation layer on top of the existing `scenarios_metrics.jsonl` data, plus the SQLite fairness fix.

## 1. Goals and Non-Goals

### Goals

- Land a new render module `bench/src/summary/render_cross_engine.rs` that produces a `cross-engine.md` markdown file from the existing scenarios metrics. Three per-metric tables — throughput, p99 latency, file size — with scenarios as rows and the three strict-mode engines as columns (Chisel, redb, SQLite). Absolute numbers; no ratios.
- The `summarize` binary writes `cross-engine.md` unconditionally alongside the existing `summary.md` and `results.json`. No new CLI flag, no new binary; the cross-engine view is just another render of the same data.
- Add `PRAGMA fullfsync=ON` to `SqliteEngine::open_file` for `DurabilityMode::Strict`. Always-on (no `#[cfg(target_os)]` gate) — Linux ignores it (no-op), macOS uses it to call `fcntl(F_FULLFSYNC)` matching Chisel's `sync_all`. Result: `DurabilityMode::Strict` produces semantically-equivalent durability across both platforms.
- Add `format_bytes_iec(bytes: u64) -> String` to `bench/src/summary/format.rs` — the existing `format_duration_ns` covers the µs/ms metrics; file size needs a binary-IEC formatter (KiB / MiB / GiB).
- The `cross-engine.md` artifact stands on its own as a release-notes-ready document: includes a header with metadata, brief scenario descriptions, the three tables, and a methodology footer linking to `summary.md` for full per-cell detail and to the master spec for workload definitions.

### Non-Goals (this PR)

- *Cross-engine regression detection in CI.* Option (b) from the brainstorm: extending the bench workflow to flag when Chisel falls behind redb/SQLite by more than a threshold. Out of scope; would add CI complexity for a noisy signal (different-engine numbers can drift independently due to runner variance). A future PR could add this on top of the cross-engine.md artifact PR 8 produces.
- *README integration.* PR 8 produces `cross-engine.md` as an artifact; updating README.md from it is a manual copy step. A future `--update-readme` mode could automate this with a marker-delimited section, but it's not in v1 — README updates remain a human-deliberate decision.
- *Micro-grid data in the cross-engine view.* Master spec §7.3 explicitly excludes the micro grid from CI / headline output; PR 8 honors that. cross-engine.md is scenarios-only.
- *Unsafe-durability columns.* Master spec §4 limits scenarios to strict mode; PR 8's cross-engine.md is also strict-only.
- *Per-scenario detail tables (the per-engine "full profile per scenario" view).* Already present in `summary.md`'s scenario-tier section — duplicating in `cross-engine.md` would just add bytes. cross-engine.md is per-metric only.
- *Chisel-relative ratios.* Just absolute numbers. Ratios on lower-is-better metrics (latency, file size) invert sign and create a confusing reading frame; absolute values let the reader compute their own comparison without an editorial spin layer.
- *Counters and Chisel-internal diagnostics.* `cross-engine.md` is a competitive comparison; Chisel-internal counters live in `summary.md`'s scenario appendix where they belong.
- *Auto-generated TL;DR or "verdict" column.* The numbers stand on their own. Editorializing belongs in the README/release notes a human writes by reading cross-engine.md.

## 2. Architecture & data flow

PR 8 adds one new render path on top of the existing post-processor — no changes to data collection, no changes to the discovery layer, no changes to the existing renderers.

```
bench/benches/scenarios.rs
   ↓ (writes scenarios_metrics.jsonl — unchanged from PR 6)
bench/src/summary/discover.rs::load_scenarios_jsonl
   ↓ (returns Vec<ScenarioMetrics> — unchanged from PR 6)
bench/src/bin/summarize.rs
   ├─► render_md::render_markdown            → summary.md       (unchanged)
   ├─► render_json::render_json              → results.json     (unchanged)
   └─► render_cross_engine::render_cross_engine_markdown → cross-engine.md (NEW)
```

The new render module is independent of the existing two — no shared state, no shared buffer. It reads from the same `&[ScenarioMetrics]` slice that `render_md` already consumes, so adding it requires zero changes to the discover/load path.

### 2.1 SqliteEngine fairness fix is a 3-line change

`SqliteEngine::open_file` already issues `PRAGMA cache_size = ...`, `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = FULL` (for Strict mode). PR 8 appends `PRAGMA fullfsync=ON` to the Strict-mode pragma application. Linux ignores it. On macOS, every subsequent `sync()` call inside SQLite uses `fcntl(F_FULLFSYNC)` — the same call Rust's `File::sync_all` makes, which is the same call Chisel's `PageIo::fsync` makes. Result: equivalent durability semantics across platforms for the Strict mode.

Unsafe mode is untouched. `DurabilityMode::Unsafe` is the speed-over-safety dial; pulling it back via fullfsync would defeat its purpose.

## 3. The `cross-engine.md` output format

### 3.1 Full document structure

```markdown
# Chisel Bench: Cross-engine comparison

Generated by chisel-bench-summarize at <ISO8601 UTC>.
Machine: <os> <arch> <hostname>; Chisel commit <short SHA>.

Three engines, all in their **Strict durability mode** (every commit
fsynced through the disk's write cache). On macOS, `SqliteEngine` uses
`PRAGMA fullfsync=ON` so its fsync semantics match Chisel's `sync_all`
(`fcntl(F_FULLFSYNC)`); on Linux the pragma is a no-op.

Cache size: 256 pages (2 MiB) for all three engines.

## Scenarios

- **YCSB-A** — 50/50 read/update mix, Zipfian access (θ=0.99). 100K records × 1 KiB.
- **YCSB-B** — 95/5 read-heavy, Zipfian (θ=0.99). Same dataset as YCSB-A.
- **Mutation Log** — 25/25/25/25 allocate/read/update/delete mix, uniform random
  access. 10K records, sizes uniform in [64 B, 4 KiB].
- **Document Store** — 70/20/10 read/allocate/update mix, Zipfian (θ=0.7),
  log-normal value sizes (median 4 KiB, p99 ≈ 1 MiB). 10K records.

## Throughput (ops/sec, higher is better)

| Scenario        | Chisel  | redb   | SQLite  |
| --------------- | ------- | ------ | ------- |
| ycsb-a          | 6500    | 5500   | 8000    |
| ycsb-b          | 8000    | 7000   | 10000   |
| mutation-log    | 1500    | 1800   | 4500    |
| document-store  | 3500    | 4000   | 5000    |

## p99 latency per op (lower is better)

| Scenario        | Chisel  | redb    | SQLite  |
| --------------- | ------- | ------- | ------- |
| ycsb-a          | 250 µs  | 320 µs  | 195 µs  |
| ycsb-b          | 220 µs  | 280 µs  | 180 µs  |
| mutation-log    | 1.62 ms | 31.7 ms | 454 µs  |
| document-store  | 1.75 ms | 1.82 ms | 1.20 ms |

## File size after workload (smaller is better)

| Scenario        | Chisel       | redb         | SQLite       |
| --------------- | ------------ | ------------ | ------------ |
| ycsb-a          | 100.0 MiB    | 110.0 MiB    | 95.0 MiB     |
| ycsb-b          | 100.0 MiB    | 110.0 MiB    | 95.0 MiB     |
| mutation-log    | 4.2 MiB      | 5.0 MiB      | 1.0 MiB      |
| document-store  | 60.0 MiB     | 65.0 MiB     | 55.0 MiB     |

---

Methodology: each cell is the result of a single end-to-end run of the
named scenario against the engine in strict durability mode. See
[`summary.md`](summary.md) in the same directory for the full per-cell
detail (p50, p95, total wall clock, file-size delta, and Chisel-internal
counter snapshots) and [the master bench spec](../../../docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md)
for workload definitions. Each engine takes a single fsync per commit
through the disk write cache; numbers depend on the platform's storage
stack and are not portable across machine classes.
```

### 3.2 Behavioral specifics

- **Sort order:** scenarios appear in the order returned by `bench/benches/scenarios.rs::build_scenarios` — YCSB-A, YCSB-B, Mutation Log, Document Store. Engines columns: Chisel, redb, SQLite (left to right; Chisel first because it's the subject of the comparison).
- **Throughput formatting:** integer ops/sec (`ops.round() as u64`), no suffix beyond the column header.
- **p99 latency formatting:** the existing `format_duration_ns` from `bench/src/summary/format.rs` (auto-magnitude — ns → µs → ms based on value).
- **File size formatting:** new helper `format_bytes_iec` (KiB / MiB / GiB binary suffixes; one decimal place for KiB/MiB, two for GiB). Bytes < 1024 render as `N B`.
- **Missing-cell handling:** if a scenario lacks data for one engine, the cell renders as `—`. Won't happen in v1 (all 12 cells present), but defensive against future divergence (e.g., an engine adapter not yet updated when a new scenario lands).
- **Empty-input handling:** if `scenarios` is empty (no `scenarios_metrics.jsonl`, or it's empty), write a single line `No scenario data available — run \`cargo bench --bench scenarios\` first.` instead of the empty tables.
- **Generated-at timestamp:** ISO8601 UTC, computed at render time (not stored on the input). `chrono::Utc::now()`.
- **Machine info / commit:** reused from the existing `Metadata` struct that PR 5/6 already populate.

### 3.3 Why no ratios

A natural alternative would show "engine X is N% faster/slower than Chisel" alongside each absolute value. Rejected because:

1. Lower-is-better metrics (latency, file size) invert the ratio direction. `redb p99 = 0.85x Chisel's` is "redb 15% faster." `redb file_size = 0.85x Chisel's` is "redb 15% smaller." Same ratio, different verdicts on the engines' relative quality. Confusing in a single rendering style.
2. Absolute values let the reader compute their own ratio for the comparison they care about. Forcing one comparison frame ("vs Chisel") implies Chisel is the baseline, which is editorial.
3. The numbers stand on their own. A reader scanning "Chisel 6500 vs SQLite 8000" doesn't need a parenthesized "+23%" to interpret it.
4. README/release-notes copy that DOES need ratios is one human-written sentence away — the writer reads cross-engine.md and decides which comparison frame to emphasize.

## 4. macOS-fsync fairness fix

### 4.1 The bug being fixed

PR 6's CLAUDE.md status note documented this caveat:

> On macOS that ceiling is unreachable — chisel uses Rust's `sync_all` which calls `fcntl(F_FULLFSYNC)` (durable through the disk cache), while SQLite by default uses plain `fsync()` (which on macOS only flushes to the disk's write cache without F_FULLFSYNC). Result: chisel-strict cells are fsync-bound at ~5–10 ms per commit while sqlite-strict cells run ~3 orders of magnitude faster.

This makes any macOS-local cross-engine comparison meaningless: SQLite isn't really durable in the same sense as Chisel, but the bench reports them under the same `Strict` label. The bench workflow runs Linux-only (`ubuntu-latest`), so the workflow's numbers are correct; it's only macOS dev-machine bench runs that are misleading.

### 4.2 The fix

`SqliteEngine::open_file` adds a single PRAGMA call after the existing Strict-mode `synchronous=FULL`:

```rust
let synchronous = match durability {
    DurabilityMode::Strict => "FULL",
    DurabilityMode::Unsafe => "OFF",
};
conn.execute_batch(&format!("PRAGMA synchronous = {synchronous};"))?;

// PR 8 fairness fix: on macOS, plain fsync() flushes to OS write
// buffer but not to the disk's write cache. Chisel's sync_all uses
// fcntl(F_FULLFSYNC) which is durable through the disk cache;
// without the equivalent in SQLite, sqlite-strict on macOS is
// ~3 orders of magnitude faster than chisel-strict, which is a
// measurement artifact, not a real performance difference.
// PRAGMA fullfsync=ON makes SQLite call F_FULLFSYNC on every fsync.
// Linux ignores the pragma (its fsync() already flushes through).
if matches!(durability, DurabilityMode::Strict) {
    conn.execute_batch("PRAGMA fullfsync = ON;")?;
}
```

**Always-on, no `#[cfg(target_os)]` gate.** Reasons:
- SQLite docs: `fullfsync=ON` is a no-op on Linux.
- Single-platform conditional logic is one less ifdef to maintain.
- Makes `Strict` produce semantically-equivalent durability across platforms — the whole point of the mode label.

**Strict-mode only.** `DurabilityMode::Unsafe` is the speed-over-safety dial. Adding fullfsync there would defeat its purpose; the bench needs an unsafe column precisely to see the floor of "what does this engine do without a durability barrier."

### 4.3 Retroactive impact on existing numbers

This change makes existing macOS bench numbers slower for SQLite-strict cells. Anyone running a comparison between "old SQLite numbers" and "post-PR-8 SQLite numbers" on macOS will see SQLite get correctly slower — by orders of magnitude.

The PR 7 bench workflow runs on Linux where the pragma is a no-op, so the workflow's PR-vs-main diff comment will show no change for sqlite-strict cells across the PR 8 boundary. macOS-local bench runs that compare across the boundary will see the correct-but-large delta and need to know it's the fairness fix, not a regression.

**Documentation:** the PR 8 commit message and CLAUDE.md note this explicitly so the apparent regression is identifiable as the fix.

## 5. File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/src/sqlite_engine.rs` | Modify | Append `PRAGMA fullfsync=ON` for Strict mode in `open_file`. ~6 LOC including comment. Add a unit test asserting the pragma reads back as `1` after Strict open. |
| `bench/src/summary/format.rs` | Modify | Add `pub fn format_bytes_iec(bytes: u64) -> String` — binary IEC (KiB, MiB, GiB). ~15 LOC + 3 tests. |
| `bench/src/summary/render_cross_engine.rs` | Create | New module: `pub fn render_cross_engine_markdown(scenarios: &[ScenarioMetrics], metadata: &Metadata) -> String`. Header + scenarios list + three tables + footer per §3.1. ~120 LOC + 5 tests. |
| `bench/src/summary/mod.rs` | Modify | `pub mod render_cross_engine; pub use render_cross_engine::render_cross_engine_markdown;`. ~2 LOC. |
| `bench/src/bin/summarize.rs` | Modify | After existing `render_markdown` and `render_json` writes, also call `render_cross_engine_markdown` and write `cross-engine.md` to the output directory. Print the new file path in the success output. ~10 LOC. |
| `bench/tests/summarize_smoke.rs` | Modify | Extend the existing smoke test: assert `cross-engine.md` exists in the output dir after the summarize run; assert it starts with the expected header; assert it contains all three engine column names and all four scenario names. ~15 LOC added. |

**Total new code:** ~150 LOC production + ~80 LOC tests + the existing PR 5 module structure unchanged. Roughly 230 LOC across the PR.

## 6. Testing

Three tiers, mirroring the established bench-PR pattern.

### 6.1 Unit tests (in-module)

**`render_cross_engine.rs` tests** (5 tests):
- `render_full_fixture_includes_all_tables`: 12-cell synthetic input → output starts with the expected header, contains "## Throughput", "## p99 latency per op", "## File size after workload", all four scenario names, all three engine column names.
- `render_empty_scenarios_emits_placeholder`: empty `scenarios` slice → output contains "No scenario data available", does NOT contain any table header.
- `render_single_scenario_one_row_per_table`: one scenario across all three engines → each table has exactly one data row plus the header rows.
- `render_missing_engine_renders_em_dash`: one scenario with only chisel-strict data → other engine cells render as `—`.
- `render_includes_methodology_footer_and_summary_link`: output contains `[\`summary.md\`](summary.md)` link and the spec cross-link in the footer.

**`format.rs::format_bytes_iec` tests** (3 tests):
- `format_bytes_iec_under_1k_uses_bytes`: 0 → "0 B"; 512 → "512 B"; 1023 → "1023 B".
- `format_bytes_iec_uses_binary_suffixes`: 1024 → "1.0 KiB"; 1024 * 1024 → "1.0 MiB"; 1024_u64.pow(3) → "1.00 GiB".
- `format_bytes_iec_intermediate_values`: 1500 → "1.5 KiB"; 1_500_000 → "1.4 MiB" (or whatever the rounding yields — pin the value).

**`sqlite_engine.rs` test** (1 test):
- `strict_mode_sets_fullfsync_pragma`: open in Strict mode, query `PRAGMA fullfsync`, assert returns `1`. Skipped via cfg or assertion-relaxation on Unsafe mode (which should NOT set it).

**Total in-module new tests:** ~9. Bench test count goes from 88 → ~97.

### 6.2 Integration smoke (`bench/tests/summarize_smoke.rs` extension)

The existing smoke test already runs the summarize binary against a fixture and asserts on the output directory contents. Add three assertions:
- `cross-engine.md` exists in the output dir.
- Its first line is `# Chisel Bench: Cross-engine comparison`.
- It contains all three engine column names AND all four scenario names.

No new test file needed — just extend the existing one.

### 6.3 Acceptance gate (manual, post-merge)

Three things to verify:

1. **macOS bench run shows the fairness fix took effect.** Before PR 8: `cd bench && cargo bench --bench scenarios` on macOS produces sqlite-strict throughput ~1000× higher than chisel-strict. After PR 8: sqlite-strict drops to within ~5× of chisel-strict (Chisel still likely faster on durable workloads where its commit protocol is more efficient than SQLite's WAL+checkpoint+fsync). Eyeball cross-engine.md's throughput row.

2. **Linux CI bench workflow is unchanged.** PR 7's bench workflow runs on `ubuntu-latest`. `PRAGMA fullfsync=ON` is a no-op on Linux, so the workflow's PR-vs-main diff comment on the next PR after PR 8 should show all-zero deltas (or noise-floor wobble) for sqlite-strict cells. Confirms the fix didn't break Linux numbers.

3. **`cross-engine.md` is presentable.** Open the artifact in a markdown renderer (GitHub preview, an editor) and confirm:
   - Tables align (no broken column alignment)
   - Metadata header is informative without being verbose
   - Methodology footer gives sufficient context for a release-notes audience
   - Scenario descriptions are accurate

These are manual checks, not CI gates. The only automated gate is the unit + smoke tests passing.

## 7. Hard constraints

- **No new runtime dependencies.** `format_bytes_iec` is pure stdlib. The render module uses the same `serde_json::Value` data structures the existing renderers use.
- **Always-on `PRAGMA fullfsync=ON` for Strict mode.** No `#[cfg(target_os = "macos")]` gate. Strict means strict everywhere.
- **Unsafe mode untouched.** `DurabilityMode::Unsafe` remains the speed-over-safety dial; PR 8 does not pull it back via fullfsync.
- **No micro-grid in cross-engine.md.** Master spec §7.3 excludes micro-grid from CI / headline output. PR 8 honors that — cross-engine.md is scenarios-only.
- **`cross-engine.md` is unconditional.** No CLI flag toggles it. summarize always writes all three artifacts (summary.md, results.json, cross-engine.md). Consumers who don't care about cross-engine.md just ignore the file.
- **No `Co-Authored-By` trailer; no Claude-referencing text in commits.**
- **`cargo test` from inside `bench/` clean; clippy `--all-targets -- -D warnings` clean; fmt clean.** Lessons-from-spillway-rollout entry in CLAUDE.md: per-task gates skipped `cd bench && cargo test`. PR 8's task list will explicitly run it.

## 8. Open implementation-phase questions

These are deferred to the implementation plan:

- Exact byte-format thresholds for `format_bytes_iec` (where does KiB stop and MiB start — at 1024 KiB exactly, or at some "human readable" cutoff like 999 KiB?). Plan picks the cleanest value and documents it.
- Whether the `sqlite_engine.rs` fullfsync test uses `Connection::query_row` or extracts the value via `prepare`/`query_map`. Either works; plan picks idiomatic.
- Exact wording of the methodology footer's link to the master spec — the link target will be a relative path from `bench/results/<UTC>/cross-engine.md`, which involves several `../` segments. Plan resolves the exact path.
- Whether to include the cache-size mention ("256 pages (2 MiB)") as a hardcoded string in the header or to extract it from `Metadata`. Hardcoded is simpler; extracted is forward-compatible if the bench ever varies cache size between runs. Plan picks one.

These are implementation details that don't affect the design contract.

## 9. Lessons from prior bench-suite PRs codified into this design

- **Per-task verification must include `cd bench && cargo test`.** Spillway's final review caught a bench test regression that per-task gates missed because they only ran `cargo test` from the repo root (which excludes the bench subcrate). PR 8's plan will include `cd bench && cargo test` in every commit task's pre-commit checklist.
- **Reserve "this is just a presentation tweak" for actual presentation tweaks.** Spillway's "two-fsync commit" claim turned out to be wrong about the baseline (actual is 3). PR 8's macOS fairness fix is similarly the kind of "small change" that shifts measured numbers retroactively — the commit message and CLAUDE.md will document the apparent SQLite regression on macOS so it's identifiable as the fix.
- **Standalone artifacts beat README mutations for v1.** PR 7's lesson about `pull_request` vs `pull_request_target` for fork-PR comments: keep automation outputs as artifacts, leave incorporation into human-curated docs (README) as a manual deliberate step.
