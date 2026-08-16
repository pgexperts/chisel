---
id: 0027
title: Bound every write path to exactly what its reader accepts
date: 2026-08-16
status: Accepted
summary: A writer that accepts input its reader would refuse, or refuses input its reader resolves, converts a loud failure into silent corruption; both radix and freemap write paths now mirror their readers exactly.
---

# 0027. Bound every write path to exactly what its reader accepts

## Context

Two defects found in the same review turned out to be the same defect in mirror
image, in different subsystems.

**The freemap writer was laxer than its reader.** `FreeMapTree::find_leaf` (read)
guards three ways: `check_depth()`, a capacity test with a `cap != u64::MAX`
carve-out, and a slot bound against `PTRS_PER_INTERIOR`. `cow_descend` (write)
had only `check_depth()`, and its own doc said "Assumes `id` is in range." At
depth 0 the descent loop never runs, so the operation lands on
`id % LEAF_CAPACITY`: `mark_free(LEAF_CAPACITY + 7)` set the free bit for page
7 — a LIVE page — which the next `allocate_first` hands straight out. Asking
`is_free(LEAF_CAPACITY + 7)` afterwards answers "in use". The corruption is
invisible at the id written and lethal at the id that was not.

**The radix growth loop was, in one specific way, at risk of becoming stricter
than its reader.** `capacity()` uses `saturating_mul`, so from depth 6 it
reports `u64::MAX` instead of the true 510 * 1021^6, and `handle >= capacity()`
stays true after every grow for exactly one value: `u64::MAX`. The loop extends
the file one page per lap until the allocator gives up. The obvious fix — return
a typed error for the handle that will not fit — would have made the engine
refuse to WRITE a handle its own `find_leaf` is explicitly built to READ (that
is what `find_leaf`'s `cap != u64::MAX` clause exists for).

Neither defect was reachable from a caller. The freemap one is prevented by
`mark_free_growing` growing first and `clear_bit` being private; the radix one
needs a corrupt-but-checksum-valid superblock, since `next_handle` is read
verbatim at open with no range validation. Both survived a suite of 765 tests
precisely because they are unreachable — which is the argument for fixing them,
not against.

## Decision

We will make every write path accept exactly the input set its corresponding
read path accepts — no more, and no less.

Concretely: `cow_descend` gains the capacity guard (with the same
`cap != u64::MAX` carve-out `find_leaf` has) and the `PTRS_PER_INTERIOR` slot
guard, both returning `CorruptPage`; `mark_free` becomes module-private so the
only entry point is the one that grows first. Both radix growth loops gain
`depth < MAX_DEPTH` as a terminating conjunct and NO error branch, because at
depth 6 the true capacity exceeds `u64::MAX` and every u64 handle is genuinely
addressable. `HandleTable::delete` and `RadixU64::delete` gain the same
`cap != u64::MAX` carve-out their readers have, so a handle that inserts and
reads back can also be deleted.

## Alternatives considered

- **A typed error on the radix growth loop**, as the reporting issue proposed.
  Rejected: it makes `u64::MAX` writable-but-refused while `find_leaf` still
  resolves it — the asymmetry inverted rather than removed. If it were ever
  adopted it would have to be written `cap != u64::MAX && handle >= cap` to
  avoid rejecting the one legitimate handle, which is a strong hint the bound
  alone is the real fix.

- **`debug_assert!` on the freemap write path** instead of a returned error.
  Rejected outright: silent in release is the exact failure mode being fixed.

- **A new operational error variant** for the freemap guards rather than
  `CorruptPage`. Considered seriously, because these are arguably caller bugs
  rather than page corruption. Rejected for consistency with `check_depth`'s
  existing choice in the same function, and because the slot guard fires after
  the root has been COW'd and `self.root` reassigned — there is no clean state
  to hand back, so "operational and recoverable" would be a lie.

- **Leaving `delete` alone as out of scope** for the growth-loop fix. Rejected:
  the fix is what makes `u64::MAX` insertable in the first place, so it is what
  makes the delete gap reachable. A prior review had already prescribed both
  clauses in one sentence; only the first had landed.

## Consequences

Both guards are unreachable from any current caller, so they cost a comparison
on paths that will never take them. That is the intended trade: the engine now
fails closed on a class of input it previously mishandled silently.

`CorruptPage` is fatal, so if either freemap guard ever fires it poisons the
manager. That is a deliberate escalation, not a free change — but it matches the
fail-closed posture of every other guard in that function.

`u64::MAX` is now fully symmetric across insert, lookup and delete in both
radices, pinned by tests that exercise the whole round trip rather than one leg.

The growth-loop fix moves the next domino into view: `next_handle += 1` in the
allocate install phase is unguarded, so a transaction that successfully mints
`u64::MAX` now reaches an overflow that panics in debug and wraps to `0` — the
reserved no-handle sentinel — in release. The systematic answer is range
validation of `next_handle` at open, alongside `freemap_depth` and the inner
depth, which already get it. Filed separately.
