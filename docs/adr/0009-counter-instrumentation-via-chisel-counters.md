---
id: 0009
title: Counter instrumentation via `Chisel::counters()`
date: unknown
status: Accepted
---

# 0009. Counter instrumentation via `Chisel::counters()`

**Context:** Bench harnesses, performance debugging, and long-running operational visibility all want "what did the engine actually do during this operation?" Without counters, the only signal is wall-clock time, which is too noisy for component-level analysis (cache hit rate vs. fsync rate vs. allocation rate).

**Decision:** Four cumulative-from-open counters exposed via `Chisel::counters() -> ChiselCounters`: `cache_hits`, `cache_misses`, `pages_allocated`, `fsync_calls`. Each is a `Cell<u64>` living at the increment site (`PageCache` for the first three, `PageIo` for fsync). `PageCache::counters()` aggregates them into a single struct. `ChiselCounters` is `#[non_exhaustive]` so future counters can be added without a breaking change.

Three semantic conventions matter:

- **Counters reset on close + reopen.** No persistent state on disk.
- **Misses, allocations, and hit increments record *attempts*, not successes.** A `CacheFull` allocation still bumps `pages_allocated`. Asymmetric exception: `fsync_calls` counts only *successful* fsyncs, because a failed fsync poisons the engine and the counter on a poisoned engine has no defined further meaning.
- **Reads via `Chisel::counters()` are `&self`** and do not mutate.

**Alternatives considered:**

- *Internal logging/tracing.* Would require log-line parsing on the consumer side. Counters give a structured, allocation-free read.
- *Histogram of operation latencies.* Out of scope for v1 — adds dependency on a histogram crate and increases per-operation overhead. The bench harness can compute its own latency distributions externally.
- *Configurable counter set.* Adds complexity for marginal benefit. v1 picks four; `#[non_exhaustive]` keeps the door open.

**Consequences:**

- *Positive:* The bench harness reads counters before/after each scenario cell and reports deltas. This is what makes the bench-suite's per-cell analysis possible.
- *Positive:* Operational debugging: "how many cache misses did this query cause?" is one `counters()` call.
- *Negative:* Four counters is a v1 minimum; might want fsync byte counts, spillway hits, etc. later. `#[non_exhaustive]` makes additions cheap.

Landed as PR 1 of the bench-suite series.

---
