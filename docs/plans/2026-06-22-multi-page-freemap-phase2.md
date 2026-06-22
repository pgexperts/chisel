# Multi-Page Freemap — Phase 2 Revision Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the multi-page-freemap integration against the *revised* structural-reclamation design: validate the salvaged hot path (in-memory recycle + session-COW dedup), add the defrag orphan-sweep that makes crash-orphaned freemap pages reclaimable, and add the tests the revised spec requires.

**Architecture:** The integration already landed on this branch (commit `6c81d4d`): the freemap lives in the page cache; allocation/`persist_freemap` route through `FreeMapTree`; freemap-structural COW reuses dead pages from an in-memory one-commit-deferred recycle pool (`structural_reuse` / `structural_superseded` / `pending_structural_frees`) before extending; a per-transaction `session_owned` set COWs each freemap node once per commit. This phase **adds the missing crash-recovery story** — a `defrag` sweep that reclaims freemap-typed pages orphaned when an in-memory pool is lost to a crash — and the spec's missing tests.

**Tech Stack:** Rust, `cargo test` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt`, `proptest`, in-memory + file fixtures.

**Spec:** `docs/specs/2026-06-22-multi-page-freemap-design.md` — read **"Structural-page reclamation"** before starting.

**Branch:** `feature/multi-page-freemap-phase2` (base commit `6c81d4d` + spec revision `4ce54c8`).

**Conventions:** comments explain WHY; NO Claude/AI/Anthropic references; full `cargo test` (not `--lib`) before declaring done.

---

## File Structure

- **`src/freemap_tree.rs`** — add `reachable_pages` (collect every page id in the live tree). Already has the tree + session-COW dedup.
- **`src/transaction.rs`** — add `reclaim_freemap_orphans` (the sweep body: walk + scan + mark, excluding the live recycle pool). Already has the recycle machinery and `persist_freemap`.
- **`src/defrag.rs`** — call `reclaim_freemap_orphans` as a defrag phase; add a `freemap_orphans_reclaimed` field to `DefragStats`.
- **`tests/freemap_multipage.rs`** (new) — the revised spec's integration tests.

---

### Task 1: Pre-flight adversarial verification of the salvaged hot path

This is a verify-then-fix task on the existing `6c81d4d` integration — confirm the durability-critical properties hold before building the sweep on top. **Do not change behavior unless you find a real defect.**

**Files:** read `src/transaction.rs` (`cow_alloc`, `structural_extend`, `allocate_data_page`, `ht_insert`, the membership site, `persist_freemap`, `take_freemap_tree`/`put_freemap_tree`, `begin_inner`, `rollback_inner`, commit's promotion of `structural_superseded`→`pending_structural_frees`, the field docs ~256-310) and `src/freemap_tree.rs` (`cow_node`, `session_owned`).

- [ ] **Step 1: Verify the one-commit-defer crash-safety by reading + asserting.** Confirm a page superseded in transaction T is NOT reused as a structural COW target until AFTER T's superblock fsync (i.e. `structural_reuse` for T+1 is sourced only from `pending_structural_frees` promoted at T's commit, never from `structural_superseded` of the in-flight T). Write a focused test in `transaction.rs` tests that: commits a structural churn (T), records the pool ids promoted at T's commit, runs T+1, and asserts T+1's structural COW targets are drawn from exactly that promoted set (not from T+1's own fresh supersedes). If the code reuses a same-transaction supersede, that is a crash-safety BUG — fix it.

- [ ] **Step 2: Verify rollback resets the pools.** Write a test: begin, do a structural churn (mutating `structural_reuse`/`structural_superseded`), then `rollback`; assert `structural_reuse` is restored to what `pending_structural_frees` held at begin (the committed recycle state) and `structural_superseded` is cleared, and `freemap_session_owned` is cleared. A rollback that leaks pool state into the next transaction is a BUG — fix it.

- [ ] **Step 3: Verify no lost/double frees across a reuse cycle.** Extend the existing `persist_freemap_does_not_reuse_committed_live_pages` reasoning with a multi-commit test: churn for several commits (each freeing whole pages), and after each commit assert (a) every committed-free page reads `is_free` true via a fresh `FreeMapTree::from_roots`, and (b) no page id appears simultaneously reachable-in-the-live-tree AND in `structural_reuse` (a reuse pool page must be dead). Any violation is a BUG.

- [ ] **Step 4: Run the full suite + clippy.** `cargo test`, `cargo clippy --workspace --all-targets -- -D warnings`. Green.

- [ ] **Step 5: Commit** (tests + any fixes): `test: verify freemap recycle one-commit defer, rollback reset, no double-free`. If you found and fixed a real defect, say so explicitly in the commit body.

> If any of Steps 1-3 reveals a defect you cannot cleanly fix within the existing design, STOP and report it — it may mean the salvage was unsound and we revisit.

---

### Task 2: `FreeMapTree::reachable_pages`

The sweep needs the set of page ids the live tree occupies.

**Files:** Modify `src/freemap_tree.rs` (+ inline test).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn reachable_pages_collects_every_node() {
    let mut cache = make_cache(256);
    let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
    // Force depth >= 1 and materialize two leaves so the set has interior+leaves.
    t.mark_free_growing(&mut cache, 5, &mut |c| c.new_page()).unwrap();
    t.mark_free_growing(&mut cache, LEAF_CAPACITY + 7, &mut |c| c.new_page()).unwrap();
    let reachable = t.reachable_pages(&mut cache).unwrap();
    // Root is reachable; both materialized leaves are reachable; the count
    // equals 1 root-interior + 2 leaves (depth 1).
    assert!(reachable.contains(&t.root));
    assert_eq!(reachable.len(), 3);
    // is_free ids live inside reachable leaves, so their leaf pages are present.
    for id in reachable.iter() {
        assert!(*id != 0); // never the superblock
    }
}
```

