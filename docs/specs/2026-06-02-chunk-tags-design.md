# Chunk Tags — Design Spec

- **Date:** 2026-06-02
- **Status:** approved design (open questions resolved 2026-06-02), pending implementation plan
- **Source:** brainstorm with the primary Chisel client (relational layer)

## Summary

Add an optional, immutable `u32` "chunk tag" to each chunk (handle). The tag is a
client-supplied, opaque grouping key — in the relational layer that builds on Chisel,
one tag per *relation* (table or index), and a chunk's tag is the relation it belongs
to. Chisel maintains a reverse membership index (`tag → {handles}`) so the client can
sequentially scan a relation, drop a relation incrementally, and remove a single chunk
from its relation without scanning. Untagged chunks (the common case for any
non-relational use of Chisel) cost nothing — they never enter the index and carry only a
sentinel tag value.

This is a secondary index. It reuses Chisel's existing radix-tree primitive and
shadow-paging discipline rather than introducing a foreign structure, so it composes with
the engine rather than fighting it.

## Motivation

A relational database layered on Chisel needs to track which chunks make up each
relation, for three operations the engine cannot currently do efficiently:

1. **Sequential scan** of a relation — enumerate all chunks of one table/index.
2. **Drop** a relation — delete all its chunks. (This also closes the existing
   `drop_table` / `drop_index_table` handle-and-page leak tracked as F1 / I12: today those
   operations orphan row/node handles that only `defrag` eventually reclaims.)
3. **Single-chunk delete** that also removes the chunk from its relation's membership,
   without scanning the membership to find it.

The client could store the tag on the chunk itself and scan all chunks to answer (1) and
(2), but that is `O(all chunks)` per operation. The membership index makes (1) and (2)
proportional to the relation's size, and the forward tag (stored in the handle entry)
makes (3) `O(1)`.

## Design decisions

These were settled during the brainstorm; each entry records the decision and why.

1. **Tag is `u32`, not `u64`.** A `u32` (4 bytes) fits in the `HandleEntry`'s 5 reserved
   bytes; a `u64` does not. That single fact makes forward storage *free* — see decision 4.
   `2^32 − 1 ≈ 4.3 billion` distinct tags is effectively unbounded for relation ids.
2. **`tag == 0` is the "no tag" sentinel.** Untagged chunks carry tag `0` and have no
   membership-index entry. Old databases (whose reserved bytes are already zero) therefore
   read as fully untagged with no migration — backward compatibility is automatic.
3. **Tags are immutable.** The tag is set at chunk creation and never changes. To re-tag a
   chunk, the client creates a new chunk with the new tag and deletes the old one. This
   removes the re-tag code path entirely (no "find and remove the old tag" step, half the
   index churn, no transient "moving between tags" state). The tag is removed only when the
   chunk is deleted. (Note: `u32`-in-entry would also make *mutable* tags cheap, since the
   forward tag is readable; immutability is a deliberate simplification, not a constraint
   forced by the storage choice.)
4. **The forward tag lives in the `HandleEntry` reserved bytes.** The full `u32` occupies 4
   of the 5 currently-reserved bytes of each 16-byte handle entry. Consequences:
   - No data-page format change and no side structure: the tag rides the handle entry,
     which every read/update/delete already loads, so reading the tag adds *zero* page
     reads.
   - `tag(handle)` is `O(1)` — a field read off the entry.
   - A bare `delete(handle)` self-maintains the index: it reads the tag from the entry and
     removes `(tag, handle)` from the membership index with no client cooperation.
   - No separate "tagged" flag bit is needed; `tag == 0` encodes "untagged."
   - Cost: 4 of the 5 per-entry reserved bytes are permanently committed (1 byte, plus the
     per-page common-header reserved region from I31, remain for future use).
