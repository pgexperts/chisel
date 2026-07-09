# Multi-Page Freemap — Design Spec

Status: approved 2026-06-22
Supersedes: the single-page freemap (`src/freemap.rs`, "a multi-page freemap is
not yet implemented").

---

## Summary

The freemap — the bitmap that tracks which page ids are free for reuse — is
today a single page covering ids `0..65_344` (`PAGE_BODY_SIZE * 8`), i.e.
~512 MB at an 8 KB page size. Past that ceiling `mark_free` silently no-ops and
reclamation stops, so a database that grows beyond ~512 MB leaks every freed
page forever (a fresh-eyes review finding, 2026-06-22).

This design generalizes the freemap into a **copy-on-write radix tree of bitmap
leaves** — a third radix structure alongside the handle table
(`handle_table.rs`) and the membership index (`membership_index.rs`), reusing
their established conventions. Coverage becomes `65_344 × 1021^depth` pages, so
depth grows logarithmically with database size and the ceiling is removed for
all practical scales. The current single-page freemap is exactly the depth-0
instance, so **existing databases keep working with no migration**.

The change also moves the freemap *into the page cache*: the per-transaction
eager clone of the whole bitmap is replaced by a tiny `{ root_page, depth }`
handle, and freemap mutations follow the same COW/dirty/flush discipline as
every other structure. This is what keeps "unbounded" affordable — per-
transaction cost is independent of database size.

## Motivation

- **Correctness:** past 65,344 pages, `FreeMap::mark_free` silently drops the
  free bit (`freemap.rs`), so the COW steady-state the engine depends on
  (bounded handle-table / membership page counts; the I118 `free_subtree`
  reclamation) degrades to monotonic file growth with **no error surfaced**.
- **Scale:** the single-writer embedded engine should not impose a ~512 MB hard
  reclamation cliff. A radix-of-bitmaps removes the ceiling with depth growing
  as `log_1021(pages / 65_344)`.
- **Architecture fit:** the engine already has two COW radix trees. A third one
  for the freemap is the lowest-surprise design and reuses proven invariants
  (lazy/sparse subtrees, COW spine rewrite, depth growth, freed-list discipline).

## Design decisions

1. **Radix tree of bitmap leaves**, not a linked chain or a fixed two-level
   directory. A chain is O(n) to scan for a free page in a high range; a fixed
   directory re-introduces a (higher) ceiling. The tree is the only structure
   that meets "unbounded" while keeping descent at O(depth). (Chain and
   two-level were considered and rejected.)

2. **Leaf = today's bitmap page, unchanged.** `PageType::FreeMap = 0x04` is the
   leaf type: 65,344 bits, 1 bit = 1 page, LSB-first within a byte. The on-disk
   leaf format is byte-for-byte what ships today.

3. **Interior = new `PageType::FreeMapInterior = 0x07`.** Up to
   `SLOTS_PER_PAGE = 1021` child page-id pointers (u64 LE), identical in layout
   to the membership interior page. A zero child pointer means "that entire
   sub-range is all in use" — no leaf is materialized (lazy/sparse, mirroring
   the other two radixes).

4. **Depth 0 is the current format.** A depth-0 tree *is* a single bitmap leaf
   pointed at directly by `root_freemap_page`. Existing databases load as
   depth 0 with zero on-disk change.

5. **The freemap lives in the page cache.** Drop the
   `committed_freemap` / `current_freemap: Box<[u8; PAGE_SIZE]>` fields. The
   freemap's in-memory identity becomes `{ root_page: u64, depth: u32 }` carried
   in `committed_roots` / `current_roots`. Bitmap pages are read through, and
   COW-mutated in, the page cache like every other structure.

6. **Structural pages recycle out-of-band, never from the bitmap.** Freemap
   interior/leaf COW copies and newly-materialized nodes are allocated by the
   freemap's structural allocator, which draws from an in-memory recycle of dead
   freemap pages and falls back to extending the file (`PageCache::new_page`) —
   but is NEVER sourced from the freemap's own bitmap. Sourcing a structural page
   from a free bit would clear that bit, which COWs a leaf, which recurses; the
   out-of-band recycle is what breaks the "the free-list needs a free page to
   record free pages" recursion *and* bounds the file (pure extend would march
   the high-water up one structural page per commit forever — see "Structural-
   page reclamation"). It generalizes today's `persist_freemap`, which already
   special-allocates the new freemap page outside the bitmap.

