---
id: 0012
title: Chunk tags + reverse membership index
date: 2026-06-02
status: Accepted
---

# 0012. Chunk tags + reverse membership index

**Context:** The relational layer that builds on Chisel (the primary client) needs three operations the bare handle API cannot do efficiently: sequentially scan all chunks of one relation, drop a whole relation, and delete a single chunk while removing it from its relation's set — all without an `O(all chunks)` pass. Chisel had no notion of grouping chunks; the client could store a group id inside each value and scan everything, but that is linear in the whole database per operation.

**Decision:** Attach an optional, immutable `u32` **tag** to each chunk at allocation (`allocate_tagged(value, tag)`); `tag == 0` is the "untagged" sentinel. The mapping is split into a *forward* map and a *reverse* map. The forward map (handle → tag) lives in 4 of the `HandleEntry`'s 5 previously-reserved bytes, so `tag(handle)` is `O(1)` off the entry the engine already loads, and a bare `delete(handle)` self-maintains the index by reading the tag from the entry. The reverse map (tag → {handles}) is a two-level copy-on-write radix (`MembershipIndex` over the generic `RadixU64`): an outer tree keyed by tag whose leaf value bit-packs `(inner_depth | inner_root)`, and a per-tag inner tree keyed by handle. `delete_with_tag(tag, max)` is the bounded relation-drop primitive (returns `TagDropProgress { deleted, complete }`; the caller loops `begin → delete_with_tag → commit` until `complete`).

**Alternatives considered:**

- *Store the group id in the value, scan all chunks.* `O(all chunks)` per scan/drop. Rejected — the membership index makes scan and drop `O(members of the relation)`.
- *Single flat radix over a packed `(tag:handle)` key.* Rejected in favor of the two-level form, which reuses the existing `u64`-radix shape directly and makes "enumerate the distinct tags" a first-class `O(T)` operation.
- *`u64` tag.* Rejected — a `u32` fits in the `HandleEntry`'s reserved bytes, which is what makes forward storage *free* (no side table, no extra page reads). `2^32 − 1` tags is effectively unbounded for relation ids.
- *Mutable tags.* Cheap to support (the forward tag is readable), but deliberately deferred; immutability removes the retag code path entirely. To re-tag, the client allocates a new chunk and deletes the old.
- *Bitmap inner sets.* Deferred — per-tag handle sets are expected sparse, so radix is the right default.

**Consequences:**

- *Positive:* Untagged chunks cost nothing — tag `0`, no index entry, no index COW. The feature is invisible to any workload that does not use it.
- *Positive:* Reuses the existing radix machinery, COW discipline, superblock-anchored root, freemap-backed page reclaim, and poison model — no new commit-protocol surface. "Radix all the way down," not a foreign B-tree.
- *Positive:* Backward compatible by construction. Old databases open with all chunks untagged and an empty index (zeroed bytes read as tag `0` / `PAGE_ID_NONE`); no migration. The on-disk additions ride ADR-7's two-tier versioning as a MINOR bump.
- *Negative:* 4 of the 5 per-entry reserved bytes are permanently committed to the tag; the 5th (byte `[15]`) became the client byte (ADR-14).
- *Negative:* `delete_with_tag` is bounded by `max`; dropping a large relation requires the caller to loop until `complete`, each batch a separately-durable transaction (this bound also caps a drop's working set within the cache/spillway budget).
- *Reversibility:* Medium — it is a new on-disk subsystem (the `HandleEntry` tag field, the superblock `root_membership_index_page`, and `membership_index.rs`). Because the format additions are additive, reversal reverts them without affecting existing readers, but it removes a module and a public API cluster.

Spec: `docs/specs/2026-06-02-chunk-tags-design.md`. See ARCHITECTURE.md "Chunk tags (the membership index in use)" and "Membership index pages." Closes the `drop_table` / `drop_index_table` handle-and-page leak (ISSUES.md F1 / I12).

---
