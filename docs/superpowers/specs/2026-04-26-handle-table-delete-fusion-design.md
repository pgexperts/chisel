# Handle-Table Delete Fusion — Design

**Date:** 2026-04-26
**Status:** Design approved; implementation plan pending.
**Scope:** Fuse the lookup + tombstone-write into a single radix-tree descent in `handle_table::delete`. Eliminates one of the two tree walks that `delete_inner` currently performs per handle. Tracked in ISSUES.md as I32 (resolved by this work) and I33 (deferred per-leaf batching, open).

This is PR-A of the F1 finding from the 2026-04-26 chisel-performance review (`docs/reviews/perf-review-2026-04-26.md`). The deferred PR-B (per-leaf batching of `delete_many`) is filed as I33; its fix is workload-gated and not part of this design.

## 1. Goals and Non-Goals

### Goals

- Eliminate the redundant tree walk in `delete_inner`. Today, deleting one handle costs **two** full radix-tree descents — one via `handle_table::lookup` to read the existing entry, one via `handle_table::delete` (which is `insert(deleted_entry)` underneath) to write the tombstone. After this change, deleting one handle costs **one** descent: a fused primitive that reads the entry and writes the tombstone in the same walk.
- For 1000 sequential `delete()` calls inside a transaction, halve the number of cache_get calls on the handle-table path. This is the cheapest reduction possible without algorithmic change.
- Fix ISSUES.md's tracking by adding I32 (resolved) and I33 (open). The audit doc (`perf-review-2026-04-26.md`) stays as-is; it is a point-in-time record.

### Non-Goals (this PR)

- *Per-leaf batching.* Sorting handles by their target leaf and descending the tree once per unique leaf is a separate, larger optimization. Deferred and tracked as ISSUES.md I33. `delete_many_inner` stays a loop over the new fused primitive.
- *Co-located data-page slot batching.* When multiple handles in a batch live on the same data page, `release_data_slot` could batch the slot-count decrements. Out of scope; same deferral as I33.
- *`update_inner` fusion.* `update` has a similar lookup-then-write pattern. Tempting to fold in, but that's a separate change with its own correctness considerations. YAGNI.
- *Performance benchmarks.* PR 4 of the bench-suite series will be the witness for whether this fusion produces the predicted ~2× win on rows 7 and 8 of the micro grid. This PR doesn't add its own bench harness.
- *Skill text update.* The chisel-performance skill (in a plugin cache outside this repo) currently overstates `delete_many`'s efficiency. Updating it is a separate concern from this PR's git history.

## 2. Algorithm Changes in `handle_table.rs`

### 2.1 New signature

```rust
pub fn delete(
    &mut self,
    cache: &mut PageCache,
    root: u64,
    handle: u64,
) -> Result<(u64, Option<HandleEntry>)>;
```

The second tuple element is `Some(entry)` if the handle had a Live or Overflow entry that was just tombstoned, `None` if the handle was absent or already tombstoned.

### 2.2 Behavior changes from today

| Case | Today | After this PR |
|------|-------|---------------|
| Live or Overflow entry | Walks tree twice (lookup + insert-tombstone), grows tree if needed, returns new root | One walk; returns `(new_root, Some(entry))` |
| Already-Deleted entry | Walks tree, COWs path, writes another tombstone over it | Walks tree, returns `(root, None)` — **no COW**, no wasted writes |
| Absent (no subtree allocated) | `insert` grows the tree to accommodate, then writes tombstone | Returns `(root, None)` immediately at the zero-child-pointer step — **no growth, no allocation** |
| `handle >= capacity()` | `insert` grows tree until handle fits, then writes tombstone | Returns `(root, None)` at the capacity-check guard (mirrors I26 in `find_leaf`) |

The "no COW for already-Deleted or absent" case is a small bonus optimization. Today's code path is unreachable from `delete_inner` (which short-circuits via `lookup` first), so removing the redundant COW affects no caller. It falls naturally out of the fusion and produces strictly cleaner write behavior.

`grow()` is no longer called from `delete`. The growth path stays in `insert` where it belongs.

### 2.3 Recursion shape