7. **Depth stored in the superblock** (vs. spine-walk recovery). A previously-
   zero reserved region in the superblock (byte 320) holds `freemap_depth`.
   Sparse trees make leftmost-spine depth recovery ambiguous (a zero pointer near
   the root looks like a shallow tree), so an explicit stored depth is the robust
   choice. Existing databases read the reserved region as 0 → depth 0.

8. **In-memory lowest-free hint** (vs. per-interior summary bits) for
   find-first-free in v1. A single `u64` "lowest id that might be free" avoids
   rescanning exhausted prefixes and covers the common allocate-ascending
   pattern. Per-interior "subtree-has-free" summary bits are a deferred
   optimization (would add a per-page field and maintenance cost; YAGNI for v1).

## On-disk format changes

All changes are **additive within the current major** — no existing field is
moved or resized, and existing depth-0 databases are bit-identical.

1. **New page type** `PageType::FreeMapInterior = 0x07`. Layout mirrors
   `MembershipInterior`:
   - byte 0: `0x07`
   - byte 1: per-page format version (`page::current_version`, 0 today)
   - bytes 2..16: reserved (I31 common header region), zeroed
   - bytes 16..8184: up to 1021 child page-id slots (u64 LE; 0 = absent child)
   - bytes 8184..8192: XXH3 checksum
2. **New superblock field** `freemap_depth: u32`, written at **byte 320**, in the
   reserved-zero region immediately after `root_membership_index_page` (which
   occupies 312..320; the named-root table fills 52..308 and `superblock_count`
   308..312, so byte 52 is *not* free — 320..8184 is the next reserved region).
   Serialized/deserialized alongside the existing fields; reads as 0 for any
   database written before this change (so a pre-existing single-page freemap is
   unambiguously depth 0).
3. **`root_freemap_page` semantics widen**: it now points at the tree *root*,
   which is a `FreeMap` leaf when `freemap_depth == 0` and a `FreeMapInterior`
   otherwise. `PAGE_ID_NONE` still means "no freemap materialized yet."

`current_version(FreeMapInterior)` returns `PAGE_FORMAT_VERSION_CURRENT` (0),
consistent with every other page type (I31).

## API surface

A new module `src/freemap_tree.rs` owns the tree; `src/freemap.rs` is retained
as the **leaf-bitmap primitive** (bit get/set/scan on a single
`[u8; PAGE_SIZE]`), which the tree composes. This keeps the bitmap arithmetic
isolated and unit-testable, matching how `membership_index.rs` layers a tree
over slot pages.

```rust
// src/freemap_tree.rs  (layer 4: page-type-specific logic, like the other radixes)
pub(crate) struct FreeMapTree {
    pub root: u64,     // PAGE_ID_NONE until first materialization
    pub depth: u32,    // 0 = single bitmap leaf
    // in-memory only; not persisted, rebuilt lazily:
    lowest_free_hint: u64,
}

impl FreeMapTree {
    // Find the lowest free page id, clear its bit (COW leaf+spine via `extend`),
    // and return it. None if the tree holds no free page.
    fn allocate_first(&mut self, cache: &mut PageCache,
                      extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>)
        -> Result<Option<u64>>;

    // Mark `id` free, materializing the leaf+spine (and growing depth) as
    // needed; structural pages via `extend`. Idempotent.
    fn mark_free(&mut self, cache: &mut PageCache, id: u64,
                 extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>) -> Result<()>;

    // Read-only: is `id` currently free?  (Health-check / tests / asserts.)
    fn is_free(&self, cache: &mut PageCache, id: u64) -> Result<bool>;

    // Recover a tree handle from a committed root + depth (open path).
    fn from_roots(root: u64, depth: u32) -> FreeMapTree;
}
```

`extend` is the freemap's structural allocator (decision 6): in production it
pops a dead freemap page from the in-memory recycle pool and falls back to
`PageCache::new_page`, but is never sourced from the bitmap. Passing it as a
closure (rather than baking `new_page` in) keeps `freemap_tree.rs` free of any
allocation *policy* — the recycle pool lives in the transaction layer — and
mirrors the `alloc` closure the membership tree already takes.

The persisted identity is `{ root_page, depth }` only — these mirror the
superblock and live in `committed_roots` / `current_roots`. The
`lowest_free_hint` is **manager-side in-memory state**, never serialized; a
`FreeMapTree` is the transient working handle the manager assembles from the
current roots plus the hint, and `from_roots` seeds a fresh (conservative) hint
on open. So `begin()` still copies only `{ root, depth, hint }` — three words.

`src/freemap.rs` keeps `FreeMap::{init_page, is_free, mark_free,
allocate_first, capacity}` operating on one page buffer; the tree calls these on
the leaf it has descended to.

