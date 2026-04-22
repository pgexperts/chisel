# Chisel Issues and Backlog

Tracked work for the Chisel storage engine. Items are grouped by source and
rough category. Each entry carries a priority tag; see legend below.

Sources:
- **[comment-pass]** — found during the 2026-04-10 commenting pass (read-only review, no tests run)
- **[comment-pass 2026-04-17]** — found during the 2026-04-17 re-commenting pass (also read-only; covered changed `src/` files and the new `python/src/` subcrate)
- **[comment-pass 2026-04-22]** — found during the 2026-04-22 third review pass (read-only; five-agent parallel audit over `src/` and `python/src/`)
- **[roadmap]** — from the README roadmap
- **[client]** — requested by the primary Chisel client

Priority legend:
- **P0** — correctness / data loss / unsafe behavior. Fix before relying on Chisel for anything that matters.
- **P1** — real bugs or API pain that block clients or make future work harder. Plan for the next milestone.
- **P2** — known-correct v1 simplifications, latent issues, stat accuracy. Batch with related work.
- **P3** — nice-to-have, forward-compat, speculative, or trivial add-ons to other PRs.

---

## Suggested fix order

> **Status note (2026-04-17):** every item below has landed. The order is preserved here as historical context — a reader looking at this file for "what's open?" should conclude: nothing from the 2026-04-10 or 2026-04-17 review passes is still actionable. The individual entries in later sections carry the definitive status.
>
> **Status note (2026-04-22):** a fresh third review pass re-opened the file with new items I26 (P1, handle-table bounds), I27 (P2, savepoint freed-pages leak on commit), I28 (P2, `CacheFull` poisons during commit), and a doc-sweep bundle (C4). These are NOT in the suggested fix order above — see each entry in its own section below. The 2026-04-22 pass specifically looked for invariant mismatches across module boundaries, which is where the remaining bugs now live.

Dependencies and batching drove this more than raw priority. Earlier items unblocked later ones.

1. **I2** — first commit wipes the only valid superblock. One-day fix, unblocks every other durability guarantee.
2. **I15** — superblock `format_version` validation. One-hour fix, do while I2 is in review.
3. **I6** — `find_leaf` sentinel returns the root as the leaf. Latent corruption; needs a test that forces a sparse handle range.
4. **I1** — commit error handling. Design decided (poison model — see I1 below); implement after I2/I6 so the recovery path is clean.
5. **I18** — `persist_freemap` can reuse pages still referenced by the last-durable superblock. ✅ FIXED 2026-04-17.
6. **F3** — `read()` → `&self`. Do before F2 and I12 pile more API on top; also unblocks R5 (Python bindings).
7. **F2 + I7** — named roots and handle-table rollback tracking. Both touch the handle table / superblock boundary; one coherent PR.
8. **I3 + I4** — rollback file-extension cleanup and `next_page_id` seeding audit.
9. **Freemap bundle: R2 + I9 + I10 + I11 + I12 (F1)** — wire the freemap, plug the leaks, expose bulk delete. One coherent effort; reclamation has to be consistent.
10. **R1** — pack multiple values per data page. Biggest space/perf win; best done on top of a working freemap.
11. **R3 + I17** — selective defrag (and fix the stat accuracy while rewriting the loop).
12. **I13 + I14** — overflow hardening pass.
13. **Page-cache hardening: I19 + I20** — add bounds/asserts on `maybe_evict` and `claim_page`. ✅ FIXED 2026-04-17.
14. **Python binding cleanup: I21–I25** — ergonomics and dead-code audit. ✅ RESOLVED 2026-04-17 (I23 was a false alarm; the other four landed as one PR).
15. **P3 cleanup sweep** — I5, I8, I16, C1–C3, and the "invariants to verify" section.

R4 (configurable superblock count) and R5 (Python bindings) sat outside this order — R4 was gated on I2, R5 on F3. Both have shipped.

---

## Durability and crash safety

### I1. Commit error handling — poison model [comment-pass] — **P0** ✅ IMPLEMENTED 2026-04-10
**Where:** `transaction.rs` `commit()`

**Problem:** `txn_counter` is incremented **before** the linearization write (`write_page(inactive)` + second `fsync`). If either fails:
- The in-memory counter is already bumped.
- `PageCache::flush()` has already cleared dirty flags on the first-phase pages.
- A naive retry produces a `txn_counter` gap and will **not** re-flush the now-clean pages.
- Rollback after partial commit failure is fragile because `rollback()` only discards pages currently dirty in cache.

**Resolution:** Adopt a **poison model** (matches `std::sync::Mutex` semantics). On any commit error, the `TransactionManager` becomes poisoned; the only legal recovery is to `close()` and reopen. Reopen uses the existing shadow-paging recovery path (pick the winning superblock), which returns the database to the last durable state.

**Rationale — fsyncgate:** On Linux (post-2018), a failed `fsync()` cannot be safely retried. The kernel records the error, reports it on the next `fsync()`, then **clears the error state**. A subsequent successful `fsync()` does not mean earlier data is durable — it may have been dropped from the page cache entirely. The only safe response is to treat the file as corrupt-in-memory and start over from a known-good on-disk state. PostgreSQL `PANIC`s on fsync failure for exactly this reason. macOS `F_FULLFSYNC` has similar semantics. Shadow paging + embedded single-writer means reopen is cheap, and it exercises the same recovery code path as a real crash — which is a testing win, not just a correctness one.

