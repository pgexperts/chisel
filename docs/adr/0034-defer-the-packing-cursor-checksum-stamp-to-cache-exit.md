---
id: 0034
title: Defer the packing cursor's checksum stamp to the points where the page leaves the cache
date: 2026-10-15
status: Accepted
summary: Inserts into the data-page packing cursor flag the cache entry (needs_stamp) instead of restamping, and the page cache stamps flagged pages before the flush main-file write and before the evict seal/spill — the only two points cached bytes are persisted.
---

# 0034. Defer the packing cursor's checksum stamp to the points where the page leaves the cache

## Context

Every page carries an XXH3 checksum over its body, verified when the page is
cold-loaded from the main file; a mismatch is fatal and poisons the manager
(ADR 0006). The rule had been that the module mutating a page restamps it after
each mutation. With slot packing (R1), `SlotPacker::insert` restamped the
cursor page after every `DataPage::insert`, so a transaction packing N small
values into one page hashed that page's 8 KB body N times: O(values × 8 KB)
where O(pages × 8 KB) would do (I78, #138, from the 2026-06-01 performance
review).

The optimization was deferred on 2026-06-01 pending a bulk-insert bench on
dedicated hardware, which has not materialized (ADR 0010). It was also held
back by a belief that the eager stamp protected the evict-mid-transaction
case. Later investigation (recorded on #138) corrected that: the spillway
round-trip verifies its own `slot_checksum`, never the internal page checksum.
The internal checksum only has to be valid before a page's bytes reach the
main file — either at commit flush or via spill-then-drain.

On 2026-10-15 the deferral was implemented without waiting for the bench, on
the strength of the bounded design below and its tests.

## Decision

We will stamp the packing cursor page lazily, owned by the page cache:

- `CacheEntry` carries a `needs_stamp` flag. `SlotPacker` mutates the cursor
  through `PageCache::get_mut_deferred_stamp`, which raises the flag before
  handing out the buffer. The fresh-page path still stamps once.
- The cache settles the flag at the only two points where cached bytes are
  persisted: before `write_sealed` in `flush` Phase 1a, and before the
  seal/spill in `maybe_evict` Phase B. The Phase 1b drain copies the already
  stamped spilled blob verbatim and needs nothing.
- Invariant: `needs_stamp` implies `dirty`. Every `CacheEntry` constructor
  starts with the flag clear, and the only `dirty = false` transition follows
  the Phase 1a stamp.
- In debug builds a tripwire asserts at Phase 1a, Phase B and the drain that
  every data-typed page leaving the cache verifies, so the whole debug test
  suite also detects any missed stamp point.

The invariant is documented at the flag's definition in `src/page_cache.rs`,
with a landmine note in ARCHITECTURE.md.

## Alternatives considered

- **Keep the eager per-insert stamp** — the status quo. Rejected: it is simple
  and obviously correct, but its cost scales with the number of packed values.
  Its stated justification (the evict-mid-transaction case) turned out to be
  wrong.
- **Stamp on cursor retirement, plus an eviction hook** — the issue's other
  suggested shape. It does not cover a cursor page that is still current when
  `flush` runs, so it would still need the flush-time stamp. Not pursued:
  because the flag lives on the cache entry, the cache settles it on every
  exit path with no extra hook in the packer.
- **Wait for a dedicated-hardware bench before deciding** — the 2026-06-01
  disposition. Set aside by the user's decision to implement unconditionally;
  the bench machine (ADR 0010) is still pending.

## Consequences

- Packing cost is now one hash per page per exit rather than one per value. An
  ad-hoc local timing showed about 12–20% faster for 1,000 small inserts in one
  transaction. That was report-only, on a shared laptop, and not measured at
  scale.
- A new path that writes a cached page's bytes to the main file or the
  spillway must settle the flag first. If it does not, the page commits with a
  stale checksum, and its next cold read is a fatal `ChecksumMismatch`. The
  debug tripwire and `tests/deferred_stamp.rs` (spill, drain and cold reopen,
  plain and encrypted) exist to catch this. In release builds only the tests
  guard it.
- `copy_page` copies bytes between cache entries without settling the flag. It
  is safe because its callers clone structural pages only, and data pages are
  never COW-copied. A future caller that copies a data page would need to
  settle the flag first.
- `page::stamp_checksum`'s "after every mutation" contract and the
  `data_page.rs` mutation-discipline note no longer describe the cursor path;
  both are tracked in #203.
