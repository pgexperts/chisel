# Chisel Issues and Backlog

Tracked work for the Chisel storage engine. Items are grouped by source and
rough category. Each entry carries a priority tag; see legend below.

Sources:
- **[comment-pass]** — found during the 2026-04-10 commenting pass (read-only review, no tests run)
- **[comment-pass 2026-04-17]** — found during the 2026-04-17 re-commenting pass (also read-only; covered changed `src/` files and the new `python/src/` subcrate)
- **[roadmap]** — from the README roadmap
- **[client]** — requested by the primary Chisel client

Priority legend:
- **P0** — correctness / data loss / unsafe behavior. Fix before relying on Chisel for anything that matters.
- **P1** — real bugs or API pain that block clients or make future work harder. Plan for the next milestone.
- **P2** — known-correct v1 simplifications, latent issues, stat accuracy. Batch with related work.
- **P3** — nice-to-have, forward-compat, speculative, or trivial add-ons to other PRs.

---

## Suggested fix order

Dependencies and batching drive this more than raw priority. Earlier items unblock later ones.

1. **I2** — first commit wipes the only valid superblock. One-day fix, unblocks every other durability guarantee.
2. **I15** — superblock `format_version` validation. One-hour fix, do while I2 is in review.
3. **I6** — `find_leaf` sentinel returns the root as the leaf. Latent corruption; needs a test that forces a sparse handle range.
4. **I1** — commit error handling. Design decided (poison model — see I1 below); implement after I2/I6 so the recovery path is clean.
5. **I18** — `persist_freemap` can reuse pages still referenced by the last-durable superblock. Violates shadow-paging invariant; needs a crash-injection test and a fix that either excludes in-flight freemap consolidation ids from allocation or defers the merge until after the superblock fsync.
6. **F3** — `read()` → `&self`. Do before F2 and I12 pile more API on top; also unblocks R5 (Python bindings).
7. **F2 + I7** — named roots and handle-table rollback tracking. Both touch the handle table / superblock boundary; one coherent PR.
8. **I3 + I4** — rollback file-extension cleanup and `next_page_id` seeding audit.
9. **Freemap bundle: R2 + I9 + I10 + I11 + I12 (F1)** — wire the freemap, plug the leaks, expose bulk delete. One coherent effort; reclamation has to be consistent.
10. **R1** — pack multiple values per data page. Biggest space/perf win; best done on top of a working freemap.
11. **R3 + I17** — selective defrag (and fix the stat accuracy while rewriting the loop).
12. **I13 + I14** — overflow hardening pass.
13. **Page-cache hardening: I19 + I20** — add bounds/asserts on `maybe_evict` and `claim_page`. Batch with any future PageCache refactor.
14. **Python binding cleanup: I21–I25** — ergonomics and dead-code audit; batch as one PR once the Rust-side dust has settled.
15. **P3 cleanup sweep** — I5, I8, I16, C1–C3, and the "invariants to verify" section.

R4 (configurable superblock count) and R5 (Python bindings) sit outside this order — R4 is gated on I2, R5 is gated on F3.

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

### I18. `persist_freemap` can reuse pages the last-durable superblock still references [comment-pass 2026-04-17] — **P0**
**Where:** `transaction.rs:634–662` `persist_freemap`

During commit, `persist_freemap` merges `txn_freed_pages` and `old_freemap_page` into `current_freemap` (steps 1 and 3) **before** calling `allocate_data_page` (step 4) to pick a page for the new freemap snapshot. `allocate_data_page` may reuse from `current_freemap`; `allocate_first` returns the lowest free id, which is very likely `old_freemap_page`. The subsequent `claim_page` + `cache.flush()` then overwrites the bytes of a page that the **currently-committed** on-disk superblock still references. A crash in the window between that flush and the superblock fsync leaves the last-durable superblock pointing at overwritten bytes.

This directly violates the shadow-paging invariant spelled out in `allocate_data_page`'s own doc comment ("pages reused by the freemap must not be referenced by the currently-committed superblock"). A comment flagging the hazard was added inline during the 2026-04-17 pass.

**Severity:** potentially serious — defeats the "old state untouched until swap" guarantee that the whole durability story depends on.

**Fix candidates:**
- Defer the merge of `old_freemap_page` and `txn_freed_pages` until after the superblock fsync, keeping the committed snapshot's pages off-limits for the duration of the commit.
- Or, pass `allocate_data_page` an "exclusion set" containing `old_freemap_page` and anything in `txn_freed_pages` that the last-durable superblock still references.

**Test:** crash-injection regression between the `persist_freemap`-triggered `cache.flush()` and the superblock fsync; after reopen, verify the last-durable tree is intact.

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

### I19. `PageCache::maybe_evict` grows unboundedly when every page is dirty [comment-pass 2026-04-17] — **P2**
**Where:** `page_cache.rs:446–463`

When every cached entry is dirty the eviction loop `break`s and the cache silently grows past `max_pages`. The code comments this as a deliberate soft-limit (dirty pages may not be evicted without losing writes), but there is no upper bound and no warning. A long-running transaction that calls `new_page()` many times without intervening flushes can exhaust memory.

**Fix candidates:**
- Add a hard ceiling (e.g., `max_pages * K`) that returns `ChiselError::CacheFull` rather than continuing to grow.
- Or, log / `debug_assert!` on an all-dirty eviction so runaway growth shows up in tests.

### I20. `PageCache::claim_page` silently discards dirty writes on re-insertion [comment-pass 2026-04-17] — **P3**
**Where:** `page_cache.rs:354–367`