**Implementation steps:**
1. Add `poisoned: Option<PoisonReason>` (or `bool` for v1) to `TransactionManager`.
2. Every public entry point checks it first and returns `ChiselError::Poisoned` if set.
3. `commit()` sets the flag on **any** error in its steps. No in-place recovery attempt.
4. `close()` / `Drop` is the only method that may run on a poisoned manager — drops the flock and file handle cleanly.
5. Any fatal `ChiselError` variant encountered outside commit (`ChecksumMismatch`, `CorruptSuperblock`, `IoError`) should also poison — fatal is fatal.
6. Document the recovery procedure: on `Err(Poisoned)`, drop the `Chisel` and call `Chisel::open` again.
7. Add a comment in `PageCache::flush()` noting the window between step 1 and step 5 where cached pages lie about durability, and why it's OK under poison (the manager is about to be discarded).

**Downstream effects:**
- **C3** (cache dirty flags) — no fix needed under poison, but annotate.
- **I3, I4** — rollback still has to revert file extension, independently of poison.
- **F3** (`read()` → `&self`) — poison flag needs to be accessible from `&self` contexts. A `Cell<bool>` or `AtomicBool` solves this; do it as part of F3's refactor.

### I2. First commit can wipe the only valid superblock [comment-pass] — **P0** ✅ FIXED 2026-04-10
**Where:** `transaction.rs` `create_new` + commit slot selection

`create_new` writes slot 0 with `txn_counter = 1` and leaves slot 1 all-zero (invalid). The first user commit increments to 2 (even) and overwrites slot 0 — the only previously valid superblock. A torn write on that commit leaves **no** valid superblock and `open_existing` errors with `CorruptSuperblock`.

Fix: initialize both slots with staggered valid superblocks (e.g., counters 0 and 1) so there is always a fallback.

### I3. Rollback does not revert file extension [comment-pass] — **P1** ✅ FIXED 2026-04-10
**Where:** `transaction.rs` `rollback()`

`rollback()` calls `cache.discard(id)` but never truncates the file or rewinds `next_page_id`. Every rolled-back transaction permanently grows the file with zero-checksum garbage pages. Unreachable, so crash-safe, but leaked until defrag.

### I4. `PageCache::new_page()` may return IDs pointing at post-crash garbage [comment-pass] — **P3** (audit) ✅ RESOLVED 2026-04-10
**Where:** `page_cache.rs` `new_page()`

`next_page_id` is seeded from physical file length in `PageCache::new()`. If a previous crash (see I3) left the file extended past the authoritative superblock's `total_pages`, and the open path forgets to call `set_next_page_id`, `new_page()` returns IDs pointing at stale content.

Audit as part of I3 cleanup: confirm `TransactionManager::open` always resets `next_page_id` from the winning superblock.

### I5. `PageCache::truncate()` silently drops dirty pages [comment-pass] — **P3** ✅ RESOLVED 2026-04-10 (docs only — the behavior is intentional under the watermark rollback design)
**Where:** `page_cache.rs` `truncate()`

No error or debug_assert if discarded entries are dirty. Safe as long as all callers are post-commit, but there is no runtime guard. Add a `debug_assert!(!entry.dirty)` on any future handle-table PR.

### I18. `persist_freemap` can reuse pages the last-durable superblock still references [comment-pass 2026-04-17] — **P0** ✅ FIXED 2026-04-17
**Where:** `transaction.rs` `persist_freemap`

During commit, `persist_freemap` merged `txn_freed_pages` and `old_freemap_page` into `current_freemap` **before** calling `allocate_data_page` to pick a page for the new freemap snapshot. `FreeMap::allocate_first` returns the lowest free id, which was very likely `old_freemap_page` itself or one of the ids just merged from `txn_freed_pages`. The subsequent `claim_page` + `cache.flush()` then overwrote the bytes of a page that the **currently-committed** on-disk superblock still referenced. A crash in the window between that flush and the superblock fsync would leave the last-durable superblock pointing at overwritten bytes.

This directly violated the shadow-paging invariant spelled out in `allocate_data_page`'s own doc comment ("pages reused by the freemap must not be referenced by the currently-committed superblock").

**Fix (landed 2026-04-17):** restructured `persist_freemap` to allocate the new freemap page BEFORE merging `txn_freed_pages` or `old_freemap_page` into `current_freemap`. At the moment of allocation, `current_freemap` still reflects only committed-state frees minus this transaction's allocations, so `FreeMap::allocate_first` can only return a page that was already free in the committed state or a freshly-extended page — both safe. The merges happen AFTER the allocation; the resulting freemap still serializes to disk with those ids marked free, so future transactions can reclaim them.

**Regression test:** `persist_freemap_does_not_reuse_committed_live_pages` in `src/transaction.rs`. It seeds a committed freemap page via overflow-delete (R1 slot-packing otherwise keeps multi-slot data pages live), then runs a second commit whose deletes populate `txn_freed_pages`, and asserts the new `committed_roots.freemap_page` is not in the at-risk set (old freemap page ∪ frozen `txn_freed_pages`). The test is framed as a direct invariant check rather than a crash-injection harness because the at-risk set is observable purely in post-commit internal state.

