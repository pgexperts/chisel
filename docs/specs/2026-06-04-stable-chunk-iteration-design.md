# Stable Chunk Iteration — Design Spec

- **Date:** 2026-06-04
- **Status:** designed (pending implementation)
- **Source:** brainstorm with the primary Chisel client (the relational layer); hardens the
  chunk-tags iteration APIs added 2026-06-02 (`docs/specs/2026-06-02-chunk-tags-design.md`)
- **Shape:** contract-hardening — **documentation + tests only, no production code change.**

## Summary

Promote a property the iteration APIs already exhibit into a *documented, tested* guarantee:
within a single open `Chisel` instance, `handles()` and `handles_with_tag(tag)` return
identical results across repeated calls as long as the relevant live set is unchanged
between them. The *order itself stays unspecified* — this commits Chisel only to
**repeatability within a session**, not to any particular order, and not across reopen or
`defrag`.

The implementation already satisfies this: both APIs walk fixed-arithmetic radix trees in a
deterministic, structure-only traversal, so two scans of an unmutated tree are byte-identical
`Vec`s. What is missing is the **contract**. The current doc comments say "order is
unspecified" and say nothing about repeatability, so a client cannot rely on "scan, do work,
scan again, get the same answer." This spec writes that contract down and locks it with
adversarial differential tests.

## Motivation

