# Chunk Tags — Design Spec

- **Date:** 2026-06-02
- **Status:** approved design, pending implementation plan
- **Source:** brainstorm with the primary Chisel client (relational layer)

## Summary

Add an optional, immutable `u32` "chunk tag" to each chunk (handle). The tag is a
client-supplied, opaque grouping key — in the relational layer that builds on Chisel,
one tag per *relation* (table or index), and a chunk's tag is the relation it belongs
to. Chisel maintains a reverse membership index (`tag → {handles}`) so the client can
sequentially scan a relation, drop a relation in one pass, and remove a single chunk
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
2. **Drop** a relation — delete all its chunks in one transaction. (This also closes the
   existing `drop_table` / `drop_index_table` handle-and-page leak tracked as F1 / I12:
   today those operations orphan row/node handles that only `defrag` eventually reclaims.)
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
   - No data-page format change and no separate side structure: the tag rides the handle
     entry, which every read/update/delete already loads, so reading the tag adds *zero*
     page reads.
   - `tag_of(handle)` is `O(1)` — a field read off the entry. (This dissolves the earlier
     `O(T)` skip-scan workaround, which only existed to compensate for *not* storing the
     tag forward.)
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

Names are provisional; the implementation plan can refine them. Rust core (mirrored in
`chisel-py`):

- `allocate_tagged(&mut self, value: &[u8], tag: u32) -> Result<u64>` — allocate a chunk
  and set its tag. `tag == 0` is equivalent to `allocate()` (untagged). Inserts
  `(tag, handle)` into the membership index when `tag != 0`.
- `tag_of(&self, handle: u64) -> Result<u32>` — the chunk's tag (`0` if untagged). `O(1)`.
- `for_each_with_tag(&self, tag: u32, f: impl FnMut(u64)) -> Result<()>` — enumerate the
  handles of a tag (callback, mirroring the I97 enumeration shape so large memberships do
  not materialize). `handles_with_tag(tag) -> Result<Vec<u64>>` is the eager convenience.
- `delete_all_with_tag(&mut self, tag: u32) -> Result<usize>` — delete every chunk of a
  tag (its value/overflow pages freed) and drop the tag's inner subtree + outer entry.
  Returns the count deleted. This is the relation-drop primitive (closes F1 / I12).
- `delete(&mut self, handle: u64)` — unchanged signature; now also removes `(tag, handle)`
  from the index if the chunk is tagged (self-maintaining).
- `delete_tagged(&mut self, handle: u64, tag: u32) -> Result<()>` — optional defensive
  variant that verifies `(tag, handle)` is present in the index before deleting, erroring
  if not (catches a caller passing the wrong tag). Since a chunk has exactly one tag,
  `(tag, handle)` exists only for the correct tag, so the check is total.

`read`/`update` are unchanged except that `update` copies the (immutable) tag forward when
it COWs the handle entry.

## Operation semantics and cost

| Operation | Work | Cost |
|---|---|---|
| `allocate_tagged(v, t)` | allocate chunk + insert `(t, handle)` in index (if `t≠0`) | chunk alloc + COW the index path (outer descent + inner insert) |
| `read` / `update` | unchanged; `update` preserves tag on entry COW | unchanged |
| `delete(h)` | read tag from entry; if tagged, remove `(tag, h)` from index; delete chunk | chunk delete + COW the index path |
| `delete_tagged(h, t)` | verify `(t, h)` present (error if not) then as `delete` | + one keyed index lookup |
| `tag_of(h)` | read tag field off the handle entry | `O(1)` (handle-table lookup already done) |
| `for_each_with_tag(t)` | enumerate tag `t`'s inner radix | `O(members of t)`, sequential |
| `delete_all_with_tag(t)` | enumerate + delete each member + drop subtree | `O(members of t)`, one transaction (~3 fsyncs total) |

**Untagged chunks cost nothing extra:** tag `0`, no index entry, no index COW. The feature
is invisible to any workload that does not use it.

When a tag's last member is deleted (via `delete`/`delete_tagged`), its now-empty inner
radix root is freed and the outer entry removed, so a tag occupies index space only while
it has members.

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
- **Untagged chunk:** tag `0`, no index entry; `tag_of` returns `0`; `delete` skips index
  maintenance.
- **Backward compatibility:** an old database opens with all chunks untagged and an empty
  membership index, with no migration step.
- **`update` of a tagged chunk:** the handle entry is COW'd to a new `(page_id, slot)`; the
  immutable tag is copied to the new entry. Membership is unaffected (the handle is stable).
- **Re-tagging:** unsupported by design; the client creates a new chunk and deletes the old
  (the new chunk gets a new handle — the client is responsible for updating any of its own
  references, exactly as it would for any value it chooses to relocate).

## Testing surface

- Round-trip: `allocate_tagged` → `tag_of` matches; `for_each_with_tag` returns exactly the
  members; `delete_all_with_tag` removes all members and frees their value/overflow pages.
- Untagged: tag `0` creates no index entry; `tag_of` returns `0`; zero index growth.
- Self-maintaining delete: `delete(h)` of a tagged chunk removes its index entry; iterating
  the tag afterward does not return it.
- `delete_tagged` with the wrong tag errors and leaves the index intact.
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
- F1 / I12 regression: dropping a relation via `delete_all_with_tag` leaks no handles or
  pages (the original motivation).
- Both backends (in-memory and file) and the cross-engine bench harness exercise the new
  paths.

## Format-version / Don't-Break compliance

- On-disk format changes are additive and backward-compatible (zero = untagged / empty),
  bumping MINOR per I29; no MAJOR break, no reinterpretation of any currently-meaningful
  byte. Pre-1.0, so within the tentative-format window.
- No change to the commit fsync protocol, the pre-drain (I28), the poison model (I1), the
  checksum coverage, the layer dependency graph, or the single-writer contract.
- `PageType = 0x00` reservation untouched; the membership-index pages are ordinary radix
  pages with their own page type and checksum.

## Out of scope for v1 (possible future refinements)

- **Mutable tags.** Cheaply possible now (forward tag is readable), but deliberately
  deferred; immutable for v1.
- **Per-tag summaries** (Bloom filter / `[min,max]` handle range) to accelerate a
  hypothetical handle-across-tags scan — moot while `tag_of` is `O(1)`.
- **Bitmap inner sets** instead of radix, if profiling ever shows dense per-tag handle
  ranges (expected sparse → radix is the right default).
- **Batched per-page frees** in `delete_all_with_tag`, mirroring the deferred I33 batching
  for `delete_many`.

## Relationship to existing tracked work

- **Closes F1 / I12** (`drop_table` / `drop_index_table` handle-and-page leak) —
  `delete_all_with_tag` is the clean bulk-drop primitive those requests wanted.
- **Reuses the I97 enumeration shape** (`for_each_*` callback) for `for_each_with_tag`.
- **Rides I29/I31** format-version machinery for the additive on-disk changes.

## Open questions

- Final API names (`allocate_tagged` vs `allocate_with_tag`, `tag_of` vs `tag`, etc.).
- Whether `delete_all_with_tag` should also accept a per-call cap (like `DefragOptions`)
  to bound the work of dropping a very large relation in one transaction, or whether the
  caller chunks it.
- Whether to expose `delete_tagged`'s verification as the default and make bare `delete`
  the unchecked fast path, or vice versa.