- [ ] **Step 2: Run, expect compile failure** — `cargo test --lib reachable_pages_collects_every_node` → no method `reachable_pages`.

- [ ] **Step 3: Implement**

```rust
    /// Collect every page id the live tree occupies (root + all interiors +
    /// all materialized leaves). Used by the defrag orphan-sweep to tell a
    /// live freemap page from a dead (orphaned) one. Validates page types on
    /// the walk; a corrupt interior surfaces as CorruptPage.
    pub fn reachable_pages(&self, cache: &mut PageCache) -> Result<rustc_hash::FxHashSet<u64>> {
        let mut set = rustc_hash::FxHashSet::default();
        if self.root != PAGE_ID_NONE {
            self.collect_reachable(cache, self.root, self.depth, &mut set)?;
        }
        Ok(set)
    }

    fn collect_reachable(
        &self,
        cache: &mut PageCache,
        page: u64,
        depth: u32,
        set: &mut rustc_hash::FxHashSet<u64>,
    ) -> Result<()> {
        set.insert(page);
        if depth == 0 {
            // Leaf: validate type, no children.
            check_type(cache.get(page)?, PageType::FreeMap, page)?;
            return Ok(());
        }
        let buf = *cache.get(page)?;
        check_type(&buf, PageType::FreeMapInterior, page)?;
        for idx in 0..PTRS_PER_INTERIOR {
            let child = read_child(&buf, idx);
            if child != 0 {
                self.collect_reachable(cache, child, depth - 1, set)?;
            }
        }
        Ok(())
    }
```

- [ ] **Step 4: Run, expect PASS** — `cargo test --lib freemap_tree`.

- [ ] **Step 5: Commit** — `feat: FreeMapTree::reachable_pages (live-tree page set for the orphan sweep)`.

---

### Task 3: `TransactionManager::reclaim_freemap_orphans` (the sweep)

**Files:** Modify `src/transaction.rs` (+ inline test).

Semantics (from the spec): an orphan is a `FreeMap`/`FreeMapInterior`-typed page that is **(a)** within `total_pages`, **(b)** not reachable from the live tree, **(c)** not already free in the bitmap, **AND (d)** not currently in the in-memory recycle pool** (`structural_reuse` ∪ `structural_superseded` ∪ `pending_structural_frees`) — those are *live* recycling state, not orphans. After a crash the pool is empty, so the crash-lost pages are correctly flagged; in a normal defrag the live pool is excluded so we never double-hand-out a page.

- [ ] **Step 1: Write the failing test** (simulate an orphan by leaking a page the way a crash would, then sweep)

