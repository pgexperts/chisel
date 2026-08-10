---
id: 0022
title: Make transaction rewind paths restore state structurally, not by caller convention
date: 2026-08-10
status: Accepted
summary: Eagerly-mutated in-memory state is unwound by the code that mutates it, and rollback_to rewinds the freemap recycle and clears the insert cursor, replacing prose invariants with enforced ones.
---

# 0022. Make transaction rewind paths restore state structurally, not by caller convention

## Context

A cluster of defects shared one shape: a rewind path restored most of its state, with
the gap held closed by a comment rather than by construction.

`HandleTable::grow` bumped the descent depth before a fallible COW; only one of three
mutation paths unwound it, the others relying on an inter-module argument about resolved
handles. `rollback_to` restored the pre-savepoint insert cursor while the savepoint
provably survives the rewind, so slot packing resumed into a below-watermark page. The
freemap allocation hint was advanced but never rewound, stranding free page ids. `defrag`
skipped the orphan sweep on a premise that the allocate-abort path falsifies. And
`rollback_to` never rewound the freemap recycle at all, unreachable only because every
mutation site happens to be gated on `savepoints.is_empty()` for an unrelated reason.

Several were latent. That is the point: each was one new caller away from being live, and
nothing recorded that the accidental gates were load-bearing.

## Decision

State mutated eagerly is unwound by the code that mutates it, not by its callers.
`HandleTable::insert` snapshots and restores its own depth; `SlotPacker::restore` clears
the cursor itself so the rule cannot be undone by editing the savepoint code; `Savepoint`
carries a `FreemapMark` that `rollback_to` applies; the freemap hint is snapshotted at
begin and restored at rollback; and `defrag`'s fast path consults both roots.

Where a caller-side unwind covers a case the callee cannot see — a grow that SUCCEEDED
whose candidate root is discarded by a later prepare step — it is kept and its comment
is scoped to say exactly which case it owns.

## Alternatives considered

- **Assert the invariants instead** (`debug_assert!` at the boundaries) — rejected: it
  documents the constraint but only fires in debug builds, and leaves the next caller to
  discover it by breaking it.
- **Leave the latent ones alone** as unreachable — rejected for the same reason the
  original comments were insufficient: reachability rests on gates that exist for
  unrelated reasons and could be relaxed by someone who does not know they are load-bearing.
- **Snapshot `structural_reuse` as well** — rejected, but note the reason: it is
  pointless, not dangerous. Nothing consumes it inside a savepoint scope, and the cost of
  omitting it is a bounded, self-healing leak.

## Consequences

Each savepoint now clones an `FxHashSet` in addition to the map it already cloned —
smaller than the existing clone, and empty in any workload that has not COW'd the freemap.

Several standing comments became false and were rewritten, including two that described
the un-rewound `rollback_to` as the reason a defensive bail exists. That bail is now
defence in depth rather than the sole guarantee, and says so.
