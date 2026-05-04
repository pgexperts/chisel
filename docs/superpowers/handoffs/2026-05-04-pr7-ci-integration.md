# PR 7 Handoff — CI Integration for the Bench Suite

**Date written:** 2026-05-04
**Author of handoff:** end-of-PR-6 controller session
**Audience:** the next session that picks up PR 7 (likely a fresh Claude with no
conversational memory of PR 6's execution, or a human collaborator).
**Status:** PR 6 is merged to `main`; PR 7 has not been brainstormed, specced,
or planned yet. This doc gives the next session enough context to start.

---

## 1. State of the bench suite as of 2026-05-04

Landed on `main` (in chronological order):

| PR | Description | Landed |
|----|-------------|--------|
| 1  | Counter instrumentation (`Chisel::counters()`) | 2026-04-30 |
| 2  | `bench/` subcrate + `Engine` trait + `ChiselEngine` | 2026-04-30 |
| 3  | `RedbEngine`, `SqliteEngine`, cross-engine equivalence tests | 2026-04-30 |
| 4a | Workload data layer (`Operation`, `Workload`, six seeded generators) | 2026-04-30 |
| 4b | `Runner` machinery + 6-row Criterion micro grid | 2026-05-01 |
| 5  | Markdown summary post-processor (`bench/src/bin/summarize.rs`) | 2026-05-03 |
| 6  | Scenario tier (4 YCSB-style workloads × 3 strict modes = 12 cells) | 2026-05-03 |

Pending (in order):
- **PR 7 — CI integration.** This handoff.
- **PR 8 — Cross-engine relative-performance tests.** Addendum to the master
  spec; needs its own brainstorm + spec + plan. Out of scope for PR 7.

---

## 2. What PR 7 is supposed to do

From the master spec (`docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`)
§7.2:

- New file: `.github/workflows/bench.yml`. Triggers on `pull_request` against
  `main`. Pinned to `ubuntu-latest`.
- Workflow runs the **scenario tier only** (12 cells), not the micro grid.
- Two-checkout strategy: build & run scenarios on `main`, then on the PR head,
  diff the two `results.json` files, post a PR comment.
- The comment flags regressions > 5% with a warning emoji but **never blocks
  merge**. Signal, not gate.
- The diff machinery is a small Rust binary in `bench/` (similar shape to the
  PR 5 `summarize` binary).

The master spec's PR 7 section is short (~30 lines). Most of the design is
left for PR 7's own spec/plan to settle.

---

## 3. What PR 7 has to work with (inputs from prior PRs)

- **`cargo bench --bench scenarios`** produces
  `bench/results/scenarios_metrics.jsonl` (12 lines, one JSON object per cell).
  Schema is the `ScenarioResult` struct in `bench/src/runner.rs`.
- **`cargo run --bin summarize`** post-processes the JSONL into
  `bench/results/<UTC-ISO8601>/results.json`. The `scenarios` key in
  `results.json` is keyed by `<scenario>/<mode>` and each entry contains
  throughput, p50/p95/p99 ns, total wall-clock, and (for chisel-strict)
  internal counter deltas. This is the artifact PR 7 should diff.
- **`results.json` schema** is stable as of PR 6: top-level `cells`,
  `scenarios`, `metadata`. The diff binary should accept two files of this
  shape.
- The summarize binary's CLI accepts `--scenarios <path>` so a CI job can
  point at any scenario JSONL location.

---

## 4. Files / docs the next session should read first

Recommended order (the "fast onboarding" path):

1. **This handoff** (`docs/superpowers/handoffs/2026-05-04-pr7-ci-integration.md`).
2. **Master spec § 7** for PR 7's broad strokes
   (`docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`).
3. **`CLAUDE.md`** at repo root — has the latest bench-suite status notes
   and the macOS-fsync caveat (§ 6 below).
4. **PR 6's spec + plan** at
   `docs/superpowers/specs/2026-05-03-chisel-bench-scenario-tier-design.md`
   and `docs/superpowers/plans/2026-05-03-chisel-bench-scenario-tier.md`.
   The shape of those documents is a good template for PR 7's.
5. **Existing workflows** in `.github/workflows/` — the project already has
   `ci.yml` (Rust + Python matrix) and `wheels.yml` (abi3 wheels). The bench
   workflow should be a new sibling, not a modification of either.
6. **`bench/src/summary/render_json.rs`** — to understand the `results.json`
   shape that PR 7's diff binary will consume.

A claude-bot picking this up cold should also skim
`bench/benches/scenarios.rs` and `bench/src/runner.rs::run_scenario_cell` to
see how a scenario cell is structured end-to-end. It's about 250 LOC total.

---

## 5. Open questions for the brainstorm

These need user input. Don't pick defaults silently.

1. **Where does the diff binary live?** Options: a new `bench/src/bin/diff.rs`
   binary, or extend `summarize` with a `--baseline` flag. The latter has
   appeal (one tool, one CLI) but the use cases differ (summarize is
   single-input-render-pretty; diff is two-input-compare-and-flag).
2. **Threshold for the "regression" warning.** Spec says > 5%. Confirm whether
   that's per-percentile (p50, p95, p99 each independently) or only on a
   single primary metric (throughput?). Confirm whether tail-latency widening
   should also trigger (e.g., p99 went up > 10% even though p50 is flat).
3. **Cache `main`'s baseline?** The spec calls out the doubled runtime as a
   v1 acceptable cost but flags caching as a "future optimization." Worth
   asking the user up front whether to bite that off in PR 7 or defer.
4. **PR comment format.** Markdown table, fixed-width code block, collapsible
   `<details>` section? What does the user want to read? Recommendation:
   start with a fixed-width table; collapse the per-scenario detail behind
   `<details>` so the comment doesn't overwhelm the PR view.
5. **What counts as a "no result" cell in the diff?** If a scenario fails to
   produce a JSONL line on either main or PR side, how should the diff be
   reported? Probably: print the failure, not a percentage.
6. **GitHub Actions auth for posting comments.** Plain `${{ github.token }}`
   is fine for same-repo PRs; needs explicit thought for fork PRs (where the
   token is read-only). Recommendation: just-document the limitation, don't
   try to solve it in v1.
7. **Concurrency control.** Should multiple PR pushes cancel an in-flight
   bench run? Recommendation: yes, use a `concurrency:` group keyed on PR
   number with `cancel-in-progress: true`. Bench runs are too long to leave
   stale ones running.

---

## 6. Hard constraints / non-negotiables

- **Runner must be Linux** (`ubuntu-latest`), not macOS. Reason in § 7.
- **Never block merge** on the regression report. Master spec § 7.2 is
  explicit. The comment is signal, not gate.
- **Only the scenario tier** runs in CI. Master spec § 7.3 is explicit.
- **Don't touch the `results.json` schema** without understanding that PR 5
  consumers depend on it. If the diff tool needs additional fields, add them
  rather than restructuring existing keys.
- **No `Co-Authored-By` trailer** in commits. No Claude-referencing text in
  commit messages, license files, docs, etc. (project-wide convention from
  the user's CLAUDE.md).
- **`cargo clippy --all-targets -- -D warnings` clean** at every commit.
  `cargo fmt -- --check` clean. CI's existing `ci.yml` enforces this; PR 7
  must not regress those.
- **No commits with secrets.** The new workflow YAML must use
  `${{ secrets.* }}` references properly; never hard-code tokens.

---

## 7. The macOS-fsync caveat (CRITICAL for runner choice)

Discovered during PR 6 verification. Read carefully.

- Chisel's commit path uses Rust's `std::fs::File::sync_all`, which on macOS
  calls `fcntl(F_FULLFSYNC)`. That flag is durable through the disk's write
  cache — slow but correct.
- macOS's plain `fsync()` only flushes to the OS write buffer; it doesn't
  force the disk's own write cache to flush. SQLite by default uses plain
  `fsync()` (it flips to F_FULLFSYNC only with `PRAGMA fullfsync=ON`).
- Result: on macOS, chisel-strict cells run fsync-bound at ~5–10 ms per
  commit while sqlite-strict cells run ~3 orders of magnitude faster. This
  is **not** a SQLite-is-faster result; it's a measurement artifact.
- On Linux, `fsync()` and `F_FULLFSYNC`-equivalent are the same call. All
  three engines pay similar fsync cost. The 12-cell grid runs in ~2-12
  minutes per the spec's budget.

**Implication for PR 7:** the workflow MUST run on Linux. macOS runners
would produce numbers that are useless for regression detection. Don't be
tempted by mac runners "because the dev does work on macOS" — they're a
measurement trap.

**Implication for PR 8 (not for PR 7):** the cross-engine fairness fix
(adding `PRAGMA fullfsync=ON` to `SqliteEngine` for macOS) is PR 8's
problem. PR 7 doesn't need to touch it.

---

## 8. Recommended workflow

Mirror what PR 6 did. The user has explicit memory authorization for
subagent dispatch on Chisel plan execution
(`feedback_chisel_subagent_dispatch.md` in user memory) — no need to ask
"may I dispatch?" each time.

1. **Brainstorm** (interactive with user). Use
   `superpowers:brainstorming-design-decisions`. Settle § 5's open questions
   plus anything that surfaces.
2. **Spec** under `docs/superpowers/specs/2026-05-XX-chisel-bench-ci-design.md`.
   Use PR 6's spec (`2026-05-03-chisel-bench-scenario-tier-design.md`) as
   structural template.
3. **Plan** under `docs/superpowers/plans/2026-05-XX-chisel-bench-ci.md`.
   Tasks should be 50-200 LOC each; bigger tasks should be split. PR 6's
   plan (16 tasks) is a reasonable scale.
4. **Worktree** via `superpowers:using-git-worktrees`. The user's
   convention is `.worktrees/<branch-name>` at repo root. Branch name
   like `bench-ci-integration`.
5. **Execute** via `superpowers:subagent-driven-development`. The user
   prefers this over `executing-plans` (same-session subagents instead of
   parallel-session handoff).
6. **Final acceptance** task: actually trigger the workflow on a test PR
   in the GitHub UI before declaring victory. PR 6 taught that synthetic
   unit tests don't catch end-to-end issues.
7. **Finish** via `superpowers:finishing-a-development-branch`. User's
   typical choice is option 1 (merge locally).
8. **Update CLAUDE.md** to note PR 7 landing.

---

## 9. Lessons from PR 6 worth remembering

These are the structural lessons, not specific to PR 7's content. Internalize
them before starting:

1. **End-to-end runs catch what unit tests miss.** PR 6's three latent bugs
   (prepop fsync budget, mutation-log live-set, summarize empty-cells) all
   passed every unit test but failed at the actual bench invocation. PR 7's
   acceptance gate must include a real workflow run on a real PR (a test PR
   created for that purpose), not just YAML lint.
2. **Spec/plan budget estimates can be off by orders of magnitude on macOS.**
   PR 6's spec said "1-6 min target / 10 min ceiling." Reality on macOS APFS:
   70-90 min. The mistake was not modeling F_FULLFSYNC's per-commit cost.
   For PR 7, runtime estimates should explicitly cite Linux (the actual CI
   target), not local-dev-machine timings.
3. **Three small bugs surfaced from one Task 16.** Plan-vs-actual ratio in
   PR 6 was ~15 planned commits : 3 unplanned fix commits. Allocate slack
   in PR 7's task list for the same possibility — don't assume your plan
   nails it.
4. **Don't dispatch the same subagent task twice when rate-limited or
   blocked.** Either provide more context, switch model tier, or break the
   task down. The PR 6 controller hit one rate-limit on a trivial Cargo.toml
   edit and just did it inline rather than waiting.
5. **`cargo bench` on macOS during interactive sessions is brutal.** A full
   scenario run is ~70-90 min wall-time. If verification needs `cargo bench`
   to complete inline, schedule a Monitor on cell-completion events and use
   the controller's idle time for other prep work — don't sleep-loop.

---

## 10. Things explicitly out of scope for PR 7

- Cross-engine fairness fix (`PRAGMA fullfsync=ON` for `SqliteEngine` on
  macOS). That's PR 8.
- Persistent baseline storage (e.g., a separate ref or artifact for `main`'s
  cached baseline). Master spec calls this out as a future optimization.
- Micro-grid runs in CI. Master spec § 7.3 explicitly excludes them.
- Unsafe-durability columns in CI. Same exclusion.
- Chisel-internal counter deltas in the regression diff (used to investigate
  *why* a regression happened, not to detect it). Same.
- Re-enabling the three dropped 1000-per-tx micro-grid rows
  (`update_random_1000_per_tx`, etc.). Different PR; needs a configurable
  larger cache.

---

## 11. Quick-start checklist for the next session

```
[ ] Read this handoff in full
[ ] Read master spec § 7 (CI Integration)
[ ] Read CLAUDE.md (latest bench-suite status + macOS-fsync caveat)
[ ] Skim PR 6's spec + plan as templates
[ ] Skim bench/src/summary/render_json.rs for the results.json schema
[ ] Brainstorm with user: the 7 open questions in § 5
[ ] Write spec → user reviews → write plan → user approves
[ ] Create worktree, dispatch subagents, verify with a real test PR
[ ] Finish branch (option 1 — local merge), update CLAUDE.md
```

---

## 12. Contact / origin

This handoff was written at the close of PR 6's session. The PR 6 commits
in chronological order on `main` are:
- `0141a47` bench: batch prepop into chunked transactions in run_scenario_cell
- `e34cd77` bench: make mutation-log generator state-aware
- `02e40cf` bench: let summarize run when only scenarios are present
- `3e5d4ec` CLAUDE.md: note PR 6 (scenario tier) landing + macOS fsync caveat

The transcript of PR 6's execution is at
`/Users/xof/.claude/projects/-Users-xof-Documents-Dev-chisel/`
(JSONL, latest file). It's there if a future session needs to reconstruct
why a particular decision was made — but it shouldn't need to. This handoff
plus the spec/plan/CLAUDE.md should be sufficient.