The relational layer that builds on Chisel scans a relation (a tag's members, or all chunks)
and must be able to scan it *again* — to re-drive a query, resume an interrupted pass, or
cross-check — and get the same handles in the same order, provided it has not mutated the
store in between. Today nothing promises that; the README example even annotates the result
`// order unspecified`. A client wanting a stable scan must currently sort or snapshot
defensively. A documented within-session repeatability guarantee removes that defensive work
for the common case.

The guarantee is scoped deliberately narrow (see Non-goals) so it constrains Chisel's
internals as little as possible while still being useful: Chisel stays free to change the
actual order, and to reorder across reopen or `defrag`.

## The guarantee

For a single open `Chisel` instance:

> Two calls to `handles()` (resp. `handles_with_tag(tag)`) return identical `Vec<u64>`
> values — same length, same elements, **same order** — whenever the relevant live set is
> unchanged between the calls and no `defrag`/`compact` has run between them.

- The **relevant live set** is the set of live handles (for `handles()`) or the set of live
  handles carrying `tag` (for `handles_with_tag(tag)`).
- That set is changed only by `allocate` / `allocate_tagged` / `delete` / `delete_tagged` /
  `delete_with_tag` / `delete_many`. Operations that do **not** change it — `read`, an
  interleaved scan, an `update` (handles are stable across update, so both the set and each
  handle's radix position are unchanged), a rolled-back transaction or savepoint, and
  page-cache eviction / spillway — therefore do **not** perturb either the contents or the
  order of a subsequent scan.

This holds whether the two calls are made inside an active transaction (both read the
in-progress view) or outside one (both read the committed view): what matters is only that
the relevant live set is unchanged between them.

The simple client rule is: **no inserts/updates/deletes between two scans ⇒ the two scans are
identical.** (`update` is in fact invisible to handle iteration, but clients need not reason
about that — the simple rule is sufficient and safe.)

## What is already true (why no production change)

Both APIs are deterministic functions of the radix tree's structure:

- `handles()` → `HandleTable::iter_live` walks the handle-table radix tree, visiting interior
  children and leaf slots in ascending slot-index order.
- `handles_with_tag(tag)` → `MembershipIndex::handles_for_tag` → `RadixU64::iter` walks the
  tag's inner radix tree the same way.

Both walks take `&self`, so the borrow checker forbids structural mutation during a call;
within a session with the live set unchanged, the tree is fixed, so the walk yields a
byte-identical `Vec`. The order is *currently* ascending-handle as an arithmetic consequence
of the radix layout, but that is **not** promised (see Non-goals). No hot path is touched.

The radix walk's correctness after a *rolled-back grow* depends on the depth-recovery fix
I99 / C1 (ARCHITECTURE.md, "in-memory radix depth is re-derived from the root"): a stale
post-rollback depth would mis-descend and mis-enumerate. The rolled-back-transaction tests
below double as a regression guard for that invariant, approached from the iteration angle.

## Scope

In scope — the two public iteration APIs:

- `Chisel::handles() -> Result<Vec<u64>>`
- `Chisel::handles_with_tag(tag: u32) -> Result<Vec<u64>>`

Explicitly out of scope:

- The internal physical-page enumerators (`sparse_data_pages`, `data_page_ids_snapshot`):
  they back `defrag`, use `std::collections::HashMap` (per-process random seed → order varies
  across processes by design), and are not "iterate over chunks." Not part of this contract.
- `handles_for_tag_bounded` / the bounded `iter` used by `delete_with_tag`: an internal
  early-exit variant, not a public scan.

## Non-goals

- **No specific order.** We do not promise ascending-handle, insertion, or any other order.
  (Strengthening to "ascending" was considered and declined, to preserve Chisel's freedom to
  change its index internals.)
- **No cross-session stability.** The guarantee does not survive `close`+reopen.
- **No defrag stability.** `defrag` / `compact` may reorder (handles are preserved, so
  *contents* are unchanged, but *order* is free to change).
- **No snapshot / MVCC isolation.** The contract says nothing about scans that straddle a
  mutation; concurrent mutation is out (Chisel is single-writer anyway).
- **No new iteration shape.** No streaming iterator and no `HandleScan` wrapper type; the
  materialized `Vec` is unchanged. (A future `for_each_handle_with_tag` per I97 would carry
  the same contract but is independent of this work.)

## Enforcement & testing surface

Because the guarantee is "repeatable" rather than "ordered," it cannot be checked inside a
single call — only *differentially*, by scanning, churning the state we are *allowed* to
churn, and scanning again. New integration test file `tests/iteration_stability.rs` (public
API only, in-memory backend, mirroring `tag_ops.rs` / `transactions.rs` and the
`tests/common` helpers):

1. **Back-to-back, all chunks.** Allocate a mix of tagged and untagged chunks; `handles()`
   twice; assert identical (order + contents).
2. **Back-to-back, tagged.** Same for `handles_with_tag(t)` on a populated tag.
3. **Interleaved reads.** Scan; `read` several handles (including the most recent); scan
   again; identical.
4. **Interleaved update.** Scan; `update` several chunks, including one grown enough to
   relocate its storage; scan again; identical. (Demonstrates update-invisibility.)
5. **Cache eviction / spillway.** Open with a small `cache_size`; allocate past it; scan;
   force eviction by reading many distinct chunks; scan again; identical. (Proves order is
   structural, not cache-residency-dependent.)
6. **Rolled-back transaction.** Scan (baseline); `begin`; allocate/tag/update/delete a batch
   large enough to grow the handle-table and a tag's inner radix past a level; `rollback`;
   scan; identical to baseline. (Also guards I99 / C1 depth recovery.)
7. **Savepoint rollback.** Variant of (6) using a savepoint rolled back within an otherwise
   net-empty transaction.

A comment at the top of the file records that cross-reopen and cross-defrag order are
**deliberately not** asserted — that non-guarantee is the point of the single-session scope,
and a test there would silently strengthen the contract.

## Documentation changes

- **Doc comments** (the source of truth for the contract):
  - `Chisel::handles` and `Chisel::handles_with_tag` (`src/lib.rs`): replace "Order is
    unspecified and callers must not depend on it" with the within-session repeatability
    guarantee plus the explicit order / reopen / defrag disclaimer.
  - `RadixU64::iter` (`src/membership_index.rs`): note the walk is a deterministic function
    of tree structure (repeatable for a fixed tree) while the order remains unspecified, so
    the public guarantee rests on it.
- **README.md**: refine the `handles_with_tag` example annotation
  (`// order unspecified` → `// order unspecified, but repeatable within a session`) and the
  API-table notes for `handles` / `handles_with_tag`. (Matches the recent
  "docs: reflect chunk-tags API in README" pass.)
- **ARCHITECTURE.md**: a short note in the handles / membership-index sections stating the
  within-session repeatability contract and that it is intentionally not promised across
  reopen / defrag, cross-referencing the radix-depth-recovery invariant.

## Relationship to existing work

- **Hardens the chunk-tags feature** (`docs/specs/2026-06-02-chunk-tags-design.md`):
  `handles_with_tag` is two days old; this pins the iteration contract its client needs.
- **Guards I99 / C1** (radix depth re-derivation on rollback) from the iteration angle via
  tests 6–7.
- **Independent of I97 / `for_each_handle_with_tag`**: a future callback enumerator carries
  the same contract but is out of scope here.
- **Client-driven** ([client] in ISSUES.md terms): requested by the relational layer.