### I27. `commit()` silently drops `savepoints[*].freed_pages` when savepoints are still active [comment-pass 2026-04-22] — **P2** ✅ FIXED 2026-04-22
**Where:** `transaction.rs` `commit_inner` (the `self.savepoints.clear()` at the end of commit) vs `release_inner` (which DOES merge freed pages back via `merged_freed.extend_from_slice(&sp.freed_pages)`)

`release()` and `commit()` are asymmetric about what happens to a savepoint's `freed_pages`. When a savepoint is released, its `freed_pages` are merged back into the enclosing transaction's `txn_freed_pages`, so commit can return those ids to the freemap. When `commit()` runs with savepoints still on the stack, `commit_inner` simply calls `self.savepoints.clear()` — the per-savepoint `freed_pages` lists are dropped on the floor. `persist_freemap` only iterates `self.txn_freed_pages`, so any page freed in a scope enclosed by an unreleased savepoint is permanently orphaned from the freemap.

**Leak workflow:** `begin → delete(h1) → delete(h2) → savepoint("s") → <more work> → commit`. The pages backing h1 and h2 were moved from `txn_freed_pages` into the savepoint's `freed_pages` by `savepoint_inner`, the savepoint was never released, commit clears the stack, those ids never reach the freemap. Nothing corrupts — the superblock is consistent, the pages are unreachable — but the freemap no longer knows they are reusable. Defrag is the only thing that can reclaim them.

**Why prior passes missed it:** the 2026-04-10 pass predated R2 (freemap wiring) — leaks were known and batched into the freemap bundle. The 2026-04-17 pass focused on the I18 `persist_freemap` restructure and the page-cache hardening; savepoint semantics weren't in scope. The bug has been latent since R2 landed and will trip any workload that commits with a savepoint still active.

**Fix (landed 2026-04-22):** chose option (1) — at the top of `commit_inner`, before `persist_freemap`, iterate every active savepoint and `append` its `freed_pages` onto `self.txn_freed_pages`. This matches `release_inner`'s merge pattern but applied across the full stack. The existing `savepoints.clear()` at step 5 still runs afterwards; we drain rather than iterate-by-reference so the savepoints don't hold stale `freed_pages` if step 5 ever changes. No new error variant, no caller-visible behaviour change beyond the leak going away.

**Regression test:** `commit_with_active_savepoint_returns_freed_pages_to_freemap` in `src/transaction.rs`. Seeds two overflow-sized handles, opens a transaction, deletes both (populating `txn_freed_pages`), takes a savepoint (which empties `txn_freed_pages` into `savepoint.freed_pages`), commits WITHOUT release, and asserts `FreeMap::is_free(&committed_freemap, id)` holds for every previously-captured id. Pre-fix, none of the ids were marked free — they were permanently leaked.

---

## Handle table

### I6. `find_leaf` sentinel returns the root page, not the leaf [comment-pass] — **P0** ✅ FIXED 2026-04-10
**Where:** `handle_table.rs:252`

On a zero child pointer mid-descent, `find_leaf` returns `(root_page_id, 0)` — the original root, **not** the leaf currently being walked. The caller (`lookup`) then reads slot 0 of whatever that page is (possibly an interior page) as a `HandleEntry`.

Today this "works" because:
- For small child-0 page IDs, byte 2 of the little-endian u64 is usually zero and decodes as `HandleFlags::Deleted`.
- For child-0 pointing at page ID ≥ 2^16, byte 2 can be nonzero (0x01 → `Live`, 0x02 → `Overflow`) and **a bogus HandleEntry will be returned for a handle that does not exist.**

Fix: return a proper `Option<(page_id, slot)>` or a distinct sentinel, or have `lookup` check for zero child pointers directly during descent. Resolve C1 at the same time.

### I7. Interior COW pages not recorded in `txn_dirty_pages` [comment-pass] — **P1** ✅ FIXED 2026-04-10
**Where:** `handle_table.rs` `insert_recursive`

Only the final new root is pushed to `txn_dirty_pages`. Intermediate cloned interior pages are dirty in the cache but not tracked for rollback, so `rollback()` will not discard them. A subsequent `cache.flush()` from a future commit will write them to disk as orphans.

Batch with F2 — same area of code.

### I8. `find_leaf` sentinel accidentally relies on page 0 being the superblock [comment-pass] — **P3** ✅ FIXED 2026-04-10
**Where:** `handle_table.rs`

The "zero child pointer is unambiguous" invariant only holds because page 0 is superblock A and never a handle-table node. True today but not enforced. Add an `assert!` when next touching `handle_table.rs`.

### I26. `find_leaf` does not bounds-check `child_idx` against `PTRS_PER_INTERIOR` [comment-pass 2026-04-22] — **P1** ✅ FIXED 2026-04-22
**Where:** `handle_table.rs` `find_leaf` (the descent loop around line 419)

For any `handle >= HandleTable::capacity()`, the descent loop computes `child_idx = remaining / child_span` without bounding the result to `< PTRS_PER_INTERIOR (= 1021)`. The resulting byte offset `DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE` walks off the valid child-pointer region. At the first-overflow boundary (`child_idx == 1021`, reachable with `handle == 520_710` at depth 1) it reads `buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 8]` — the XXH3 checksum bytes of the interior page — and treats that nonzero u64 as a child page id. The descent then calls `cache.get(checksum_as_id)`, which will almost always fail with `InvalidPageId` or `ChecksumMismatch`, both of which the TransactionManager classifies fatal and poisons on. At `child_idx >= 1022` the slice op panics outright with out-of-bounds access.