```rust
#[test]
fn reclaim_freemap_orphans_marks_lost_freemap_pages_free() {
    let mut tm = fresh_manager();
    // Create a multi-page freemap with some churn so a freemap leaf/interior
    // exists. Use overflow-sized values so deletes free whole pages.
    let big: Vec<u8> = vec![0xCD; MAX_INLINE_VALUE + 32];
    tm.begin().unwrap();
    let mut hs = Vec::new();
    for _ in 0..40 { hs.push(tm.allocate(&big).unwrap()); }
    tm.commit().unwrap();
    tm.begin().unwrap();
    for h in hs.iter().step_by(2) { tm.delete(*h).unwrap(); }
    tm.commit().unwrap();

    // Forge an orphan: extend a fresh page, stamp it as a FreeMapInterior, and
    // do NOT reference it from the tree or mark it free — exactly the state a
    // crash leaves a lost recycle-pool page in.
    let orphan = tm.test_forge_freemap_orphan().unwrap(); // helper added below

    tm.begin().unwrap();
    let reclaimed = tm.reclaim_freemap_orphans().unwrap();
    tm.commit().unwrap();
    assert!(reclaimed >= 1, "the forged orphan must be reclaimed");
    // The orphan now reads free in the committed tree.
    let tree = FreeMapTree::from_roots(tm.committed_freemap_root(), tm.committed_freemap_depth());
    let mut cache = tm.cache_for_test();
    assert!(tree.is_free(&mut cache, orphan).unwrap());
}
```

