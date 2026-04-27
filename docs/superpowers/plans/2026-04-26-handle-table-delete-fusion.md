# Handle-Table Delete Fusion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fuse `handle_table::lookup` and `handle_table::delete` into a single radix-tree descent, eliminating one of the two tree walks that `delete_inner` currently performs per handle.

**Architecture:** Modify `handle_table::delete`'s signature to return `(u64, Option<HandleEntry>)` instead of just `u64`. New private `delete_recursive` method does a single-pass descent: reads the existing entry from the leaf, writes the tombstone via COW, propagates the new root up the path. For absent or already-tombstoned handles, returns `(root, None)` immediately without COWing or growing the tree. `delete_inner` in `transaction.rs` migrates to the new tuple return shape.

**Tech Stack:** Rust 2021, single-crate `chisel` change. No new dependencies.

---

## Notes for the executing engineer

- This is PR-A of the F1 finding from `docs/reviews/perf-review-2026-04-26.md`. The deferred PR-B (per-leaf batching of `delete_many`) is filed as I33 in ISSUES.md after this PR lands.
- Spec: `docs/superpowers/specs/2026-04-26-handle-table-delete-fusion-design.md`. Don't read it during execution — everything you need is in this plan. Reference it only if you hit something unclear.
- The signature change is in-place: there is no parallel "old `delete` and new `delete`" period. Task 1 lands signature + implementation + caller migration as one commit because the codebase must compile at every commit boundary.
- `cargo clippy -- -D warnings` and `cargo fmt -- --check` must pass at every commit. Doc-comment list items use 2-space hanging indent (clippy `doc_overindented_list_items` enforces this — it caught a similar plan-pasted bug in PR 1).
- If the executing engineer wants isolation, create a worktree before Task 1: `git worktree add .worktrees/delete-fusion -b delete-fusion`. All subsequent work happens inside.
- The change only touches engine internals; no public API surface changes; no `bench/` or `python/` work.

## File Structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `src/handle_table.rs` | Modify | Replace `pub fn delete` signature + body with the fused version. Add private `delete_recursive`. Add 5 unit tests in `mod tests`. |
| `src/transaction.rs` | Modify | Rewrite `delete_inner` to consume the tuple return. Update `delete_many` doc-comment and `delete_many_inner` body comment. |
| `ISSUES.md` | Modify | Add I32 (resolved) and I33 (open) entries in the "Handle table" section. |

No files created, no public API surface changes, no test files added (new tests go in the existing `#[cfg(test)] mod tests` block of `handle_table.rs`).

---

## Task 1: Core fusion — `handle_table::delete` signature + recursion + caller migration + tests

This is the substantive task. It lands the signature change, the new fused implementation, the migration of the sole caller (`delete_inner`), and 5 new unit tests as one coherent commit. The codebase compiles cleanly throughout because all coupled pieces land together.

**Files:**
- Modify: `src/handle_table.rs` (replace `pub fn delete`, add `fn delete_recursive`, add 5 tests in `mod tests`)
- Modify: `src/transaction.rs` (rewrite `delete_inner` body)

- [ ] **Step 1: Confirm baseline — full test suite green from repo root**

Run: `cargo test`
Expected: every existing test passes. If anything is red, stop and report.

- [ ] **Step 2: Add the 5 failing tests to `src/handle_table.rs`'s `mod tests` block**

Find the `#[cfg(test)] mod tests` block (it contains the existing test `lookup_handle_beyond_capacity_returns_none` and others). Append these five tests:

```rust
#[test]
fn delete_returns_some_for_live_entry() {
    let mut cache = make_cache();
    let mut ht = HandleTable::new();
    let root = ht.create_root(&mut cache).unwrap();

    // Insert a Live entry.
    let live_entry = HandleEntry {
        page_id: 42,
        slot_index: 7,
        flags: HandleFlags::Live,
    };
    let root_after_insert = ht.insert(&mut cache, root, 100, &live_entry).unwrap();

    // Delete it. Expect (new_root, Some(live_entry-equivalent)).
    let (new_root, prev_entry) = ht.delete(&mut cache, root_after_insert, 100).unwrap();

    assert_ne!(new_root, root_after_insert, "delete of a Live entry must COW the leaf");
    let entry = prev_entry.expect("delete of a Live entry must return Some(entry)");
    assert_eq!(entry.page_id, 42);
    assert_eq!(entry.slot_index, 7);
    assert_eq!(entry.flags, HandleFlags::Live);
}

#[test]
fn delete_returns_some_for_overflow_entry() {
    let mut cache = make_cache();
    let mut ht = HandleTable::new();
    let root = ht.create_root(&mut cache).unwrap();

    let overflow_entry = HandleEntry {
        page_id: 99,
        slot_index: 0,
        flags: HandleFlags::Overflow,
    };
    let root_after_insert = ht.insert(&mut cache, root, 200, &overflow_entry).unwrap();

    let (new_root, prev_entry) = ht.delete(&mut cache, root_after_insert, 200).unwrap();

    assert_ne!(new_root, root_after_insert, "delete of an Overflow entry must COW the leaf");
    let entry = prev_entry.expect("delete of an Overflow entry must return Some(entry)");
    assert_eq!(entry.page_id, 99);
    assert_eq!(entry.flags, HandleFlags::Overflow);
}

#[test]
fn delete_returns_none_for_already_deleted() {
    let mut cache = make_cache();
    let mut ht = HandleTable::new();
    let root = ht.create_root(&mut cache).unwrap();

    let live_entry = HandleEntry {
        page_id: 42,
        slot_index: 7,
        flags: HandleFlags::Live,
    };
    let root_after_insert = ht.insert(&mut cache, root, 100, &live_entry).unwrap();

    // First delete: returns Some(entry).
    let (root_after_first_delete, _) = ht.delete(&mut cache, root_after_insert, 100).unwrap();

    // Second delete: handle is now a tombstone. Expect (root, None) with NO COW.
    let (root_after_second_delete, prev_entry) =
        ht.delete(&mut cache, root_after_first_delete, 100).unwrap();

    assert_eq!(prev_entry, None, "delete of an already-tombstoned handle must return None");
    assert_eq!(
        root_after_second_delete, root_after_first_delete,
        "no-op delete must not COW the tree (root unchanged)"
    );
}

#[test]
fn delete_returns_none_for_absent_handle_in_existing_subtree() {
    let mut cache = make_cache();
    let mut ht = HandleTable::new();
    let root = ht.create_root(&mut cache).unwrap();

    // Insert handle 100 — this allocates the depth-0 leaf containing slots
    // 0..510. Handle 200 lands in the same leaf (slot 200) but was never
    // written, so its slot is zeroed (read_entry returns flags = 0 = treated
    // as Deleted by the leaf-level branch).
    let live_entry = HandleEntry {
        page_id: 42,
        slot_index: 7,
        flags: HandleFlags::Live,
    };
    let root_after_insert = ht.insert(&mut cache, root, 100, &live_entry).unwrap();

    let (new_root, prev_entry) = ht.delete(&mut cache, root_after_insert, 200).unwrap();

    assert_eq!(prev_entry, None, "delete of an absent slot must return None");
    assert_eq!(
        new_root, root_after_insert,
        "delete of an absent slot must not COW the tree"
    );
}

#[test]
fn delete_does_not_grow_tree_for_handle_beyond_capacity() {
    let mut cache = make_cache();
    let mut ht = HandleTable::new();
    let root = ht.create_root(&mut cache).unwrap();

    // Tree is at depth 0 (capacity = 510). u64::MAX is far beyond.
    assert_eq!(ht.depth(), 0);

    let (new_root, prev_entry) = ht.delete(&mut cache, root, u64::MAX).unwrap();

    assert_eq!(prev_entry, None);
    assert_eq!(new_root, root, "delete beyond capacity must not COW the tree");
    assert_eq!(
        ht.depth(),
        0,
        "delete beyond capacity must NOT grow the tree (insert would have grown; delete must not)"
    );
}
```

The `make_cache()` helper, `HandleTable::new()` / `ht.create_root(&mut cache)` constructor pattern, and `ht.depth()` are all confirmed present in the existing test module — see `lookup_sparse_range_in_depth1_tree_returns_none` for the canonical usage pattern. `HandleEntry` and `HandleFlags` are in scope via the existing `use super::*;` at the top of `mod tests`. No new test helpers needed.

(`ht.depth()` returns `u32`, so the assertion `assert_eq!(ht.depth(), 0)` works via the integer-literal coercion — same as the existing `assert_eq!(ht.depth(), 1)` in `lookup_sparse_range_in_depth1_tree_returns_none`.)

- [ ] **Step 3: Run cargo build, expect compile error**

Run: `cargo build`
Expected: fails to compile. The first error should be in `handle_table.rs` test module — the new tests destructure `let (new_root, prev_entry) = ht.delete(...)` but `ht.delete` currently returns `Result<u64>`, not a tuple. This is the failing-test signal that the new behavior isn't implemented yet.

