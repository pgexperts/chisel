# Chisel Issues and Backlog

Tracked work for the Chisel storage engine. Items are grouped by source and
rough category. Each entry carries a priority tag; see legend below.

Sources:
- **[comment-pass]** — found during the 2026-04-10 commenting pass (read-only review, no tests run)
- **[comment-pass 2026-04-17]** — found during the 2026-04-17 re-commenting pass (also read-only; covered changed `src/` files and the new `python/src/` subcrate)
- **[comment-pass 2026-04-22]** — found during the 2026-04-22 third review pass (read-only; five-agent parallel audit over `src/` and `python/src/`)
- **[perf-review 2026-04-26]** — found during the `chisel-performance` skill fresh-eyes pass after PR-1 + PR-2 of the bench-suite series landed (read-only; output at `docs/reviews/perf-review-2026-04-26.md`)
- **[deepdive 2026-05-22]** — found during the `deepdive-rust` skill fresh-eyes pass after the spillway feature and the runtime-config setters landed (read-only; output at `docs/reviews/review-20260522-073901.md`)
- **[perf-review 2026-06-01]** — found during the `chisel-performance` + `rust-performance` full hot-path sweep (read-only; six-agent parallel audit over `src/` and `bench/`; output at `docs/reviews/perf-review-2026-06-01.md`)
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
>
> **Status note (2026-06-01):** the `chisel-performance` + `rust-performance` full hot-path sweep (output at `docs/reviews/perf-review-2026-06-01.md`) adds I77–I96 in a new "Perf-review findings (2026-06-01)" section below. No correctness bugs and no Don't-Break-List violations were found — the engine's data-handling is confirmed tight (zero-copy reads, in-place slot mutation, dead-on-prod `compact()`, verified I18 / I51 / 3-fsync discipline). The cluster splits into engine hot-path optimizations (I77–I89) and benchmark-harness integrity (I90–I96). The central conclusion is a **sequencing constraint**: the bench harness cannot currently validate an engine change (no `black_box`, no `[profile.release]`, no in-memory-backend row), so the I90/I91/I92 measurement-integrity fixes GATE the two engine P1s (I77 SipHash hasher, I78 per-insert XXH3 re-stamp) — and `black_box` (I90) must precede the LTO profile (I91) because LTO widens the dead-code-elimination window. Prior deferred perf items are unchanged: F2 (`read` → `to_vec`) and F3 (`Cell` counters) still UNCHANGED; I33 (`delete_many` per-leaf) and I34 (mmap cache) still DEFERRED.
>
> **Status note (2026-06-21):** the `deepdive-rust` fresh-eyes pass (output at `docs/reviews/review-20260621-185541.md`, run after PRs #34–#50 and the per-page format-versioning feature landed) adds **I104–I140** in a new "Deepdive review findings (2026-06-21)" section below. The *entire* prior executive summary (2026-06-16) is resolved with no regressions, so the new cluster is fresh — concentrated in error-classification defaults, one CI lint hole, the radix test gap, and doc drift introduced by the #46 dead-code sweep. No new P0 / data-loss bugs were found. Highest leverage first: **I108** (the 1,300-line PyO3 binding escapes the gating `cargo clippy` because `default-members` excludes it — a real CI hole that ships clippy regressions silently) and **I111** (the radix key-math — the engine's most off-by-one-prone code — has no property tests, and its one "two-level" test only reaches depth 1). Then the cheap zero-risk cluster: **I104** (`is_fatal` exhaustiveness test, NOT a default flip — see the review's countercase), **I127** (the `live_slots` SipHash maps the I77 FxHashMap pass missed), and the doc-drift fixes **I128 / I130 / I131 / I132 / I133**. Carried-not-regressed: F2 (`read`→`to_vec`) and the `delete_with_tag` partial-progress drop (re-filed as **I107**) remain open by design. The ADR-to-repo question (**I129**) is a maintainer process call. Three 2026-05-22 fixes have narrow follow-ups here (I46→I117, I63→I131, I71→I111) — the original fix was correct but didn't cover the site this re-review found.

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

**Phase 2 (landed 2026-06-21):** the file-MINOR write-gate is live. If the file's MINOR exceeds the binary's MINOR, the open path forces the store read-only (`TransactionManager::open_existing` calls `PageIo::force_read_only`) — no writes are permitted. See `docs/specs/2026-06-21-per-page-format-versioning-design.md`.

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

**Phase 1a (landed 2026-06-21):** the read-side dispatch helpers `page::page_format_version()` and `page::current_version()` are now active — every `init_page` site stamps `current_version(type)` explicitly. The per-module version switch (Phase 2 / "eager upgrader") remains deferred pending a real format change that needs it. See `docs/specs/2026-06-21-per-page-format-versioning-design.md`.

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

#### I35. `pub mod` declarations expose engine internals as 1.0 API surface [deepdive 2026-05-22] — **P1** ✅ FIXED 2026-05-22 (PR #11; pub → pub(crate) reshape with selective re-exports of the surface kept public)
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

#### I36. Public types not marked `#[non_exhaustive]` [deepdive 2026-05-22] — **P1** ✅ FIXED 2026-05-22 (PR #12; ChiselError, Options, DrainInsertion, Stats, DefragOptions, DefragStats all gained `#[non_exhaustive]`; fluent builders added on Options + DefragOptions so external callers can still construct them)
**Where:** `src/lib.rs:80-88` (`Options`), `src/lib.rs:99-102` (`DrainInsertion`), `src/lib.rs:107-111` (`SpillwayLocation`), `src/error.rs:16` (`ChiselError`), `src/stats.rs::Stats`

**Problem:** of all the public types Chisel ships, only `ChiselCounters` carries `#[non_exhaustive]`. Adding a field to `Options`, a variant to `ChiselError` or `DrainInsertion`, or a backing to `SpillwayLocation` is a breaking change today. This bites both struct-literal callers and exhaustive `match` callers — exactly the patterns Rust idiom encourages.

**Direction of fix:** add `#[non_exhaustive]` to all five types before 1.0. For `Options`, follow up with a `Options::builder()` so callers don't have to construct via `Options { …, …: Default::default() }`. The existing fields stay; only the breakage shape changes — struct-literal construction now requires `..Default::default()`.

Trade-off: `#[non_exhaustive]` enums force callers to write `_ => …` arms, which is a real ergonomic cost for `match` on `DrainInsertion` (only two variants today). The alternative is to commit to "no new variants, ever" — fine for `DrainInsertion`, defensible but constraining for `ChiselError`.

#### I37. `SpillwayLocation` is `pub` but used only internally [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (PR #11; folded into the I35 reshape — `pub(crate)`-scoped along with the other engine-internal types)
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

#### I41. `ChiselError` has no `source()` impl [deepdive 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (PR #14; `source()` returns the inner `io::Error` for the `IoError` arm and `None` for all other variants — preserves the error-chain walker contract for anyhow / eyre / tracing)
**Where:** `src/error.rs:215` (`impl std::error::Error for ChiselError {}`)

**Problem:** the trait impl is empty. `IoError(io::Error)` wraps an inner cause but exposes it nowhere — `e.source()` returns `None` for every variant. This breaks error-chain walkers (`anyhow::Error::root_cause`, structured-logging adapters, `eyre` reports). The Display message is the only signal an upstream caller can see.

**Direction of fix:** implement `fn source(&self) -> Option<&(dyn Error + 'static)>` that returns the inner `io::Error` for `ChiselError::IoError(e)` and `None` for the rest. If future variants gain inner causes (e.g., wrapping a deserialization error), extend the match.

#### I42. Python `to_py_err` discards inner `io::Error` from `IoError(_)` [deepdive 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (PR #15; IoError instances now carry `.errno` (raw OS error code) and `.kind` (string form of `std::io::ErrorKind`) as Python attributes — callers branch on those instead of parsing the Display message)
**Where:** `python/src/errors.rs:209-249`

**Problem:** `to_py_err` formats `ChiselError` via `Display` and then drops the variant. A Python caller cannot programmatically distinguish ENOSPC from EACCES from EIO — they all become `chisel.IoError("I/O error: <prose>")`. The comment at lines 211-213 documents the choice as "the string is the only cross-boundary contract"; defensible but worth re-litigating if any caller wants disk-full-vs-permission-denied handling on the Python side.

**Direction of fix:** for `IoError`, attach the inner errno (where available) as a Python exception attribute (`errno` or `winerror`-style). PyO3 exception classes can hold arbitrary data; a `PyIoError::new_err((msg, errno))` would surface it. Trade-off: cross-boundary error fidelity vs. holding the Rust error chain in memory across the FFI boundary.

#### I43. bench `EngineResult` uses `Box<dyn Error + Send + Sync>` and erases engine class [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
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

#### I44. `libc::flock` `unsafe { … }` block missing `// SAFETY:` comment [deepdive 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (PR #14; multi-line SAFETY block documents the fd validity, flag composition, and signal-safety reasoning)
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

#### I45. `unreachable!` in `delete_inner` should be `CorruptPage` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
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

#### I49. `expect("LRU referenced page id not in entries")` should be `CorruptPage` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/page_cache.rs:865`

**Problem:** reachable if the LRU index and entries map ever desync. Currently kept in sync by `discard`/`truncate`/`flush`, but a future refactor that touches one without the other turns this into a library-reachable panic.

**Direction of fix:** translate to `Err(ChiselError::CorruptPage { page_id: victim_id })` and document that reaching this branch indicates the cache's two-data-structure invariant broke, which is genuinely a corruption signal worth surfacing typed.

#### I50. Hex literal `0x02` used instead of `FLAG_INTERIOR` constant in `open_existing` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `src/transaction.rs:417, 423`

**Problem:** `if root_buf[1] == 0x02 { ... }` reaches for a raw hex literal rather than `handle_table::FLAG_INTERIOR`. The constant exists; using the literal defeats the single-source-of-truth promise for the on-disk format and makes a grep for "interior" miss this site.

**Direction of fix:** import `handle_table::FLAG_INTERIOR` (exposing it if currently private — it's already implicitly public via the on-disk format) and compare against it. Same fix at both line 417 and the implicit comparison logic at 423.

### Performance

#### I51. `read_page` calls `page_count()` (one extra `lseek`) on every call [deepdive 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (PR #16; `PageIo` carries a `cached_page_count: Cell<u64>` HWM seeded at open and updated on every extending write — eliminates one `lseek` per cache miss; 5 regression tests in src/page_io.rs verify the cache stays coherent across truncate / extend / set_page_count)
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

#### I53. bench `file_size_bytes` triggers an O(live handles) walk via `stats()` [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (added `Chisel::file_size_bytes() -> Result<u64>` — O(1) single-field accessor that skips the handle-table walk; bench `chisel_engine.rs::file_size_bytes` now calls it instead of materializing the full Stats struct)
**Where:** `bench/src/chisel_engine.rs:106`

**Problem:** `self.db.stats()?.file_size_bytes` calls `Chisel::stats()`, which walks the entire handle table (O(live handles)) just to populate `handle_count`. Used per measurement cell in the bench harness — for 100K-handle scenarios this is ~milliseconds per call, dragged into every reporting step.

**Direction of fix:** add a dedicated `Chisel::file_size_bytes() -> Result<u64>` that reads `page_count * PAGE_SIZE` directly (the existing math in `Chisel::stats`) without the handle walk. The bench `file_size_bytes` impl calls the new method; the existing `stats()` keeps its current shape because callers want all three fields together.

### CI, packaging, and publication

#### I54. CI runs no supply-chain check [perf-review 2026-04-26 / deepdive 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (PR #13; `audit` job via `rustsec/audit-check@v1.4.1` runs on every push/PR; the hotfix history at PRs #17 + #26 cleared two transient ignores — paste 1.0.15 unmaintained and pyo3 0.22.x PyString overflow — both via real fixes, not permanent ignores)
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

#### I55. No MSRV pinned in `Cargo.toml` or CI [deepdive 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (PR #13; `rust-version = "1.82"` in all three subcrates' Cargo.toml; CI `msrv` job uses `dtolnay/rust-toolchain@1.82` and builds `-p chisel` only — scoping per I61 since bench's deps adopt edition2024 faster than we want to float the floor)
**Where:** `Cargo.toml`, `bench/Cargo.toml`, `python/Cargo.toml`, `.github/workflows/ci.yml`

**Problem:** `rust-version` is absent from every `Cargo.toml`. CI uses `dtolnay/rust-toolchain@stable`, so an unannounced 1.x MSRV bump can land silently. README says "Rust stable, edition 2021"; that's not a pinned MSRV. The codebase uses `let-else` (1.65+), `is_none_or` (1.82+), `is_some_and` (1.70+); actual floor is currently ≥ 1.82.

**Direction of fix:** pin `rust-version = "1.82"` (or whatever the current actual floor is — verify via `cargo msrv` if available) in `Cargo.toml`. Add a `msrv` job to CI that uses `dtolnay/rust-toolchain@1.82` and runs `cargo build`. If the project doesn't commit to MSRV stability pre-1.0, document that decision in the README.

#### I56. `Cargo.toml` lacks crates.io publication metadata [deepdive 2026-05-22] — **P1** ✅ FIXED 2026-05-22 (PR #10; root Cargo.toml has license, repository, readme, keywords, categories, description, authors; python and bench subcrates also have publish=false and matching metadata)
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
repository = "https://github.com/pgexperts/chisel"
readme = "README.md"
keywords = ["database", "storage", "embedded", "transactional", "shadow-paging"]
categories = ["database", "data-structures"]
```

#### I57. `License: TBD` blocks any third-party use [deepdive 2026-05-22] — **P1** ✅ FIXED 2026-05-22 (PR #10; MIT license; LICENSE file at repo root, README badge updated, license = "MIT" in Cargo.toml. User explicitly chose MIT-only over the conventional MIT-OR-Apache-2.0 dual)
**Where:** `README.md:326-327`

**Problem:** with no license, the code is "all rights reserved" by default — no one can legally use or distribute it. For a pre-1.0 project that's worth flagging visibly on the README.

**Direction of fix:** pick a license now. The prevailing Rust convention is `MIT OR Apache-2.0` dual. Drop `LICENSE-MIT` and `LICENSE-APACHE` files at the repo root, update README from "TBD" to the chosen license, and add `license = "MIT OR Apache-2.0"` to `Cargo.toml` (covered by I56).

#### I58. `bench/` is not in `ci.yml` [deepdive 2026-05-22, formalizing spillway-rollout lesson #1] — **P2** ✅ FIXED 2026-05-22 (PR #13 added a dedicated `bench-tests` job; superseded by I61's workspace migration in PR #23 — `cargo test` from root now covers bench via `default-members = [".", "bench"]`, so the standalone bench-tests job was removed in the same PR)
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

#### I59. `wheels.yml` has no early `cargo test` gate [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
**Where:** `.github/workflows/wheels.yml`

**Problem:** the wheels workflow builds and tests wheels at tag time. The CI matrix runs Rust tests on push/PR, and wheels.yml runs `pytest` after building wheels, so underlying Rust correctness is covered transitively. But a wheels build on a tag whose underlying commit is broken still runs pytest against broken bindings and fails there — slower feedback than a pre-wheel `cargo test`.

**Direction of fix:** either add an early `cargo test` step in wheels.yml, or make the wheels job `needs:` the test job (cross-workflow `needs` is supported via a reusable workflow or by re-running tests inline). Less urgent than I54/I55/I56 because wheels.yml is tag-triggered and the underlying problem only manifests if someone tags a broken commit.

#### I60. Orphaned `bench-disk-cleanup.yml` and `bench-os-update.yml` workflows [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (option b — schedule disabled, dispatch retained)
**Where:** `.github/workflows/bench-disk-cleanup.yml`, `.github/workflows/bench-os-update.yml`

**Problem:** both workflows are queued waiting for a self-hosted runner that hasn't been provisioned. While they're queueing/expiring, GitHub may surface them as "stuck workflows" in the Actions UI.

**Direction of fix:** either:
- (a) commit to provisioning the dedicated runner per the "Dedicated bench machine foundation" spec; or
- (b) flip both workflows to `workflow_dispatch:` only with a `# DISABLED until self-hosted runner provisioned` header, so they don't accumulate failed runs.

Choice (a) is the planned path per the spec; choice (b) is the cleanup if the plan moves out by months.

#### I61. No workspace manifest [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (default-members = [".", "bench"]; python excluded due to maturin linker requirements)
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

#### I70. No `#[should_panic]` tests for `unreachable!` / `expect` invariant sites [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (reframed post-Group F: typed CorruptPage variant contract covered by `corrupt_page_variant_contract` in src/error.rs; integration-test coverage of the I14 path already existed in src/recovery_tests.rs)
**Where:** test coverage for `src/transaction.rs:1375` (`unreachable!`), `src/page_cache.rs:865` (`expect`), `src/transaction.rs:1846` (`expect`)

**Problem:** the codebase has good regression tests for documented invariants (I1, I3, I7, I18, I27, I28, I29 all have dedicated tests). The `unreachable!` and `expect` sites name invariants but don't have tests that exercise the invariant-violating path.

**Direction of fix:** lower priority — and if I45 + I49 convert these to typed `CorruptPage` errors, this finding becomes a test for the `CorruptPage` arm instead. Wait for those decisions; revisit afterward.

#### I71. No property tests (`proptest` / `quickcheck`) for byte-roundtrip code [deepdive 2026-05-22] — **P3** ✅ FIXED 2026-05-22
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

#### I73. GitHub Actions Node.js 20 deprecation (every job uses `actions/checkout@v4`) [post-P2 CI run 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (checkout v4 → v5 across all four workflows; other actions already Node 22+)
**Where:** every job in `.github/workflows/ci.yml` (test, clippy, fmt, audit, msrv, bench-tests, python matrix) — currently 7 distinct jobs all pinned to `actions/checkout@v4`. The `dtolnay/rust-toolchain`, `Swatinem/rust-cache@v2`, `actions/setup-python@v5`, and `rustsec/audit-check@v1.4.1` actions should also be re-checked for Node 24 readiness.

**Problem:** GitHub is forcing Node.js 24 as the default on hosted runners on **June 2nd, 2026** (~10 days from today). Node.js 20 will be removed entirely on **September 16th, 2026**. Surfaced as warning annotations on the first green post-P2 audit run on main:

> `! Node.js 20 actions are deprecated. The following actions are running on Node.js 20 and may not work as expected: actions/checkout@v4. … For more information see: https://github.blog/changelog/2025-09-19-deprecation-of-node-20-on-github-actions-runners/`

CI keeps passing today, but the warning is on every run and there is a hard cutoff coming.

**Direction of fix:** bump each `uses:` line to a version whose action manifest declares `node24` once those versions are GA. As of 2026-05-22, the v5 line of `actions/checkout` is the natural target; the other actions cited above need a one-shot audit of their action.yml `runs.using` field. Until then, the temporary opt-in is the `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` env var on the runner — but that's worse than just bumping the pin once. Single-PR scope: edit `.github/workflows/ci.yml`, push, verify one CI run goes green.

Low priority because runners auto-upgrade on the cutoff date anyway; medium-low because if `actions/checkout@v5` is GA before then, this is a five-minute PR that closes the warning noise immediately.

#### I74. Expose `Spillway::logical_bytes` and `max_bytes` via `Chisel::stats` / `ChiselCounters` [I65 follow-up 2026-05-22] — **P3** ✅ FIXED 2026-05-22 (added `spillway_logical_bytes: Option<u64>` and `spillway_max_bytes: Option<u64>` to Stats; wired through PageCache → TransactionManager → Chisel::stats; Python `chisel.Stats` dataclass updated; integration tests in tests/spillway_integration.rs + python/tests/test_stats_defrag.py exercise the None → Some(>0) → Some(0) lifecycle)
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

#### I75. Bump `pyo3` from 0.22 to 0.24+ to clear RUSTSEC-2025-0020 [I61 follow-up 2026-05-22] — **P2** ✅ FIXED 2026-05-22 (bumped to 0.24; RefCell→Mutex, Cell→AtomicBool for PyO3 0.24's Sync requirement; cargo audit shows 0 vulns / 0 warns; 85/85 Python tests pass)
**Where:** `python/Cargo.toml` (`pyo3 = { version = "0.22", features = ["extension-module", "abi3-py311"] }`); call sites throughout `python/src/`.

**Problem:** `pyo3 0.22.x` (we're pinned at 0.22.6 in Cargo.lock) ships `PyString::from_object` with a "Risk of buffer overflow" — `from_object` takes `&str` arguments and forwards them directly to the Python C API without checking for terminating NUL bytes, so the Python interpreter can read beyond the end of the `&str` data and potentially leak the contents of the OOB read by raising a Python exception containing a copy of the data including the overflow.

RustSec advisory: RUSTSEC-2025-0020. Patched in pyo3 0.24.1+. Our binding may or may not call `PyString::from_object` directly; either way, the vulnerable function ships in our `.so` and gets exposed to any code that links against it (including future internal code).

Surfaced when I61 unified all three subcrates' lockfiles under a workspace `Cargo.lock` — pre-I61, only the root crate's deps (no pyo3) were audited. Worked around in the I61 PR by adding `ignore: RUSTSEC-2025-0020` to the audit job; this entry documents the proper fix.

**Direction of fix:** bump `pyo3` from 0.22 → 0.24 (or whatever's current as of the fix). The migration is non-trivial because PyO3 has breaking API changes between majors — notable changes in 0.23/0.24 include:
- `Bound<T>` is the standard reference type now (most APIs that returned `&PyAny` now return `Bound<PyAny>`).
- `value_bound` / `extract_bound` style was made the default.
- Some attribute-setting paths changed.

Plan:
1. Read PyO3 0.23 and 0.24 migration guides.
2. Bump `pyo3` to the target version in `python/Cargo.toml`.
3. Iterate on `python/src/db.rs`, `transaction.rs`, `savepoint.rs`, `errors.rs` to fix breakage. The errno/kind setattr code (added in I42) and the `_chisel.create_exception!`-style code in `errors.rs` are the most likely friction points.
4. Run `cd python && maturin develop --release && pytest` until green.
5. Remove `ignore: RUSTSEC-2025-0020` from `.github/workflows/ci.yml`.
6. Mark I75 as ✅ FIXED here.

Higher priority than most other P3s because (a) it's a real (not informational) security advisory and (b) the `ignore:` workaround is a temporary measure that should be cleared promptly.

#### I76. Periodic clean-checkout `cargo clippy` job to catch lint regressions hidden by build cache [I53/I74 follow-up 2026-05-22] — **P3**
**Where:** `.github/workflows/ci.yml` (would add a new scheduled job).

**Problem:** CI uses `Swatinem/rust-cache@v2` so per-PR clippy runs are fast — but the cache can mask a clippy regression. Concretely: PR #27 introduced `1024u64 * cache_max_bytes as u64` in `tests/spillway_integration.rs`. The `as u64` cast is redundant (`cache_max_bytes` was inferred to u64), but PR #27's CI ran clippy against a warm cache from the previous PR's build and the lint never fired. PR #28 (cleanup, against a fresh branch) caught it because the cache key changed (different branch, different file set) and clippy did a full re-check.

The general shape: any time `cargo clippy --all-targets` skips a file because the cached build artifact is still valid, lints that depend on context (`-D warnings`, new lints in a clippy upgrade, lints that fire only against fresh expansion) can be silently missed. The cost is a wrong-feedback signal at PR time — the regression lands and is found by a future unrelated PR.

**Direction of fix:** add a scheduled-trigger CI job that runs clippy without the cache:

```yaml
clippy-no-cache:
  # Periodic clean-checkout clippy. Catches lint regressions hidden by
  # Swatinem/rust-cache@v2's incremental cache on PR-triggered runs.
  # See I76 in ISSUES.md.
  on:
    schedule:
      - cron: '0 6 * * 1'  # Mondays, 06:00 UTC
    workflow_dispatch:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v5
    - uses: dtolnay/rust-toolchain@stable
      with:
        components: clippy
    # No Swatinem/rust-cache step — intentional.
    - run: cargo clippy --all-targets --workspace --exclude chisel-py -- -D warnings
```

Runs once a week; ~5 minutes per run; would have caught the PR #27 leak immediately on the following Monday. Cheap insurance for a real (if rare) failure mode.

Low priority because (a) the leak is caught eventually by the next unrelated PR's CI, (b) the leak is always a clippy warning (not a runtime bug), (c) the cleanup PR #28 already absorbed the actual fix.

---

## Perf-review findings (2026-06-01)

Source: `docs/reviews/perf-review-2026-06-01.md` (read-only full hot-path sweep, `chisel-performance` + `rust-performance` skills; six-agent parallel audit over `src/` and `bench/`). No correctness bugs, no Don't-Break-List violations. Each item carries its review-doc ref (PR-x) and classification — **[STATIC FACT]** (provable by reading) or **[HYPOTHESIS]** (mechanism certain, magnitude needs a bench). Severities are as-assessed by the sweep and open to re-triage. Line numbers are as-of the sweep and prefixed `~`; the symbol names are the durable anchor.

**Sequencing:** the benchmark-harness items (I90–I92) are prerequisites — the grid cannot trustworthily validate an engine change until they land, and I90 (`black_box`) must precede I91 (LTO profile) because LTO widens the dead-code-elimination window. Measure the two engine P1s (I77, I78) only afterward. Do not gate CI pass/fail on the resulting numbers (shared-runner noise; perf is report-only per the project CI policy).

### Engine — hot path & commit

#### I77. Default SipHash on `PageCache.entries` and `LruIndex.nodes` [perf-review 2026-06-01] — **P1** (PR-A, [HYPOTHESIS]) ✅ FIXED 2026-06-01 (rustc_hash::FxHashMap on both maps; zero new transitive deps — only the 3rd runtime dependency. Behavior-preserving: 174 engine tests green. Magnitude still unmeasured — run the read-warm bench, with I90 black_box + I91 profile, on dedicated hardware)
**Where:** `src/page_cache.rs` `entries` (init ~:167) + `get`/`get_mut`; `src/lru.rs` `nodes` (~:67) + `push_front`/`unlink`

**Problem:** both hot maps use `std::collections::HashMap` with the default SipHash-1-3 `RandomState`. A warm `cache.get(id)` costs ≈ six SipHash probes of a `u64` key (`contains_key` + `get` + the `touch_lru` bookkeeping); a depth-2 read calls `get` four times → ≈ 24 SipHash hashings per cache-hit read. SipHash's DoS-resistance is worthless for a single-writer embedded engine keyed on trusted local page IDs. Related (PR-R): `get`/`get_mut` and the `LruIndex` ops also double-probe (`contains_key` then `get`) — folding those into a single probe is the same area.

**Direction of fix:** swap both maps to a fast `u64` hasher (`foldhash`, `rustc_hash::FxHashMap`, or `ahash`). Validate on the existing `read-warm` micro-grid row (all cache hits, zero I/O — isolates hashing + LRU cost). In-memory only; no on-disk format, fsync, poison, or `&mut self` interaction; LRU order rides the linked-list pointers, not map iteration order.

#### I78. Per-insert full-page XXH3 re-stamp on the slot-packing path [perf-review 2026-06-01] — **P1** (PR-B, [HYPOTHESIS]) — ⏸️ OPTIMIZATION DEFERRED 2026-06-01 (pending a bulk-insert bench on Phase 0's tooling / dedicated hardware). Investigation CORRECTED the caveat below: the eager stamp is NOT needed for the evict-mid-transaction case — the spillway round-trip verifies its own `slot_checksum`, never the internal page checksum (only main-file cold-load does, `page_cache.rs:872`). The eager stamp's real job is "valid internal checksum before the main-file write — commit flush OR spill-then-drain"; that drain path is precisely what makes the deferral non-trivial. The misleading `transaction.rs` comment is fixed; see memory `project_chisel_i78_restamp_deferred.md`.
**Where:** `src/transaction.rs` `insert_into_data_page` (cursor path ~:1856-1869, fresh-page path ~:1892); cost in `src/page.rs` `compute_checksum`

**Problem:** `page::stamp_checksum(buf)` runs after **every** `DataPage::insert`, hashing all 8184 page-body bytes regardless of how few changed. A 1000-small-value transaction packing ≈ 39 values/page re-hashes its data pages once per value instead of once per page — `O(values × 8 KB)` where `O(pages × 8 KB)` would do.

**Direction of fix:** defer stamping — stamp a data page lazily at flush time and on the eviction path (a "needs-stamp" sub-flag, or stamp on cursor retirement + an eviction hook). **Caveat (Don't-Break #8):** a stamped checksum must still be guaranteed before any eviction-to-spillway/disk and before flush; eager stamping exists for exactly the evict-mid-transaction case (documented at `transaction.rs:1839-1842`). Validate on `allocate-1000pertx` / `update-1000pertx` at small value sizes.

#### I79. `commit_inner` promotes `current_freemap` / `current_live_slots` by clone, not swap [perf-review 2026-06-01] — **P2** (PR-F, [STATIC FACT])
**Where:** `src/transaction.rs` `commit_inner` step 5 (~:917-920); mirrored per `begin()` (~:717-721)

**Problem:** every commit deep-copies the full 8 KB `Box<[u8; PAGE_SIZE]>` freemap and rebuilds the `O(live-pages)` `current_live_slots` HashMap, even for a single-row commit (one bit / one page changed). The bitmap is moved ≈ 3× per small commit (this clone + the `persist_freemap` copy at ~:684).

**Direction of fix:** `std::mem::swap(&mut committed_*, &mut current_*)` at step 5, then reseed `current_*` from `committed_*` at the next `begin()` (which already clones). Same shape as the accepted I52 fix; strictly fewer allocations, so it does not need a bench to justify (one would size it). **Don't-Break:** the swap must occur after the step-4 fsync linearization point (it does).

#### I80. `maybe_evict` re-scans the dirty LRU prefix per eviction in the mixed clean/dirty regime [perf-review 2026-06-01] — **P2** (PR-G, [HYPOTHESIS])
**Where:** `src/page_cache.rs` `maybe_evict` Phase A (~:912-930)

**Problem:** the `dirty_count == entries.len()` early-out only fires when **every** entry is dirty. In a mixed regime (clean read-through pages interleaved with dirty pinned ones, clean victims toward the MRU end) the `iter_lru_to_mru().find(|id| !dirty)` walks past the whole dirty prefix from the tail on each call — `O(n²)` across a transaction that evicts repeatedly.

**Direction of fix:** a separate intrusive "clean LRU" sub-list or a free-victim cursor so eviction is `O(1)` amortized. Confirm the regime occurs first (pure-write hits the all-dirty early-out; pure-read finds the first tail item — the quadratic only bites mixed read+write transactions large enough to evict). No invariant exposure.

#### I81. No cache buffer pool — malloc/free per page churn + 8 KB memset on `new_page` [perf-review 2026-06-01] — **P2** (PR-H, [HYPOTHESIS])
**Where:** `src/page_cache.rs` page allocation sites (~:326, :446, :736, :853, :873); eviction drops the `CacheEntry` (frees its Box)

**Problem:** under sustained pressure (working set > cache) the cache does malloc-on-load / free-on-evict in lockstep — a fresh 8 KB allocation per miss and per `new_page`, the just-freed buffer not reused; `new_page` additionally zeroes 8 KB. (Alloc-heavy → worst case for shared-CI bench noise; measure on dedicated hardware, report-only.)

**Direction of fix:** a small free-list capped at a few× the cache size — push freed `Box`es on eviction, pop on load / `new_page` (zero only when handing out a `new_page`). The **small-pool** idea, explicitly NOT the deferred I34 mmap-backed-storage redesign.

#### I82. `page_io` / `spillway` use seek+write (two syscalls/page) instead of positioned pread/pwrite [perf-review 2026-06-01] — **P2** (PR-I, [HYPOTHESIS])
**Where:** `src/page_io.rs` `read_page` (~:209-213) / `write_page` (~:236-240); `src/spillway.rs` `write_slot` (~:280-284) / `read_slot` (~:307-311)

**Problem:** each page I/O issues `seek(Start(off))` then `read_exact`/`write_all` — two syscalls where `FileExt::{read_exact_at, write_all_at}` (pread/pwrite) does one. `flush()` calls `write_page` once per dirty page per commit; the spillway drain adds more (and splits header/page into two writes).

**Direction of fix:** switch the `File` arm to positioned I/O; for the spillway, assemble header+page into one `[u8; SLOT_SIZE]` and issue a single positioned write. Stays inside the I/O modules (invariant 6); the single-writer flock makes shared-offset removal safe. For fsync-dominated commits the win may be in the noise — bench with a large-dirty-set commit + syscall count.

#### I83. No `read_many` — batched reads re-descend the handle table per key [perf-review 2026-06-01] — **P2** (PR-J, [HYPOTHESIS])
**Where:** absent from `src/lib.rs` / `src/transaction.rs` (only single `read`)

**Problem:** the read-side analogue of the deferred I33. K reads pay K independent `find_leaf` descents from the root; keys sharing interior/leaf pages (monotonic handles read in ranges) re-fetch and re-hash the same interior pages K times.

**Direction of fix:** land I77 first (cuts per-descent cost for free). Whether a leaf-grouping `read_many` beats the simple loop is the hypothesis — settle with a clustered-read bench row before building the batched descent (same YAGNI posture that deferred I33). Read-only `&self`; preserve per-handle error semantics.

#### I84. Freemap `allocate_first` linear-scans the bitmap from byte 0 with no cursor [perf-review 2026-06-01] — **P3** (PR-O, [HYPOTHESIS])
**Where:** `src/freemap.rs` `allocate_first` (~:135-146); found independently by the commit and write-path agents

**Problem:** every freemap-backed allocation (and the per-commit freemap-page placement) scans from byte 0. With a dense low region and frees concentrated high, each allocation re-walks the same leading zero bytes — `O(n²)` across a reuse-heavy transaction. Minor (single 8 KB page, whole-byte skip, L1/L2-resident), but pure repeated work.

**Direction of fix:** an in-memory `next_free_hint` byte cursor (reset on `begin()` clone / lower-id `mark_free`), optionally `u64`-word scanning. In-memory allocator state only — no on-disk format impact, and the I18 lowest-id ordering is about which ids are visible, not scan order. ARCHITECTURE.md:434 documents the from-0 scan as behavior; this is the perf entry for it.

#### I85. `update_inner` always relocates; in-code cost-model text inaccurate under R1 [perf-review 2026-06-01] — **P3** (PR-K, [STATIC FACT]; doc + narrow opt)
**Where:** `src/transaction.rs` `update_inner` (~:1251-1321); unused in-place `src/data_page.rs` `DataPage::update`

**Problem:** a same-size update unconditionally retires the old slot and packs the new value into the cursor page (delete+insert leaving a tombstone), so the cost-model line "update(same size) = 1 data page COW" does not describe reality — there is no in-place data COW. Correct under R1 (a committed packed page can't be rewritten in place without rewriting every co-resident handle); the finding is a doc inaccuracy plus a narrow optimization.

**Direction of fix:** correct the cost-model text. Optional narrow win: when the old value lives on a page dirtied **in the current transaction** and the new value fits the old slot, overwrite in place via `DataPage::update`. **Don't-Break:** any in-place path MUST be restricted to current-transaction dirty pages — overwriting a committed data page violates shadow paging.

### Spillway

#### I86. Spillway re-validates XXH3 on every unspill — redundant with the main-file checksum on the drain path [perf-review 2026-06-01] — **P3** (PR-P, [STATIC FACT]; hazard-flagged)
**Where:** `src/spillway.rs` `rehydrate` / `slot_checksum` (~:235-255); callers `src/page_cache.rs` drain (~:438) and resident read-back (~:848)

**Problem:** every `rehydrate` recomputes an XXH3 over the slot. On the **drain** path the bytes are immediately written to the main file and re-validated structurally on their next cold load, so that verify is arguably redundant (spilled → checksummed → drain-verified → cold-load-verified). **But** the resident read-back path re-inserts the rehydrated page as dirty without a fresh main-file write, so dropping its checksum would let an undetected sidecar bit-flip enter the cache and later flush with a freshly-stamped valid-looking checksum — silent corruption.

**Direction of fix:** at most skip the verify on the drain-and-immediately-rewrite path; **keep** it on the resident read-back path (Don't-Break #8). The spill-side checksum (torn-sidecar-write detection) stays. Marginal — bench a spill-heavy workload before touching.

#### I87. Spillway micro-allocations: rehydrate `Box::new(buf)` copy + per-batch `drain_batch` Vec [perf-review 2026-06-01] — **P3** (PR-Q, [STATIC FACT])
**Where:** `src/page_cache.rs` rehydrate sites (~:436-448, :848-856, :866-873); `src/spillway.rs` `drain_batch` (~:204-210)

**Problem:** (a) `read_slot` / `rehydrate` return `[u8; PAGE_SIZE]` by value, then the cache does `Box::new(buf)` — a stack→heap 8 KB copy on top of the file read (the spill *write* side is clean). (b) `drain_batch` allocates a fresh `Vec<u64>` per batch — the exact shape I52 eliminated elsewhere with a reused scratch vec, not applied here.

**Direction of fix:** (a) have `rehydrate` / `read_slot` take a `&mut Box<[u8; PAGE_SIZE]>` out-param and `read_exact` directly into the heap buffer (pairs with I81's pool). (b) reuse a `drain_scratch: Vec<u64>` mirroring `dirty_scratch`. Sidecar layout is ephemeral — free to change.

#### I88. `Spillway::logical_bytes` over-reports after `forget` (high-water, not live) [perf-review 2026-06-01] — **P3** (PR-U, [STATIC FACT]; stats-accuracy, non-perf)
**Where:** `src/spillway.rs` `logical_bytes` (~:128-130)

**Problem:** `logical_bytes` returns `next_slot_index * PAGE_SIZE`, but `forget` / `forget_above` remove from `slots` without decrementing `next_slot_index` (slots aren't reused until `truncate`). During a drain (which calls `forget` per page) the I74 `spillway_logical_bytes` stat reports the high-water size, not the live resident size. Stats-accuracy quirk surfaced via the I74 surface; not a perf or durability issue.

**Direction of fix:** track a live-byte counter decremented in `forget` / `forget_above`, or document `logical_bytes` as a high-water mark. Decide alongside any I74 stats follow-up.

### Error path

#### I89. Savepoint error variants allocate a `String` at construction [perf-review 2026-06-01] — **P3** (PR-S, [STATIC FACT])
**Where:** `src/error.rs` `SavepointNotFound(String)` / `DuplicateSavepoint(String)` (~:30-31); constructed at `src/transaction.rs:1005,1054,1093` via `name.to_string()`

**Problem:** these eagerly heap-allocate at error construction (error branch only). Cold savepoint-control path, so no perf impact — recorded for completeness and as the contrast to `InvalidHandle(u64)`, which correctly carries a bare `u64` with lazy `Display` formatting on the hot lookup-miss path.

**Direction of fix:** none required for perf. If the error enum is ever reworked, a borrowed / `Box<str>` payload would remove the allocation, but it is not worth a standalone change.

### Bench harness & build (measurement-integrity cluster — prerequisites)

#### I90. Bench harness has no `black_box` — read/alloc results are dead-code-eligible [perf-review 2026-06-01] — **P1** (PR-C, [STATIC FACT]) ✅ FIXED 2026-06-01 (std::hint::black_box on both ends of apply_op's Read arm; one funnel covers the scenario tier + warm/cold micro-grid rows + drive_workload_with_tx_granularity)
**Where:** `bench/src/runner.rs` `apply_op` (~:216-250); `bench/benches/micro_grid.rs` read loops (~:111-115, :139-144); `bench/benches/scenarios.rs` timed loop. `grep black_box bench/` → none.

**Problem:** read results (`Vec<u8>`) are dropped, never observed; the criterion closures return `()`, so criterion's built-in `black_box` on the closure return value doesn't protect per-op results. The optimizer may elide the unused-result work — and the risk **grows when the I91 LTO profile lands**. This is the single most important bench-hygiene gap and must be fixed first.

**Direction of fix:** feed the resolved id through `black_box` on the way in and `black_box` the returned bytes in `apply_op` and the read loops. Bench-only; consumer-neutral.

#### I91. No `[profile.release]` — Chisel/redb benched under weaker codegen than bundled SQLite [perf-review 2026-06-01] — **P1** (PR-D, [STATIC FACT] asymmetry / [HYPOTHESIS] magnitude) ✅ FIXED 2026-06-01 (workspace-root [profile.release] lto="fat" + codegen-units=1, plus [profile.release-with-debug] for profiling; consumer-neutral — root profile affects only in-repo builds. Tracked baselines need a one-time re-record)
**Where:** workspace `Cargo.toml` (no `[profile.*]`); confirmed absent in `bench/` and `python/`; no `.cargo/config.toml`.

**Problem:** with no profile override, `cargo bench` builds `chisel` + `redb` at `opt-level=3` but `lto=false`, `codegen-units=16`, `panic=unwind`, while `rusqlite` `bundled` compiles SQLite C at its own `-O2`. The headline cross-engine table handicaps the two Rust engines (no cross-crate inlining across the `chisel`→bench boundary) — a "SQLite is faster here" conclusion is partly a build-config artifact. (The harness already equalizes the macOS `F_FULLFSYNC` path in `sqlite_engine.rs:50-62`; profile parity is the missing build-side half.)

**Direction of fix:** add a tuned profile at the **workspace root** (`lto = "thin"`/`"fat"`, `codegen-units = 1`) and re-record baselines once (uniform shift, not a regression). Land **after** I90. **Consumer-neutral:** a workspace-root profile affects only in-repo builds (benches/tests/examples); Cargo ignores a dependency's profile, so downstream `chisel = "0.1"` consumers are unaffected. Do NOT tune via the published lib crate.

#### I92. In-memory Chisel backend exists but is never benched [perf-review 2026-06-01] — **P1** (PR-E, [STATIC FACT]) ✅ FIXED 2026-06-01 (ChiselMemory EngineMode → chisel-mem column in the scenario tier; chisel-mem − chisel-strict wall-clock delta = the fsync tax. Scenario-tier only — micro-grid's fs::copy fixtures can't seed a Vec-backed engine. Regression test pins wiring + the fsync_calls protocol-counter subtlety)
**Where:** `bench/src/runner.rs` `EngineMode` (~:30-98, only `ChiselStrict` → `open_file`); `micro_grid.rs:168`, `scenarios.rs:27-31` all file-backed. `ChiselEngine::open_in_memory` only used in `bench/tests/`.

**Problem:** without an in-memory row the harness can't separate Chisel's CPU cost (slot packing, XXH3, handle-table walk, COW) from its full durability cost (fsync, `F_FULLFSYNC`, pwrite) — the decomposition that makes a "commit is slow" finding actionable. A pure-CPU regression can be masked by fsync-dominated wall time. (chisel-performance Lever 5.)

**Direction of fix:** add a `ChiselMemory` `EngineMode` via `open_in_memory(cache_size)`, include it in `EngineMode::ALL` and the scenarios mode list (cleanest through `run_scenario_cell`, which pre-populates in-process). Track both backends so the ratio is visible. Bench-only.

#### I93. Bench binaries could pin `mimalloc` for realistic best-case + cross-engine allocator parity [perf-review 2026-06-01] — **P2** (PR-N, [STATIC FACT] absent / [HYPOTHESIS] win) ✅ FIXED 2026-06-01 (#[global_allocator] = MiMalloc in the micro_grid + scenarios bench binaries; mimalloc 0.1.52 dev-dep, publish=false — never the library. Builds on MSRV 1.82, fat-LTO release links + launches clean. Re-record baselines)
**Where:** no `#[global_allocator]` anywhere; natural home `bench/benches/{scenarios,micro_grid}.rs`

**Problem:** Chisel (`Box<[u8;8192]>` per page, `Vec<u8>` per read) and redb (`value().to_vec()`) are alloc-heavy; SQLite's C core does far less Rust-side heap traffic. All three run on the platform default allocator, so the tracked numbers reflect "Chisel on system malloc," not its best, and the allocator tax falls unevenly.

**Direction of fix:** set `#[global_allocator] = MiMalloc` in the two bench binaries (the `publish=false` crate) and re-record baselines. **Consumer-neutral and load-bearing:** a `#[global_allocator]` is process-global and a library must never force one — the bench crate is the only correct home; do NOT add it to `chisel` or `chisel-py`.

#### I94. Scenario timed region includes begin/commit framing, per-op `Instant::now()`, and payload `Vec` alloc [perf-review 2026-06-01] — **P2** (PR-M, [STATIC FACT] region / [HYPOTHESIS] distortion)
**Where:** `bench/src/runner.rs` scenario timed loop (~:548-563) + `apply_op` payload alloc (~:230-240)

**Problem:** the CI-tracked scenario tier brackets the whole loop, takes a per-op `Instant::now()` pair inside it, and builds `vec![0u8; size]` inside the timed op. Per-op clock reads (200K on a 100K-op run) and payload alloc/zeroing fold into throughput, taxing fast (read) ops more than fsync-bound (write) ops and compressing cross-engine deltas. (The micro-grid correctly hoists setup into `iter_batched`; the hand-rolled scenario timer does not.)

**Direction of fix:** pre-build payloads once per size outside the loop; compute throughput from a single outer timer over a loop with no inner `Instant::now()` (run the latency-sampled pass separately); document that scenario throughput is per-transaction. Bench-only.

#### I95. Noise gate N=1 false-green; gate/diff conflate "no signal" with PASS/FAIL [perf-review 2026-06-01] — **P2** (PR-L, [STATIC FACT])
**Where:** `bench/src/noise_gate/cov.rs` (~:24-30, :36), `bench/src/bin/noise_gate.rs` (~:147-148), `bench/src/diff/compare.rs` (~:200-208)

**Problem:** `compute_cov` returns `0.0` for a single sample and the gate treats `cov <= threshold` as pass, so a `--runs 1` invocation reports PASS with zero observed variance — qualifying a noisy machine (defeating the gate's purpose). Zero-mean cells produce `NaN`/`inf` cov and `delta_pct`, marked failing rather than recognized as broken. (The gate is correctly **report-only** — `diff.rs` returns success regardless of regression count and `bench.yml` has no `needs:` into the gating jobs — so it cannot turn CI red on shared-runner noise, consistent with policy; these gaps are about report trustworthiness, not gating.)

**Direction of fix:** require `runs >= 2`; surface a distinct `INDETERMINATE` / `Undefined` state for N<2 and zero-mean cells so "no signal" renders separately from PASS/FAIL. Guard `baseline == 0.0` in the diff. Bench-only.

#### I96. Scenario tier has no warmup-discard — cold-start ops inflate tracked p95/p99 [perf-review 2026-06-01] — **P3** (PR-T, [STATIC FACT])
**Where:** `bench/src/runner.rs` `run_scenario_cell` percentile computation (~:570-574)

**Problem:** the single-run-no-warmup scenario design folds cold-start ops (cold cache, unwarmed allocator arenas / branch predictors) into the same distribution as steady-state, inflating the tracked `THRESHOLD_PCT_P95/P99` tails.

**Direction of fix:** discard a warmup prefix from `per_op_ns` before percentile computation, or run a short untimed warmup pass first. Bench-only; document the choice.

---

## Feature requests (2026-06-02)

Source: primary Chisel client, 2026-06-02.

### I97. Streaming handle enumeration (`for_each_handle` callback) [client 2026-06-02] — **P3**
**Where:** `src/lib.rs` `Chisel::handles` (~:512); `src/transaction.rs::handles_inner` (~:1435); `src/handle_table.rs::iter_live` / `iter_recursive` (~:353 / ~:604)

**Motivation:** `Chisel::handles()` already enumerates all live handles, but it MATERIALIZES the full `Vec<u64>` (plus a transient `Vec<(u64, HandleEntry)>` inside `iter_live`). For a database with millions of live handles that is a multi-megabyte allocation spike on top of the `O(live handles)` tree walk — the same walk that makes `stats()` O(live handles), see I53. A caller that wants to process every handle without holding them all in memory has no lazy option today.

**Direction of fix:** add a callback enumerator — e.g. `Chisel::for_each_handle(&self, f: impl FnMut(u64)) -> Result<()>` — that drives `iter_recursive` directly and invokes the closure per live handle instead of pushing into a Vec. A callback, NOT a lazy `Iterator`, is the right shape: the walk holds `&mut PageCache` throughout (pages load/evict as it descends), so a lazy iterator would have to carry that mutable borrow across every `next()` — borrow-checker-hostile. `iter_recursive` already has the structure; it would call `f(handle)` where it currently does `result.push((handle, entry))`. Keep `handles()` as the eager convenience built on top. The Python binding can expose a generator backed by the callback — which would also make the existing `Iterable[int]` `.pyi` annotation honest (it returns an eager `list` today).

**Thin-impl note (client):** even a callback that internally collects then iterates is a strict ergonomic win over returning the Vec; the zero-materialization version is the actual goal.

### I98. Make the I28 commit pre-drain conditional — common-case 3→2 fsyncs [client 2026-06-02] — **P2** (investigate; durability-sensitive)
**Where:** `src/transaction.rs::commit_inner` — the pre-drain `self.cache.borrow_mut().flush()?` at ~:855 (I28), ahead of `persist_freemap` (~:864) and the step-1 flush (~:873). The count is pinned by `tests/spillway_integration.rs::no_spill_workload_preserves_two_fsync_commit` (~:111, currently asserts `== 3`).

**Motivation (client 2026-06-02):** clients want Chisel to minimize fsyncs internally and transparently — no client API needed (the transaction model already batches all ops of a commit into one durable write set; `delete_many` + I33 cover bulk delete). The concrete internal lever is the commit fsync count. Every commit does THREE fsyncs today: the I28 pre-drain (~:855), the step-1 data flush (~:873), and the superblock fsync (step 4). The pre-drain exists ONLY to stop `persist_freemap`'s `allocate_data_page` from tripping `maybe_evict`'s spill-or-`CacheFull`/`SpillwayFull` decision — and per the I28 comment that requires a narrow state: *"every existing entry dirty, nothing evictable, and either spillway disabled or full."* In the COMMON case (cache below the cap, or with clean evictable pages, or spillway headroom) the pre-drain is dead weight: a single step-1 flush could carry every user-dirty page PLUS the one freemap page in ONE fsync, returning the commit to its 2-fsync shadow-paging floor.

**Direction of fix:** make the pre-drain conditional. Before ~:855, ask the cache whether `persist_freemap`'s single-page allocation could trip the ceiling — a cheap `PageCache` predicate over `entries.len()` vs the cap, the presence of any clean (evictable) entry, and spillway headroom. If allocation is provably safe, SKIP the pre-drain and let the step-1 flush (~:873) fsync everything once (2 fsyncs). Keep the pre-drain whenever the predicate is unsure or the saturated-all-dirty-no-spillway state holds. A conservative predicate (keep the pre-drain on ANY doubt) preserves the I28 guarantee with zero new risk in the rare case while removing a full fsync from every ordinary commit.

**Don't-Break #2 compliance:** this is NOT "optimizing the pre-drain away" (which the Don't-Break list forbids) — it ADDRESSES the underlying cache-ceiling interaction directly, keeping the flush precisely when the ceiling can bite. The poison-on-`CacheFull` hazard (I28) is untouched in the only state where it can occur.

**Measurement:** deterministic, not timing-dependent — `ChiselCounters::fsync_calls` counts fsyncs directly. A test asserts a no-spill commit drops 3 → 2, which would restore `no_spill_workload_preserves_two_fsync_commit` to its namesake (it pins `== 3` today only because I28 forced the third fsync — see I63). Wall-time win is one fewer fsync per commit on the file backend — significant on slow storage and on many-small-commit workloads (fsync is the dominant cost per the cost model). The in-memory backend shows the counter drop but no wall-time (its fsyncs are no-ops, cf. I92).

**Risk / why investigate-not-do:** durability-critical path. The predicate must be provably conservative against the post-spillway `CacheFull`/`SpillwayFull` interaction — the I28 reasoning predates the spillway and now composes with it. Same posture as I78: measure + an explicit architectural argument before implementation, not a blind edit.

### I99. Handle-table in-memory `depth` not restored on rollback — silent corruption after a rolled-back grow [internal 2026-06-02] — **P1 (critical correctness — silent data loss)** ✅ FIXED 2026-06-02
**Fix:** extracted the open-time spine walk into `HandleTable::recover_depth` and call it in `rollback_inner` + `rollback_to_inner` (mirroring the `MembershipIndex.outer_depth` fix); regression tests `handle_table_depth_restored_after_rolled_back_grow` / `..._after_rollback_to_savepoint` in `src/transaction.rs`. Direction A (minimal mirror-fix) below was taken; the structural Direction B (depth in `Roots`) remains a possible future cleanup, not needed now.

**Where:** `src/handle_table.rs` (`HandleTable.depth`, mutated by `grow` at ~:425; descent uses `self.depth` in `find_leaf`/`lookup`/`iter`); `src/transaction.rs::rollback_inner` (~:1005) and `rollback_to_inner` (~:1100), which restore `current_roots` from a snapshot but never touch `handle_table.depth`; the open-time left-spine recovery walk in `open_existing` (~:426–449) is the only place depth is ever re-derived.

**The bug:** `HandleTable` caches its tree depth in an in-memory `u32` for fast descent. `insert` mutates it (`self.depth += 1`) when the radix tree grows a level. But only the handle-table ROOT page id lives in `Roots`/the superblock — `depth` is NOT part of the snapshotted transactional state. So when a transaction that GREW the handle table is rolled back, `current_roots.handle_table_page` snaps back to the shallow committed root while `handle_table.depth` stays at the grown (too-deep) value. Every subsequent descent then walks one phantom interior level: `find_leaf` reads `buf[DATA_PAGE_HEADER_SIZE..+8]` of a *leaf* page (actually a `HandleEntry`'s `page_id` field) as if it were a child pointer, descends into a data page, and returns `None` → the caller sees `InvalidHandle` for a **previously-committed handle**, or (depending on the bytes) a wrong value / `CorruptPage`. This is silent data loss from valid input.

**Confirmed (probe, 2026-06-02):** commit exactly `ENTRIES_PER_LEAF` (510) handles (fills depth 0, no grow); `begin`; `allocate` one more (forces grow to depth 1); `rollback`; then `read(5)` for a committed handle → `Err(InvalidHandle(5))`, where the same read returned its 2-byte value moments earlier. Minimal trigger: N=510 committed handles + 1 grown-then-rolled-back allocate + any read of a committed handle.

**Why it stayed latent:** handle ids are dense and monotonic, so the depth-0→1 grow boundary (510 live handles) is a rare, sequential event that few tests combine with rollback-then-read. The identical bug class in the membership index's outer tree (`MembershipIndex.outer_depth`) was hit immediately during chunk-tags development because tag values are client-supplied and sparse — a single `allocate_tagged(v, tag>=1021)` grows the outer tree — and was FIXED on branch `feature/chunk-tags` (commit `7228c85`) by re-deriving `outer_depth` from the restored root in both rollback paths, mirroring open-time recovery. This issue is the **handle-table half of the same root cause**, left unfixed deliberately to keep that fix scoped and give the core handle table its own focused change + tests + review.

**Direction of fix (two options):**
- **A — minimal mirror-fix (low risk, proven):** extract the open-time left-spine walk (`open_existing` ~:426–449) into a reusable `HandleTable::recover_depth(cache, root) -> Result<u32>` (the analogue of `RadixU64::recover_depth`), call it at open AND in `rollback_inner`/`rollback_to_inner` right after `current_roots` is restored, then `handle_table.set_depth(d)`. Same shape as the membership fix already on `feature/chunk-tags`. Cost: one extra `cache.get` (possibly a disk read) per rollback. Eliminates the bug with the smallest, most local change.
- **B — structural (cleaner, eliminates the class):** make the in-memory depth part of the snapshotted state instead of a side-channel field — add `handle_table_depth` (and fold the membership `outer_depth` in too) to the in-memory `Roots` struct so `begin`/`commit`/`rollback`/`savepoint`'s existing `Roots` clones carry it automatically. No superblock format change needed (depth is derivable from the root; it only needs to ride the in-memory snapshot). Removes the entire "stranded in-memory radix depth across rollback" failure mode for both trees at once, at the cost of a moderate refactor threading depth through `Roots` rather than the `HandleTable`/`MembershipIndex` fields.

**Test to add (either fix):** the probe above as a regression test — commit `ENTRIES_PER_LEAF` handles, grow+rollback one more, assert every committed handle still reads back; plus the `rollback_to(savepoint)` variant. (The membership-index equivalents — `tagged_membership_survives_rolled_back_outer_grow` / `..._rollback_to_savepoint` in `src/transaction.rs` — are the template.)

**Severity note:** P1 because it is silent data loss on the engine's most fundamental structure, reachable from ordinary `allocate`+`rollback` usage with no unsafe code or corruption precondition. Mitigating factor: no production databases exist yet (cf. format-version tentativeness), and a process restart accidentally "heals" the in-memory depth via open-time recovery — so the corruption is confined to the lifetime of a single open handle after a rolled-back grow.

### I100. Callback enumeration `for_each_handle_with_tag` [chunk-tags out-of-scope 2026-06-02] — **P3**
**Where:** `src/transaction.rs` (`handles_with_tag`); `src/membership_index.rs` (`handles_for_tag`); `src/lib.rs` / `python/src/db.rs` public surface.

**Motivation:** `handles_with_tag(tag)` materializes the full `Vec<u64>` of a tag's members, the same multi-megabyte spike I97 calls out for `handles()`. A relation with millions of chunks has no lazy option. Mirror I97's planned `for_each_handle(f)` shape with `for_each_handle_with_tag(tag, f: impl FnMut(u64))` driving the inner radix iteration without collecting. Blocked on I97 landing first so the two share one callback idiom. Additive; the eager `handles_with_tag` stays as the convenience wrapper. The Python binding can expose a generator over the callback. Filed from the chunk-tags spec's "Out of scope for v1".

### I101. Bitmap inner sets for dense per-tag handle ranges [chunk-tags out-of-scope 2026-06-02] — **P3** (profile-gated)
**Where:** `src/membership_index.rs` — the per-tag inner `RadixU64` (handle → 1 membership bit).

**Motivation:** the membership index stores each tag's member set as a radix tree, optimal for the EXPECTED sparse handle distribution. If profiling on a real relational workload ever shows DENSE per-tag handle ranges (e.g. a relation whose chunks are allocated contiguously), a bitmap inner set would be both smaller and faster to scan than the radix. Do NOT do this speculatively — the radix is the right default; this is a measured-need optimization only. Filed from the chunk-tags spec's "Out of scope for v1".

### I102. Batched per-page frees / streaming drop in `delete_with_tag` [chunk-tags out-of-scope 2026-06-02] — **P3**
**Where:** `src/transaction.rs::delete_with_tag_inner` — the `for &h in &take { self.delete_inner(h)?; }` loop.

**Motivation:** `delete_with_tag` deletes one handle per `delete_inner` call, each a full handle-table walk + per-handle freeing, exactly the per-handle shape I33 wants to batch for `delete_many` (per-leaf batched delete walks the tree once per leaf, not once per handle). When I33's batched-delete primitive lands, route `delete_with_tag`'s bounded batch through it for dense-relation drops. Also relevant: a relation far larger than one `max` batch already drops incrementally (the caller loops `begin → delete_with_tag → commit`), so this is a per-batch efficiency item, not a correctness or unbounded-time gap. Filed from the chunk-tags spec's "Out of scope for v1".

### I103. Bump `pyo3` from 0.24 to 0.29 to clear RUSTSEC-2026-0176 / RUSTSEC-2026-0177 [audit 2026-06-16] — **P2** ✅ FIXED 2026-06-16
**Where:** `python/Cargo.toml` (`pyo3 = "0.29"`); migration touches `python/src/{db,transaction,savepoint,errors}.rs`.

**What:** two RustSec advisories landed against `pyo3` < 0.29 — RUSTSEC-2026-0176 (out-of-bounds read in `PyList` / `PyTuple` `nth` / `nth_back` iterators) and RUSTSEC-2026-0177 (missing `Sync` bound on `PyCFunction::new_closure`). The chisel binding uses NEITHER vulnerable API, but `cargo audit` flags the crate transitively, turning the gating `audit` CI job red repo-wide. Both are patched only in 0.29.0, so the fix is a 0.24 → 0.29 bump (an alternative `cargo audit --ignore` was rejected in favor of the real upgrade). PyO3 0.29 migration applied: `PyObject` → `Py<PyAny>` (dropped from the prelude); `Python::with_gil` → `Python::attach` and `py.allow_threads` → `py.detach` (the attach/detach GIL model); `Bound::downcast` → `Bound::cast`; and `#[pyclass(from_py_object)]` opt-in on `PyDrainInsertion` (the auto-`FromPyObject` derive for `Clone` pyclasses is now opt-in, and this enum IS extracted from Python args). Public Python API unchanged. `cargo audit` → 0 vulnerabilities; the engine/bench suites and `cargo clippy -p chisel-py --all-targets -- -D warnings` pass; the CI Python matrix (pytest, CPython 3.11/3.13 × Linux/macOS) validates runtime. Independent of the handle-table COW page-reclamation work (PR #34).

---

## Deepdive review findings (2026-06-21)

The `deepdive-rust` fresh-eyes pass at commit `deb0303` (full output at `docs/reviews/review-20260621-185541.md`). `cargo build`/`test`/`clippy --all-targets -- -D warnings`/`fmt --check` were green on that commit. The *entire* 2026-06-16 executive summary is resolved with no regressions (handle-table COW spine leak, tag-map `CacheFull` divergence, corrupt-radix OOB panic, the `Deleted=0x03` doc, the crash-between-fsyncs test — all closed by PRs #34/#40/#44). No new P0 / data-loss bugs. The cluster below (severity tags map to the file's P-legend: BUG/blocking → P1, DESIGN/latent → P2, SMELL/NIT → P3) concentrates in error-classification defaults, a CI lint hole, the radix test gap, and doc drift from the #46 dead-code sweep.

Suggested order for this cluster: **I108** (CI lint hole) and **I111** (radix proptest gap) first — both are real gaps with cheap fixes; then the zero-risk cheap batch **I104 / I127 / I128 / I130 / I131 / I132 / I133** (a test, a hasher swap, and doc edits — no runtime behaviour change); then the P2 DESIGN items as they get touched. The P3 idiom/NIT items batch with any PR in their file.

### Error handling and classification

#### I104. `ChiselError::is_fatal()` is fail-open — unlisted future variants classified non-fatal [deepdive 2026-06-21] — **P2**
**Where:** `src/error.rs:168-181`

**Problem:** the `matches!`-based classifier returns `false` for any variant not in its Fatal list, so a future fatal variant a maintainer forgets to add is silently classified *operational* and will NOT poison the manager. For a durability-first engine this is the one function whose default leans the wrong way; the in-code INVARIANT comment already admits it's "a durability hole, not a compile error." All 24 current variants are classified correctly — the gap is latent, not a present bug.

**Direction of fix (NOT a default flip):** the review argues a `_ => true` flip just moves the latent bug (a future *operational* variant would then over-poison and tank availability). Instead add an exhaustiveness test that constructs every `ChiselError` variant and asserts `is_fatal()` matches the value the enum's documented Fatal/Operational block prescribes. Converts the prose invariant into a compile-time-ish signal the `#[non_exhaustive]` enum can't give, catches *both* misclassification directions, changes zero runtime behaviour (~30 lines). See the review's countercase section for the full argument.

#### I105. Blanket `From<io::Error>` makes every `?` on an I/O call produce a *fatal* `IoError` invisibly [deepdive 2026-06-21] — **P2**
**Where:** `src/error.rs:282-286` (conversion); `:280-281` (the caveat comment)

**Problem:** the blanket conversion means any `?` on an io call silently produces a fatal `IoError` and poisons, even on a benign `NotFound`. The comment concedes callers "must catch and remap" to classify e.g. `NotFound` as operational, but nothing enforces it — the `FileNotFound` operational variant exists *because* the conversion is too coarse. A future `?` on an io call that should be recoverable will poison.

**Direction of fix:** drop the blanket `From` (force explicit classification at each I/O site), or have the conversion inspect `ErrorKind` and route the known-operational kinds (`NotFound`, `AlreadyExists`, …) to operational variants.

#### I106. `CorruptSuperblock` folds three distinct causes into one nullary fatal variant [deepdive 2026-06-21] — **P3**
**Where:** `src/error.rs:106`

**Problem:** bad checksum, bad magic, and out-of-range `superblock_count` all surface as the same nullary, fatal, unrecoverable `CorruptSuperblock`; the comment admits operators "should look at the raw slot bytes." Discarding which-slot-and-why on the engine's worst failure is an operability loss. (Related: `InvalidMagic` at `:111` is likely unreachable because a bad magic already surfaces here via `select` — see I115.)

**Direction of fix:** add a cause field (`enum SuperblockDefect { Checksum, Magic, BadCount(u32) }`) and/or the offending slot index.

#### I107. `delete_with_tag` discards its partial-deletion list on a mid-pass error [deepdive 2026-06-21, carried from 2026-05-22] — **P3**
**Where:** `src/lib.rs:553`; `src/transaction.rs:2041` (`delete_with_tag_inner`)

**Problem:** the `?` inside the per-handle loop drops the already-built `deleted` set; the caller gets `Err` with no `TagDropProgress`. On-disk state stays consistent (the failed transaction rolls back), but a resumable relation-drop cannot reconcile what it had dropped. Unchanged since the 2026-05-22 pass — not a regression, restated to keep it tracked under a number.

**Direction of fix:** attach the partial progress to the error variant, or document that rollback-on-error is mandatory and the caller must restart the drop. (Interacts with I102's batched-drop work.)

### CI and automation

#### I108. The PyO3 binding (1,300+ lines of Rust) is never clippy-linted in CI [deepdive 2026-06-21] — **P1**
**Where:** `.github/workflows/ci.yml:40` (root clippy step); python job at `:118-160`

**Problem:** the gating `cargo clippy -- -D warnings` (no `--workspace`/`-p`) honors `default-members = [".", "bench"]`, which excludes `chisel-py`; the python job runs only `maturin develop` + `pytest`. Confirmed empirically — clippy only checks `chisel` and `chisel-bench`. So clippy regressions in `python/src/*` ship silently. (`cargo fmt -- --check` *does* cover it — rustfmt walks all members — so only clippy has the hole.)

**Direction of fix:** add `cargo clippy -p chisel-py --all-targets -- -D warnings` (with the maturin linker env) to the python job, or run root clippy with `--workspace`.

#### I109. The release/tag wheel path has no `cargo audit` gate [deepdive 2026-06-21] — **P2**
**Where:** `.github/workflows/wheels.yml:11-17`

**Problem:** the `audit` job lives in `ci.yml` (push/PR to main) but is not a dependency of the tag-triggered wheel build, which runs only `cargo test --release`. A vulnerable dependency introduced and tagged without merging through a PR would publish wheels unaudited.

**Direction of fix:** add a `cargo audit` step (or reuse the `rustsec/audit-check` action from I54) as a gate on the wheel job.

#### I110. The MSRV job builds but never `--tests` and skips bench/python [deepdive 2026-06-21] — **P3**
**Where:** `.github/workflows/ci.yml:98-107`

**Problem:** `cargo build -p chisel` verifies the library compiles at 1.82 but not its test code, so an MSRV-breaking construct in a test module passes. Low risk.

**Direction of fix:** `cargo build --tests -p chisel` in the msrv job (a cheap strengthening; keep bench/python scoped out per I55/I61's deliberate floor-floating decision).

### Tests

#### I111. Radix key-math has no property tests, and the one "two-level" test reaches depth 1 [deepdive 2026-06-21, follow-up to I71] — **P1**
**Where:** `src/handle_table.rs:1313` (`test_handle_table_grows_to_two_levels`); the `find_leaf`/`span_at_level`/`insert`/`grow`/`recover_depth` paths and the identical `src/membership_index.rs` paths

**Problem:** `test_handle_table_grows_to_two_levels` is misnamed — it inserts `ENTRIES_PER_LEAF + 10` (520) handles (forces depth 1) and never asserts `ht.depth()`. No test exercises a depth-≥2 descent, so the multi-digit `span_at_level` / `handle % child_span` decomposition — the engine's most off-by-one-prone code, addressing both the stable-handle table and the tag index — is exercised only at depth 0–1. I71 added proptests for slot packing / freemap bit math / checksum round-trip but not for the radix math.

**Direction of fix:** insert `> ENTRIES_PER_LEAF²` and assert depth ≥2; add a `HashMap`-oracle property (`insert(h,e); lookup(h)==Some(e)` over arbitrary `u64` on a depth-≥2 tree, plus a `grow → recover_depth` round-trip) — ~15 lines that would catch any `span_at_level` off-by-one instantly. `transaction.rs` also lacks a stateful commit/rollback/savepoint proptest despite a ready oracle (`assert_no_reachable_page_is_free`, `:2642`).

#### I112. Fault-injection test layer absent — the poison/flush coupling and the fatal-`IoError` path are reasoned, not tested [deepdive 2026-06-21] — **P2**
**Where:** `src/page_cache.rs` flush (clears dirty flags before the trailing fsync); `src/transaction.rs:2833` (`fatal_error_outside_commit_also_poisons`); `error.rs:94` (`IoError`)

**Problem:** flush clears per-page dirty flags *before* the trailing fsync — safe *only* under the poison model, and the comment warns it breaks if poisoning is weakened. No test asserts a flush/fsync error actually poisons: `test_poison_recovery_by_reopen` can't inject a fault, and `fatal_error_outside_commit_also_poisons` is tautological (`force_poison_for_test()` then asserts `Poisoned`). The most-traveled fatal path (`IoError`) is constructed-only — no test induces a real I/O fault and observes the engine returning it.

**Direction of fix:** a `Backing::FaultyFile` test variant that fails a chosen syscall closes the poison/flush gap and exercises `IoError` / `CorruptSuperblock` / `InvalidMagic` end-to-end (also addresses open question 4 and several I115 gaps at once).

#### I113. Weak assertions that survive a subtle break [deepdive 2026-06-21] — **P3**
**Where:** `tests/defrag.rs:63`; `src/page_cache.rs:1294`; `tests/spillway_runtime_mutability.rs:33`; assorted `assert!(x.is_some()/is_err())` sites

**Problem:** `assert!(pages_freed > 0)` passes if defrag freed 1 of ~45 (the sibling `:154` bounds it correctly); `assert_eq!(dh+dm,1)` passes if a hit is recorded as a miss; the spillway-mutability test sets the cap then asserts nothing about its effect; several `is_some`/`is_err` checks never inspect the value/variant.

**Direction of fix:** tighten each to assert the actual expected value/variant/bound.

#### I114. `claim_page_asserts_on_dirty_page` is `#[cfg(debug_assertions)]` and vanishes under `--release` [deepdive 2026-06-21] — **P3**
**Where:** `src/page_cache.rs:1236`; the release-profile gate at `wheels.yml:17`

**Problem:** the test guarding the I20 dirty-page invariant is debug-only, so it silently disappears under `cargo test --release` — which the wheel gate uses. The invariant is checked only in dev-profile runs.

**Direction of fix:** either keep the `debug_assert!` but assert the *observable* consequence in a release-safe test, or document why debug-only coverage is acceptable here.

#### I115. Error-variant coverage gaps; `InvalidMagic` likely unreachable; `CorruptSuperblock` never pinned [deepdive 2026-06-21] — **P3**
**Where:** `src/error.rs:111` (`InvalidMagic`); `tests/recovery_tests.rs:533` (the OR-arm); the `Poisoned` end-to-end loop

**Problem:** `InvalidMagic` is probably unreachable (a bad magic surfaces as `CorruptSuperblock` via `select` — see I106); `CorruptSuperblock` is never *pinned* as the expected variant (only matched in an OR-arm); the `Poisoned` end-to-end path is synthetic (`force_poison_for_test`, never a real fatal error driven into a live manager — see I112). **Memory correction:** the project note "ReadOnlyMode never raised" is stale — `tests/options_validation.rs:240` genuinely drives and matches it.

**Direction of fix:** pin `CorruptSuperblock` as the sole expected variant in the recovery tests; decide whether `InvalidMagic` is dead (remove) or reachable (add a test); drive a real fatal error for the `Poisoned` loop (depends on I112).

### Correctness

#### I116. `PageCache::truncate` decrements `dirty_count` unguarded — underflow disables the cache cap [deepdive 2026-06-21] — **P2**
**Where:** `src/page_cache.rs:625-654` (`truncate`); the guarded siblings `discard`, `maybe_evict` Phase B, `set_cache_max_bytes` (justification comment at `:986-994`)

**Problem:** `self.dirty_count -= 1` per removed dirty entry is the one decrement site not guarded against an `entries`/`dirty_count` desync. On any desync it underflows (debug panic / release wrap to `usize::MAX`), after which `maybe_evict`'s `dirty_count == entries.len()` short-circuit never fires and the cache silently stops enforcing its cap. This `dirty_count`-underflow shape has now appeared in two consecutive reviews at different call sites (2026-06-16 flagged the `maybe_evict` site, since guarded).

**Direction of fix:** recompute `dirty_count` from surviving entries after the loop, or saturate — and consolidate all four decrement sites behind one helper to end the recurrence (ties into I136's eviction-loop extraction).

#### I117. `insert_into_data_page`'s `.expect("value fits in empty page")` is a library-reachable panic if the inline-size constants drift [deepdive 2026-06-21, follow-up to I46] — **P2**
**Where:** `src/transaction.rs:2576`; the hand-maintained literal `MAX_INLINE_VALUE` at `:66`

**Problem:** correct today (`MAX_INLINE_VALUE = 8162` exactly equals `CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE - SLOT_ENTRY_SIZE`), but `MAX_INLINE_VALUE` is a hand-maintained literal with only a prose "keep in sync" note. If any of the three constants is edited independently, `DataPage::insert` returns `None` on the "fits" path and a plain `allocate()` of the wrong-sized value panics the process. I46 added an `// INVARIANT:` comment here; this asks for a compile-time guard.

**Direction of fix:** `const _: () = assert!(MAX_INLINE_VALUE == CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE - SLOT_ENTRY_SIZE);`, or map the `None` to a typed error.

#### I118. Fresh inner-tree roots bypass the freemap-aware allocator — churn workloads extend the file [deepdive 2026-06-21] — **P3**
**Where:** `src/membership_index.rs:603` (`create_root` → `init_page` → `cache.new_page()`); the "bounded steady-state page count" header claim at `:41-49`

**Problem:** `MembershipIndex::insert` threads the freemap-aware `alloc` closure into `inner.insert`, but a tag's first-ever `create_root` always extends (never reuses a freed page). Not a correctness bug (the id is past the watermark; rollback's `truncate` and commit's freemap merge both handle it), but a workload that repeatedly empties and re-creates a tag's inner tree extends the file once per re-create, undercutting the module's own bounded-page-count claim.

**Direction of fix:** plumb the `alloc` closure into `create_root`/`init_page`, or narrow the header comment to exclude first-create.

#### I119. `commit_inner`'s `txn_counter += 1` is an unchecked `u64` increment [deepdive 2026-06-21, carried] — **P3**
**Where:** `src/transaction.rs:889`

**Problem:** practically unreachable, but a wrapped counter corrupts `Superblock::select`'s "highest counter wins", and a debug overflow panics mid-protocol after dirty flags were cleared. Inconsistent with the `saturating_add` discipline used elsewhere.

**Direction of fix:** `checked_add` → fatal error on overflow.

### API design

#### I120. Handles are raw `u64` and tags raw `u32` across the whole public surface [deepdive 2026-06-21] — **P2**
**Where:** `src/lib.rs` (`allocate -> u64`; `read`/`update`/`delete`/`tag`/`handle_live_page_id` take `u64`; `delete_tagged(handle, tag)` takes two integers)

**Problem:** a crate that is `#[non_exhaustive]`-everything and careful about API stability leaves its most-used values primitive-obsessed; `delete_tagged`'s two integer args can be transposed with no compiler error. (Related: `handles()` / `handles_with_tag()` return eager `Vec<u64>` with a three-paragraph prose contract — the streaming variant is already tracked as I97/I100; the newtype is the orthogonal half.)

**Direction of fix:** opaque `Handle(u64)` / `Tag(u32)` newtypes with `From`/`Into` — makes `delete_tagged` un-transposable and distinguishes `handles()` from `handles_with_tag()` return types.

#### I121. `DefragStats` and `TagDropProgress` are outcome reports but not `#[must_use]` [deepdive 2026-06-21] — **P3**
**Where:** `src/defrag.rs:111` (`DefragStats`); `src/membership_index.rs:538` (`TagDropProgress`)

**Problem:** `db.defrag(opts)?;` silently discards `pages_freed`; `delete_with_tag(...)?;` discards `complete` — the field the caller must loop on (see I107). The crate already added `#[must_use]` to `close()` (I38), so it values the pattern.

**Direction of fix:** `#[must_use]` on both type definitions.

#### I122. `DefragOptions::max_pages` counts values relocated, not pages [deepdive 2026-06-21] — **P3**
**Where:** `src/defrag.rs:64-70` (the "DESPITE THE NAME…" comment)

**Problem:** preserving a misleading public field/setter name "for stability" on a pre-1.0, `#[non_exhaustive]`, no-production-users crate is the wrong trade — the C4 doc-sweep and the Python `__init__.py` note both already paper over the mismatch instead of fixing it.

**Direction of fix:** rename to `max_values` now (and the Python `DefragOptions.max_pages` mirror), while there are no users to break.

#### I123. `page_count` / `file_page_count` keep `&mut self` "for API stability" on a pure `Cell` read [deepdive 2026-06-21] — **P3**
**Where:** `src/page_io.rs:330,594`

**Problem:** these now-pure cache reads keep `&mut self`, forcing a `borrow_mut()` on semantically-read `&self` paths — a latent double-borrow risk if a future `&self` path nests a cache read. The comment already concedes a future patch can drop the `&mut`.

**Direction of fix:** drop the `&mut self` now (the ripple is small).

#### I124. Public fallible methods broadly lack `# Errors` rustdoc; several omit `# Panics` [deepdive 2026-06-21] — **P3**
**Where:** `savepoint`/`rollback_to`/`release`/`set_root_name`/`get_root_name` and most `TransactionManager` mutators; assert sites at `transaction.rs:307`, `membership_index.rs:167`

**Problem:** for a published crate this is the `missing_errors_doc` / `missing_panics_doc` surface clippy would flag under `--all-targets` pedantic.

**Direction of fix:** a shared "Errors & poisoning" doc convention plus targeted `# Errors`/`# Panics` sections.

### Type system

#### I125. Three different deleted-handle semantics for the same "look up a live handle" [deepdive 2026-06-21] — **P2**
**Where:** `src/transaction.rs:1563-1602` (`read`, `client_byte`, `tag`); `:2014` (`delete_tagged`)

**Problem:** `read()` and `client_byte()` reject a tombstone with `InvalidHandle`; `tag()` reads through it; `delete_tagged()` reads `.tag` off the looked-up entry with no Deleted guard. The "a handle is live" invariant lives in scattered per-method guards, so a fourth caller can easily get it wrong.

**Direction of fix:** one `lookup_live(handle) -> Result<HandleEntry>` applying the Deleted ⇒ `InvalidHandle` rule once, used by all four sites.

#### I126. Tag/client-byte `0` is overloaded as the "untagged"/"unset" sentinel [deepdive 2026-06-21] — **P3**
**Where:** `src/lib.rs` tag surface; threaded through the membership layer

**Problem:** `0` doubles as a valid value and the "no tag" sentinel for both tag (`u32`) and client byte (`u8`); `allocate_tagged(value, tag: u32)` can be called with the sentinel by mistake and "membership index not updated for tag 0" is an implicit rule.

**Direction of fix:** `Option<NonZeroU32>` at the tag API boundary — "no tag" becomes expressible in the type (composes with the I120 newtype).

### Performance

#### I127. `current_live_slots` / `committed_live_slots` / `Savepoint.live_slots` use std `HashMap` (SipHash) on the per-op path [deepdive 2026-06-21, follow-up to I77] — **P2**
**Where:** `src/transaction.rs:144` (field decls ~144/216); probed/updated at `insert_into_data_page:2555/2591`, `release_data_slot:2463`

**Problem:** these slot-accounting maps are probed and updated on every insert/update/delete. Keys are trusted local `u64` page ids — the identical threat model to the page-cache/LRU maps the I77 pass already moved to `FxHashMap` with an explicit "no DoS surface, SipHash is pure cost" rationale. I77 simply missed these.

**Direction of fix:** `rustc_hash::FxHashMap` for all three; zero-risk, same free win. (Verified-clean by the same review: LRU is O(1), eviction short-circuits on `dirty_count`, radix descent is allocation-free, XXH3 re-stamps once per touched page — no other perf gaps on the hot path.)

### Docs vs reality

#### I128. ADR-7 + `ARCHITECTURE.md:580` describe per-page version read-dispatch as live, but no code reads the byte [deepdive 2026-06-21] — **P2**
**Where:** ADR-7 (`.codebase-memory/adr.md`); `ARCHITECTURE.md:580`; `PageCache::load_page`

**Problem:** both claim "reads dispatch on the per-page version byte" / "the page-cache load path validates the per-page version on every miss." Every page type *writes* `current_version(type)` at init, but `load_page` validates only the XXH3 checksum — there is no version read or dispatch anywhere. The mechanism is reserved-and-stamped, not implemented (consistent with the ADR's own "I31 eager upgrader (deferred)" note; the consequence bullets contradict it). ADR-7 was partially corrected 2026-06-21 — verify the corrected text and `ARCHITECTURE.md:580` both read reserved/future tense.

**Direction of fix:** reword the live-dispatch claims to reserved/future tense in both ADR-7 and ARCHITECTURE.md. See also I31's "Phase 1a" note (the read helpers exist but are dormant — no production caller).

#### I129. The ADR is not git-tracked — a fresh clone has no ADR and `// see ADR-N` comments resolve to nothing [deepdive 2026-06-21] — **P2**
**Where:** `.codebase-memory/adr.md` (the codebase-memory MCP's local store, untracked); the many `// see ARCHITECTURE.md ADR-N` code comments

**Problem:** the ADR-0..14 record lives only in the untracked MCP store; ARCHITECTURE.md has no ADR sections, so every code comment citing an ADR number is a dangling reference for anyone who clones the repo. This is a process/maintainer decision, not a code fix.

**Direction of fix:** commit the ADR into the repo as `docs/ADR.md` (and either re-point the `// see …` comments at it or keep them once the file exists). Edit the MCP store via `manage_adr mode=update` with the FULL content — the `sections` param wipes the whole store (recorded footgun). **Open question for the maintainer:** is the ADR meant to be a repo artifact? If yes, this is the migration; if it stays MCP-only, the code comments should stop citing ADR numbers.

#### I130. `ARCHITECTURE.md` cites functions deleted in the #46 dead-code sweep [deepdive 2026-06-21] — **P2**
**Where:** `ARCHITECTURE.md:246` (`page::page_format_version()`); `:108` and `:471` (`allocate_near`, the latter claiming it's "currently used by data-page allocation" — already false before deletion)

**Problem:** the #46 sweep updated code comments but not ARCHITECTURE.md, leaving three references to deleted functions. (Note: `page::page_format_version()` was *re-introduced* as a dormant read helper by the 2026-06-21 versioning work per I31's Phase 1a — confirm whether the `:246` reference is now accurate or still describes the deleted signature.)

**Direction of fix:** reconcile the three references against current code — delete the `allocate_near` ones; correct or keep `page_format_version` depending on its current dormant-but-present status.

#### I131. `README.md:14` says commit is "two-phase" but the protocol does three fsyncs [deepdive 2026-06-21, follow-up to I63] — **P3**
**Where:** `README.md:14`; the `commit()` rustdoc (numbers only two fsyncs)

**Problem:** I63 fixed the *docstring's* "two fsyncs" claim, but the README's "two-phase durability (fsync data pages, then fsync superblock)" and the rustdoc step-numbering were missed. The protocol does THREE fsyncs (pre-drain I28 flush `transaction.rs:971`, step-1 data flush `:989`, superblock `:1026`); ARCHITECTURE.md gets it right ("3 fsyncs", with a `== 3` regression test).

**Direction of fix:** README → three-fsync; renumber the `commit()` rustdoc to give the pre-drain a step number.

#### I132. `ARCHITECTURE.md` says `bench/` is "not a workspace member" — it is [deepdive 2026-06-21] — **P3**
**Where:** `ARCHITECTURE.md:588`, `:619`, `:668` (the last draws a now-false build-topology inference)

**Problem:** `Cargo.toml:20` is `members = [".", "python", "bench"]`; bench is merely excluded from `default-members`. README.md:72 is correct; ARCHITECTURE contradicts it three times. (Post-I61 state.)

**Direction of fix:** "a workspace member excluded from `default-members`."

#### I133. `ARCHITECTURE.md` "common 16-byte header" vs `COMMON_HEADER_SIZE = 12` [deepdive 2026-06-21] — **P3**
**Where:** `ARCHITECTURE.md:232`, `:235`; `src/page.rs:41`

**Problem:** the 16 figure is actually `DATA_PAGE_HEADER_SIZE` (the data-only extended header); the named common-header constant is 12.

**Direction of fix:** reconcile the number against the constant name.

### Idiomaticity

#### I134. The COW page copy bounces through a named 8 KB stack array for the borrow checker [deepdive 2026-06-21] — **P3**
**Where:** `src/handle_table.rs:330-333`; `src/membership_index.rs:205-208`, `:290-292`

**Problem:** three sites copy a page through a named stack buffer purely to satisfy the borrow checker; the pattern is unnamed and duplicated.

**Direction of fix:** a shared `cow_copy_page(cache, src, dst)` helper using `copy_from_slice`.

#### I135. `u64::from_le_bytes(buf[off..off+8].try_into().unwrap())` repeated ~15× [deepdive 2026-06-21] — **P3**
**Where:** across `src/handle_table.rs` and `src/superblock.rs::deserialize`

**Problem:** the load-bearing infallibility of each `unwrap()` is undocumented at every site.

**Direction of fix:** a tiny `read_u64_le(buf, off)` helper that names the invariant once.

#### I136. Three hand-copied clean-victim eviction loops with divergent short-circuits [deepdive 2026-06-21, carried] — **P3**
**Where:** `src/page_cache.rs` — `maybe_evict` Phase A, `set_cache_max_bytes`, `flush` Phase 1b (the last omits the `dirty_count` early-out)

**Problem:** the three loops are subtly divergent; carried from the prior review. Relates to the `dirty_count` underflow trend (I116).

**Direction of fix:** extract `evict_clean_to_cap` and consolidate (do alongside I116's `dirty_count` helper).

### Python binding (PyO3)

#### I137. `db.rs` / `transaction.rs` file-header docs still describe the engine as `RefCell<Option<Chisel>>` — it's a `Mutex` since I75 [deepdive 2026-06-21] — **P2**
**Where:** `python/src/db.rs:2-29` (header) vs the actual field at `:76`; `python/src/transaction.rs:44` (same stale claim)

**Problem:** the file headers argue "why RefCell (and not Mutex)", but I75 migrated the field to `Mutex<Option<Chisel>>` (the struct comment 30 lines below correctly documents the Mutex and contradicts the header above it). A first-time reader gets the wrong concurrency model and the wrong I21 failure mode (RefCell *panics* on re-entry; the Mutex *deadlocks*).

**Direction of fix:** rewrite both file headers to describe the Mutex model and the deadlock-not-panic re-entry semantics.

#### I138. `errors.rs` catchall routes any future variant to the abstract base, bypassing the Operational/Fatal split [deepdive 2026-06-21] — **P2**
**Where:** `python/src/errors.rs:326` (the `_` arm); the "exhaustive, no silent fallback" header claim at `:11-13`

**Problem:** the same fail-open class as engine I104, on the binding side. A new *fatal* engine variant would surface in Python as a bare `ChiselError`, so `except chisel.FatalError:` poison-recovery handlers silently miss it — breaking the documented `FatalError` contract (`errors.rs:5-7`). The header still claims the match is exhaustive with "no silent fallback", which `#[non_exhaustive]` (I36) made false.

**Direction of fix:** branch the catchall through the Operational/Fatal base class instead of the abstract `ChiselError`; correct the header claim.

#### I139. The typed-exception contract is largely unverified [deepdive 2026-06-21] — **P3**
**Where:** `python/tests/` (`test_operational_hierarchy`, `test_threading.py`); `python/src/errors.rs::to_py_err`

**Problem:** routing is correct today, but `SavepointNotFoundError`, `DuplicateSavepointError`, `CacheFullError`, `SpillwayFullError`, `RootNameTableFullError`, `InvalidRootNameError` are never raised in any test, and `test_operational_hierarchy` omits `TagMismatch`/`SpillwayFull`/`TransactionInProgress` — a swapped `to_py_err` arm ships green. `test_threading.py` joins before re-touching, so the Mutex deadlock/poison path (the whole reason for I75's Mutex-over-RefCell decision) is unexercised.

**Direction of fix:** a parametrized "trigger each variant, assert the concrete class" test, plus a two-thread contention test.

### Cargo hygiene

#### I140. `python/Cargo.toml:6` carries a stale "PyO3 0.22" comment — the dep is 0.29 [deepdive 2026-06-21] — **P3**
**Where:** `python/Cargo.toml:6`

**Problem:** cosmetic — the dependency is `pyo3 = "0.29"` (I103) but the comment still says 0.22. The only cargo-hygiene finding; every declared dependency is otherwise used and MSRV 1.82 holds against every floor.

**Direction of fix:** update or delete the comment.
