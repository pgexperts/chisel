---
id: 0026
title: Recycle a transaction's own dead pages rather than mutating them in place
date: 2026-08-16
status: Accepted
summary: Within-transaction radix churn is bounded by reissuing pages the transaction both allocated and superseded, never by COW-dedup in-place mutation.
---

# 0026. Recycle a transaction's own dead pages rather than mutating them in place

## Context

Neither the handle-table radix nor the membership index reclaimed anything
within a transaction. Every mutation re-COWs the whole root-to-leaf path and
queues the old path as freed, but nothing freed during a transaction reaches
the committed bitmap until commit — so N mutations on a depth-d table allocate
N*(d+1) pages, all dirty, all unevictable, none reusable. A 2000-allocate
transaction grew the file by 3318 pages, and the same defect exhausts an 8 MiB
default cache: the dirty set has nowhere to go.

`FreeMapTree` already solves this for itself with `session_owned`: a page this
handle already COW'd is mutated IN PLACE on a re-touch, no second allocation,
no second supersede. The obvious move was to give both radices the same
treatment, and the issue that reported the defect proposed exactly that.

It does not work, for two independent reasons that only became visible when the
mutation paths were traced end to end.

**The prepare-discard hazard.** All three mutation paths (`allocate_inner`,
`update_inner`, `delete_inner`) compute a CANDIDATE radix root and discard it on
a later fallible step by restoring a saved root ID. That is sound only because
the candidate is a fresh COW page: on failure `current_roots` still names the
old root and the candidate is orphaned. Under in-place mutation the candidate
root IS the current root, so "do not install" becomes a no-op and the mutation
is already live. In `update_inner` the new entry has overwritten the old, the
new inline slot has been released, and the old storage is never retired — a
live, caller-held handle resolving to a released slot. There is no bookkeeping
fix: COWing only the root does not help, because an in-place write to any
interior below it is equally visible through the restored root's unchanged child
pointer. The discard contract requires a freshly copied root-to-leaf path, which
is precisely what the dedup removes.

**The savepoint hazard.** `rollback_to` truncates only pages at or above the
savepoint watermark. An in-place write to a page allocated BEFORE the savepoint
survives the rollback as a phantom entry, visible to `handles()`, `stats()`,
`iter_live`, defrag and validate — and durable if the caller commits there.

`FreeMapTree` escapes both by accident of scoping rather than by design: nothing
mutates the freemap tree while a savepoint is open, because every `cow_alloc`
site passes `reuse = savepoints.is_empty()` and `reclaim_orphans` bails on
`savepoint_active`. The radices inherit no such escape — mutating inside
savepoint scopes is the entire point of savepoints.

## Decision

We will bound within-transaction page growth with a RECYCLE POOL, and never by
mutating a page in place.

`cow_alloc` draws COW targets from a per-transaction pool of pages this
transaction BOTH allocated AND superseded, consulted before the committed
bitmap. Membership requires both halves, and each retires one class of
restorable snapshot:

  - "this transaction allocated it" retires `committed_roots`. `allocate_first`
    yields only ids the committed bitmap marks free (I18); `new_page` is
    monotonic above the begin watermark. `committed_roots` is frozen until
    commit, so this cannot go stale mid-transaction.
  - "this transaction superseded it" retires `current_roots`, because `retire`
    runs only AFTER the replacement root is installed.

Savepoint roots are retired by gating feed and draw on `savepoints.is_empty()`
and draining the pool on savepoint push, so the pool is provably empty and inert
for the whole scope.

Both hazards therefore become structurally impossible rather than argued about.
A discarded candidate drops its `freed` vec and contributes nothing to the pool;
the savepoint gate is the one `cow_alloc` already applies.

## Alternatives considered

- **Session-COW dedup, as the issue proposed** — mutate in place any page this
  transaction already COW'd. Rejected: silently corrupts data on all three
  discard paths and on savepoint rollback, as traced above. This is the option
  that would have been implemented had the mutation paths not been read.

- **Dedup restricted to sites with nothing fallible after them, plus a
  "transaction must roll back" flag** set whenever an in-place mutation is
  discarded, with `commit()` refusing. This recovers the per-level memcpy and
  XXH3 savings that the recycle pool does not. Rejected for now: it changes the
  documented contract that a mid-prepare `CacheFull` is a complete no-op, and it
  is one careless edit away from silent corruption — a future change adding a
  fallible step to the untagged `allocate_inner` path reintroduces the bug with
  no compiler or test objection. If the memcpy cost ever justifies revisiting
  this, it needs its own record.

- **Comment-only: correct the "bounded steady-state" claims and leave the
  behaviour.** Rejected as insufficient on its own, but the comment corrections
  shipped regardless — they were wrong in two more places than the issue
  identified.

## Consequences

Per-operation page allocation drops from 1.659 to 0.005; a 2000-allocate
transaction grows the file by 10 pages instead of 3318, and no longer exhausts
the default cache.

What this does NOT recover is the per-level memcpy and XXH3 restamp, which is
the smaller half of the cost. Pages are still copied on every re-touch; they are
merely copied into a recycled destination instead of a fresh one.

`reclaim_dead_txn_page` deliberately bypasses `claim_page`'s I20 assertion that
forbids reissuing a dirty page, because a pooled page is dirty by construction.
The assertion is traded for the membership proof above, which makes the spillway
`forget` load-bearing in EVERY build profile rather than only where the debug
assert compiles out.

`txn_allocated` is new per-transaction memory, growing with distinct pages
allocated (~8 bytes per 8 KiB page). Bounded by data volume rather than mutation
count, precisely because the pool recycles.

The orphan sweep now has a fourth stream of unreachable-but-not-free pages that
is excluded by construction rather than by membership in its exclusion set: a
pooled page is dirty, and the sweep reads cache-first, so it never presents its
stale on-disk bytes. That argument breaks if the sweep is ever changed to
cold-read the file, and is recorded at the sweep.

Two tripwire tests encode the rejected design's hazards and fail under it. The
`allocated` half of the membership rule is pinned by a test verified to fail
when the check is replaced with `true` — adversarial review found that
substitution left the entire suite green, so that half had been protected by
reasoning alone.
