# transaction.rs Module Extraction — Design Spec

Status: approved 2026-06-22
Type: pure behavior-preserving refactor (no on-disk format change, no API change,
no behavior change).
Origin: the last carve-out of the 2026-06-22 fresh-eyes review (SMELL #4,
`transaction.rs:1-2611` god-module).

---

## Summary

`src/transaction.rs` is 5,377 lines (~3,096 production, ~2,281 tests) — by far the
largest file in the crate — holding the entire `TransactionManager`: the commit
protocol, savepoints, the R1 slot-packer, the freemap recycle machinery, the
BUG#2 atomic staging, named roots, reads, mutations, the poison flag, and four
`#[cfg(test)]` fault-injection flags baked into the production struct. Every
durability invariant (3-fsync ordering, I18 freemap window, BUG#2 atomic staging,
R1 cursor accounting, watermark rollback, the one-commit structural-recycle
defer) is encoded as prose cross-references rather than types or module
boundaries, so a reviewer must hold all of them simultaneously to safely change
the commit path.

This refactor splits the file into a `src/transaction/` module and extracts
cohesive units behind narrow interfaces: three **owned state sub-structs**
(`SlotPacker`, `FreemapRecycle`, `FaultInjector`) and two **behavior units**
(`CommitProtocol`, `StagingTxn`). It is **purely structural** — the byte-for-byte
behavior, the public API, and the on-disk format are unchanged, and the existing
~2,281-line test suite is the oracle that proves it.

## Motivation

- The commit path is "the single biggest barrier to safely changing" (review):
  a one-line change requires understanding the freemap recycle, the slot packer,
  the staging, and the fsync ordering at once.
- Test concerns leak into the core type: four `#[cfg(test)]` `Cell` fields sit on
  the production `TransactionManager`.