If the freemap ever hands back an id that the current transaction has already dirtied, `claim_page` will replace the cached entry and lose the pending writes. The only current caller is `allocate_data_page` via the freemap, which is not supposed to return an already-dirty id; the invariant is just not enforced.

**Fix:** add `debug_assert!(!self.is_dirty(page_id))` to `claim_page`. Trivial; batch with any future freemap or cache change (see I19 and I18).

---

## Python binding

These are all from the 2026-04-17 pass over the `python/src/` subcrate. Python-side API surface items; none block the Rust core, but they should be settled before R5 ships broadly.

### I21. `PyChisel` latent `RefCell` re-entry hazard [comment-pass 2026-04-17] — **P3**
**Where:** `python/src/db.rs:108–118` (and every `with_inner_mut_io` caller)

Pyclass methods take `&self` and internally `borrow_mut()` through `with_inner_mut_io`. The existing comment claims "Python's GIL prevents concurrent re-entry" — true for **cross-thread** callers, but **not** for same-thread Rust→Python→PyChisel callbacks. No such path exists today (Chisel has no Python-side callbacks in its Rust API), but any future engine callback that dispatches into Python would deadlock on `borrow_mut`.

**Fix:** document the constraint explicitly, and if/when a callback API is introduced, convert to `try_borrow_mut` with an explicit reentrancy error — or use a different interior-mutability story. Revisit when R5's public surface expands.

### I22. `PySavepoint::rollback_to()` is silently idempotent [comment-pass 2026-04-17] — **P3**
**Where:** `python/src/savepoint.rs:66–83`

Calling `rollback_to()` twice on the same savepoint (without an intervening savepoint re-creation) succeeds silently the second time because of the `finished` guard. A user who writes `sp.rollback_to()` in an `if` branch and then exits the `with` block also silently succeeds. Arguably correct, but it masks a "called `rollback_to` on the wrong savepoint object" bug.

**Fix candidates:**
- Raise `AlreadyFinishedError` on the second call, matching the transaction API's usual idempotency-as-error stance.
- Or document the idempotency explicitly and leave it.

### I23. `DuplicateSavepointError` may be dead code [comment-pass 2026-04-17] — **P3**
**Where:** `python/src/errors.rs`

`DuplicateSavepointError` is declared in the Python exception tree, but no `ChiselError::DuplicateSavepoint` variant appears to exist in `src/error.rs` — only `SavepointNotFound(_)` is matched in `to_py_err`. Either the Rust-side variant is missing a raise path, or the Python-side exception is reachable only by name and should be removed.

**Fix:** audit `src/error.rs` for a duplicate-savepoint case; either wire it through or drop the Python class. Batch with I21.

### I24. `PyTransaction` has no explicit `.commit()` / `.rollback()` methods [comment-pass 2026-04-17] — **P3**
**Where:** `python/src/transaction.rs`, `python/chisel/chisel.pyi`

The `finished: Cell<bool>` guard inside `PyTransaction` is structured as if `.commit()` / `.rollback()` were exposed explicitly (the guard short-circuits the second call), but they are not. The `.pyi` stubs do not list them either. Users are limited to the `with db.transaction():` context-manager form.

**Decision needed:** either expose explicit `.commit()` / `.rollback()` and keep the guard (matches savepoint shape), or remove the guard machinery as unused. Consistent with savepoint API is probably preferable.

### I25. `db.close()` silently cancels live `PyTransaction` / `PySavepoint` objects [comment-pass 2026-04-17] — **P3**
**Where:** `python/src/db.rs close()` + `with_inner_mut_io` contract

After `db.close()` clears `inner`, any subsequent call through a still-live `PyTransaction` or `PySavepoint` returns `PoisonedError` (because `with_inner_mut_io` sees `None`). Calling `db.close()` inside a `with db.transaction():` block therefore cancels the transaction, and the `with`-exit commit then raises `PoisonedError` while attempting to commit.

This is arguably graceful, but surprising enough to deserve docs. Possibly upgrade to a dedicated `ClosedError` so the user can tell "closed underneath me" from "Rust-side poison."

---

## Misleading existing comments

### C1. `handle_table.rs:252` — "Will read as Deleted." — **P3** (byproduct of I6) ✅ RESOLVED 2026-04-10
Resolved alongside I6 — the comment was removed when `find_leaf` was changed to return `Option`.

### C2. `freemap.rs` `allocate_near` — "then falls back to allocate_first" — **P3** ✅ FIXED 2026-04-10
The doc comment claims a fallback to `allocate_first`, but the implementation just exhausts its own outward radius scan and never calls `allocate_first`. Behaviorally equivalent when only one free bit exists, but the doc is wrong. Fix on any freemap-adjacent PR.

### C3. `page_cache.rs` original header — "dirty pages are never evicted" — **P3** (annotate under I1) ✅ RESOLVED 2026-04-10
Annotated in `PageCache::flush()` as part of I1: documented the "durability window" between dirty-flag clearing and the trailing fsync, explained that the I1 poison model is what makes the window benign, and flagged what would need to change if the poison model is ever weakened.

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

### R5. Python bindings [roadmap] — **P3** (gated on F3)
> PyO3-based wrapper exposing the full Chisel API to Python, including context managers for transactions and savepoints.

Blocked on F3 — PyO3 wants `&self` methods too, so fixing once helps both clients. Don't ship bindings until the API shape is settled.

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
