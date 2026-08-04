---
id: 0005
title: Spillway sidecar file over hard ceiling
date: 2026-05-04
status: Accepted
---

# 0005. Spillway sidecar file over hard ceiling

**Context:** The page cache (`page_cache.rs`) has a strict size cap (`Options::cache_max_bytes`). When every cached entry is dirty (no clean page available for eviction), the cache cannot accept a new dirty page without violating its bound. The pre-2026-05-04 design used a `HARD_CEILING_MULTIPLIER = 8` to allow temporary growth past `cache_max_bytes` up to 8× the configured limit, then errored with `CacheFull`. This worked for moderate transactions but failed silently on workloads that legitimately needed to dirty more than 8× the cache: notably, the bench-suite scenarios with `document-store`'s log-normal value sizes.

**Decision:** Replaced the `HARD_CEILING_MULTIPLIER` elasticity with a *spillway*: a sidecar file `<db_path>.spillway` that absorbs LRU-tail dirty pages when the cache is full of dirty pages. The cache becomes a strict bound (no elasticity). The spillway is bounded by `Options::spillway_max_bytes` (default `1024 × cache_max_bytes` = 8 GiB at the 8 MiB cache default). Spillway slots carry their own per-slot XXH3 checksum over `page_id || page_bytes`, distinct from the main-file page checksum. The spillway is never `fsync`ed — its content does not need to survive a crash; it's truncated at open and at every commit/rollback. Setting `Options::spillway_max_bytes = 0` disables the spillway and restores `CacheFull`-at-cap semantics.

**Alternatives considered:**

- *Keep the 8× ceiling, raise the multiplier.* Would just delay the same problem. Bench-suite document-store workloads can dirty 100× the cache.
- *No bound at all (unbounded growth).* Rejected — embedded engines must respect their configured memory budget, and "unbounded dirty" is an OOM path under sustained write workloads.
- *Disk-spill into the main file using a reserved area.* Adds a permanent on-disk artifact that has to be checksum-protected and durability-managed. Rejected because the spillway's contents are by definition uncommitted; never needing to fsync them is a key simplification.

**Consequences:**

- *Positive:* Bench-suite workloads that exceed cache size now succeed. `document-store` was the motivating case.
- *Positive:* Strict cache bound. The `cache_max_bytes` budget is now actually a budget, not a guideline.
- *Positive:* The spillway's "never fsync" policy is correct because its contents are uncommitted dirty state. A crash with a non-empty spillway just discards its contents on the next open — the previous committed superblock is still active.
- *Negative:* The no-spill commit cost is now 3 fsyncs (I28 pre-drain + main-pages flush + superblock), not 2. The pre-drain handles a subtle interaction in the commit protocol: `persist_freemap`'s `allocate_data_page` could trip `maybe_evict`'s spill-or-error path mid-commit if every cached page is dirty; pre-draining clears every dirty pin so the strict cap is reachable via normal eviction.
- *Negative:* New error variant `SpillwayFull { limit_bytes }` fires when both cache and spillway are exhausted. Operational, recoverable via commit/rollback.
- *Breaking change:* `Options::cache_size: usize` (page count) → `Options::cache_max_bytes: u64` (bytes); default unchanged at 8 MiB.

Spec: `docs/superpowers/specs/2026-05-03-chisel-spillway-design.md` (frozen at decision time). **Update (2026-06-30):** for encrypted databases the spillway slot widens to carry the 8232-byte sealed on-disk unit and stores *ciphertext* (seal-once on evict, verbatim copy on drain); its per-slot XXH3 still guards the round-trip. See ADR-15.

---
