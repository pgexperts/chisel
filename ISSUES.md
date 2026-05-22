# Chisel Issues and Backlog

Tracked work for the Chisel storage engine. Items are grouped by source and
rough category. Each entry carries a priority tag; see legend below.

Sources:
- **[comment-pass]** — found during the 2026-04-10 commenting pass (read-only review, no tests run)
- **[comment-pass 2026-04-17]** — found during the 2026-04-17 re-commenting pass (also read-only; covered changed `src/` files and the new `python/src/` subcrate)
- **[comment-pass 2026-04-22]** — found during the 2026-04-22 third review pass (read-only; five-agent parallel audit over `src/` and `python/src/`)
- **[perf-review 2026-04-26]** — found during the `chisel-performance` skill fresh-eyes pass after PR-1 + PR-2 of the bench-suite series landed (read-only; output at `docs/reviews/perf-review-2026-04-26.md`)
- **[deepdive 2026-05-22]** — found during the `deepdive-rust` skill fresh-eyes pass after the spillway feature and the runtime-config setters landed (read-only; output at `docs/reviews/review-20260522-073901.md`)
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
> **Status note (2026-04-22):** a fresh third review pass re-opened the file with new items I26 (P1, handle-table bounds), I27 (P2, savepoint freed-pages leak on commit), I28 (P2, `CacheFull` poisons during commit), and a doc-sweep bundle (C4), all resolved the same day. Two pre-1.0 infrastructure items also landed that day: I29 (split `format_version` into packed MAJOR / MINOR so the README's "sacred within a major version" promise is enforceable at the bytes level) and I31 (per-page format-version byte + 64-bit reserved common-header region, the foundation for lazy per-page upgrade). These are NOT in the suggested fix order above — see each entry in its own section below. The 2026-04-22 pass specifically looked for invariant mismatches across module boundaries, which is where the remaining bugs now live.
>
> **Status note (2026-05-22):** the deepdive-rust fresh-eyes review (output at `docs/reviews/review-20260522-073901.md`) adds I35–I71 in a new "Deepdive review findings (2026-05-22)" section below. The cluster is dominated by 1.0-readiness work: public-API surface (`pub mod` exposure of engine internals, missing `#[non_exhaustive]` on `Options` / `ChiselError` / `DrainInsertion` / `SpillwayLocation`), Cargo.toml publication metadata gaps, `License: TBD` in the README, and a small batch of code-quality, performance, and doc-drift items. None are correctness bugs. Highest leverage before the 1.0 freeze: I35 (`pub` → `pub(crate)` reshape), which forces the urgency of I36 (`#[non_exhaustive]` on the types that remain public); then I54–I57 (CI supply-chain check + MSRV pin + publication metadata + license) to unblock crates.io publication. The 2026-04-26 perf-review's deltas (F1/F4/F5 resolved, F2/F3/F6 unchanged) are recorded inline at the top of the new section.

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

### I29. Split `format_version` into packed MAJOR / MINOR for public stability promise [infrastructure 2026-04-22] — **P1** (pre-1.0 foundation) ✅ PHASE 1 LANDED 2026-04-22
**Where:** `page.rs` `FORMAT_VERSION`, `transaction.rs::open_existing` gate at the I15 site

**Motivation:** the README's "sacred within a major version" promise requires distinguishing additive minor changes from structural major changes in the on-disk marker. The pre-I29 scheme was a flat `u32 FORMAT_VERSION = 2` checked with exact equality, which conflated the two: any change bumped the number, any mismatch rejected the file. Layering the public stability guarantee on top of that would have required interpretation conventions that lived outside the field itself.

**Scheme (byte-packed u32):**
- Upper 16 bits = MAJOR. Lower 16 bits = MINOR.
- `FORMAT_MAJOR_VERSION` and `FORMAT_MINOR_VERSION` are `u16` constants; `FORMAT_VERSION` is derived by `pack_format_version(major, minor)` at compile time.
- First 1.0 release: MAJOR = 1, MINOR = 0, `FORMAT_VERSION = 0x00010000`.
- Helpers: `pack_format_version(major, minor)`, `format_major(v)`, `format_minor(v)`. All `const fn` so they compose in constants.

**Why packed over decimal-coded** (e.g. `major * 100 + minor`): semantics compile into the data-type (`>> 16`, `& 0xFFFF`) rather than relying on a "why 100?" arithmetic convention. Same `u32` on-disk width, same superblock bytes 4..8.

**Phase 1 (landed 2026-04-22):** open-time gate now compares MAJOR only (`format_major(sb.format_version) != FORMAT_MAJOR_VERSION`). A file written by any 1.x binary opens in any other 1.x binary regardless of minor drift — which is what makes the README promise true. Minor-newer files are accepted as read+write for now because there are no minor variants yet to protect.

**Phase 2 (deferred until 1.1 run-up):** add a "refuse writes if file MINOR > binary MINOR" arm to protect against a binary silently clobbering superblock fields added in a later minor. Likely shape: introduce a new operational error (`NewerFormatMinor`) or reuse `ReadOnlyMode`; set a flag on the `TransactionManager` at open time; check it in `begin_inner`. No-op today (no newer-minor files exist), so deferring costs nothing but documenting the intent now.

**Pre-1.0 compatibility note:** any file written by a prior development build carries `format_version = 1` or `format_version = 2` in the flat scheme. Under the packed interpretation those decode as MAJOR = 0, MINOR = 1 or 2. MAJOR = 0 ≠ current MAJOR = 1 → rejected with `UnsupportedFormatVersion`. This is the documented pre-1.0 break; there are no production DBs to migrate, and release notes call it out. MAJOR = 0 is implicitly reserved forever as "pre-1.0 development" and will never be written by a released binary.

**Regression test:** `format_version_gate_is_major_only` in `src/transaction.rs`. Creates a fresh database, closes, patches both superblock slots to (a) the current major with a bumped minor — asserts open succeeds (pre-fix this rejected with `UnsupportedFormatVersion`); then patches to a bumped major — asserts open fails with `UnsupportedFormatVersion`. Exercises both halves of the MAJOR-only check.

### I31. Per-page format version byte + reserved common-header space [infrastructure 2026-04-22] — **P1** (pre-1.0 foundation) ✅ PHASE 1 LANDED 2026-04-22
**Where:** `page.rs` `page_format_version` / `PAGE_FORMAT_VERSION_CURRENT` / `COMMON_RESERVED_*`; every non-superblock page-type module's `init_page` (data_page, overflow, freemap, handle_table)

**Motivation:** the upgrade plan for post-1.0 format evolution calls for **lazy per-page migration** — reads dispatch on each page's declared format version; writes always produce the current format; pages get migrated as the application happens to touch them. A later task (the "eager upgrader", see below) sweeps remaining cold pages. Both depend on having a per-page format version to dispatch on. Pre-I31 there was no such byte.

**Scheme:**
- Each non-superblock page carries a one-byte `page_format_version`. `PAGE_FORMAT_VERSION_CURRENT = 0` is "the layout as of the I31 commit."
- Storage offset is per-type, dispatched via `page::page_format_version(buf)`:
  - `Data`, `Overflow`, `FreeMap`: byte 1 (was "reserved / padding" today, already zero on every existing page).
  - `HandleTable`: byte 2 (byte 1 holds `FLAG_LEAF` / `FLAG_INTERIOR`; the flag is forensic-only, no runtime code reads it, moving it would have cost a gratuitous format break).
- Bytes 8..16 of every non-superblock page are RESERVED for future common-header fields (8 bytes / 64 bits, `COMMON_RESERVED_OFFSET` / `COMMON_RESERVED_LEN`). Universally zero today; a future common field added there will bump the affected page type's per-page version, not the superblock's MAJOR. This generalizes and extends the existing data_page "reserved for future per-page txn_counter" slot.

**Why per-type dispatch rather than uniform byte 1:** moving `FLAG_LEAF`/`FLAG_INTERIOR` would not have been a behavior change (no code reads them) but WOULD have made every existing handle-table page on disk differ from a freshly-initialized one at byte 1 vs byte 2, which would have required a MAJOR bump to avoid silent reinterpretation. The per-type dispatch costs one `if` in the reader and avoids any break — pre-I31 files Just Work (byte 1 or 2 was already zero = "version 0" = current).

**Phase 1 (landed 2026-04-22):** byte allocation only. `page_format_version` exists and is testable; every page-type `init_page` writes `PAGE_FORMAT_VERSION_CURRENT` explicitly (even though `buf.fill(0)` already zeroed it) so future CURRENT bumps flow through a single authoritative site per type. No dispatch code yet — there are no non-zero versions in use.

**Phase 2 (deferred — the "eager upgrader"):** when a realistic format change requires it, the read path in the affected page-type module grows a version switch (`match page_format_version(buf) { 0 => read_v0(buf), 1 => read_v1(buf), _ => Err(Unsupported) }`), writes always produce the latest version, and an opt-in `db.upgrade(on_progress)` method rewrites every cold page. `on_progress: FnMut(UpgradeProgress)` lets the caller surface progress to logs / TUI / IPC. A later phase 3 would wrap this in an async worker thread for fully-unattended upgrade — but per the design discussion, that's polish on top of the synchronous scanner, not a separate architecture.

**Regression tests:** `page_format_version_dispatches_by_page_type` (pure unit test pinning the per-type offset) and `fresh_pages_report_current_version` (asserts Data/FreeMap `init_page` output reports `PAGE_FORMAT_VERSION_CURRENT` through the `page_format_version` reader; Overflow and HandleTable init through their cache-aware paths and are covered end-to-end by existing integration tests). Both in `src/page.rs`'s test module.

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

### I32. `delete_inner` walks the handle table twice per handle [perf-review 2026-04-26] — **P2** ✅ IMPLEMENTED 2026-04-26
**Where:** `transaction.rs` `delete_inner`; `handle_table.rs` `delete`

**Problem:** `delete_inner` calls `handle_table::lookup` to read the
existing entry, then calls `handle_table::delete` which is
`insert(deleted_entry)` underneath — that walks the tree a second time,
COWing as it goes. Per handle, the radix tree is descended twice. For
1000 sequential deletes inside one transaction, that's 2000 tree walks
instead of 1000.

**Fix:** Fuse the two operations. `handle_table::delete` becomes a
single recursive descent that reads the existing entry from the leaf,
writes the tombstone in the same COW pass, and returns
`(new_root, Option<HandleEntry>)`. The Option lets `delete_inner`
distinguish "was Live/Overflow → escalate to release" from "was absent
or already tombstoned → escalate to InvalidHandle." `delete_inner`
becomes one tree walk per handle.

**Bonus optimizations falling out of the fusion:**
  - For absent handles or already-tombstoned entries, the new
    implementation returns `(root, None)` immediately without COWing
    the path or growing the tree. Today's code path (unreachable from
    `delete_inner` since `lookup` short-circuits first) would have
    COWed and possibly grown — wasted writes that no caller benefits
    from.
  - `delete` no longer calls `grow()`. Tree growth stays in `insert`.