5. **Reverse index is a two-level radix.** Outer radix keyed by `u32 tag` → value is the
   root page id of that tag's inner radix; inner radix keyed by `u64 handle` → presence.
   Both are "handle-table-shaped" trees, so they reuse the existing radix machinery rather
   than introducing a B-tree. Chosen over a single flat radix over a packed `(tag:handle)`
   key because the two-level form reuses the existing `u64`-radix shapes directly and makes
   "enumerate the distinct tags" a first-class `O(T)` operation. The membership index root
   is anchored in the superblock reserved region, tracked through commit/rollback exactly
   like the handle-table and freemap roots.

## On-disk format changes

Both changes are additive and backward-compatible; both ride the existing format-version
machinery (I29 packed MAJOR/MINOR, I31 per-page version). Pre-1.0, so within the
"format_version is tentative" window.

1. **`HandleEntry`:** 4 of the 5 reserved bytes become a little-endian `u32 tag` field.
   Layout becomes `{u64 page_id, u16 slot_index, u8 flags, u32 tag, 1 reserved}`. Old
   entries have zero in those bytes → tag `0` → untagged. `ENTRIES_PER_LEAF` is unchanged
   (the entry stays 16 bytes), so handle-table geometry is untouched.
2. **`Superblock`:** a new `u64 root_membership_index_page` field in the reserved region
   after `superblock_count`. Old superblocks have zero there → `PAGE_ID_NONE` → empty
   membership index.

No data-page, overflow, or freemap format change. Untagged databases are byte-identical to
today except for the (zero) tag bytes and the (zero) superblock field.

## API surface

Names follow the existing Chisel idioms: CRUD verbs (`allocate`/`read`/`update`/`delete`);
the `_tagged` qualifier for single-handle operations that take a tag (cf. `delete_many`);
the `_with_tag` qualifier for operations over the set of handles bearing a tag; the
bare-noun accessor style (`handles`, `stats`, `tag`); and `defrag`'s bounded-work-returns-
progress shape for the drop. Rust core, mirrored in `chisel-py`:

- `allocate_tagged(&mut self, value: &[u8], tag: u32) -> Result<u64>` — allocate a chunk
  and set its (immutable) tag. `tag == 0` is exactly `allocate()` (untagged). Inserts
  `(tag, handle)` into the membership index when `tag != 0`.
- `tag(&self, handle: u64) -> Result<u32>` — the chunk's tag (`0` if untagged). `O(1)`
  (a field read off the handle entry).
- `handles_with_tag(&self, tag: u32) -> Result<Vec<u64>>` — the handles of a tag, parallel
  to `handles()`. (A callback variant — `for_each_handle_with_tag` — can follow once I97's
  `for_each_handle` shape lands, so very large memberships need not materialize.)
- `delete(&mut self, handle: u64) -> Result<()>` — **the fast path, unchanged signature.**
  Self-maintaining: it reads the tag from the handle entry (free — the entry is loaded
  anyway) and, if tagged, removes `(tag, handle)` from the index. Untagged chunks do no
  index work. No tag argument; correct by construction because the tag comes from the entry.
- `delete_tagged(&mut self, handle: u64, tag: u32) -> Result<()>` — the specialized,
  defensive variant: asserts the chunk's tag equals `tag` (typed error on mismatch) before
  deleting. For callers that want to assert membership rather than trust it.
- `delete_with_tag(&mut self, tag: u32, max: usize) -> Result<TagDropProgress>` — delete up
  to `max` chunks of a tag (each chunk's value/overflow pages freed; an emptied inner
  subtree + outer entry dropped). Returns `TagDropProgress { deleted: usize, complete: bool }`.
  **Bounded** so a large relation drops incrementally: the caller loops
  `begin → delete_with_tag → commit` until `complete`, each batch a bounded, separately-
  durable transaction. This is the relation-drop primitive (closes F1 / I12). Operates
  within the active transaction, like `delete_many`. The `max` bound doubles as a cap on the
  transaction's working set, keeping each batch within the cache/spillway budget.

`read` / `update` are unchanged except that `update` copies the (immutable) tag forward
when it COWs the handle entry.