- [ ] **Step 4: Replace `pub fn delete` and add `fn delete_recursive` in `src/handle_table.rs`**

Locate the existing `pub fn delete` (around line 226). Replace its entire body with:

```rust
    /// Delete a handle: write a tombstone for it and return the previous
    /// entry in a single tree descent.
    ///
    /// Returns `(new_root, Some(entry))` if the handle had a Live or
    /// Overflow entry that was just tombstoned. Returns `(root, None)`
    /// — note unchanged root — if the handle was absent or already a
    /// tombstone; in those cases no COW is performed and the tree is
    /// not grown. This contrasts with the historical `insert(deleted)`
    /// implementation which always COWed and could grow the tree even
    /// for no-op deletes.
    ///
    /// Tombstone-write semantics are unchanged: the leaf entry stays
    /// at its fixed `(handle % ENTRIES_PER_LEAF)` position forever
    /// once written. This is why `next_handle` in the transaction
    /// layer is monotonic — reusing a deleted handle would be
    /// ambiguous against a stale reader.
    pub fn delete(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
    ) -> Result<(u64, Option<HandleEntry>)> {
        // Empty tree: nothing to delete.
        if root == PAGE_ID_NONE {
            return Ok((root, None));
        }
        // I26-style guard: handle outside tree's reach is definitionally
        // absent. No tree growth (insert grows; delete doesn't).
        if handle >= self.capacity() {
            return Ok((root, None));
        }
        self.delete_recursive(cache, root, handle, self.depth)
    }
```

Then add the new `delete_recursive` method. Place it directly after `delete` (before `iter_live` is fine):

```rust
    /// Single-pass recursive descent that reads the existing entry at
    /// the leaf and writes the tombstone via COW on the way back up.
    /// If the leaf entry is already Deleted (or the subtree is absent
    /// via a zero child pointer), no COW is performed at any level —
    /// the original page IDs propagate back up unchanged.
    fn delete_recursive(
        &mut self,
        cache: &mut PageCache,
        page: u64,
        handle: u64,
        level: usize,
    ) -> Result<(u64, Option<HandleEntry>)> {
        if level == 0 {
            // Leaf: read the entry. Decide whether to write tombstone.
            let index = (handle as usize) % ENTRIES_PER_LEAF;
            let entry = {
                let buf = cache.get(page)?;
                Self::read_entry(buf, index)
            };
            match entry.flags {
                HandleFlags::Deleted => {
                    // Already tombstoned (or never written — read_entry
                    // on a zeroed slot decodes flag byte 0 which is not
                    // Live (1) or Overflow (2), so it presents as
                    // Deleted by elimination). No COW.
                    Ok((page, None))
                }
                HandleFlags::Live | HandleFlags::Overflow => {
                    // COW the leaf, write tombstone, return Some(entry).
                    // Pattern mirrors insert_recursive's leaf branch.
                    let new_leaf = cache.new_page()?;
                    debug_assert_ne!(new_leaf, 0); // I8
                    {
                        let buf_copy: [u8; PAGE_SIZE] = *cache.get(page)?;
                        let new_buf = cache.get_mut(new_leaf)?;
                        *new_buf = buf_copy;
                    }
                    let tombstone = HandleEntry {
                        page_id: 0,
                        slot_index: 0,
                        flags: HandleFlags::Deleted,
                    };
                    {
                        let new_buf = cache.get_mut(new_leaf)?;
                        Self::write_entry(new_buf, index, &tombstone);
                        page::stamp_checksum(new_buf);
                    }
                    Ok((new_leaf, Some(entry)))
                }
            }
        } else {
            // Interior: descend the appropriate child.
            let child_span = self.span_at_level(level);
            let child_idx = (handle / child_span) as usize;
            let child_page = {
                let buf = cache.get(page)?;
                let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
                u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
            };
            if child_page == 0 {
                // Subtree never allocated — handle is definitionally
                // absent. No COW at any level above; return the
                // original page id unchanged.
                return Ok((page, None));
            }
            let (new_child, prev_entry) =
                self.delete_recursive(cache, child_page, handle % child_span, level - 1)?;
            if prev_entry.is_none() {
                // Recursion did not write a tombstone (subtree already
                // tombstoned or absent at the leaf). No COW at this
                // level either; the child pointer is unchanged.
                return Ok((page, None));
            }
            // Recursion COWed below us. COW this interior page so it
            // points at the new child.
            let new_page = cache.new_page()?;
            debug_assert_ne!(new_page, 0); // I8
            {
                let buf_copy: [u8; PAGE_SIZE] = *cache.get(page)?;
                let new_buf = cache.get_mut(new_page)?;
                *new_buf = buf_copy;
            }
            {
                let new_buf = cache.get_mut(new_page)?;
                let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
                new_buf[offset..offset + 8].copy_from_slice(&new_child.to_le_bytes());
                page::stamp_checksum(new_buf);
            }
            Ok((new_page, prev_entry))
        }
    }
```