**Regression tests:** Five new unit tests in `handle_table.rs`
covering the four return-value cases (Live, Overflow, already-Deleted,
absent) plus a no-tree-growth assertion for the beyond-capacity case.

### I33. `delete_many` is not actually batched per-leaf [perf-review 2026-04-26] — **P3** (deferred)
**Where:** `transaction.rs` `delete_many_inner`

**Problem:** `delete_many` is a thin loop over `delete_inner`. After
I32, each delete walks the handle table once per handle. For dense
delete patterns (e.g., 1000 handles concentrated in 5 leaves), a true
batched implementation would walk once per unique leaf — 5 walks
instead of 1000, with all tombstones for each leaf written in a
single COW pass.

**Why deferred:** Sparse delete patterns get no benefit; the win is
shape-specific. No concrete client currently demands bulk delete
latency below the fsync floor. PR 4 of the bench-suite series
(scenario tier S3 "mutation log") will surface whether real workloads
hit the dense pattern; if they do, this becomes actionable. Until
then, YAGNI.

**Fix when actionable:**
  - Sort handles by their target leaf (computed via `handle / span`
    decomposition without actually descending).
  - Group handles by leaf.
  - For each unique leaf: descend the tree once, COW the path, write
    all tombstones for that leaf in one pass.
  - Parallel optimization for `release_data_slot`: handles whose Live
    entries point to the same data page can have their slot-count
    decrements batched.
  - Estimated 5–10× speedup for dense delete patterns; no change for
    sparse.

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

**SUPERSEDED 2026-05-04** by the spillway design (see
`ARCHITECTURE.md` Cross-cutting concepts → Spillway). The
`HARD_CEILING_MULTIPLIER` constant is removed; the cache is now a
strict bound (`Options::cache_max_bytes`); overflow dirty pages
spill to a sidecar `Spillway` file capped at
`Options::spillway_max_bytes`. The pre-existing `CacheFull` variant
remains operational and now fires only when the spillway is
disabled (`spillway_max_bytes = 0`) at the strict cache cap. New
operational error `SpillwayFull { limit_bytes }` fires when both
cache and spillway are exhausted.

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

### I34. mmap-backed shadow page cache region [client 2026-04-30] — **P3** (deferred design)
**Where:** `page_cache.rs` (cache storage backing)

**Problem:** Today's `PageCache` stores pages as `Box<[u8; PAGE_SIZE]>` heap allocations indexed by `HashMap<u64, CacheEntry>`. Memory is process RSS, capped by `Options::cache_max_bytes` (default 8 MiB). Workloads with working sets larger than the cache cap either need the user to raise `cache_max_bytes` (consuming proportional RSS) or accept high cache-miss rates against the database file. (Note: as of 2026-05-04 the cache is no longer elastic — `cache_max_bytes` is a strict cap; the spillway sidecar handles overflow dirty pages via `Options::spillway_max_bytes`. The deferred mmap design replaces the cache storage backing itself, not the spillway, so the design here remains valid; only the per-`PageCache` memory ceiling math needs adjustment to reference `cache_max_bytes` instead of the removed `max_pages × HARD_CEILING_MULTIPLIER`.)

**Proposed design:** Keep the cache logic unchanged — `HashMap<u64, CacheEntry>`, `LruIndex`, `dirty_count`, hit/miss counters. Replace the `Box<[u8; PAGE_SIZE]>` storage with offsets into an mmap'd region backed by a separate ephemeral file:

- On `PageCache::new`, allocate a sized region in a temp file (preferably `O_TMPFILE` on Linux, or `open(O_CREAT, O_EXCL) + unlink` on macOS — the file has no path on the filesystem after the unlink, and is auto-released on process exit or crash). Cleanup is automatic; no leftover state.
- Each `CacheEntry` stores an offset into the region rather than a heap pointer. Reads / writes go through the mmap pointer.
- The OS pages cold cache entries to the cache file under memory pressure; pages them back in on access. Effective cache capacity becomes the file size (configurable in GBs), not the RSS budget (capped at MBs).

**Architectural compatibility:**

- *Checksum-on-load invariant unchanged.* Cache loads still go through `load_page`, which validates the XXH3 checksum from the database file before the bytes enter the cache. The mmap region is a transient, process-private store; what's in it has already been validated.
- *COW lifecycle unchanged.* New pages are allocated via `cache.new_page()` the same way; dirty pages still pinned against eviction; the mmap is just where the bytes live.
- *Counter semantics unchanged.* `cache_hits` / `cache_misses` continue to mean "was the entry in our HashMap?" — orthogonal to whether the kernel currently has the mmap'd page resident in RAM. The Chisel-internal cache abstraction is one layer above the OS's page-resident state.
- *Commit protocol unchanged.* Database-file fsync semantics are unaffected — the cache file is never part of the durability path. The two-fsync ordering, the pre-drain flush, and the poison model all stay exactly as today.

**Implementation questions for the eventual design pass:**

- Slot allocator within the mmap region: linear append vs. free-list of fixed-size slots indexed by offset.
- Cache file size policy: fixed at open vs. grow-as-needed via `ftruncate`.
- Behavior at cache-file `ENOSPC`: surface as a new error, evict more aggressively, or fall back to heap allocation.
- `O_TMPFILE` availability is Linux-specific; macOS needs the open-then-unlink dance, which has a tiny window where the path exists.
- Default-on or feature-flagged: a feature flag preserves the current `Box<[u8]>` cache for users who prefer it (tests can stay unchanged), at the cost of two code paths to maintain.

**Why deferred:** The actual win shows up at working sets larger than ~64 MB (the current hard ceiling). PR 4's micro grid will tell us whether real workloads hit that limit. If they do, this becomes actionable; if they don't, the existing in-process cache is fine and the implementation complexity isn't justified.

**Source:** Proposed by the Chisel client on 2026-04-30 during PR 3 brainstorming.

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

### C4. Documentation sweep [comment-pass 2026-04-22] — **P3** (batch) ✅ RESOLVED 2026-04-22

The 2026-04-22 pass surfaced a cluster of small doc / sharp-edge items that didn't warrant individual entries. Landed as a single cleanup commit.