(Adjust the exact test-helper names to the codebase's `#[cfg(test)]` accessor conventions; `fresh_manager`, `MAX_INLINE_VALUE` already exist. Add `#[cfg(test)] test_forge_freemap_orphan` that extends a page via the cache, writes `buf[0]=FreeMapInterior`, stamps the checksum, and returns the id; and small `#[cfg(test)]` accessors for the committed freemap root/depth and a cache handle, mirroring existing test accessors.)

- [ ] **Step 2: Run, expect failure** — method missing.

- [ ] **Step 3: Implement** (collect orphans read-only, then mark them free)

```rust
    /// Reclaim freemap-typed pages orphaned by a crash that lost the in-memory
    /// recycle pool: pages of FreeMap/FreeMapInterior type that are unreachable
    /// from the live tree, not already free, and NOT in the current in-memory
    /// recycle pool (those are live recycling state, not orphans). Marks each
    /// orphan free in the freemap. Requires an active transaction (called by
    /// defrag). Returns the count reclaimed.
    pub fn reclaim_freemap_orphans(&mut self) -> Result<u64> {
        let root = self.current_roots.freemap_page;
        let depth = self.current_roots.freemap_depth;
        if root == PAGE_ID_NONE {
            return Ok(0); // no tree yet → no freemap pages to orphan
        }
        let total = self.current_roots.total_pages;

        // Pages that are NOT orphans even though unreachable+not-free: the live
        // recycle pool (all three streams).
        let mut excluded: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
        excluded.extend(self.structural_reuse.iter().copied());
        excluded.extend(self.structural_superseded.iter().copied());
        excluded.extend(self.pending_structural_frees.iter().copied());

        let tree = FreeMapTree::from_roots(root, depth);
        let mut orphans: Vec<u64> = Vec::new();
        {
            let mut cache = self.cache.borrow_mut();
            let reachable = tree.reachable_pages(&mut cache)?;
            // Page 0..superblock_count are superblocks; start the scan above them.
            for id in self.superblock_count as u64..total {
                if reachable.contains(&id) || excluded.contains(&id) {
                    continue;
                }
                let ty = cache.get(id)?[0];
                if (ty == PageType::FreeMap as u8 || ty == PageType::FreeMapInterior as u8)
                    && !tree.is_free(&mut cache, id)?
                {
                    orphans.push(id);
                }
            }
        }
        // Mark each orphan free — routes through the freemap (COW + recycle),
        // landing them in the BITMAP (data-reusable), disjoint from the pool.
        for id in &orphans {
            self.freemap_mark_free_committed_path(*id)?; // the same helper persist_freemap uses
        }
        Ok(orphans.len() as u64)
    }
```

Notes for the implementer:
- Use whatever the integration already calls to mark a page free into the working tree (the body inside `persist_freemap`'s loop — `tree.mark_free_growing(... structural_extend ...)` with the pool threaded). Factor that into a small `freemap_mark_free_committed_path(id)` helper so the sweep and `persist_freemap` share one tested marking path. Drain the resulting `pending_superseded` into the normal stream.
- Reading each page through the cache checksum-verifies it; a corrupt page surfaces as a fatal error (consistent with the engine's fail-closed stance for a maintenance pass). That is acceptable — note it in a comment.
- The scan is O(total_pages) I/O — off the hot path (defrag), bounded, fine.

- [ ] **Step 4: Run, expect PASS** — `cargo test --lib reclaim_freemap_orphans_marks_lost_freemap_pages_free`. Add a second test asserting a page CURRENTLY in `structural_reuse` is NOT reclaimed (exclusion works): seed the pool, forge no orphan, assert `reclaim_freemap_orphans()` returns 0 and the pool is untouched.

- [ ] **Step 5: Commit** — `feat: reclaim_freemap_orphans (defrag sweep for crash-lost freemap pages)`.

---

### Task 4: Wire the sweep into `defrag` + `DefragStats`

**Files:** Modify `src/defrag.rs`.

- [ ] **Step 1: Write the failing test** (in `tests/` or defrag's module): a defrag run after a forged orphan reports it reclaimed.

```rust
#[test]
fn defrag_reports_freemap_orphans_reclaimed() {
    // build a manager with a forged freemap orphan (as in Task 3), begin a txn,
    // run defrag, assert stats.freemap_orphans_reclaimed >= 1.
    // (Mirror the Task 3 setup; assert on the new DefragStats field.)
}
```

- [ ] **Step 2: Run, expect failure** — no field `freemap_orphans_reclaimed`.

- [ ] **Step 3: Implement** — add `pub freemap_orphans_reclaimed: u64` to `DefragStats` (init 0 in the constructor; it is `#[non_exhaustive]`, so external construction is unaffected). After the data-page relocation loop in `defrag()`, call:

```rust
    // Reclaim freemap pages orphaned by a prior crash that lost the in-memory
    // recycle pool. Off the hot path; this is exactly the place for it.
    stats.freemap_orphans_reclaimed = txm.reclaim_freemap_orphans()?;
```

Update the `defrag` doc comment's "What this does NOT do" list — it currently says handle-table/overflow COW garbage isn't reclaimed; add that freemap orphans now ARE swept (and that handle-table/overflow garbage still is not).

- [ ] **Step 4: Run, expect PASS** — `cargo test --test <defrag test file>` and `cargo test --lib defrag`.

- [ ] **Step 5: Commit** — `feat: defrag sweeps crash-orphaned freemap pages (DefragStats.freemap_orphans_reclaimed)`.

---

### Task 5: The revised spec's integration tests

**Files:** Create `tests/freemap_multipage.rs`.

- [ ] **Step 1: Write the tests**

```rust
use chisel::{Chisel, Options, DefragOptions};

// 1. Depth>0 reclamation through the public API.
#[test]
fn reclaims_freed_pages_with_multipage_freemap() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let mut hs = Vec::new();
    for i in 0..3000u64 { hs.push(db.allocate(format!("v{i}").as_bytes()).unwrap()); }
    db.commit().unwrap();
    db.begin().unwrap();
    for h in hs.iter().take(1500) { db.delete(*h).unwrap(); }
    db.commit().unwrap();
    let before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..1000u64 { db.allocate(format!("r{i}").as_bytes()).unwrap(); }
    db.commit().unwrap();
    let after = db.stats().unwrap().total_pages;
    assert!(after - before < 1000, "freed pages reused (before={before} after={after})");
}

// 2. Steady-state flat file — the property the whole reclamation design exists for.
#[test]
fn steady_state_file_is_flat_under_churn() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let mut hs = Vec::new();
    for i in 0..500u64 { hs.push(db.allocate(format!("v{i}").as_bytes()).unwrap()); }
    db.commit().unwrap();
    // Warm up to steady state.
    for _ in 0..10 {
        db.begin().unwrap();
        let old: Vec<_> = hs.drain(..).collect();
        for h in &old { db.delete(*h).unwrap(); }
        for i in 0..500u64 { hs.push(db.allocate(format!("c{i}").as_bytes()).unwrap()); }
        db.commit().unwrap();
    }
    let baseline = db.stats().unwrap().total_pages;
    // Many more identical churn cycles must NOT grow the file (this is what the
    // freemap-structural recycle guarantees; the original leak failed here).
    for _ in 0..30 {
        db.begin().unwrap();
        let old: Vec<_> = hs.drain(..).collect();
        for h in &old { db.delete(*h).unwrap(); }
        for i in 0..500u64 { hs.push(db.allocate(format!("c{i}").as_bytes()).unwrap()); }
        db.commit().unwrap();
    }
    let after = db.stats().unwrap().total_pages;
    assert_eq!(after, baseline, "file high-water must be flat under steady churn");
}

// 3. Crash -> reopen -> defrag reclaims orphans (file-backed).
#[test]
fn crash_orphans_are_reclaimed_by_defrag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.chisel");
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        let mut hs = Vec::new();
        for i in 0..2000u64 { hs.push(db.allocate(format!("v{i}").as_bytes()).unwrap()); }
        db.commit().unwrap();
        db.begin().unwrap();
        for h in hs.iter().step_by(2) { db.delete(*h).unwrap(); }
        db.commit().unwrap(); // commit leaves dead freemap pages in the in-memory pool
        // Drop without further commits == crash: the in-memory recycle pool is lost,
        // orphaning the last commit's structural supersedes.
    }
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    db.begin().unwrap();
    let stats = chisel::defrag_for_test_or_public_api(&mut db, &DefragOptions::default()).unwrap();
    db.commit().unwrap();
    // If the crash orphaned any freemap page, defrag reclaimed it; if the steady
    // state happened to leave none, the count is 0 — either way no orphan remains.
    let _ = stats.freemap_orphans_reclaimed;
    // Re-running defrag now finds nothing (idempotent: orphans already reclaimed).
    db.begin().unwrap();
    let again = /* run defrag again */ ;
    db.commit().unwrap();
    assert_eq!(again.freemap_orphans_reclaimed, 0, "second sweep finds no orphans");
}
```

(Use the real public defrag entry point — `db.defrag(&opts)` if exposed, else the `chisel::defrag` free function; fix the placeholder call sites. The crash test's value is the **idempotence** assertion: after one sweep, a second finds zero — proving the sweep actually reclaimed whatever the crash orphaned.)

- [ ] **Step 2: Run, expect PASS** — `cargo test --test freemap_multipage`. If `steady_state_file_is_flat_under_churn` shows growth, the recycle is not actually bounding the file — that is a real regression; debug before continuing.

- [ ] **Step 3: Commit** — `test: multi-page freemap reclamation, steady-state flat file, crash->defrag reclaim`.

---

### Task 6 (conditional): Simplify the recycle bookkeeping

Only if Task 1's verification or the code-quality review flags the three-`Vec` recycle (`structural_reuse` / `structural_superseded` / `pending_structural_frees`) as harder to follow than the two logical states (*pending* vs *reusable*) warrant.

- [ ] **Step 1:** If warranted, collapse to the minimal state the one-commit defer needs (e.g. `reuse_now: Vec<u64>` + `superseded_pending: Vec<u64>`), preserving the exact promote-at-commit / reset-on-rollback semantics Task 1 pinned with tests. Keep all Task 1-3 tests green.
- [ ] **Step 2:** Run full `cargo test` + clippy. Commit: `refactor: simplify freemap structural recycle bookkeeping`.

If not warranted, skip this task and say so.

---

> **Phase 2 gate:** full `cargo test` (incl. the new file), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, and the Python suite (`maturin develop && pytest`) all green. Then a final adversarial code review of the whole Phase 2 diff (`git diff main..HEAD`) focused on the recycle crash-safety, the sweep's exclusion correctness, and rollback. Then open **PR 2** off `main`.

---

## Self-Review (against the revised spec)

**Spec coverage:**
- In-memory recycle + session-COW dedup → already on `6c81d4d`; *verified* by Task 1. ✅
- Out-of-band structural alloc (never bitmap) → Task 1 Step 3 (no reachable-page in the reuse pool). ✅
- Defrag orphan-sweep (the crash-recovery story) → Tasks 2-4. ✅
- Sweep excludes the live recycle pool (no double-hand-out) → Task 3 (exclusion + the not-reclaimed test). ✅
- Steady-state flat file → Task 5 test 2. ✅
- Crash → defrag reclaim → Task 5 test 3 (with idempotence). ✅
- Depth>0 reclamation → Task 5 test 1. ✅
- COW-path corruption detection → already on `6c81d4d` (`mark_free_rejects_wrong_position_page_on_cow_spine`); confirmed in the final review. ✅
- One-commit-defer crash-safety + rollback reset → Task 1 Steps 1-2. ✅

**Placeholder scan:** the Task 5 crash test has two `/* run defrag */` call sites the implementer resolves to the real defrag entry point — they are explicitly flagged, not silent. No `TODO`/`TBD` in implementation steps.

**Type consistency:** `reachable_pages(&self, cache) -> Result<FxHashSet<u64>>`; `reclaim_freemap_orphans(&mut self) -> Result<u64>`; `DefragStats.freemap_orphans_reclaimed: u64`; the shared `freemap_mark_free_committed_path` helper used by both `persist_freemap` and the sweep. Consistent across tasks.

> **Open risk:** the sweep's correctness hinges on the exclusion set being EXACTLY the live recycle pool. If the recycle bookkeeping is simplified (Task 6), the exclusion set in Task 3 must be updated to match the new field set in the same change.
