---
id: 0024
title: Make the bench noise gate refuse to certify when it has no variance data
date: 2026-08-10
status: Accepted
summary: all_pass() requires a non-empty cell set, --runs below 2 is rejected at parse time, and a cell with fewer than two distinct runs fails on its own.
---

# 0024. Make the bench noise gate refuse to certify when it has no variance data

## Context

`chisel-bench-noise-gate` exists to refuse hardware that cannot hold a stable number.
Its verdict was `cells.iter().all(|c| c.passes)` — vacuously true on an empty set — and
`--runs` was taken verbatim from argv.

So `--runs 0` executed no benchmark, collected no cells, printed "Noise gate PASSED —
0 / 0 cells under threshold", and exited 0. `--runs 1` was worse: it ran once, and
`compute_cov` returns a placeholder 0.0 COV at N=1, so every cell cleared every threshold
by construction. A candidate machine could be qualified on data that cannot detect noise.

## Decision

Absence of evidence is a FAIL. `all_pass()` requires a non-empty cell set; `--runs` is
rejected below two at argv-parse time with an error that says why; and a cell with fewer
than two DISTINCT contributing runs fails on its own, independent of `--runs`.

`CellResult` carries its sample count so the report can explain a FAIL sitting next to a
0.0% COV, and an empty run states that it measured nothing rather than emitting a bare
FAIL over an empty table.

## Alternatives considered

- **Reject `--runs < 2` only** — rejected as insufficient: a scenario present in only
  some runs' metrics still yields a single-sample cell carrying the placeholder COV.
- **Make `compute_cov` return NaN at N=1** — rejected: it is a general-purpose library
  function whose N=1 convention is documented and tested, and NaN would propagate into the
  report's formatting. The gate, not the statistic, is the right place to encode refusal.
- **Count JSONL rows as samples** — rejected once the sample count became load-bearing: a
  single run emitting one cell twice would look like two independent observations.

## Consequences

The tool now fails closed. An operator who genuinely wants a single exploratory run must
say so some other way; there is no flag for it, because a single run cannot produce the
number the tool reports.