## Operation semantics and cost

Cost model (per the `chisel-performance` skill): descent is O(depth); depth is
`ceil(log_1021(highest_freed_page / 65_344))` — **2 for ≤ 533 GB, 3 for
≤ 545 TB**. Cache-resident interior pages make a warm descent a handful of
memory reads.

| Operation | Page reads | Pages dirtied (COW) | Notes |
|-----------|-----------|---------------------|-------|
| `mark_free(id)` (leaf present) | O(depth) | 1 leaf (first touch this txn) | subsequent frees in the same leaf are in-place |
| `mark_free(id)` (subtree absent) | O(depth) | 1 leaf + new spine | materialize via `extend` |
| `mark_free(id)` (beyond capacity) | O(depth) | grow + leaf + spine | depth += 1 (rare) |
| `allocate_first()` | O(depth) from hint | 1 leaf (first touch) | clears one bit |
| `is_free(id)` | O(depth) | 0 | read-only |
| commit (`persist_freemap`) | — | touched leaves + spines | one data fsync covers all |

The per-transaction cost is bounded by the number of *distinct* freemap pages a
transaction touches, never by total database size — the property that makes the
unbounded design affordable. `begin()` clones `{root, depth, hint}` (three
words), not a bitmap.

## Transaction, durability, and poison semantics

The freemap has **no independent durability**: it rides the same single atomic
commit (the superblock swap) as the data it describes, and it can only ever be
"true" as of the last committed superblock. Concretely:

- **Copy-on-write:** a freemap mutation never overwrites a live page. It writes
  fresh interior/leaf copies (dirty in the cache) and leaves the committed tree
  intact until the superblock flips.
- **Commit ordering (3-fsync, unchanged):** all new data *and* freemap pages are
  written and fsync'd **before** the superblock that references them is written
  and fsync'd. A crash at any point before the superblock fsync leaves the old
  superblock — and therefore the old freemap root+depth — authoritative; the
  half-written new pages are unreferenced scratch and are ignored. The freemap
  and the data it describes thus become durable together, in lockstep; a crash
  can discard an in-progress change *whole* but can never tear one in half. The
  invariant that must never break: **a committed freemap never reports a page
  free while live committed data still occupies it.**
- **Allocate-before-merge (I18), generalized:** `persist_freemap` allocates the
  COW page ids it needs *before* merging this transaction's frees into the tree,
  because until the new superblock commits the *old* superblock still references
  the to-be-freed pages. A page is not reusable until the commit that frees it
  has fully landed. This per-leaf ordering is preserved.
- **Structural-page reuse (decision 6):** the freemap never overwrites a page
  the live superblock still points at, and it never recurses into itself for
  space — its COW targets come from an out-of-band in-memory recycle (one-commit
  deferred) or a file extension, never the bitmap. The full mechanism, and how
  crash-orphaned recycle entries are reclaimed, is in **Structural-page
  reclamation** below.
- **Rollback:** dirty freemap pages above the pre-transaction watermark are
  dropped (the existing I3 mechanism); `{root, depth, hint}` snap back to the
  committed values. No special freemap rollback path remains.
- **Poison:** an fsync failure or any fatal error inside the commit protocol
  poisons the `TransactionManager` (I1); recovery is drop-and-reopen, which
  re-reads the last durable superblock and reconstructs the committed tree
  handle. The freemap is built to never need its own recovery path.

## Structural-page reclamation

