---
id: 0023
title: Gate the bench workflow on compilation, never on measurements
date: 2026-08-10
status: Accepted
summary: bench.yml's build steps gate; every measurement, diff, and comment step is continue-on-error, and comment steps are skipped outright on fork PRs.
---

# 0023. Gate the bench workflow on compilation, never on measurements

## Context

`bench.yml` documented itself as "Never blocks merge — signal, not gate" and said the
fork-PR comment step "will fail gracefully". No step carried `continue-on-error`, so both
statements were false.

On a fork PR the comment action gets HTTP 403 — a `permissions: pull-requests: write`
block cannot grant write to a fork's read-only token — the step fails, and the Bench job
goes red on every fork contribution. Any harness flake did the same. The `if: always()`
already present on the artifact upload showed the author expected step failures but had
not neutralised the one they described as harmless.

## Decision

The two `Build bench` steps GATE: a bench target that no longer compiles is real
breakage, and the root `cargo build` does not build bench targets, so this is the only
place it surfaces. Every step after them is `continue-on-error: true`. The comment steps
are additionally skipped on fork PRs via a same-repo `if:`, so the guaranteed-403 write is
never attempted.

The four setup steps before the builds are left bare and also gate. That is deliberate and
stated: a failure there means the job did not run, and hiding it would make a green check
a lie.

## Alternatives considered

- **Job-level `continue-on-error`** — rejected: it would make the documented claim
  literally true but would also hide a genuine bench-code compile failure, which nothing
  else catches.
- **`continue-on-error` on the comment steps only** — rejected: it leaves harness flakes
  reddening the check, which is the other half of the reported problem.
- **Allow the fork comment step to fail and ignore it** — rejected in favour of skipping:
  a skipped step reads as N/A in the UI rather than as a red herring in the log.

## Consequences

The workflow's header now enumerates exactly what can turn the job red instead of making
a blanket claim. Benchmark NUMBERS never block merge; benchmark CODE failing to compile
does, as does the runner failing to get far enough to compile it.