`lookup` is the only external call site (reached from `read()`, `update()`, `delete()`, `delete_many()`, and `handles()`), and it does not pre-validate the handle. A caller who supplies a u64 larger than the current tree capacity triggers the bug externally — an operational mistake whose expected response is `InvalidHandle` gets escalated to an engine-poisoning fatal error or a process crash.

**Same failure shape as the historical I6**: `find_leaf` reporting wrong information for a handle that does not exist in the tree. `insert` is unaffected because it pre-grows via `while handle >= capacity { grow() }`.

**Fix (landed 2026-04-22):** added a capacity guard at the top of `find_leaf`, scoped to `self.depth > 0`. At depth 0 the existing `handle % ENTRIES_PER_LEAF` is already total (wraps cleanly for any u64) and the descent loop never runs, so the guard only matters on the descent path where the out-of-bounds `child_idx` actually arises. Scoping it this way avoids silently changing depth-0 semantics for callers that happen to pass large handles.

**Regression test:** `lookup_handle_beyond_capacity_returns_none` in `src/handle_table.rs`. Grows the tree to depth 1 and asserts both the first-overflow boundary (`handle == ENTRIES_PER_LEAF * PTRS_PER_INTERIOR`, the historical reads-checksum-as-child case — which without the fix returned `Err(InvalidPageId { page_id: <xxhash of interior page> })`) and `handle == u64::MAX` (the would-panic case) both return `Ok(None)`.

---

## Space leaks (freemap not wired)

These are all facets of the v1 simplification that the freemap bitmap is
built but unused. They become real bugs the moment the freemap is wired up
(R2) — reclamation logic has to know what to reclaim. **Fix all of these
together as the freemap bundle.**

### I9. `update()` inline→inline path leaks the old data-page slot [comment-pass] — **P2** ✅ FIXED 2026-04-10
**Where:** `transaction.rs` `update()`

The old `(page_id, slot_index)` is never freed nor added to `txn_freed_pages`. Combined with "fresh page per insert" (R1), every update leaks a page and consumes a new one.

### I10. `delete()` leaks the Live data-page slot [comment-pass] — **P2** ✅ FIXED 2026-04-10
**Where:** `transaction.rs` `delete()`

Handle-table entry is removed; the data page lingers forever.

### I11. `commit()` drops `txn_freed_pages` on the floor [comment-pass] — **P2** ✅ FIXED 2026-04-10
**Where:** `transaction.rs` `commit()`

The field name "freed" implies reclamation, but the vector is cleared on commit without returning pages to any freemap.

### I12. `delete_subtree(handle)` bulk-delete primitive [client] — **P2** (see also F1) ✅ IMPLEMENTED 2026-04-10 as `delete_many`

> `drop_table` and `drop_index_table` currently leak row/node handles. Chisel's defrag can reclaim them eventually, but a `delete_subtree(handle)` or similar bulk-delete primitive on Chisel would be cleaner.

Needs design: what defines the subtree? Options:
- Client provides an iterator of handles to delete in one transaction.
- Chisel grows a "handle group" concept that can be bulk-freed.
- Handles get optional parent pointers (probably too invasive).

The simplest shape is probably `delete_many(&[Handle])` with atomic semantics. Fold into the freemap bundle.

---

## Other bugs

### I13. `Overflow::write` panics on zero-length values [comment-pass] — **P2** ✅ FIXED 2026-04-10
**Where:** `overflow.rs:50`

For `value.len() == 0`, `num_pages == 0`, no pages are allocated, and the function unconditionally returns `Ok(page_ids[0])` — index out of bounds. Zero-length values stay inline today, so this is unreachable, but it is a latent panic with no defensive check.

### I14. Overflow chain read/delete lack cycle detection [comment-pass] — **P2** ✅ FIXED 2026-04-10
**Where:** `overflow.rs` `read`, `delete`

A corrupt chain with a cycle loops forever. `read` also does not bound `result.len()` against `total_length`. Should bail with `ChiselError::CorruptPage` after exceeding expected length or a cap on chain depth.

### I15. `Superblock::deserialize()` does not validate `format_version` [comment-pass] — **P1** ✅ FIXED 2026-04-10
**Where:** `superblock.rs`

A future v2 file opened by a v1 binary will be silently accepted and fields could be misinterpreted. Add a check against `page::FORMAT_VERSION` in `deserialize` or `select`. Cheap; do it early.

### I16. `PageIo::read_page` returns `UnexpectedEof` instead of `InvalidPageId` [comment-pass] — **P3** ✅ FIXED 2026-04-10
**Where:** `page_io.rs:47`

Error-quality only. Add a bounds check on any PR touching `page_io.rs`.

### I17. Defrag stats count values, not pages [comment-pass] — **P2** ✅ FIXED 2026-04-10
**Where:** `defrag.rs:59-60`

`stats.pages_examined` and `stats.pages_freed` are populated from a per-value counter, not actual page counts. Fix while doing R3.

---

## Page-cache hardening