## Operation semantics and cost

| Operation | Work | Cost |
|---|---|---|
| `allocate_tagged(v, t)` | allocate chunk + insert `(t, handle)` in index (if `t≠0`) | chunk alloc + COW the index path (outer descent + inner insert) |
| `read` / `update` | unchanged; `update` preserves tag on entry COW | unchanged |
| `delete(h)` | read tag from entry; if tagged, remove `(tag, h)` from index; delete chunk | chunk delete + COW the index path |
| `delete_tagged(h, t)` | verify `(t, h)` present (error if not) then as `delete` | + one keyed index lookup |
| `tag(h)` | read tag field off the handle entry | `O(1)` (handle-table lookup already done) |
| `handles_with_tag(t)` | enumerate tag `t`'s inner radix | `O(members of t)`, sequential |
| `delete_with_tag(t, max)` | enumerate up to `max` members, delete each, drop emptied subtree/entry | `O(min(max, members))` per call, one bounded transaction |

**Untagged chunks cost nothing extra:** tag `0`, no index entry, no index COW. The feature
is invisible to any workload that does not use it.

When a tag's last member is deleted — via `delete` / `delete_tagged`, or as the final batch
of `delete_with_tag` — its now-empty inner radix root is freed and the outer entry removed,
so a tag occupies index space only while it has members.

## Transaction, durability, and poison semantics

The membership index is part of the transactional shadow-paging set, identical in
discipline to the handle table and freemap:

- Every modification COWs the affected index pages; the working root lives in
  `current_roots`, is promoted to `committed_roots` on commit, and is discarded on rollback
  (the watermark drops the COW'd pages). No special-casing.
- The new index root is written and fsync'd as part of the same durable write set as all
  other dirty pages, before the superblock fsync — the existing two-fsync ordering and the
  I28 pre-drain are unchanged.
- Index pages freed by deletes go through the freemap (R2) like every other freed page.
- A fatal error during index maintenance poisons the `TransactionManager` (I1), same as any
  other fatal in the commit protocol.
- Single-writer `&mut self` means there is no concurrent index update to coordinate.

## Architecture-fit assessment

Not hostile. The reasons it composes cleanly:

- **Reuses the core data structure.** The index is two more radix trees of the same shape
  as the handle table — "radix all the way down," not a foreign B-tree.
- **Uses existing format-evolution hooks.** Additive `HandleEntry` reserved bytes,
  additive superblock reserved field, both backward-compatible by construction (zero =
  untagged / empty). Rides I29/I31 versioning.
- **Same durability discipline.** COW, superblock-anchored root, freemap-backed reclaim,
  poison model — nothing new in the commit protocol.
- **Single-writer simplicity.** No concurrency to design around.

The genuinely new work is bounded: the two-level membership-index module, the two format
additions, and the API. It is "more of what Chisel already does."

## Edge cases and soundness

- **Wrong tag on `delete_tagged`:** the membership verification fails → typed error; the
  index is never silently desynced. (A bare `delete` cannot pass a wrong tag because it
  reads the correct tag from the entry itself.)
- **Untagged chunk:** tag `0`, no index entry; `tag` returns `0`; `delete` skips index
  maintenance.
- **Backward compatibility:** an old database opens with all chunks untagged and an empty
  membership index, with no migration step.
- **`update` of a tagged chunk:** the handle entry is COW'd to a new `(page_id, slot)`; the
  immutable tag is copied to the new entry. Membership is unaffected (the handle is stable).
- **Re-tagging:** unsupported by design; the client creates a new chunk and deletes the old
  (the new chunk gets a new handle — the client is responsible for updating any of its own
  references, exactly as it would for any value it chooses to relocate).
- **Partial drop:** `delete_with_tag` returning `complete == false` leaves the tag with its
  remaining members intact and consistent; the next batch resumes. A crash between batches
  loses only the uncommitted batch (each committed batch is durable).

## Testing surface

- Round-trip: `allocate_tagged` → `tag` matches; `handles_with_tag` returns exactly the
  members; `delete_with_tag` removes them and frees their value/overflow pages.
- Untagged: tag `0` creates no index entry; `tag` returns `0`; zero index growth.
- Self-maintaining delete: `delete(h)` of a tagged chunk removes its index entry; iterating
  the tag afterward does not return it.
- `delete_tagged` with the wrong tag errors and leaves the index intact.
- Bounded incremental drop: `delete_with_tag(t, max)` deletes at most `max`, reports
  `complete` correctly, and a loop to completion removes every member exactly once; a
  committed batch survives a simulated crash mid-loop.
- Empty-tag reclaim: deleting a tag's last member frees the inner root and removes the outer
  entry.
- COW / rollback: tagged ops inside a transaction revert on rollback (index returns to the
  committed root).
- Crash recovery: membership root survives via the superblock; reopening reconstructs the
  index state from the winning superblock.
- Backward compatibility: opening a pre-feature database yields all-untagged chunks and an
  empty index.
- Format round-trips: `HandleEntry` tag bytes and the superblock membership root serialize
  and deserialize losslessly.
- F1 / I12 regression: dropping a relation via `delete_with_tag` leaks no handles or pages
  (the original motivation).
- Both backends (in-memory and file) and the cross-engine bench harness exercise the new
  paths.

## Format-version / Don't-Break compliance

- On-disk format changes are additive and backward-compatible (zero = untagged / empty),
  bumping MINOR per I29; no MAJOR break, no reinterpretation of any currently-meaningful
  byte. Pre-1.0, so within the tentative-format window.
- No change to the commit fsync protocol, the pre-drain (I28), the poison model (I1), the
  checksum coverage, the layer dependency graph, or the single-writer contract.
- `PageType = 0x00` reservation untouched; the membership-index pages are ordinary radix
  pages with their own (nonzero) page type and checksum.

## Resolved decisions (2026-06-02 review)

- **API names** follow the existing idioms (see API surface): `_tagged` for single-handle
  ops with a tag argument, `_with_tag` for ops over a tag's member set, the bare-noun
  accessor `tag(handle)`, and a `defrag`-style bounded drop.
- **`delete_with_tag` is bounded** by a `max` argument and returns
  `TagDropProgress { deleted, complete }`, so a large relation drops incrementally in
  bounded-time batches (the caller loops until `complete`). This mirrors `defrag`'s
  `max_pages` posture — incremental cleanup that never blocks for an unbounded span.
- **`delete(handle)` is the fast path**, self-maintaining and unchanged in signature;
  `delete_tagged(handle, tag)` is the specialized variant that additionally verifies the
  supplied tag. The fast path needs no tag argument because it reads the tag from the entry.

## Out of scope for v1 (possible future refinements)

- **Mutable tags.** Cheaply possible now (forward tag is readable), but deliberately
  deferred; immutable for v1.
- **Callback enumeration** (`for_each_handle_with_tag`) once I97's `for_each_handle` lands,
  so very large memberships need not materialize a `Vec`.
- **Per-tag summaries** (Bloom filter / `[min,max]` handle range) — moot while `tag` is
  `O(1)`; relevant only if a forward-less variant were ever revisited.
- **Bitmap inner sets** instead of radix, if profiling ever shows dense per-tag handle
  ranges (expected sparse → radix is the right default).
- **Batched per-page frees** in `delete_with_tag`, mirroring the deferred I33 batching for
  `delete_many`.

## Relationship to existing tracked work

- **Closes F1 / I12** (`drop_table` / `drop_index_table` handle-and-page leak) —
  `delete_with_tag` is the clean, bounded bulk-drop primitive those requests wanted.
- **Reuses the I97 enumeration shape** (`for_each_*` callback) for the future
  `for_each_handle_with_tag`.
- **Rides I29/I31** format-version machinery for the additive on-disk changes.