Notes on the borrow-checker pattern: the leaf and interior cases both copy the buffer to the stack via `let buf_copy: [u8; PAGE_SIZE] = *cache.get(page)?;` before taking the mutable borrow. This is the established pattern in this file — `insert_recursive` uses the same shape. The 8 KB stack copy is acceptable; alternatives (Vec + clone, mem::take, etc.) would allocate.

- [ ] **Step 5: Run cargo build again, expect compile error in `delete_inner`**

Run: `cargo build`
Expected: fails. The error should now be in `src/transaction.rs` at the call site `self.handle_table.delete(&mut cache, ...)` — the existing code does `let new_root = ... .delete(...)?` but the new signature returns a tuple. This is the next failing-test signal.

- [ ] **Step 6: Migrate `delete_inner` in `src/transaction.rs`**

Locate `fn delete_inner` (around line 1335). Replace the entire body with:

```rust
    fn delete_inner(&mut self, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // Single tree walk (I32): writes the tombstone AND returns the
        // previous entry. Returns Some(entry) if a Live/Overflow handle
        // was tombstoned; None if the handle was absent or already a
        // tombstone. We escalate None to InvalidHandle here at the
        // caller layer to preserve the public-API behavior.
        let (new_root, prev_entry) = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .delete(&mut cache, self.current_roots.handle_table_page, handle)?
        };
        let entry = prev_entry.ok_or(ChiselError::InvalidHandle(handle))?;

        // Tombstone is now in current_roots; release the value's
        // storage. Order vs. the tombstone write doesn't matter for
        // correctness — both halves become durable (or rolled back)
        // atomically at commit. Tombstone-first is simpler than the
        // historical lookup-release-tombstone shape and avoids the
        // redundant lookup walk.
        match entry.flags {
            HandleFlags::Live => {
                self.release_data_slot(entry.page_id);
            }
            HandleFlags::Overflow => {
                let freed = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::delete(&mut cache, entry.page_id)?
                };
                self.txn_freed_pages.extend_from_slice(&freed);
            }
            HandleFlags::Deleted => {
                // Unreachable: handle_table::delete returns None for
                // already-tombstoned entries, and we escalated None
                // to InvalidHandle above.
                unreachable!("handle_table::delete returns None for Deleted entries");
            }
        }

        self.current_roots.handle_table_page = new_root;
        Ok(())
    }
```

- [ ] **Step 7: Run the full test suite**

Run: `cargo test`
Expected: all tests pass, including the 5 new ones in `handle_table::tests`. If any fail, address the failure before continuing — the most likely cause is a borrow-checker pattern mismatch in `delete_recursive` (try the buf_copy pattern above) or a test using slightly different helper names than what's actually in the existing test module (adjust names to match what's there).

- [ ] **Step 8: Run clippy and fmt**

Run: `cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: both silent (clean).

If clippy fires `doc_overindented_list_items`: fix the doc-comment indents (2-space hanging indent for list-item continuations under `///`).
If `cargo fmt -- --check` reports diffs: run `cargo fmt` to apply, re-stage, re-verify.

- [ ] **Step 9: Commit**

```bash
git add src/handle_table.rs src/transaction.rs
git commit -m "Fuse handle_table::delete lookup+tombstone (I32)

delete_inner used to walk the radix tree twice per handle: once via
handle_table::lookup to read the existing entry, then again via
handle_table::delete (which was insert(deleted_entry) underneath) to
write the tombstone. After this commit, handle_table::delete returns
(new_root, Option<HandleEntry>) and does both in a single descent.

For absent handles or already-tombstoned entries, delete returns
(root, None) with no COW and no tree growth. Today's caller
(delete_inner) short-circuits absent handles via lookup before
calling delete, so the COW/growth on those paths was wasted work.

Per ISSUES.md I32. Resolves F1 from
docs/reviews/perf-review-2026-04-26.md."
```

---

## Task 2: Update `delete_many` doc comment + add ISSUES.md entries