### I19. `PageCache::maybe_evict` grows unboundedly when every page is dirty [comment-pass 2026-04-17] — **P2** ✅ FIXED 2026-04-17
**Where:** `page_cache.rs` `maybe_evict`

When every cached entry was dirty, the eviction loop broke out and the cache silently grew past `max_pages` without bound. A long-running transaction allocating many `new_page()`s without intervening flushes could exhaust memory.

**Fix:** added a `HARD_CEILING_MULTIPLIER` constant (currently 8×) and a check at the end of `maybe_evict` that returns the new operational `ChiselError::CacheFull { limit }` once `entries.len()` exceeds `max_pages * HARD_CEILING_MULTIPLIER`. The soft-limit semantics for `max_pages` are unchanged — write-heavy transactions can still grow past it — but runaway growth now trips a recoverable error. Caller recovery is to commit (which flushes, freeing the dirty pin) or roll back. The Python binding gets a parallel `CacheFullError` in the OperationalError tier. Tests `cache_full_fires_when_all_pages_dirty_past_hard_ceiling` and `cache_full_is_recoverable_via_flush` in `page_cache.rs` cover trigger and recovery; `fresh_manager`'s test cache size in `transaction.rs` was bumped from 64 to 1024 so existing high-allocation tests stay well under the new ceiling.

### I20. `PageCache::claim_page` silently discards dirty writes on re-insertion [comment-pass 2026-04-17] — **P3** ✅ FIXED 2026-04-17
**Where:** `page_cache.rs` `claim_page`

If the freemap ever handed back an id the current transaction had already dirtied, `claim_page` would silently replace the cached entry and lose the pending writes. The only legitimate caller is `allocate_data_page` via the freemap, which post-I18 is well-behaved — the invariant just wasn't enforced.

**Fix:** added `debug_assert!(!self.is_dirty(page_id), …)` at the top of `claim_page` so a violation surfaces immediately in debug builds rather than as silent data loss hours later. Release builds are unchanged. Test `claim_page_asserts_on_dirty_page` covers the assertion (gated on `cfg(debug_assertions)`).

### I28. `CacheFull` raised during commit's `persist_freemap` poisons the manager [comment-pass 2026-04-22] — **P2** ✅ FIXED 2026-04-22
**Where:** `page_cache.rs` `maybe_evict` (the hard-ceiling check added in I19) × `transaction.rs` `commit()` / `poison_on_fatal`

I19 introduced `ChiselError::CacheFull` as an **operational** error: documented as "caller recovers by committing (flushes → pages become evictable) or rolling back (discards all dirty pages)", and correctly classified `is_fatal() == false`. But `commit_inner` runs `persist_freemap` → `allocate_data_page` → `claim_page` / `new_page` → `maybe_evict` **before** `cache.flush()` drains dirty pages. If `maybe_evict` fires `CacheFull` at that point, the error propagates out of `commit_inner` and `commit()`'s `poison_on_fatal` wrapper poisons the TransactionManager regardless of `is_fatal()`.

The resulting behaviour violates the operational contract in a particularly painful way: the recovery advice is "commit to flush," and commit is precisely what failed. A caller encountering `CacheFull` during commit has no legal action other than `close()` + reopen, which is the poison-model recovery — `CacheFull` was effectively reclassified fatal by the commit wrapper without anyone noticing.

In practice the window is narrow: the transaction has to be at the hard ceiling with every page dirty at the moment `persist_freemap` allocates. But the semantic mismatch is real, and any user who hits it gets an inexplicable downgrade from "operational" to "must-reopen."

**Fix (landed 2026-04-22):** chose option (1) — added `self.cache.borrow_mut().flush()?;` at the top of `commit_inner`, before `persist_freemap`. The drain clears every dirty pin so `persist_freemap`'s own `allocate_data_page` can evict clean pages rather than trip the hard ceiling. `CacheFull` can no longer surface on the commit path. Cost: one extra fsync per commit (2 → 3 total). Consistent with the project's "durability over performance" posture and cheaper than the alternative ("reclassify `CacheFull` as fatal during commit"), which would require caveats throughout the docs and the Python error hierarchy.

**Ordering safety:** the pre-drain does not weaken the shadow-paging invariant. Shadow paging requires "data-page writes durable BEFORE superblock write durable" — step 1's existing flush (now operating on just the one freemap page persist_freemap materializes) still runs between persist_freemap and the superblock write. The pre-drain only shifts user-dirty page writes earlier within the same pre-superblock window; both are part of the same durable write set the superblock linearizes.

**Regression test:** `commit_does_not_poison_when_cache_is_past_hard_ceiling` in `src/transaction.rs`. Constructs a `TransactionManager` with `max_pages=4` (hard ceiling 32), saturates the cache via an allocate-until-`CacheFull` loop in a transaction that also has a non-empty `txn_freed_pages` (so `persist_freemap` does not take its early-exit), then calls `commit()` and asserts both `Ok(())` and `!is_poisoned()`. Pre-fix commit returned `Err(CacheFull { limit: 32 })` and poisoned the manager; post-fix both assertions hold.

---

## Python binding

These are all from the 2026-04-17 pass over the `python/src/` subcrate. Python-side API surface items; none block the Rust core, but they should be settled before R5 ships broadly.

