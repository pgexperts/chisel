---
id: 0010
title: Bench-suite series (cross-engine + dedicated machine)
date: 2026-04-30
status: Accepted
---

# 0010. Bench-suite series (cross-engine + dedicated machine)

**Context:** "Is Chisel fast?" is meaningless without a baseline. A storage engine's performance is dominated by storage primitives (fsync semantics, page cache behavior, allocation patterns) that vary wildly across engines. Comparing Chisel against itself across time gives regression signal but no absolute calibration. Comparing against established engines (redb, SQLite) gives absolute calibration but requires careful fairness control.

**Decision:** A `bench/` subcrate (sibling to `python/`, NOT a workspace member of the root chisel crate — separate `cd bench && cargo test` step) provides three measurement layers:

1. **Cross-engine equivalence tests** — five scenarios × three engines × snapshot/restore checks, asserting all engines produce identical observable state for the same workload.
2. **Criterion micro-grid** — 165 cells of single-tx-shape operations.
3. **YCSB-style scenario tier** — four end-to-end workloads (YCSB-A, YCSB-B, Mutation Log, Document Store) timed with `Instant::now()` rather than Criterion (Criterion's many-samples-per-bench model exceeds the 1-6 minute scenario budget).

A post-processor (`chisel-bench-summarize`) emits `summary.md`, `results.json`, and `cross-engine.md` (per-metric Chisel-vs-redb-vs-SQLite). A diff binary (`chisel-bench-diff`) consumes two `results.json` files and posts a sticky regression-report comment on each PR via `bench.yml`.

Eight PRs landed 2026-04-30 → 2026-05-04. PR 8 added the cross-engine artifact + macOS-fsync fairness fix (see ADR-11).

A future **dedicated bench machine** will host low-noise per-PR runs, canonical release-notes numbers, and (eventually) soak workloads. Foundation spec is `docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md`; code-first phases (noise-gate Rust binary + ops workflows + operator runbook) shipped 2026-05-04. Operator phases (VM provisioning, runner registration, Phase 5 `bench.yml` migration) await operator setup.

**Alternatives considered:**

- *In-tree bench.* Rejected — the bench depends on redb and rusqlite, dependencies the storage engine itself doesn't need. Sibling crate keeps the engine's dependency graph minimal.
- *Workspace member.* Tempting but `cargo test` from the repo root would auto-run bench tests, which take 10-25 minutes. Sibling-crate forces explicit `cd bench && cargo test`, which is also a documented gotcha (see ARCHITECTURE.md "Building from source").
- *Criterion for everything.* Criterion's many-samples-per-bench model is great for micro-grid but exceeds the time budget for end-to-end scenarios. Hybrid (Criterion for micro, `Instant::now()` for scenarios) is the explicit compromise.

**Consequences:**

- *Positive:* Trustworthy cross-engine numbers. `cross-engine.md` is suitable for the README and 1.0 release notes (with the noise-floor caveats from the dedicated-machine spec).
- *Positive:* Per-PR regression signal via the bench workflow's sticky diff comment.
- *Positive:* Counter instrumentation (ADR-9) enables per-cell attribution of throughput differences to fsync count, cache pressure, or page-allocation rate.
- *Negative:* Bench tests don't run from the repo root. The CLAUDE.md / build instructions document this gotcha explicitly.
- *Negative:* GitHub-hosted runner variance (~15% on the scenario tier) limits the diff binary's actionability. Solved by the dedicated-machine foundation when operator setup completes.

Master spec: `docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md` (covers PRs 1-7). PR 8 has its own spec: `docs/superpowers/specs/2026-05-04-chisel-bench-cross-engine-design.md`. Dedicated-machine: `docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md`.

---