The freemap's own COW churn — every commit that frees a page COWs the affected
leaf and its spine — produces a stream of dead freemap pages that must be
reclaimed or the file grows without bound. These pages **cannot** be reclaimed
through the freemap's own bitmap: marking one free and later reusing it requires
clearing its bit, which COWs a leaf, which recurses (the termination hazard).
And they land at high (just-extended) ids, so lowest-first `allocate_first`
never reaches them even if marked free (data churn keeps reusing the low frees
while the freemap keeps extending → the high-water marches up forever).
Reclamation is therefore **out-of-band**. (This gap was found during the Phase 2
integration; an earlier draft of this spec wrongly assumed "reclaimed by a later
transaction" through the bitmap, which does not work.)

**Session-COW dedup (bounds the churn).** Within one commit, each freemap node is
COW'd at most once; a second `mark_free` into an already-COW'd node edits it in
place. Without this, K frees landing in one leaf would COW-extend the leaf K
times. A per-transaction `session_owned` set records the pages this transaction
has already COW'd or materialized; `cow_node` returns a session-owned page
unchanged (no extend, no supersede). The set is threaded through the transient
tree handles the integration rebuilds at each allocation site, and is transient
working state, never serialized. A session-owned page is edited in place WITHOUT
type re-validation — sound because the single writer just wrote it and it cannot
rot between two touches in one transaction; a *committed* page reached through a
fresh handle is **always** position-validated, so the COW-path corruption guard
(2026-06-22 review hardening) is preserved.

**In-memory recycle with a one-commit defer.** The structural allocator draws a
dead freemap page from an in-memory pool before extending. The one-commit defer
is mandatory for crash-safety: a page `P` superseded in transaction `T` is still
referenced by the pre-`T` superblock until `T` commits, so reusing `P` *within*
`T` and then crashing pre-commit would corrupt the page the recovered (pre-`T`)
superblock points at. So supersedes accumulate during `T`, become reusable only
after `T`'s superblock fsync lands, and are drawn from by `T+1`. The pool has two
logical states — *pending* (superseded this commit, not yet safe) and *reusable*
(safe now), promoted at commit. In steady state each commit supersedes and
consumes a similar small number of pages, so the file reaches a **flat**
high-water under sustained churn — the property the whole mechanism exists for.

**Crash recovery via defrag orphan-sweep.** Because the recycle pool is
in-memory, a crash orphans its entries: freemap-typed pages unreachable from the
committed tree and not marked free in the bitmap (a bounded handful — the last
commit's structural supersedes). They are not leaked permanently. `defrag`
reclaims them: walk the live freemap tree to build the reachable-freemap-page
set, scan for `FreeMap`/`FreeMapInterior`-typed pages that are neither reachable
nor already free, and mark those free. This is off the hot path and explicit
(scheduled with the rest of defrag), so durability-first holds without
per-commit persisted bookkeeping. A reclaimed orphan enters the BITMAP
(data-reusable), cleanly disjoint from the in-memory pool (freemap-COW-reusable)
— so no page is ever handed out twice.

## Architecture-fit assessment

- **Layering:** `freemap_tree.rs` sits at layer 4 (page-type-specific logic),
  depending only on `page` + `page_cache`, exactly like `membership_index.rs`
  and `handle_table.rs`. No upward or sideways references.
- **Single-writer `&mut self`:** unchanged. No interior mutability, no locking.
- **Checksum coverage:** every interior and leaf page carries the standard XXH3
  checksum at `8184..8192`, verified on cache miss like all pages.
- **`PageType = 0x00` reservation, strict layer dependency, format stability:**
  all preserved. The new interior type and superblock field are additive.

## Edge cases and soundness

- **Recursion termination:** structural COW is sourced out-of-band (the in-memory
  recycle or a file extension), never from the bitmap (decision 6 / Structural-
  page reclamation), so marking a page free never triggers another structural
  allocation that could clear another bit; the descent is depth-bounded by the
  freemap's own `MAX_DEPTH` (its leaf fans out to
  65,344 ≈ 2¹⁶ and each level multiplies by 1021 ≈ 2¹⁰, so depth 5 already covers
  2⁶⁶ > u64 page ids — the bound is ~5, distinct from the membership tree's 6,
  computed for this leaf fan-out); a corrupt out-of-range depth saturates
  capacity rather than overflowing (the existing `saturating_mul` pattern), so a
  bad on-disk depth fails closed, not via panic.
- **Growth race with file extension:** a page id only needs a freemap bit when
  it is *freed*; an allocated-and-never-freed page needs no leaf (absent subtree
  = all-in-use). So freeing an id beyond current capacity is the only growth
  trigger, and it grows the tree before setting the bit. Allocation
  (`new_page`/extend) never needs the freemap to pre-cover the new id.
- **Corrupt interior page:** a checksum-valid wrong-type page reached as an
  interior child surfaces as `CorruptPage` (the tree validates `buf[0] ==
  FreeMapInterior` on descent, mirroring the overflow/data-page hardening landed
  in the 2026-06-22 DESIGN review).
- **`is_free` / `allocate_first` past materialized range:** an id whose subtree
  is absent reads as "not free" (in use), which is correct — only freed pages
  get bits.
- **Empty/whole-file-free extremes:** an all-in-use database has root =
  `PAGE_ID_NONE` (or a single empty leaf); the first free materializes the tree.

## Testing surface

- **Unit (in-memory, fast):**
  - leaf-bitmap primitives (`freemap.rs`) unchanged tests retained;
  - tree descent / `mark_free` / `allocate_first` round-trips vs. a `HashSet`
    oracle of free ids, with proptests crossing depth-0→1→2 boundaries (model
    the membership proptests);
  - growth: freeing an id beyond capacity grows depth and remains consistent;
  - **termination invariant:** a test asserting the freemap's structural
    allocator is never sourced from the bitmap — it draws only from the recycle
    pool or the `extend` closure (a spy confirming no `allocate_first`/bitmap
    call re-enters during a structural COW);
  - **session-COW dedup:** marking N ids into one already-COW'd leaf in a commit
    supersedes exactly one structural page, not N (an `extend` spy counting
    materializations);
  - lowest-free hint correctness: after frees and reuses, `allocate_first`
    returns the true lowest free id.
- **Integration (the scenario the ceiling broke):** allocate > 65,344 pages,
  free a page in a high (depth-1) range, reopen, and confirm the freed page is
  reclaimed by a subsequent allocation. One file-backed reopen test for
  durability; an in-memory proptest for breadth.
- **Steady-state flat file (the reclamation property):** under sustained
  allocate/free/commit churn at constant live-data size, `total_pages` reaches a
  flat high-water (no per-commit growth) — the test that would have caught the
  original leak.
- **Crash → defrag reclaim:** simulate a crash with a non-empty recycle pool
  (drop the manager mid-flight after a committed structural churn), reopen,
  confirm orphaned freemap-typed pages exist (unreachable + not free), run
  `defrag`, and confirm they are reclaimed (marked free / file reusable).
- **Corruption on the COW path:** a fresh handle descending into a committed,
  position-wrong freemap page surfaces `CorruptPage` (the session-owned in-place
  skip does not weaken detection).
- **Recovery:** a committed multi-page freemap, reopened, yields the same
  free-set (root+depth round-trip through the superblock).
- **Backward compatibility:** a database written by the single-page format (a
  fixture or a depth-0 build) opens and reclaims correctly with the new code.

## Format-version / Don't-Break compliance

- No existing on-disk byte changes meaning. New `FreeMapInterior` pages and the
  `freemap_depth` superblock field are additive; pre-existing databases are
  depth 0 and bit-identical.
- The 3-fsync commit ordering, the I18 allocate-before-merge ordering, the
  poison model, checksum coverage, the `PageType = 0x00` reservation, and strict
  layering are all preserved (see the `chisel-performance` Don't-Break list).
- No `FORMAT_VERSION` major bump is required. (If a major bump is taken for
  other reasons before release, this change folds in cleanly; it does not force
  one.)

## Resolved decisions (2026-06-22 brainstorm)

- **Scale target:** unbounded (multi-level tree), page-cache COW model.
- **Ceiling handling:** build the real multi-page feature, not a `FreeMapFull`
  guard.
- **Depth storage:** superblock field (not spine-walk recovery).
- **find-first-free:** in-memory lowest-free hint (not interior summary bits) in
  v1.
- **Structural allocation & reclamation** (revised 2026-06-22 after the Phase 2
  integration exposed the bitmap-reclamation gap): out-of-band, never from the
  bitmap. An in-memory one-commit-deferred recycle of dead freemap pages, plus a
  session-COW dedup (one COW per node per commit). Crash-orphaned recycle entries
  are reclaimed by a `defrag` orphan-sweep — **in-memory pool, no persisted
  bookkeeping** (chosen over a persisted recycle list and over accepting a
  permanent per-crash leak). See "Structural-page reclamation".
- **Module split:** new `freemap_tree.rs` over a retained `freemap.rs` leaf
  primitive.

## Out of scope for v1 (possible future refinements)

- Per-interior "subtree-has-free" summary bits for O(depth) find-first-free
  under adversarial fragmentation (v1 uses the in-memory hint + descent).
- Shrinking depth when a database is truncated back down (the tree may keep
  empty interior levels; harmless, reclaimable, and rare for an append-mostly
  engine — mirrors the handle-table/membership "delete does not shrink depth"
  stance).
- A *general* mark-and-sweep reclaiming any leaked page (the defrag orphan-sweep
  in scope here is narrowly scoped to freemap-typed pages; a full reachability
  GC over all page types is a larger, separate effort).
- File truncation to return reclaimed high pages to the OS (reclaimed pages
  become reusable in the freemap; shrinking the file itself is separate).

## Relationship to existing tracked work

- Removes the silent-leak DESIGN finding (review-20260622-054729.md, freemap
  ceiling) — the one DESIGN item carved out of the cleanup layer for this
  feature.
- Builds directly on the COW radix machinery proven by the handle table and the
  membership index (`docs/specs/2026-06-02-chunk-tags-design.md`).
- Honors per-page format versioning (I31): the new interior page type
  participates in `current_version` / `page_format_version`.