### I21. `PyChisel` latent `RefCell` re-entry hazard [comment-pass 2026-04-17] — **P3** ✅ DOCUMENTED 2026-04-17
**Where:** `python/src/db.rs`

The existing comment said "Python's GIL prevents concurrent re-entry" — true cross-thread, but not for a hypothetical future same-thread Rust→Python→PyChisel callback. No such callback path exists today, so this was a documentation fix only: the comments at the top of `db.rs` and above `with_inner_io` / `with_inner_mut_io` now distinguish cross-thread from same-thread re-entry and spell out what a future callback API would need to do (use `try_borrow_mut` with an explicit reentrancy error, or reshape the engine call so the mutable borrow is released before the callback fires).

### I22. `PySavepoint::rollback_to()` is silently idempotent [comment-pass 2026-04-17] — **P3** ✅ FIXED 2026-04-17
**Where:** `python/src/savepoint.rs`

An explicit second `release()` or `rollback_to()` on a finished savepoint now raises the new `AlreadyFinishedError` (operational tier) rather than silently succeeding. The `__exit__` path intentionally stays idempotent — the `finished` guard short-circuits without raising so normal `with sp:` usage is unaffected whether the user also called an explicit method inside the block. Regression tests: `test_savepoint_second_release_raises`, `test_savepoint_second_rollback_to_raises`, `test_savepoint_explicit_then_with_exit_is_silent`.

### I23. `DuplicateSavepointError` may be dead code [comment-pass 2026-04-17] — **P3** ✅ RESOLVED 2026-04-17 (not actually dead; issue entry was incorrect)
**Where:** `python/src/errors.rs`

The comment-pass entry claimed `ChiselError::DuplicateSavepoint` did not exist in `src/error.rs` and that only `SavepointNotFound(_)` was matched in `to_py_err`. Both assertions were wrong. `ChiselError::DuplicateSavepoint(String)` is declared in `src/error.rs`, is raised by `TransactionManager::savepoint()` when a name is reused (exercised by the existing `operational_error_does_not_poison` unit test at `src/transaction.rs`), and is routed in `python/src/errors.rs::to_py_err` to the Python-side `DuplicateSavepointError` class. No code change needed; this entry is preserved for audit-trail value.

### I24. `PyTransaction` has no explicit `.commit()` / `.rollback()` methods [comment-pass 2026-04-17] — **P3** ✅ FIXED 2026-04-17
**Where:** `python/src/transaction.rs`, `python/chisel/chisel.pyi`

Explicit `.commit()` and `.rollback()` methods are now exposed on `PyTransaction`, mirroring the shape of `PySavepoint.release()` / `.rollback_to()`: both drive the engine and set the `finished` guard so a subsequent `__exit__` short-circuits silently; a second explicit drive raises `AlreadyFinishedError`. `.pyi` stubs updated accordingly. Regression tests: `test_tx_explicit_commit`, `test_tx_explicit_rollback`, `test_tx_second_commit_raises`, `test_tx_commit_then_with_exit_is_silent`.

### I25. `db.close()` silently cancels live `PyTransaction` / `PySavepoint` objects [comment-pass 2026-04-17] — **P3** ✅ FIXED 2026-04-17
**Where:** `python/src/db.rs close()` + `with_inner_mut_io` contract

After `db.close()` clears `inner`, any subsequent call through a still-live `PyTransaction` or `PySavepoint` (including an automatic `__exit__` commit on the enclosing `with` block) now raises the new `ClosedError` (operational tier) instead of `PoisonedError`. The `is_poisoned` getter still reports `True` for a closed handle — it answers the "can this handle still do work?" question — but the distinct exception class lets callers tell "I closed this" apart from "Rust-side corruption". Regression tests: `test_close_then_call_raises_closed`, `test_closed_error_is_not_poisoned_error`, `test_close_inside_transaction_surfaces_as_closed`.

---

## Misleading existing comments

### C1. `handle_table.rs:252` — "Will read as Deleted." — **P3** (byproduct of I6) ✅ RESOLVED 2026-04-10
Resolved alongside I6 — the comment was removed when `find_leaf` was changed to return `Option`.

### C2. `freemap.rs` `allocate_near` — "then falls back to allocate_first" — **P3** ✅ FIXED 2026-04-10
The doc comment claims a fallback to `allocate_first`, but the implementation just exhausts its own outward radius scan and never calls `allocate_first`. Behaviorally equivalent when only one free bit exists, but the doc is wrong. Fix on any freemap-adjacent PR.

### C3. `page_cache.rs` original header — "dirty pages are never evicted" — **P3** (annotate under I1) ✅ RESOLVED 2026-04-10
Annotated in `PageCache::flush()` as part of I1: documented the "durability window" between dirty-flag clearing and the trailing fsync, explained that the I1 poison model is what makes the window benign, and flagged what would need to change if the poison model is ever weakened.

### C4. Documentation sweep [comment-pass 2026-04-22] — **P3** (batch)

The 2026-04-22 pass surfaced a cluster of small doc / sharp-edge items that don't warrant individual entries. Batch into one cleanup PR when convenient.