```rust
fn delete_recursive(
    &mut self,
    cache: &mut PageCache,
    page: u64,
    handle: u64,
    level: usize,
) -> Result<(u64, Option<HandleEntry>)> {
    if level == 0 {
        // Leaf — read entry, decide whether to write tombstone.
        // Read first, then if Live/Overflow:
        //   COW leaf, write tombstone, return (new_leaf, Some(entry)).
        // If Deleted: return (page, None) — no COW.
    } else {
        // Interior — descend the appropriate child.
        // If child pointer is 0: return (page, None) — no COW, absent subtree.
        // Else: recurse; if recursion returned Some, COW this interior to point
        //       at new_child and propagate the prev_entry up; if None, return
        //       (page, None) without COW.
    }
}
```

The "no COW if recursion returned None" optimization means an absent or already-tombstoned handle costs exactly the read walk down — no writes propagate up the tree. Today's code COWs the entire path even when nothing changed.

### 2.4 What stays the same

- The leaf entry layout (handle slot at `handle % ENTRIES_PER_LEAF`).
- The COW invariant for actual tombstone writes (every page on the COW path is freshly allocated via `cache.new_page()`).
- The interior child-pointer interpretation (zero = no subtree).
- The capacity guard (I26).

## 3. Migration in `transaction.rs`

### 3.1 `delete_inner` rewrites to use the fused primitive

```rust
fn delete_inner(&mut self, handle: u64) -> Result<()> {
    if !self.active_txn {
        return Err(ChiselError::NoActiveTransaction);
    }

    // Single tree walk (I32): writes the tombstone AND returns the
    // previous entry. Returns Some(entry) if a Live/Overflow handle
    // was tombstoned; None if the handle was absent or already a
    // tombstone. We escalate None to InvalidHandle here at the caller
    // layer to preserve the public-API behavior.
    let (new_root, prev_entry) = {
        let mut cache = self.cache.borrow_mut();
        self.handle_table
            .delete(&mut cache, self.current_roots.handle_table_page, handle)?
    };
    let entry = prev_entry.ok_or(ChiselError::InvalidHandle(handle))?;

    // Tombstone is now in current_roots; release the value's storage.
    // Order vs. the tombstone write doesn't matter for correctness —
    // both halves become durable (or rolled back) atomically at commit.
    // Tombstone-first is simpler than today's lookup-release-tombstone
    // shape and avoids the redundant lookup walk.
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
            // already-tombstoned entries, and we escalated None to
            // InvalidHandle above.
            unreachable!("delete returns None for Deleted entries");
        }
    }

    self.current_roots.handle_table_page = new_root;
    Ok(())
}
```

### 3.2 `delete_many_inner` body unchanged; comments updated

```rust
/// Delete many handles in a single transaction.
///
/// Today: this is a loop over delete_inner. After PR-A's fusion (I32),
/// each delete walks the handle table once per handle. For dense
/// delete patterns (many handles in the same leaf), a per-leaf
/// batched implementation would walk once per leaf instead — that's
/// tracked as I33 in ISSUES.md, deferred until a workload
/// demonstrates the win is worth the complexity.
///
/// Error semantics: on the first error the loop stops and returns
/// the error. Handles deleted before the failure remain marked for
/// deletion in current_roots, so the caller can choose between
/// rollback() (abandon the whole batch) or commit() (keep the
/// partial work).
pub fn delete_many(&mut self, handles: &[u64]) -> Result<()> {
    self.check_alive()?;
    let result = self.delete_many_inner(handles);
    self.poison_on_fatal(result)
}

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

### 3.3 What stays the same

- `delete_inner`'s caller-visible behavior: `Err(InvalidHandle)` for absent or already-tombstoned, `Ok(())` on success, fatal errors poison via `poison_on_fatal` at the public wrapper.
- `delete_many_inner`'s caller-visible behavior: first-error-stops semantics, partial work stays in `current_roots` until commit-or-rollback.
- The single update to `current_roots.handle_table_page` per delete.

## 4. Testing

### 4.1 Required: `handle_table.rs` unit tests

Five tests in the existing `#[cfg(test)] mod` block:

- `delete_returns_some_for_live_entry` — insert a Live entry, delete it, expect `Some(Live entry with original page_id/slot_index)` and `new_root != root`.
- `delete_returns_some_for_overflow_entry` — same as above with `HandleFlags::Overflow`.
- `delete_returns_none_for_already_deleted` — insert, delete, delete again. Second delete returns `(root, None)` with unchanged root and unchanged tree depth.
- `delete_returns_none_for_absent_handle_in_existing_subtree` — an existing leaf where the slot was never written; `delete` returns `(root, None)`.
- `delete_does_not_grow_tree_for_handle_beyond_capacity` — tree at depth 0 (capacity = 510). `delete(u64::MAX)` returns `(root, None)`. Crucially: depth is still 0 after the call. No `grow()` happened.

The third and fifth are the most important — they pin the new behavior that no-op deletes don't COW the tree and don't grow it.

### 4.2 Required: `tests/transactions.rs` integration tests

Existing tests should all pass unchanged. Caller-visible behavior is identical: same `Err(InvalidHandle)` for absent/already-tombstoned, same `Ok(())` on success, same first-error-stops semantics for `delete_many`. No new integration tests needed.

Most relevant existing tests as confidence checks:
- `test_delete_file` / `test_delete_memory` — basic delete correctness.
- Anything in the test suite calling `delete()` followed by `read()` of the same handle — verifies the tombstone path.

### 4.3 Deferred: cache-counter regression test

A test asserting "deleting handle h at depth d incurs ≤ N cache_get calls" would empirically verify the fusion. Skipped for this PR:

- Pinning a tight upper bound is brittle — a future refactor of unrelated paths could break this test for the wrong reason.
- The empirical win will be measurable in PR 4's micro grid (row 7: `delete` 1-per-tx). When the Chisel column there shows the predicted ~2× improvement, that's the actual evidence.
- Nothing in this PR blocks adding such a test later via `Chisel::counters()` if a workload-specific regression makes it worth pinning.

## 5. Memorialization

### 5.1 ISSUES.md entries

Two new entries in the "Handle table" section, following the existing `I<N>` numbering convention. The current high-water mark is I31; new entries are I32 (resolved by this PR) and I33 (open, deferred).

**I32:**

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
  implementation returns `(root, None)` immediately without COWing the
  path or growing the tree. Today's code path (unreachable from
  `delete_inner` since `lookup` short-circuits first) would have COWed
  and possibly grown — wasted writes that no caller benefits from.
- `delete` no longer calls `grow()`. Tree growth stays in `insert`.

**Regression tests:** Five new unit tests in `handle_table.rs` covering
the four return-value cases (Live, Overflow, already-Deleted, absent)
plus a no-tree-growth assertion for the beyond-capacity case.
```

**I33:**

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
- For each unique leaf: descend the tree once, COW the path, write all
  tombstones for that leaf in one pass.
- Parallel optimization for `release_data_slot`: handles whose Live
  entries point to the same data page can have their slot-count
  decrements batched.
- Estimated 5–10× speedup for dense delete patterns; no change for
  sparse.
```

### 5.2 What is NOT touched

- The "Suggested fix order" header at the top of `ISSUES.md`. Its 2026-04-22 status note already says "every item below has landed"; I32 will land same-day so the spirit holds. I33 stays open but is explicitly P3-deferred, which the legend already accommodates.
- The chisel-performance skill text. Lives in a plugin cache outside the project's git history; updating it is a separate concern.
- The `perf-review-2026-04-26.md` audit doc. Audits are point-in-time records; F1 in that doc captures the state on 2026-04-26-pre-fix, and a future audit will see I32 as resolved and I33 as open. No retroactive editing.

## 6. Open Implementation-Phase Questions

These are deliberately deferred to the implementation plan:

- Borrow-checker handling for the leaf-level COW (read old leaf bytes; allocate new leaf; copy bytes; modify entry; re-checksum). The pattern in `insert_recursive` shows how to scope the immutable borrow before taking the mutable one; the implementation plan resolves the exact shape.
- Whether the new `delete_recursive` is private to the `HandleTable` impl block or appears as a sibling helper. Implementation detail.
- Test data shape for the unit tests (specific handle values, page-id sentinels, etc.). The tests should pin behavior, not internals; the plan determines the exact handle values.

These are implementation-detail decisions that don't affect the design contract.