- The freemap work (PRs #70/#71) added ~600 lines and five recycle fields,
  growing the module further.

## Design decisions

1. **Module split by concern, not by layer.** `src/transaction.rs` →
   `src/transaction/` with `mod.rs` holding the `TransactionManager` struct,
   `Roots`, and `Savepoint`, and impl blocks moved into focused submodule files.
   Rust privacy is module-scoped *and inherited by descendants*, so a child
   module (`transaction::freemap`) can access the parent struct's private fields
   with zero visibility churn — no field is made `pub` that wasn't.

2. **State sub-structs own ONLY their cluster; methods take shared resources as
   parameters.** `SlotPacker`, `FreemapRecycle`, `FaultInjector` hold just their
   fields. Their methods take `&mut PageCache` (the caller holds the `RefMut`
   from `self.cache.borrow_mut()`) and, where they allocate, an `alloc`/`extend`
   **closure** — the exact disjoint-field-borrow + closure pattern the freemap
   integration already uses (`take_freemap_tree`/`put_freemap_tree`, the
   `cow_alloc` closure). This pattern is already proven to satisfy the borrow
   checker against `self.handle_table` / `self.cache` simultaneously.

3. **Behavior units own almost nothing; they operate over a context.**
   `CommitProtocol` and `StagingTxn` are thin — they take the cache, the roots,
   and `&mut` to the relevant sub-structs (per the review's "CommitProtocol over
   a roots+freemap snapshot"), closer to function-modules-with-a-context than
   owned objects.

4. **"Owned struct vs free-function module" is decided per unit DURING execution,
   by what compiles cleanly.** If extracting a unit as an owned struct forces
   contortions (an orchestrator needing simultaneous `&mut` to three sub-structs
   through `self`), the fallback is a free-function module in the same file with
   the same narrow interface — no forced ownership. This is an explicit,
   pre-blessed fallback, not a failure.

5. **`FaultInjector` is `#[cfg(test)]`, not a trait object.** The manager holds a
   single `#[cfg(test)] fault: FaultInjector` (one field replacing four), and
   production code consults it only behind `#[cfg(test)]`. A `dyn` trait object
   would add a vtable + an always-present `Option<Box<...>>` for a test-only
   concern — more machinery than the smell warrants.

6. **Incremental, low-risk-first ordering, each step green.** Six steps:
   file-split → FaultInjector → SlotPacker → FreemapRecycle → CommitProtocol →
   StagingTxn. The risky commit/staging extractions land last, on an
   already-organized base, as small diffs.

## Module structure

```
src/transaction/
  mod.rs          TransactionManager struct, Roots, Savepoint, field docs,
                  the public-API wrappers that delegate to the units
  recovery.rs     create_new, open_existing
  lifecycle.rs    begin/commit/rollback (the *_inner orchestration that calls
                  CommitProtocol)
  commit.rs       CommitProtocol (the 3-fsync sequence + superblock write)
  staging.rs      StagingTxn (BUG#2 candidates, abort_allocate_prepare, install)
  freemap.rs      FreemapRecycle (the recycle pool + persist_freemap +
                  reclaim_freemap_orphans + take/put_freemap_tree)
  packing.rs      SlotPacker (R1 live-slots + insert_cursor)
  savepoints.rs   savepoint / rollback_to / release
  named_roots.rs  set/get/clear_root_name + encode_root_name
  read.rs         read, tag, client_byte, handles, handles_with_tag, lookups
  mutate.rs       update, delete, delete_tagged, delete_with_tag, delete_many
  config.rs       set_cache_max_bytes / set_spillway_max_bytes / set_drain_insertion
  stats.rs        counters, spillway_capacity, file_page_count, sparse_data_pages,
                  data_page_ids_snapshot, handle_live_page_id, introspection
  fault.rs        #[cfg(test)] FaultInjector
  tests.rs        the test suite (or per-concern test submodules)
```

## The units

### `SlotPacker` (packing.rs) — owned state
- **State:** `committed_live_slots: FxHashMap<u64,u32>`, `current_live_slots:
  FxHashMap<u64,u32>`, `insert_cursor: Option<u64>`.
- **Owns:** R1 slot packing — `insert_into_data_page`, `release_data_slot`, the
  live-slot accounting and the insert cursor.
- **Interface (illustrative):**
  - `insert(&mut self, cache, alloc: impl FnMut(&mut PageCache)->Result<u64>, value) -> Result<(u64, u16)>`
  - `release_slot(&mut self, page_id: u64)`
  - lifecycle: `begin(&mut self)` (clone committed→current, reset cursor),
    `commit(&mut self)` (promote current→committed), `rollback(&mut self)`
    (reset current←committed).
  - savepoint hooks: `snapshot() -> (FxHashMap, Option<u64>)` and
    `restore(snap)` — because `Savepoint` already captures `live_slots` +
    `insert_cursor`.
- **Depends on:** `PageCache` + an allocator closure (the data page comes from the
  freemap path). No reach into the freemap internals.

### `FreemapRecycle` (freemap.rs) — owned state (the hardest)
- **State:** `freemap_hint: u64`, `structural_reuse: Vec<u64>`,
  `structural_superseded: Vec<u64>`, `pending_structural_frees: Vec<u64>`,
  `freemap_session_owned: FxHashSet<u64>`.
- **Owns:** `take_freemap_tree` / `put_freemap_tree`, the structural `extend`
  (pool-then-file), `allocate_data_page`'s freemap path, `cow_alloc`,
  `freemap_mark_free_committed_path`, `persist_freemap`,
  `reclaim_freemap_orphans`, and the recycle lifecycle (begin seeds
  `structural_reuse` from `pending_structural_frees`; commit promotes
  `structural_superseded`; rollback restores).
- **Interface:** an `allocate(cache, roots, reuse_enabled) -> Result<u64>` that
  yields a reusable-or-extended page id and threads the tree handle internally;
  `mark_free_committed_path`, `persist(cache, roots)`, `reclaim_orphans(cache,
  roots, savepoints_active)`; the begin/commit/rollback hooks. The one-commit
  defer and the extend-only termination invariant live entirely inside this unit.
- **Note:** because `cow_alloc` is shared by the data path, the handle-table COW,
  and the membership COW, the closures those call sites pass will be rephrased to
  borrow `&mut FreemapRecycle` (a single field) instead of five scattered fields —
  a net simplification of the disjoint-borrow dance.

### `FaultInjector` (fault.rs) — `#[cfg(test)]`
- **State:** `fail_next_membership_op`, `fail_next_handle_table_op`,
  `fail_next_update_value_write`, `fail_membership_op_after` (the four `Cell`s).
- **Interface:** `should_fail_membership()`, `should_fail_handle_table()`, etc.,
  consulted only behind `#[cfg(test)]` in the staging/mutate paths.
- The manager carries `#[cfg(test)] fault: FaultInjector`. Production builds have
  zero test fields.

### `CommitProtocol` (commit.rs) — behavior unit
- **Owns:** the `commit_inner` sequence — the I28 pre-drain flush, the
  `FreemapRecycle::persist` call, the data fsync, the superblock build + write +
  fsync (the strict data-fsync-before-superblock ordering), the
  `committed_roots = current_roots` promotion, and the structural-frees promotion.
- **Operates over:** `&mut PageCache`, `&mut Roots` (current/committed), the
  `txn_counter`, `superblock_count`, and `&mut FreemapRecycle`. Owns no state.
- The 3-fsync ordering and the I18 allocate-before-merge invariant are preserved
  exactly; this unit makes them a single readable sequence rather than a method
  buried among 60 others.

### `StagingTxn` (staging.rs) — behavior unit
- **Owns:** the BUG#2 atomic staging of `allocate_inner` — the forward
  (handle-table) and reverse (membership) candidate computation
  (`handle_table_insert_candidate`, `membership_insert_candidate`,
  `membership_remove_candidate`), `abort_allocate_prepare`, and the infallible
  install phase. The "compute-without-install, then install atomically"
  discipline (and the bounded-residue-on-abort contract documented in PR #75)
  lives here.
- **Operates over:** the cache, the roots, `&mut HandleTable`, `&mut
  MembershipIndex`, `&mut FreemapRecycle`, `&mut SlotPacker`, and (test-only) the
  `FaultInjector`. Last to extract because it touches the most units.

### Slimmed `TransactionManager` (mod.rs)
- **Holds:** `cache`, `committed_roots`, `current_roots`, `handle_table`,
  `membership_index`, `txn_counter`, `superblock_count`, `active_txn`,
  `savepoints`, `txn_freed_pages`, `poisoned`, and the owned units (`packer`,
  `freemap`, `#[cfg(test)] fault`).
- **Is:** the public API surface + the thin orchestration (`begin`/`commit`/
  `rollback` delegate the heavy lifting to the units; `read`/`update`/`delete`/
  named-roots live in their concern files but as `impl TransactionManager`).

## Behavior preservation & testing

- **The oracle is the existing suite** (~2,281 lines incl. the recycle pins, the
  staging tests, `assert_no_reachable_page_is_free`, the recovery/superblock
  tests, the spillway/fsync-count integration tests). A pure refactor changes no
  test *behavior*; the file-split re-paths them into `transaction/tests.rs` (or
  per-concern test files), and the only test-code change is the fault-flag
  plumbing moving to `FaultInjector`.
- **Green at every step:** full `cargo test` (not `--lib`) + `cargo clippy
  --workspace --all-targets -- -D warnings` + `cargo fmt --check` + the Python
  suite must pass before any step is committed.
- **Per-unit surface check:** after each extraction, confirm the unit's public
  (crate-visible) surface is its narrow interface — no field leaks beyond it
  (clippy + a grep for direct field access from outside the unit's module).

## Execution order (six green steps, low-risk → high-risk)

Each step is its own commit, likely its own PR off `main` (per the project's
one-PR-per-unit workflow), merged before the next begins so each subsequent diff
is small and reviewable against a clean base.

1. **File-split** — pure code movement into `src/transaction/`; struct + fields +
   behavior unchanged. Establishes the module; makes every later diff smaller.
2. **FaultInjector** — consolidate the four `#[cfg(test)]` flags into one
   test-only struct field. Smallest, test-only.
3. **SlotPacker** — extract the R1 cluster + its savepoint snapshot/restore hooks.
4. **FreemapRecycle** — extract the recycle cluster; rephrase the `cow_alloc`
   call sites to borrow the single unit. The hardest data extraction.
5. **CommitProtocol** — extract `commit_inner` onto the now-clean base.
6. **StagingTxn** — extract the prepare/install. Last (most cross-unit).

## Risks & mitigations

- **Borrow-checker pushback on owned orchestrators** → the pre-blessed
  function-module fallback (decision 4). The interface stays narrow either way.
- **Subtle behavior change in a pure refactor** → the suite is the net; each step
  is small and independently green; the highest-risk steps (commit, staging) land
  last on an organized base and get adversarial review.
- **Savepoint snapshot coupling** → `Savepoint` captures `live_slots` +
  `insert_cursor`; `SlotPacker` exposes `snapshot()/restore()` so the savepoint
  machinery snapshots through the narrow interface rather than reaching fields.
- **#75 (abort-leak) in flight** → it edits `transaction.rs`; the file-split must
  start off `main` only after #75 merges, or the move-vs-edit conflict is
  unmanageable.

## Format-version / Don't-Break compliance

Pure structural refactor: no on-disk byte changes meaning, no commit-ordering
change, no poison-model change, no API change, no `FORMAT_VERSION` impact. The
3-fsync ordering, the I18 ordering, the extend-only freemap termination, the
single-writer `&mut self` contract, and strict layering are all preserved — the
refactor only relocates the code that enforces them.

## Out of scope

- Any behavior change, optimization, or new feature (this is purely structural).
- Splitting the test suite into a separate crate.
- Extracting reads/mutations/named-roots/stats/config into owned units — they
  move to concern files (file-split) but stay `impl TransactionManager`; they are
  thin delegators without a distinct state cluster, so a sub-struct would be
  ceremony without benefit (YAGNI).
- A `dyn` trait-object fault-injection seam (decision 5).