- **`superblock.rs:152–153`** — field comment on `superblock_count` says "a deserialized value of 0 is treated as 'legacy' and implies the default of 2", but `deserialize` at line 235 rejects any value outside `MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS` (i.e., 0 is rejected, not back-filled). Code is correct (rejecting 0 is the only safe behaviour — a zero modulus would direct a superblock write into the data region). Comment is misleading and could seduce a future developer into adding a bogus legacy-compat branch. Delete the sentence.
- **`superblock.rs:271` (`select()`)** — tie-break is silent: `max_by_key` returns the first maximum in iteration order (lowest slot index). Undocumented. Add one line explaining the policy.
- **`error.rs:147` (`CorruptSuperblock` Display)** — message is "no valid superblock found", which doesn't distinguish "all slots fail XXH3" from "all slots had out-of-range `superblock_count`" (both emit the same variant). Consider either threading a reason through or enriching the Display to name the failure mode.
- **`stats.rs:18–19` (`file_size_bytes` comment)** — says "may exceed `total_pages * PAGE_SIZE` briefly during a commit in progress", which implies a concurrent observer. Chisel is single-writer; there's no such observer. Reword to describe the real cause: "may exceed `total_pages * PAGE_SIZE` if a crashed commit left orphan pages in the file tail."
- **`page_cache.rs:29–33` (header)** — claims the cache "may transiently hold `max_pages + 1` entries", but under the I19 hard-ceiling design it can legitimately grow further while dirty pages pin out eviction. Tighten to "may grow by one per allocation when eviction is blocked, up to `max_pages * HARD_CEILING_MULTIPLIER`."
- **`page_cache.rs` — `max_pages == 0` sharp edge** — `new(io, 0)` trips `CacheFull { limit: 0 }` on the first allocation because `0 * 8 = 0`. Not exploitable (callers don't pass 0), but worth an `assert!(max_pages >= 1)` or a `max_pages.max(1)` clamp in `PageCache::new`.
- **`freemap.rs` — stale "NOT WIRED IN" note** — a header comment still flags the freemap as unwired. R2 wired it in 2026-04-10. Delete.
- **`transaction.rs:1117–1131` (`read`)** — two consecutive `///` doc paragraphs both start with "Read a value by handle." Merge into one.
- **`python/chisel/__init__.py:64` (`DefragOptions.max_pages` docstring)** — says "cap on pages examined in one pass"; the actual cap counts values relocated, not pages touched. `defrag.rs:63–66` acknowledges the legacy name internally but the Python-facing docstring propagates the confusion. Update to "cap on values relocated in one pass (0 = no limit); the name is a legacy carry-over."
- **`python/src/transaction.rs:1–28` (file header)** — the Design note section describes the explicit `commit()` / `rollback()` added in I24, but the very first "Semantics:" block above it lists only `__enter__` / `__exit__`. Add one sentence to the semantics block noting the explicit methods and their `finished`-guard contract.

Also one discussion item, not a fix: **`LockFailed` classification** is currently under `FatalError` in the Python error hierarchy. `LockFailed` can only fire at `open()` before a TransactionManager exists, so it cannot poison anything — but the database file itself is intact, which puts it closer to the "operational" tier conceptually. Either re-parent to `OperationalError` (alongside `DatabaseFileNotFoundError`) or document why it sits under `FatalError` despite the file being undamaged. Design call, not a bug.

---

## Invariants to verify — **P3** (one-pass audit) ✅ RESOLVED 2026-04-10

Audit pass on the assumptions added during the 2026-04-10 commenting pass. Results inline:

- `page.rs`: **corrected** — checksum is validated on every disk LOAD (cache miss), not on every cache hit. The old annotation was imprecise. Updated to "validates this checksum on every disk LOAD; cache hits skip revalidation".
- `superblock.rs`: "two slots at fixed page ids 0 and 1, alternating by commit" — **verified** against `create_new` (writes slots 0 and 1) and `commit_inner` (alternates by counter parity).
- `superblock.rs`: "orphaned pages from a crashed commit are cleaned up on next mount" — **corrected**. They are NOT actively cleaned; they remain as dead weight. `open_existing` reseeds `next_page_id` from the authoritative superblock's `total_pages` (I4), so subsequent allocations overwrite the garbage tail. Rewrote the comment to describe this accurately.
- `error.rs`: "reopen after fatal error may recover via alternate superblock" — **corrected** and softened. Only `CorruptSuperblock` on the active slot is recoverable that way; other fatal variants (`ChecksumMismatch`, `IoError`, etc.) indicate damage to the last-committed snapshot itself. Comment now notes the I1 poison-model requires close-and-reopen regardless.
- `data_page.rs`: bytes 8..16 `txn_counter` — **corrected**. The field is allocated in the on-disk layout but NOT written by any live module (init_page zeroes it, compact() faithfully preserves zeros). Re-labeled as "reserved for a future per-page txn_counter".
- `data_page.rs`: byte 1 "reserved / padding" — **verified**; no module reads or writes it.
- `overflow.rs`: bytes 1..16 labeled as "alignment padding to keep the 16-byte common-header shape" — **corrected**. The 16-byte shape is `DATA_PAGE_HEADER_SIZE`, not `COMMON_HEADER_SIZE` (which is 12). Comment now distinguishes the two.
- `handle_table.rs`: depth recovery via leftmost-spine walk is correct **only** because `grow()` installs the old root at child index 0 — **verified** by inspection.
- `page_cache.rs`: read-only opens intentionally take `LOCK_EX` — **verified**. The existing comment in `page_io::open` says so explicitly ("even a reader needs to block concurrent writers"). Not an oversight; intentional for the single-writer shadow-paging model.
- `page_cache.rs`: superblocks bypass the cache entirely — **verified** for BOTH the write path (`commit_inner` → `io_mut().write_page`) AND the read path (`open_existing` → `io_mut().read_page`). `io_mut` doc expanded to document both call sites.

---

## Roadmap items

From README.md, restated here for visibility.

### R1. Pack multiple values per data page [roadmap] — **P2** ✅ IMPLEMENTED 2026-04-10
> Currently each value gets its own page; packing small values together will significantly reduce file size and improve cache efficiency.

Biggest space/perf win. Best done on top of a working freemap (R2) so page-free-space tracking has a home.

### R2. Wire the free page map into the allocator [roadmap] — **P2** ✅ IMPLEMENTED 2026-04-10
> The bitmap is built but allocations currently extend the file; reusing free pages will eliminate file growth after delete-heavy workloads.

Anchor of the freemap bundle. Depends on resolving I9–I11: reclamation needs to know what to reclaim. Interacts with I3 (rollback file extension) and I4 (`next_page_id` seeding).

### R3. Selective defragmentation [roadmap] — **P2** ✅ IMPLEMENTED 2026-04-10
> Consolidate only sparse pages instead of re-inserting every value.

Fix I17 (defrag stats) while you're rewriting the loop.

### R4. Configurable superblock count [roadmap] — **P3** (gated on I2) ✅ IMPLEMENTED 2026-04-11
> Trade commit performance for additional crash durability (3+ superblock copies).

I2 must be fixed first — the "first commit wipes the only valid superblock" bug affects any N ≥ 2.

### R5. Python bindings [roadmap] — **P3** (gated on F3) ✅ SHIPPED 2026-04-17 (across the python-binding commit series; see `python/` subcrate and the I21–I25 follow-up batch)
> PyO3-based wrapper exposing the full Chisel API to Python, including context managers for transactions and savepoints.

Formally gated on F3 (so `&self` reads flow through without wrapping); the binding then landed incrementally as a separate PyO3 subcrate under `python/` with its own `Cargo.toml` / `pyproject.toml` / `maturin develop` workflow, and was further polished by the I21–I25 binding-cleanup batch (explicit `PyTransaction.commit()` / `.rollback()`, `AlreadyFinishedError` on double-drive, `ClosedError` distinct from `PoisonedError`, and RefCell reentrancy docs).

---

## Client feature requests

### F1. `delete_subtree(handle)` bulk-delete primitive [client] — **P2** ✅ IMPLEMENTED 2026-04-10 as `delete_many`
See I12 — filed in the leaks section because it is the cleanest fix for the client's current orphan-handle problem in `drop_table` / `drop_index_table`. Part of the freemap bundle.

### F2. Named roots [client] — **P1** ✅ IMPLEMENTED 2026-04-10
**Motivation (from the client):**
> `rollback` and `rollback_to` reset `meta_root` to 0 on the assumption that handle 0 is always the meta B-tree root. This holds today because `init_meta_root` allocates handle 0 on a fresh database and Chisel preserves handles across updates. But if the meta B-tree ever gets deleted and re-allocated, handle 0 would be orphaned and some other handle would be the root.

This is a latent correctness bug disguised as a feature request — the client is currently relying on an unwritten invariant.

Proposed API:
```rust
db.set_root_name("meta", handle)?;
let handle = db.get_root_name("meta")?;
```

Named roots would be stored in the superblock (small fixed-size table, or a small dedicated root-names page pointed to from the superblock). The key property is that they survive commit/rollback the same way the handle-table root does, and are not themselves handles that need tracking.

Design questions (open):
- How many named roots? A fixed small count (e.g., 8) keeps the superblock layout simple.
- Max name length? 16 or 32 bytes is probably plenty.
- Do they take effect at commit time, or immediately? Commit time matches the transactional semantics the client needs.

Batch with I7 — both touch handle table / superblock.

### F3. `read()` should take `&self`, not `&mut self` [client] — **P1** ✅ IMPLEMENTED 2026-04-10
**Motivation (from the client):**
> Chisel's `read()` takes `&mut self` because it goes through the mutable page cache. That forced us to use `RefCell<Chisel>` in `ChiselStorage` for the `&self` read methods on `StorageEngine`. If Chisel internally did its own interior mutability (a `RefCell` or `UnsafeCell` around the page cache), `read()` could take `&self` and we'd eliminate our wrapper layer entirely.

Pervasive change — reaches from `Chisel::read` down through `TransactionManager`, `PageCache`, and `PageIo`. Cleanest approach is probably a `RefCell<PageCache>` (or `Mutex` if we ever want `Sync`) inside `TransactionManager`, since everything flows through `PageCache`.

**Design question (open):** `RefCell<PageCache>` (single-threaded, no `Sync`, cheapest) or `Mutex<PageCache>` (leaves the `Sync` door open)? Client is single-threaded today, but committing to single-threaded in the type system is hard to undo.

Interacts with I5 (truncate dropping dirty pages), I7 (rollback not tracking all dirty pages), and I1's poison flag — all become harder to reason about under interior mutability if reads and writes can now interleave even within a single thread. Do F3 *before* F2/I12 pile more API on top; also unblocks R5.