Independent doc-only changes. Single commit.

**Files:**
- Modify: `src/transaction.rs` (`delete_many` docstring + `delete_many_inner` body comment)
- Modify: `ISSUES.md` (add I32 and I33 in the "Handle table" section)

- [ ] **Step 1: Update `delete_many` doc comment in `src/transaction.rs`**

Locate `pub fn delete_many` (around line 1391). Replace the existing doc comment block (every `///` line above the function) with:

```rust
    /// Delete many handles in a single transaction.
    ///
    /// Today: this is a loop over `delete_inner`. After PR-A's fusion
    /// (I32), each delete walks the handle table once per handle. For
    /// dense delete patterns (many handles in the same leaf), a
    /// per-leaf batched implementation would walk once per leaf
    /// instead — that's tracked as I33 in ISSUES.md, deferred until
    /// a workload demonstrates the win is worth the complexity.
    ///
    /// Error semantics: on the first error the loop stops and returns
    /// the error. Handles deleted before the failure remain marked
    /// for deletion in `current_roots`, so the caller can choose
    /// between `rollback()` (abandon the whole batch) or `commit()`
    /// (keep the partial work).
```

- [ ] **Step 2: Update `delete_many_inner` body comment**

Locate `fn delete_many_inner` (around line 1397). Replace the body to add a one-line comment referencing I33:

```rust
    fn delete_many_inner(&mut self, handles: &[u64]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        // See I33 in ISSUES.md for the deferred per-leaf batching work.
        for &handle in handles {
            self.delete_inner(handle)?;
        }
        Ok(())
    }
```

- [ ] **Step 3: Add I32 entry to `ISSUES.md` in the "Handle table" section**

Find the "## Handle table" section in `ISSUES.md` (it contains entries I6, I7, I8, I26). Append after the last existing entry in that section (I26 is the most recent based on numbering; add after its closing — before the next `---` or section header):

```markdown
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
```

- [ ] **Step 4: Add I33 entry directly after I32**

```markdown
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
```

- [ ] **Step 5: Run cargo test (regression check)**

Run: `cargo test`
Expected: all tests still pass. The doc-comment changes don't affect compilation; this is a cheap sanity check.

- [ ] **Step 6: Run clippy and fmt**

Run: `cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: both clean.

Watch for `doc_overindented_list_items` on the new doc comments.

- [ ] **Step 7: Commit**

```bash
git add src/transaction.rs ISSUES.md
git commit -m "Memorialize I32+I33: delete_many docs + ISSUES.md entries

I32 (now resolved by the previous commit) explains the lookup+tombstone
fusion in handle_table::delete. I33 (open, deferred) tracks the
per-leaf batching of delete_many — workload-gated, not actionable
until PR 4's micro grid shows whether dense delete patterns matter.

delete_many's docstring previously framed the function as having a
batching optimization it never had; the rewrite is honest about its
shape and points readers at I33 for the future work."
```

---

## Task 3: Final gate

Verification only. No code changes.

- [ ] **Step 1: Full Rust suite (regression check)**

Run: `cargo test`
Expected: all tests pass. Should be the same set as Task 1's Step 7 plus the doc-only Task 2 — no behavior changes since.

- [ ] **Step 2: Bench subcrate test suite**

Run: `cd bench && cargo test`
Expected: 1 passed (the smoke test from PR 2). The bench subcrate doesn't depend on `delete_inner` directly but it does exercise the public Chisel API; the smoke test will surface any caller-visible regression.

- [ ] **Step 3: Clippy at root**

Run: `cargo clippy -- -D warnings`
Expected: clean.

- [ ] **Step 4: Clippy in bench**

Run: `cd bench && cargo clippy --tests -- -D warnings`
Expected: clean.

- [ ] **Step 5: Fmt at root**

Run: `cargo fmt -- --check`
Expected: clean.

- [ ] **Step 6: Fmt in bench**

Run: `cd bench && cargo fmt -- --check`
Expected: clean.

- [ ] **Step 7: Confirm git state**

Run: `git status`
Expected: working tree clean.

Run: `git log --oneline -3`
Expected: top entry is the Task 2 commit; second entry is the Task 1 commit. No other unrelated commits introduced.

If everything is green, the PR is ready for the finishing-a-development-branch skill (Option 1: merge locally).

---

## Done

PR-A is complete when all three tasks above are done and gates 1–7 of Task 3 pass. The branch can be merged to main via `superpowers:finishing-a-development-branch` (Option 1).