- **`superblock.rs:152–153`** — deleted the misleading "a deserialized value of 0 is treated as 'legacy'" sentence; replaced with an explicit note that any out-of-range value (including 0) is rejected by `deserialize` because a zero modulus would be catastrophic for the `txn_counter % superblock_count` slot-selection math.
- **`superblock.rs::select()`** — added a "tie-break policy" paragraph documenting that `max_by_key` returns the first maximum in iteration order (lowest slot index wins) and noting the narrow scenarios where ties can legitimately appear.
- **`error.rs::CorruptSuperblock`** — expanded the variant comment to state that a slot rejected for out-of-range `superblock_count` also surfaces as `CorruptSuperblock`; the generic Display message is documented so operators know to inspect raw slot bytes if a specific cause is needed.
- **`stats.rs::file_size_bytes`** — replaced the "during a commit in progress" phrasing (which implied a concurrent observer that Chisel's single-writer model cannot have) with the real cause: post-crash orphan pages in the file tail, overwritten on next allocation (I4 territory).
- **`page_cache.rs` header** — rewrote the soft-limit blurb to spell out the hard-ceiling design (`max_pages * HARD_CEILING_MULTIPLIER`), reference I19, and note that I28's pre-drain prevents `CacheFull` from ever arising on the commit path.
- **`page_cache.rs::new`** — added `let max_pages = max_pages.max(1);` with an explanatory comment. A caller passing 0 would otherwise have set the hard ceiling to 0 and tripped `CacheFull` on the first allocation regardless of workload. No callers pass 0 in practice, but the clamp turns a confusing constructor-time mistake into correct (if inefficient) behaviour.
- **`freemap.rs` header** — replaced the stale "BUILT BUT NOT WIRED IN" note (accurate pre-R2) with a description of how the module is actually wired: `allocate_data_page` prefers the freemap, reclamation happens in `persist_freemap`, I18 ordering is called out, and the overflow / handle-table carve-outs are noted.
- **`transaction.rs::read`** — merged the two consecutive `/// Read a value by handle.` doc paragraphs into one; the second heading line was a leftover from the F3 doc update.
- **`python/chisel/__init__.py::DefragOptions.max_pages`** — clarified that the cap counts values relocated, not pages examined; flagged the name as a legacy carry-over to explain the surface mismatch.
- **`python/src/transaction.rs` header** — added the `.commit()` / `.rollback()` explicit-drive methods to the initial Semantics block so a first-time reader learns about them before getting to the Design note.

**Not landed (discussion item):** `LockFailed` classification in the Python error hierarchy. It currently sits under `FatalError` but can only fire at `open()` — before any TransactionManager exists — so it cannot poison. The database file is intact, which argues for `OperationalError`. Left for a future design call; either re-parent it or add a doc comment explaining why it stays put. Not a bug, so out of scope for this sweep.

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

---

## Deepdive review findings (2026-05-22)

Source: `docs/reviews/review-20260522-073901.md` (read-only first-contact review of the root `chisel` crate, the `python/` PyO3 binding, and the `bench/` subcrate).

### Delta from prior perf-review (2026-04-26)

The 2026-04-26 perf-review used its own internal F1–F6 numbering distinct from ISSUES.md's F-series (client feature requests). The deepdive pass found:

- **F1 (delete_many is a thin loop) — RESOLVED.** The doc at `src/transaction.rs:1386-1414` now accurately describes the shape and references the deferred I33 batching work. The recommended option (a) of the prior review landed.
- **F4 (`ChiselEngine::internal_counters` masks poison) — RESOLVED.** Fixed at `bench/src/chisel_engine.rs:109-114` with `Ok(Some(self.db.counters()?))`. Poison now propagates as the prior review recommended.
- **F5 (`Identifier` lacks `#[repr(transparent)]`) — RESOLVED.** Attribute applied at `bench/src/engine.rs:29-31`; documented `unsafe` slice transmute at `bench/src/chisel_engine.rs:93-103` eliminates the per-call `Vec<u64>` allocation.
- **F2 (`read()` allocates a `Vec<u8>`) — UNCHANGED.** `src/transaction.rs:1217` still calls `.to_vec()`. Author classified as deferable; restated below as part of I52 to keep visibility.
- **F3 (per-call `Cell<u64>` counter overhead in `PageCache::get`/`get_mut`/`new_page`) — UNCHANGED.** Deliberate trade-off per the prior review's resolution. Not re-flagged.
- **F6 (CI has no supply-chain check) — UNCHANGED.** Promoted to I54 below for proper tracking.

### Public API and 1.0 readiness

#### I35. `pub mod` declarations expose engine internals as 1.0 API surface [deepdive 2026-05-22] — **P1**
**Where:** `src/lib.rs:22-35`

**Problem:** twelve `pub mod` declarations expose `data_page`, `defrag`, `error`, `freemap`, `handle_table`, `overflow`, `page`, `page_cache`, `page_io`, `stats`, `superblock`, and `transaction`. Every type and method in those modules — `TransactionManager`, `HandleEntry`, `PageCache::set_next_page_id`, `Superblock::serialize`, `OverflowChain`, `DEFAULT_SUPERBLOCK_COUNT`, `MAX_INLINE_VALUE`, and dozens more — becomes part of Chisel's 1.0 stability contract once that release ships. The actual documented public API in `README.md` is 18 methods on `Chisel`; the on-the-wire surface is hundreds of types.

This is the single highest-leverage decision blocking 1.0. Until it's settled, every other API-stability finding (I36, I37, I39) is provisional — they're only worth fixing on the items that stay public.

**Direction of fix:** switch internal modules to `pub(crate)` and re-export only the genuinely public types from `lib.rs`:

```rust
pub use error::{ChiselError, Result};
pub use stats::{Stats, ChiselCounters};
pub use defrag::{DefragOptions, DefragStats};
// Options, DrainInsertion, SpillwayLocation, Chisel stay defined in lib.rs.
```

Tests that need access to internals can use `#[cfg(test)] pub use …` re-exports or live inside the modules. The `defrag` module is the trickiest because its `DefragOptions` / `DefragStats` are public; either lift those types into `lib.rs` or keep `pub mod defrag` and rely on `#[non_exhaustive]` to bound the public footprint.

#### I36. Public types not marked `#[non_exhaustive]` [deepdive 2026-05-22] — **P1**
**Where:** `src/lib.rs:80-88` (`Options`), `src/lib.rs:99-102` (`DrainInsertion`), `src/lib.rs:107-111` (`SpillwayLocation`), `src/error.rs:16` (`ChiselError`), `src/stats.rs::Stats`

**Problem:** of all the public types Chisel ships, only `ChiselCounters` carries `#[non_exhaustive]`. Adding a field to `Options`, a variant to `ChiselError` or `DrainInsertion`, or a backing to `SpillwayLocation` is a breaking change today. This bites both struct-literal callers and exhaustive `match` callers — exactly the patterns Rust idiom encourages.

**Direction of fix:** add `#[non_exhaustive]` to all five types before 1.0. For `Options`, follow up with a `Options::builder()` so callers don't have to construct via `Options { …, …: Default::default() }`. The existing fields stay; only the breakage shape changes — struct-literal construction now requires `..Default::default()`.

Trade-off: `#[non_exhaustive]` enums force callers to write `_ => …` arms, which is a real ergonomic cost for `match` on `DrainInsertion` (only two variants today). The alternative is to commit to "no new variants, ever" — fine for `DrainInsertion`, defensible but constraining for `ChiselError`.

#### I37. `SpillwayLocation` is `pub` but used only internally [deepdive 2026-05-22] — **P3**
**Where:** `src/lib.rs:107-111`

**Problem:** `SpillwayLocation` is constructed only inside `Chisel::open` and `Chisel::open_in_memory_with_options`; it's part of the `PageCache::new` constructor signature, which is only public because `pub mod page_cache`. Users have no reason to construct one; it leaks because `page_cache` does.

**Direction of fix:** `pub(crate)` once `page_cache` is gated (i.e., as part of I35's reshape). If `page_cache` stays `pub`, leave `SpillwayLocation` `pub` and add `#[non_exhaustive]` (covered by I36).

#### I38. `Chisel::close() -> Result<()>` is always `Ok(())` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/lib.rs:264-267`

**Problem:** `close(self)` consumes `self` and always returns `Ok(())`. The `Result` is documented as future-proofing for fsync-on-close failures, but today it's theatre — callers who `?` the result get no observable behaviour. Without `#[must_use]`, callers who *do* care can silently drop the result.

**Direction of fix:** add `#[must_use = "Chisel::close may surface fsync errors in a future release; ignore explicitly with let _ = if intentional"]` on the method. If you're confident close will stay infallible, change the return type to `()` instead.

#### I39. `TransactionManager::current_roots() -> (u64, u64, u64)` returns a positional tuple [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/transaction.rs:1693-1699`

**Problem:** `pub fn current_roots(&self) -> (u64, u64, u64)` returns `(handle_table_page, freemap_page, next_handle)`. A positional 3-tuple of `u64`s is a stringly-typed API in tuple clothing — the caller has to remember which slot is which. The method is exposed only because `transaction` is `pub mod`.

**Direction of fix:** if the method needs to stay public, introduce `pub struct CurrentRoots { pub handle_table_page: u64, pub freemap_page: u64, pub next_handle: u64 }` with `#[non_exhaustive]`. If it doesn't (probable once I35 lands), drop the `pub` and use it as an internal `pub(crate)` helper.

#### I40. Runtime setters return `Result<()>` but are infallible [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/page_cache.rs:691, 718, 728`

**Problem:** `PageCache::set_cache_max_bytes`, `set_spillway_max_bytes`, and `set_drain_insertion` all return `Result<()>` but none can fail in the current implementation. `set_drain_insertion` is literally `self.drain_insertion = policy; Ok(())`. The `Result` shape hedges for future fallibility, but right now the type lies about the API.

**Direction of fix:** drop the `Result` from `set_drain_insertion` (truly state-free). For the other two, leave the `Result` with a one-line doc comment explaining the hedge — both are plausibly fallible in a future world where shrinking the cache could observe pinned dirty pages.

#### I41. `ChiselError` has no `source()` impl [deepdive 2026-05-22] — **P2**
**Where:** `src/error.rs:215` (`impl std::error::Error for ChiselError {}`)

**Problem:** the trait impl is empty. `IoError(io::Error)` wraps an inner cause but exposes it nowhere — `e.source()` returns `None` for every variant. This breaks error-chain walkers (`anyhow::Error::root_cause`, structured-logging adapters, `eyre` reports). The Display message is the only signal an upstream caller can see.

**Direction of fix:** implement `fn source(&self) -> Option<&(dyn Error + 'static)>` that returns the inner `io::Error` for `ChiselError::IoError(e)` and `None` for the rest. If future variants gain inner causes (e.g., wrapping a deserialization error), extend the match.

#### I42. Python `to_py_err` discards inner `io::Error` from `IoError(_)` [deepdive 2026-05-22] — **P2**
**Where:** `python/src/errors.rs:209-249`

**Problem:** `to_py_err` formats `ChiselError` via `Display` and then drops the variant. A Python caller cannot programmatically distinguish ENOSPC from EACCES from EIO — they all become `chisel.IoError("I/O error: <prose>")`. The comment at lines 211-213 documents the choice as "the string is the only cross-boundary contract"; defensible but worth re-litigating if any caller wants disk-full-vs-permission-denied handling on the Python side.

**Direction of fix:** for `IoError`, attach the inner errno (where available) as a Python exception attribute (`errno` or `winerror`-style). PyO3 exception classes can hold arbitrary data; a `PyIoError::new_err((msg, errno))` would surface it. Trade-off: cross-boundary error fidelity vs. holding the Rust error chain in memory across the FFI boundary.

#### I43. bench `EngineResult` uses `Box<dyn Error + Send + Sync>` and erases engine class [deepdive 2026-05-22] — **P3**
**Where:** `bench/src/engine.rs:43`

**Problem:** `pub type EngineResult<T> = Result<T, Box<dyn Error + Send + Sync>>;` makes engine-specific errors invisible to downstream `match`. The bench crate is `publish = false` and internal-use-only, so this isn't a true public-API leak, but the runner / diff binary / scenarios already do `?`-propagation through `EngineResult` and can't tell `ChiselError::Poisoned` from `redb::Error::Corrupted` without `downcast`.

**Direction of fix:** introduce a thin enum:

```rust
pub enum EngineError {
    Chisel(ChiselError),
    Redb(redb::Error),
    Sqlite(rusqlite::Error),
    Other(Box<dyn Error + Send + Sync>),
}
```

with `#[from]` impls so `?` keeps working. Keeps the existing call-site ergonomics; adds introspection where needed.

### Code quality

#### I44. `libc::flock` `unsafe { … }` block missing `// SAFETY:` comment [deepdive 2026-05-22] — **P2**
**Where:** `src/page_io.rs:133-141`

**Problem:** the only `unsafe` block in the core engine — the syscall the entire single-writer contract rests on — has no `// SAFETY:` comment. The Commenting standards section of `ARCHITECTURE.md` calls this out as a convention violation; this is the one place in the engine that violates it.

The invariants the call upholds: (1) `fd` is valid for the duration of the call because we hold `&File`'s borrow; (2) the syscall returns an `errno`-style int that we check; (3) no resources are leaked because flock release is tied to fd close (which `Drop` handles).

**Direction of fix:**

```rust
fn try_lock(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY:
    //   * `fd` is valid for the duration of this call: we hold a borrow of
    //     `&File`, so the descriptor cannot be closed concurrently.
    //   * `LOCK_EX | LOCK_NB` is a fixed bitflag combination that flock(2)
    //     accepts on every supported platform (Linux, macOS).
    //   * The call returns 0 on success, -1 on failure with errno set; we
    //     do not read errno (`LockFailed` is sufficient diagnostic for the
    //     "someone else holds the lock" case, the only failure mode in
    //     practice for a path we can open).
    //   * No resources are leaked: the lock is released when the underlying
    //     fd is closed, which happens when `PageIo`'s `Drop` runs.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(ChiselError::LockFailed);
    }
    Ok(())
}
```

#### I45. `unreachable!` in `delete_inner` should be `CorruptPage` [deepdive 2026-05-22] — **P3**
**Where:** `src/transaction.rs:1375-1379`

**Problem:** `delete_inner` ends with `unreachable!("handle_table::delete returns None for Deleted entries; None was already escalated to InvalidHandle by ok_or above")`. The "unreachable" depends on `HandleTable::delete`'s cross-module behaviour. A future refactor that changes that contract turns this into a library-reachable panic instead of a typed error.

**Direction of fix:** convert to `Err(ChiselError::CorruptPage { page_id: entry.page_id })` with a comment noting that reaching this arm would mean the handle table returned a Deleted entry that ok_or didn't catch — i.e., the in-memory state contradicts itself, which is genuinely a corruption signal worth surfacing typed.

#### I46. `DataPage::insert(...).expect("value fits in empty page")` needs an `// INVARIANT:` comment [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/transaction.rs:1846`

**Problem:** the `expect` is reachable if `DataPage::insert` ever returns `None` for any reason besides "no room" (e.g., a future size-overflow check on a misuse). The invariant is currently sound — the data page was just allocated and initialized, so insert can't fail for size reasons against a value that was already length-checked against `MAX_INLINE_VALUE` — but it's not asserted at a type level.

**Direction of fix:** add a comment naming the data-page contract:

```rust
// INVARIANT: insert can only return None for "no room"; the page was just
// init'd via DataPage::init_page (empty), and the value was length-checked
// against MAX_INLINE_VALUE upstream. If DataPage::insert ever grows other
// failure modes, this expect needs to translate them to typed errors.
let slot = DataPage::insert(buf, value).expect("value fits in empty page");
```

#### I47. `file_size_bytes` multiplication lacks overflow check [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/lib.rs:411`

**Problem:** `page_count * page::PAGE_SIZE as u64` could overflow at `u64::MAX / 8192 ≈ 2.25 × 10^15` pages (18 EiB). Unreachable for any real database, but unannotated.

**Direction of fix:** `page_count.saturating_mul(page::PAGE_SIZE as u64)` is one character of armor.

#### I48. Five invariant-backed `.unwrap()` sites in `page_cache.rs` need `// INVARIANT:` annotations [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/page_cache.rs:188, 211, 353, 384, 914`

**Problem:** five `.unwrap()` sites on hashmap `get` / `Option<Spillway>` access immediately after the cache was populated or the spillway was just constructed. Each is invariant-backed but unannotated; in aggregate they're a "trust the local code" pattern that a maintenance read can't verify quickly.

**Direction of fix:** at minimum, annotate each with a one-line `// INVARIANT:` comment naming what guarantees the `Some`. Example for `:188`:

```rust
// INVARIANT: entry was just inserted by load_page on the miss branch,
// or contains_key returned true on the hit branch.
Ok(&self.entries.get(&page_id).unwrap().buf)
```

A stronger fix is to refactor `get` / `get_mut` to return the borrow from inside the `load_page` branch, but the existing shape predates the spillway and the change has knock-on borrow-checker implications.

#### I49. `expect("LRU referenced page id not in entries")` should be `CorruptPage` [deepdive 2026-05-22] — **P3**
**Where:** `src/page_cache.rs:865`

**Problem:** reachable if the LRU index and entries map ever desync. Currently kept in sync by `discard`/`truncate`/`flush`, but a future refactor that touches one without the other turns this into a library-reachable panic.

**Direction of fix:** translate to `Err(ChiselError::CorruptPage { page_id: victim_id })` and document that reaching this branch indicates the cache's two-data-structure invariant broke, which is genuinely a corruption signal worth surfacing typed.

#### I50. Hex literal `0x02` used instead of `FLAG_INTERIOR` constant in `open_existing` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/transaction.rs:417, 423`

**Problem:** `if root_buf[1] == 0x02 { ... }` reaches for a raw hex literal rather than `handle_table::FLAG_INTERIOR`. The constant exists; using the literal defeats the single-source-of-truth promise for the on-disk format and makes a grep for "interior" miss this site.

**Direction of fix:** import `handle_table::FLAG_INTERIOR` (exposing it if currently private — it's already implicitly public via the on-disk format) and compare against it. Same fix at both line 417 and the implicit comparison logic at 423.

### Performance

#### I51. `read_page` calls `page_count()` (one extra `lseek`) on every call [deepdive 2026-05-22] — **P2**
**Where:** `src/page_io.rs:166-167`

**Problem:** `read_page` calls `self.page_count()?` every call, which on the file backing does `file.seek(SeekFrom::End(0))` — one extra syscall per read. The doc comment at lines 156-163 documents the cost and the rationale (no cache invalidation complexity), but underweights it ("absorbed by `PageCache` on cache hits") — the cache miss IS the cost site by definition, and high-miss-rate workloads pay this on every page load.

**Direction of fix:** cache a high-water-mark on `PageIo`, invalidated by:
- `write_page` past EOF (extend the HWM)
- `set_page_count` (set the HWM exactly)

`page_count()` returns the cached HWM. Initial seed at open via the existing `seek(End(0))`. Two write paths to update; zero seeks on the read path. Saves one syscall per cache miss.

#### I52. `flush()` allocates a transient `Vec<u64>` per commit [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/page_cache.rs:340-358`

**Problem:** `flush()` collects dirty IDs into `Vec<u64>` for every flush. Sized to `dirty_count`, so for a 10K-page transaction that's 80 KB transient allocation per commit, repeated on every commit. The collect-first idiom is a borrow-checker dodge (the loop needs `&mut self.entries` while iterating).

**Direction of fix:** keep a scratch `Vec<u64>` on `self` and reuse it across flushes (clear before populating). Adds 24 bytes to `PageCache` (the Vec metadata) and eliminates the per-commit allocation.

Related: `read()` similarly allocates a `Vec<u8>` on every call (`src/transaction.rs:1217`, perf-review F2 unchanged). The fixes are different shapes — a `read_borrow(&self, handle) -> Result<Ref<'_, [u8]>>` sibling API would close that one for Rust callers (PyO3 callers cannot benefit because `PyBytes` wants owned bytes).

#### I53. bench `file_size_bytes` triggers an O(live handles) walk via `stats()` [deepdive 2026-05-22] — **P3**
**Where:** `bench/src/chisel_engine.rs:106`

**Problem:** `self.db.stats()?.file_size_bytes` calls `Chisel::stats()`, which walks the entire handle table (O(live handles)) just to populate `handle_count`. Used per measurement cell in the bench harness — for 100K-handle scenarios this is ~milliseconds per call, dragged into every reporting step.

**Direction of fix:** add a dedicated `Chisel::file_size_bytes() -> Result<u64>` that reads `page_count * PAGE_SIZE` directly (the existing math in `Chisel::stats`) without the handle walk. The bench `file_size_bytes` impl calls the new method; the existing `stats()` keeps its current shape because callers want all three fields together.

### CI, packaging, and publication

#### I54. CI runs no supply-chain check [perf-review 2026-04-26 / deepdive 2026-05-22] — **P2**
**Where:** `.github/workflows/ci.yml`

**Problem:** three jobs — `test`, `clippy`, `fmt` — all running their respective cargo subcommands. No `cargo audit`, no `cargo deny`, no MSRV pinning. A vulnerable transitive dep would land silently.

(Promoted from perf-review F6 which was deferred at the time.)

**Direction of fix:** add an `audit` job to `.github/workflows/ci.yml`:

```yaml
audit:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: rustsec/audit-check@v1.4.1
      with:
        token: ${{ secrets.GITHUB_TOKEN }}
```

Costs one CI minute per build. `cargo deny` is the next level up (license + advisory + source policy) and warrants a `deny.toml` policy file.

#### I55. No MSRV pinned in `Cargo.toml` or CI [deepdive 2026-05-22] — **P2**
**Where:** `Cargo.toml`, `bench/Cargo.toml`, `python/Cargo.toml`, `.github/workflows/ci.yml`

**Problem:** `rust-version` is absent from every `Cargo.toml`. CI uses `dtolnay/rust-toolchain@stable`, so an unannounced 1.x MSRV bump can land silently. README says "Rust stable, edition 2021"; that's not a pinned MSRV. The codebase uses `let-else` (1.65+), `is_none_or` (1.82+), `is_some_and` (1.70+); actual floor is currently ≥ 1.82.

**Direction of fix:** pin `rust-version = "1.82"` (or whatever the current actual floor is — verify via `cargo msrv` if available) in `Cargo.toml`. Add a `msrv` job to CI that uses `dtolnay/rust-toolchain@1.82` and runs `cargo build`. If the project doesn't commit to MSRV stability pre-1.0, document that decision in the README.

#### I56. `Cargo.toml` lacks crates.io publication metadata [deepdive 2026-05-22] — **P1**
**Where:** root `Cargo.toml`

**Problem:** missing `license`, `repository`, `readme`, `keywords`, `categories`. All required or strongly recommended for crates.io publication. `cargo publish` will refuse without `license` (or `license-file`).

**Direction of fix:**

```toml
[package]
name = "chisel"
version = "0.1.0"
edition = "2021"
rust-version = "1.82"   # see I55
description = "Transactional slot-based storage engine with shadow paging"
license = "MIT OR Apache-2.0"   # see I57
repository = "https://github.com/Xof/chisel"
readme = "README.md"
keywords = ["database", "storage", "embedded", "transactional", "shadow-paging"]
categories = ["database", "data-structures"]
```

#### I57. `License: TBD` blocks any third-party use [deepdive 2026-05-22] — **P1**
**Where:** `README.md:326-327`

**Problem:** with no license, the code is "all rights reserved" by default — no one can legally use or distribute it. For a pre-1.0 project that's worth flagging visibly on the README.

**Direction of fix:** pick a license now. The prevailing Rust convention is `MIT OR Apache-2.0` dual. Drop `LICENSE-MIT` and `LICENSE-APACHE` files at the repo root, update README from "TBD" to the chosen license, and add `license = "MIT OR Apache-2.0"` to `Cargo.toml` (covered by I56).

#### I58. `bench/` is not in `ci.yml` [deepdive 2026-05-22, formalizing spillway-rollout lesson #1] — **P2**
**Where:** `.github/workflows/ci.yml`, `bench/Cargo.toml`

**Problem:** the bench subcrate is a sibling — not a workspace member — so `cargo test` from the repo root doesn't run `bench/`'s tests, and `ci.yml` doesn't either. The spillway-rollout retrospective in `ARCHITECTURE.md` (Implementation history → Lessons learned, lesson #1) flagged this as a pattern that bit a real PR. The mid-PR review caught the missed bench test failures, but there's no CI-side safety net.

**Direction of fix:** add a `bench-tests` job to `.github/workflows/ci.yml`:

```yaml
bench-tests:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - uses: Swatinem/rust-cache@v2
      with:
        workspaces: bench
    - name: Run bench subcrate tests
      working-directory: bench
      run: cargo test --verbose
```

~2 minutes added; closes the real coverage hole. Distinct from `bench.yml` (which runs the scenario tier on PRs for regression-comment purposes and does not run `cargo test`).

#### I59. `wheels.yml` has no early `cargo test` gate [deepdive 2026-05-22] — **P3**
**Where:** `.github/workflows/wheels.yml`

**Problem:** the wheels workflow builds and tests wheels at tag time. The CI matrix runs Rust tests on push/PR, and wheels.yml runs `pytest` after building wheels, so underlying Rust correctness is covered transitively. But a wheels build on a tag whose underlying commit is broken still runs pytest against broken bindings and fails there — slower feedback than a pre-wheel `cargo test`.

**Direction of fix:** either add an early `cargo test` step in wheels.yml, or make the wheels job `needs:` the test job (cross-workflow `needs` is supported via a reusable workflow or by re-running tests inline). Less urgent than I54/I55/I56 because wheels.yml is tag-triggered and the underlying problem only manifests if someone tags a broken commit.

#### I60. Orphaned `bench-disk-cleanup.yml` and `bench-os-update.yml` workflows [deepdive 2026-05-22] — **P3**
**Where:** `.github/workflows/bench-disk-cleanup.yml`, `.github/workflows/bench-os-update.yml`

**Problem:** both workflows are queued waiting for a self-hosted runner that hasn't been provisioned. While they're queueing/expiring, GitHub may surface them as "stuck workflows" in the Actions UI.

**Direction of fix:** either:
- (a) commit to provisioning the dedicated runner per the "Dedicated bench machine foundation" spec; or
- (b) flip both workflows to `workflow_dispatch:` only with a `# DISABLED until self-hosted runner provisioned` header, so they don't accumulate failed runs.

Choice (a) is the planned path per the spec; choice (b) is the cleanup if the plan moves out by months.

#### I61. No workspace manifest [deepdive 2026-05-22] — **P3**
**Where:** repo root (no `Cargo.toml` `[workspace]`)

**Problem:** `python/` and `bench/` are sibling subcrates with `chisel = { path = ".." }` path-deps, not workspace members. Each rebuilds `chisel` separately because their `Cargo.lock` files are independent. The README's opening "Rust workspace with three crates" sentence is wrong (see I62). A real workspace would share `target/` and `Cargo.lock`, giving `cargo test --workspace` and `cargo clippy --workspace` coverage of all three at once.

**Direction of fix:** add a root `[workspace]` declaration:

```toml
[workspace]
members = [".", "python", "bench"]
resolver = "2"
```

Trade-off: workspace members share an `edition` / `rust-version` / unified feature resolution, which can be restrictive for the PyO3 binding (it has different abi3 considerations than the root). Resolver = "2" addresses most of the friction. Probably worth doing; the current setup costs the project a clean way to test the whole tree (and forces I58 as a separate job rather than `cargo test --workspace` covering it for free).

### Doc fixes

#### I62. `README.md:71` claims "Rust workspace with three crates" but the repo is not a workspace [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `README.md:71` ("Building from source" intro)

**Problem:** the README opens "Rust workspace with three crates: the root `chisel` engine, the `python/` PyO3 binding, and the `bench/` benchmark suite." There is no workspace `Cargo.toml`. The same paragraph later acknowledges the truth ("running `cargo test` from the repo root does **not** run the bench subcrate's tests, since `bench/` is a sibling crate, not a workspace member") — those two sentences contradict each other.

**Direction of fix:** either change the opening sentence to "three sibling crates" (the truth today, and aligns with the existing CLAUDE.md→ARCHITECTURE.md migration), or do I61 first and update the README to reflect the new workspace structure.

#### I63. `Chisel::commit` docstring says "two fsyncs"; protocol does three [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/lib.rs:278-280`

**Problem:** `Chisel::commit`'s docstring reads "Performs two fsyncs (dirty data pages, then the alternate superblock)". `ARCHITECTURE.md`'s commit-protocol section (and the I28 fix) document three: pre-drain flush + main-pages flush + superblock. The `no_spill_workload_preserves_two_fsync_commit` test (despite its name) pins to `== 3` per spillway-rollout lesson #3.

**Direction of fix:** update the docstring to "Performs three fsyncs (pre-drain flush, main pages flush, then the alternate superblock)". Also consider renaming the test to drop the "two_fsync" misnomer; the test's body already documents the three.

#### I64. `python/src/db.rs:155` uses plain `*` where Rust side uses `saturating_mul` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `python/src/db.rs:155`

**Problem:** `let resolved_spillway_max_bytes = spillway_max_bytes.unwrap_or(1024 * cache_max_bytes);` uses plain `*`. `src/lib.rs:118` (`Options::default`) uses `cache_max_bytes.saturating_mul(1024)`. For the default `cache_max_bytes = 8_388_608`, the result fits comfortably in `u64`. A user passing `cache_max_bytes = 1 << 54` (16 PiB) from Python would silently overflow to a small spillway cap rather than saturate to `u64::MAX`.

**Direction of fix:** mirror the Rust side:

```rust
let resolved_spillway_max_bytes = spillway_max_bytes.unwrap_or_else(|| cache_max_bytes.saturating_mul(1024));
```

#### I65. `src/spillway.rs` carries stale `#[allow(dead_code)]` on every exported item [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/spillway.rs:39-74` (and similar repeated attributes throughout the file)

**Problem:** every `pub` item — `SLOT_HEADER_SIZE`, `SLOT_SIZE`, `Backing`, `Spillway`, every `impl` method, `slot_checksum`, `write_slot`, `read_slot` — carries `#[allow(dead_code)]` with the comment "Suppressed until spillway is wired into PageCache (Tasks 7-8)". Tasks 7-8 landed (see `page_cache.rs:824` and surrounding); the spillway IS wired in. The attributes now suppress nothing real, and if any of these items genuinely becomes dead in a future refactor, the attribute will hide that.

**Direction of fix:** remove every `#[allow(dead_code)]` in `src/spillway.rs` along with the explanatory comments that go with them. A `cargo build` after the removal will fail on anything that was legitimately dead; that's the signal worth surfacing.

#### I66. Tests use `std::mem::forget(file)` to bypass `NamedTempFile` cleanup [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/transaction.rs:2211`, `src/page_cache.rs:930, 1000`, possibly others

**Problem:** `NamedTempFile` cleans up its path via `Drop`; tests that need the file to outlive the `NamedTempFile` value (because they re-open the path) leak the temp file deliberately with `std::mem::forget(file)`. This is fragile: the leaked path stays in `/tmp` after the test exits. Over many CI runs this can fill disk on a long-lived runner.

**Direction of fix:** use `tempfile::TempDir` and construct paths inside it. `TempDir`'s `Drop` cleans up the directory and everything in it, including a re-opened sibling. `src/spillway.rs:338-365` (`open_file_truncates_existing_content`) already does this correctly — use it as the pattern.

### Idiomaticity

#### I67. Three sites use awkward `!self.entries.get(&id).is_none_or(|e| e.dirty)` pattern [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/page_cache.rs:419, 702, 834`

**Problem:** the double negative ("not none-or-dirty" = "some and clean") is acknowledged as awkward in the comment at line 820. Used at three sites for the same eviction-victim search.

**Direction of fix:** replace with `self.entries.get(&id).is_some_and(|e| !e.dirty)` — reads as "some and clean" and matches the intent directly. Fixes all three sites the same way; no semantic change.

#### I68. `Chisel::Drop` doesn't fsync (correct, but worth a one-line annotation) [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/lib.rs` (no explicit `Drop` impl on `Chisel`)

**Problem:** if a user forgets to `commit()`, shadow paging guarantees the on-disk state is the last committed state — so dropping without committing is correct, not a data-loss bug. The type-level doc at `src/lib.rs:139-141` documents this. But a reader coming from other ecosystems (Postgres, RocksDB) expects an explicit "close discards uncommitted work" callout at the `Drop` site too.

**Direction of fix:** add a `// Drop intentionally omitted — shadow paging guarantees the on-disk state is the last committed state regardless of how the value goes out of scope. See type-level doc for the full semantics.` block at the top of `impl Chisel` or just below the struct declaration. No behaviour change; documentation for the next reader.

#### I69. `flock` is advisory, not mandatory — worth annotating in an ops/recovery doc [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** README + `ARCHITECTURE.md` (cross-cutting)

**Problem:** the README and ARCHITECTURE.md both mention that Chisel uses `flock` for single-process exclusion, but neither explicitly states that `flock` is advisory — an external tool that doesn't respect advisory locks (some text editors with "lock files", filesystem dump tools, naive sync utilities) can scribble on the file. This is a Linux/macOS POSIX limitation, not a Chisel bug, but it deserves a sentence so users don't trip over it.

**Direction of fix:** add a sentence to README's "Platform support" section or to ARCHITECTURE.md's "Cross-cutting concepts" — "Chisel's `flock` is *advisory*: cooperating processes (any other Chisel instance) honour it, but a tool that bypasses advisory locking (e.g., `cp` while a transaction is in flight, some sync utilities) can still corrupt the file. The shadow-paging invariants assume an exclusive owner; respect the lock."

### Test coverage gaps

#### I70. No `#[should_panic]` tests for `unreachable!` / `expect` invariant sites [deepdive 2026-05-22] — **P3**
**Where:** test coverage for `src/transaction.rs:1375` (`unreachable!`), `src/page_cache.rs:865` (`expect`), `src/transaction.rs:1846` (`expect`)

**Problem:** the codebase has good regression tests for documented invariants (I1, I3, I7, I18, I27, I28, I29 all have dedicated tests). The `unreachable!` and `expect` sites name invariants but don't have tests that exercise the invariant-violating path.

**Direction of fix:** lower priority — and if I45 + I49 convert these to typed `CorruptPage` errors, this finding becomes a test for the `CorruptPage` arm instead. Wait for those decisions; revisit afterward.

#### I71. No property tests (`proptest` / `quickcheck`) for byte-roundtrip code [deepdive 2026-05-22] — **P3**
**Where:** test coverage for `Superblock::serialize` / `deserialize`, `DataPage` slot packing, freemap bitmap operations

**Problem:** the existing targeted tests cover known cases well; property tests would cover the unknown ones. `Superblock::serialize` round-trips, slot-packing fill / compact invariants, and freemap bit operations are all natural fits.

**Direction of fix:** add `proptest = "1"` to `[dev-dependencies]` and write three property tests:
- `serialize(deserialize(buf)).map(|sb| sb.serialize()) == Some(buf)` for any well-formed superblock
- `DataPage::insert` then `DataPage::read` round-trips for any value ≤ `MAX_INLINE_VALUE`
- `FreeMap::mark_free(id)` then `FreeMap::is_free(id)` round-trips for any `id < CAPACITY`

Low priority pre-1.0; high value when format evolution starts in earnest.

#### I72. Replace `paste` dev-dependency with the maintained `pastey` fork [deepdive follow-up 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** root `Cargo.toml` (`paste = "1"` in `[dev-dependencies]`); single use site `tests/common/mod.rs:51` (`paste::paste!` inside `dual_backing_test!` macro).

**Problem:** RUSTSEC-2024-0436 — `paste 1.0.15` is **unmaintained**. The author (dtolnay) archived the GitHub repo on 2024-10-07 and the README says the project is no longer maintained. RustSec classifies this as informational (no vulnerability, no broken semantics), but `rustsec/audit-check` flags it by default and blocks the I54 supply-chain CI job.

Surfaced when the I54 audit job landed on main and immediately tripped on this dep. Worked around in the same fix-up commit (`ignore: RUSTSEC-2024-0436` in `.github/workflows/ci.yml`); this entry documents the proper fix.

**Direction of fix:** swap `paste` for `pastey`, a drop-in fork explicitly created to address this advisory. Two edits:

```toml
# Cargo.toml
[dev-dependencies]
- paste = "1"
+ pastey = "0.1"
```

```rust
// tests/common/mod.rs
- paste::paste! {
+ pastey::paste! {
```

Verify `cargo test` still produces both `_file` and `_memory` test variants via the `dual_backing_test!` expansion, then drop the `ignore: RUSTSEC-2024-0436` line from `.github/workflows/ci.yml`.

Low priority because the warning is informational and only affects a dev-dep; high enough to fix in a small PR before the `ignore` list accumulates more entries.

#### I73. GitHub Actions Node.js 20 deprecation (every job uses `actions/checkout@v4`) [post-P2 CI run 2026-05-22] — **P3**
**Where:** every job in `.github/workflows/ci.yml` (test, clippy, fmt, audit, msrv, bench-tests, python matrix) — currently 7 distinct jobs all pinned to `actions/checkout@v4`. The `dtolnay/rust-toolchain`, `Swatinem/rust-cache@v2`, `actions/setup-python@v5`, and `rustsec/audit-check@v1.4.1` actions should also be re-checked for Node 24 readiness.

**Problem:** GitHub is forcing Node.js 24 as the default on hosted runners on **June 2nd, 2026** (~10 days from today). Node.js 20 will be removed entirely on **September 16th, 2026**. Surfaced as warning annotations on the first green post-P2 audit run on main:

> `! Node.js 20 actions are deprecated. The following actions are running on Node.js 20 and may not work as expected: actions/checkout@v4. … For more information see: https://github.blog/changelog/2025-09-19-deprecation-of-node-20-on-github-actions-runners/`

CI keeps passing today, but the warning is on every run and there is a hard cutoff coming.

**Direction of fix:** bump each `uses:` line to a version whose action manifest declares `node24` once those versions are GA. As of 2026-05-22, the v5 line of `actions/checkout` is the natural target; the other actions cited above need a one-shot audit of their action.yml `runs.using` field. Until then, the temporary opt-in is the `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` env var on the runner — but that's worse than just bumping the pin once. Single-PR scope: edit `.github/workflows/ci.yml`, push, verify one CI run goes green.

Low priority because runners auto-upgrade on the cutoff date anyway; medium-low because if `actions/checkout@v5` is GA before then, this is a five-minute PR that closes the warning noise immediately.

#### I74. Expose `Spillway::logical_bytes` and `max_bytes` via `Chisel::stats` / `ChiselCounters` [I65 follow-up 2026-05-22] — **P3**
**Where:** `src/spillway.rs` (`logical_bytes`, `max_bytes` — currently `#[cfg(test)]`); `src/stats.rs` (`Stats` / `ChiselCounters`); `src/lib.rs` (`Chisel::stats`).

**Problem:** when I65 stripped every `#[allow(dead_code)]` from `src/spillway.rs`, `Spillway::logical_bytes()` and `Spillway::max_bytes()` surfaced as legitimately unused — they had no production caller. Gating them as `#[cfg(test)]` keeps the lib build clean and preserves the methods for the test module, but throws away the chance for operators to read spillway capacity utilisation through the public stats API.

Spillway capacity is exactly the kind of metric operators want for capacity planning: "how full is the spillway right now, and what's the cap?" Knowing `logical_bytes / max_bytes` answers "are we one transaction away from `SpillwayFull`?"

**Direction of fix:** add two fields to the `Stats` (or `ChiselCounters`) struct:

```rust
#[non_exhaustive]
pub struct Stats {
    // ...existing fields...
    /// Spillway logical bytes in flight (None if spillway never opened).
    pub spillway_logical_bytes: Option<u64>,
    /// Spillway max-bytes cap (None if spillway never opened).
    pub spillway_max_bytes: Option<u64>,
}
```

Wire them into `Chisel::stats` by inspecting `PageCache::spillway` (which is already `pub(crate)`). Then remove the `#[cfg(test)]` from the two `Spillway` methods. The `Option` shape is because the spillway is lazily opened on first spill — `None` distinguishes "no spillway yet" from "spillway has zero bytes in flight."

Low priority because spillway exhaustion currently surfaces as `SpillwayFull` (a typed error) rather than a silent slowdown; operators can hook on that. But adding observability is the difference between "see the wall coming" and "hit the wall."
