# transaction.rs Module Extraction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the 5,377-line `src/transaction.rs` god-module into a `src/transaction/` module and extract five cohesive units (SlotPacker, FreemapRecycle, FaultInjector, CommitProtocol, StagingTxn) behind narrow interfaces — a pure behavior-preserving refactor.

**Architecture:** Six incremental steps, low-risk → high-risk, each its own commit/PR and each leaving the full suite green. State sub-structs own only their field cluster and take `&mut PageCache` + an `alloc` closure; behavior units operate over a context. The existing ~2,281-line test suite is the oracle — no test *behavior* changes.

**Tech Stack:** Rust, `cargo test` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt`, `maturin develop && pytest` (Python binding unaffected but engine changes are re-verified).

**Spec:** `docs/specs/2026-06-22-transaction-module-extraction-design.md` (read it first).

**Branch:** `feature/transaction-extraction` carries the spec + this plan. **Step 1 must branch off `main` AFTER #75 (abort-leak) merges** — #75 edits `transaction.rs`, and a file-move-vs-edit conflict is unmanageable. Each later step branches off updated `main`.

---

## The refactor discipline (applies to EVERY task)

This is a PURE refactor: **no behavior change, no API change, no on-disk format change.** Therefore:
- **You do not write new failing tests.** The oracle is the existing suite. The "test" step for every task is: run the FULL suite and confirm it stays green (same pass count, zero failures), prove behavior was preserved.
- **The green gate for every task** (run all four; all must pass before commit):
  - `cargo test` (NOT `--lib` — the integration tests in `tests/` are part of the oracle)
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --check`
  - `cd python && source .venv/bin/activate && maturin develop && python -m pytest -q` (the binding is unaffected, but the engine moved — re-verify)
- **If a step changes a test's observed behavior** (a different error, a different value, a panic) that is a REFACTOR BUG, not a test that needs updating — fix the code. The ONLY legitimate test-code changes are: (a) re-pathing/`use` updates from the file-split, and (b) the fault-flag plumbing moving to `FaultInjector` (Task 2).
- **NO Claude/AI/Anthropic references** in commits/comments. Comments explain WHY; preserve the existing rich invariant comments verbatim when moving code (do not paraphrase or drop them).
- **Per-unit surface check:** after each extraction, grep for direct access to the extracted struct's fields from outside its module; there should be none (access goes through the interface).

---

### Task 1: File-split into `src/transaction/`

Pure code movement. The `TransactionManager` struct, its fields, and every method body are **unchanged** — they just move into concern files. This is the foundation that makes every later diff small.

**Files:**
- Create: `src/transaction/mod.rs` (the struct, `Roots`, `Savepoint`, the free fns `cow_alloc`/`structural_extend`, module `use`s, and `pub(crate) mod` declarations for the submodules)
- Create: `src/transaction/recovery.rs`, `lifecycle.rs`, `commit.rs`, `staging.rs`, `freemap.rs`, `packing.rs`, `savepoints.rs`, `named_roots.rs`, `read.rs`, `mutate.rs`, `config.rs`, `stats.rs`, `tests.rs`
- Delete: `src/transaction.rs` (its content is distributed)
- Modify: `src/lib.rs` (the `mod transaction;` line is unchanged — a directory module resolves the same)

- [ ] **Step 1: Create the module skeleton.** Make `src/transaction/` and move the WHOLE current `src/transaction.rs` into `src/transaction/mod.rs` unchanged (so it compiles identically). Add nothing yet. Run `cargo build` — must compile. This isolates the directory-rename from the content-move.

- [ ] **Step 2: Move impl blocks into concern files, one file at a time.** For each concern file, cut the relevant `impl TransactionManager { ... }` methods (and any concern-private free fns) out of `mod.rs` and into the file as `impl TransactionManager { ... }` (the child module sees the parent's private fields — no `pub` needed). Add `mod <name>;` to `mod.rs`. The method→file mapping (by the method names in `mod.rs`):
  - `recovery.rs`: `create_new`, `open_existing`
  - `lifecycle.rs`: `begin`, `begin_inner`, `commit`, `commit_inner`, `rollback`, `rollback_inner`, `check_alive`, `poison_on_fatal`, `is_poisoned`, `force_poison_for_test`, `is_active`
  - `commit.rs`: (leave `commit_inner` in `lifecycle.rs` for now — Task 5 extracts it here)
  - `staging.rs`: `allocate`, `allocate_tagged`, `allocate_inner`, `membership_insert_candidate`, `handle_table_insert_candidate`, `abort_allocate_prepare`, `membership_remove_candidate`, `inject_membership_failure`
  - `freemap.rs`: `take_freemap_tree`, `put_freemap_tree`, `allocate_data_page`, `ht_insert`, `freemap_mark_free_committed_path`, `persist_freemap`, `reclaim_freemap_orphans`, `cache_watermark`, and the free fns `cow_alloc` + `structural_extend` (move them here; they are `fn` not methods — keep them module-private, `pub(super)` if `mod.rs` or another file needs them — `ht_insert` uses `cow_alloc`)
  - `packing.rs`: `release_data_slot`, `ensure_handle_table`, `insert_into_data_page`
  - `savepoints.rs`: `savepoint`, `savepoint_inner`, `rollback_to`, `rollback_to_inner`, `release`, `release_inner`
  - `named_roots.rs`: `encode_root_name`, `set_root_name`, `set_root_name_inner`, `get_root_name`, `get_root_name_inner`, `clear_root_name`, `clear_root_name_inner`
  - `read.rs`: `read`, `read_inner`, `tag`, `tag_inner`, `lookup_live`, `live_handle_table_root`, `client_byte`, `client_byte_inner`, `set_client_byte`, `set_client_byte_inner`, `handles_with_tag`, `handles_with_tag_inner`, `handles`, `handles_inner`
  - `mutate.rs`: `update`, `update_inner`, `delete`, `delete_inner`, `delete_tagged`, `delete_tagged_inner`, `delete_with_tag`, `delete_with_tag_inner`, `delete_many`, `delete_many_inner`
  - `config.rs`: `set_cache_max_bytes`, `set_spillway_max_bytes`, `set_drain_insertion`
  - `stats.rs`: `counters`, `spillway_capacity`, `file_page_count`, `sparse_data_pages`, `sparse_data_pages_inner`, `data_page_ids_snapshot`, `handle_live_page_id`, `handle_live_page_id_inner`, `current_handle_table_root_page`, `test_forge_freemap_orphan`, `test_forge_corrupt_dead_page`
  - `tests.rs`: the entire `#[cfg(test)] mod tests { ... }` block. Becomes `#[cfg(test)] mod tests;` in `mod.rs` and the file content is `use super::*;` + the test bodies. (If a single tests.rs is unwieldy, splitting per concern is allowed but optional — do it only if it falls out cleanly.)
  - After each file move: `cargo build`. Fix only `use`/visibility fallout (e.g. a concern-private helper another file now needs becomes `pub(super)`).

- [ ] **Step 3: Resolve `use` and visibility.** `mod.rs` keeps the top-level `use` imports needed broadly; each concern file adds `use super::*;` (or specific imports). The free fns `cow_alloc`/`structural_extend` and any helper a sibling module calls become `pub(super)` (crate-internal, module-scoped). Run `cargo build` clean.

- [ ] **Step 4: Green gate.** Run all four checks (see discipline). The test count must equal the pre-split count exactly. `git diff main --stat` should show only moves (line counts roughly conserved) — no logic change.

- [ ] **Step 5: Commit.** `refactor: split transaction.rs into a transaction/ module by concern`

> **PR boundary:** Task 1 + Task 2 ship as **PR 1** (both low-risk). The remaining tasks are one PR each.

---

### Task 2: Extract `FaultInjector` (`#[cfg(test)]`)

Consolidate the four test-only `Cell` flags into one test-only struct, off the production type.

**Files:**
- Create: `src/transaction/fault.rs`
- Modify: `src/transaction/mod.rs` (the struct's four `#[cfg(test)]` fields → one), `src/transaction/staging.rs` + `mutate.rs` (the `inject_*` consult sites), `src/transaction/tests.rs` (tests that arm the flags)

- [ ] **Step 1: Define `FaultInjector`** in `fault.rs`:

```rust
//! Test-only fault injection consolidated off the production TransactionManager
//! (review 2026-06-22 SMELL #4). Each Cell arms a one-shot or countdown failure
//! at a precise commit-protocol divergence window; see the BUG#2 staging tests.
use std::cell::Cell;

#[cfg(test)]
#[derive(Default)]
pub(super) struct FaultInjector {
    pub fail_next_membership_op: Cell<bool>,
    pub fail_next_handle_table_op: Cell<bool>,
    pub fail_next_update_value_write: Cell<bool>,
    pub fail_membership_op_after: Cell<u32>,
}
```

- [ ] **Step 2: Replace the four struct fields** in `mod.rs` with one (keep the existing field doc comments, relocated to `fault.rs` or condensed):

```rust
    #[cfg(test)]
    fault: fault::FaultInjector,
```
and `#[cfg(test)] fault: fault::FaultInjector::default(),` in BOTH constructors (`create_new`, `open_existing`).

- [ ] **Step 3: Rewire the consult/arm sites.** Every `self.fail_next_membership_op` → `self.fault.fail_next_membership_op`, etc. (production consult sites are already behind `#[cfg(test)]`; the existing `inject_membership_failure` helper and `allocate_inner`'s `#[cfg(test)]` branches). In `tests.rs`, every `tm.fail_next_membership_op.set(true)` → `tm.fault.fail_next_membership_op.set(true)`. (grep `fail_next_|fail_membership_op_after` to find all sites.)

- [ ] **Step 4: Green gate** (all four). Crucially confirm `cargo build --release` compiles (the cfg(test) field is fully absent in release).

- [ ] **Step 5: Commit.** `refactor: consolidate test fault flags into a cfg(test) FaultInjector`

---

### Task 3: Extract `SlotPacker`

Owned state struct for the R1 slot-packing cluster.

**Files:**
- Modify: `src/transaction/packing.rs` (define `SlotPacker` + move the packing logic), `src/transaction/mod.rs` (replace 3 fields with `packer: SlotPacker`), `src/transaction/lifecycle.rs` (begin/commit/rollback delegate), `src/transaction/savepoints.rs` (snapshot/restore through the interface), `src/transaction/staging.rs` + `mutate.rs` (call sites)

- [ ] **Step 1: Define `SlotPacker`** in `packing.rs`, owning the three fields and the packing logic:

```rust
pub(super) struct SlotPacker {
    committed_live_slots: FxHashMap<u64, u32>,
    current_live_slots: FxHashMap<u64, u32>,
    insert_cursor: Option<u64>,
}

impl SlotPacker {
    pub(super) fn new() -> Self { /* empty maps, cursor None */ }
    // R1 packing: append `value` to the cursor page (or a fresh page from
    // `alloc`), updating live-slot counts. Returns (page_id, slot). Moves the
    // body of TransactionManager::insert_into_data_page here verbatim, with
    // field accesses self.* -> the struct's fields and the data-page allocation
    // delegated to the `alloc` closure (formerly self.allocate_data_page()).
    pub(super) fn insert(
        &mut self,
        cache: &mut PageCache,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        value: &[u8],
    ) -> Result<(u64, u16)>;
    // Decrement the live-slot count for a released slot (the body of
    // release_data_slot). Drops cursor bookkeeping consistently.
    pub(super) fn release(&mut self, page_id: u64);
    // Lifecycle, matching the current begin/commit/rollback handling of
    // current_live_slots + insert_cursor:
    pub(super) fn begin(&mut self);     // current = committed.clone(); cursor = None
    pub(super) fn commit(&mut self);    // committed = current.clone() (or take)
    pub(super) fn rollback(&mut self);  // current = committed.clone(); cursor = None
    // Savepoint snapshot/restore (Savepoint already captures live_slots + cursor):
    pub(super) fn snapshot(&self) -> (FxHashMap<u64, u32>, Option<u64>);
    pub(super) fn restore(&mut self, snap: (FxHashMap<u64, u32>, Option<u64>));
    // Read accessors the stats/introspection paths need (current counts):
    pub(super) fn current_live_slots(&self) -> &FxHashMap<u64, u32>;
    pub(super) fn is_current_empty(&self) -> bool;
}
```

- [ ] **Step 2: Replace the three fields** in `mod.rs` with `packer: SlotPacker`, init `SlotPacker::new()` in both constructors.

- [ ] **Step 3: Move `insert_into_data_page` and `release_data_slot` bodies into `SlotPacker::insert`/`release`.** Where `insert_into_data_page` called `self.allocate_data_page()`, the caller now passes that as the `alloc` closure: in the staging/mutate call sites, `self.packer.insert(&mut cache, &mut |c| self.allocate_data_page_for(c), value)` — but `self.allocate_data_page` borrows `self.freemap`-state, disjoint from `self.packer`, so build the closure with the freemap fields borrowed as locals (the existing disjoint-borrow pattern; see `ht_insert`). The `ensure_handle_table` method stays on the manager (it touches the handle table + roots, not the packer).

- [ ] **Step 4: Delegate lifecycle + savepoints.** In `lifecycle.rs`, `begin_inner`/`commit_inner`/`rollback_inner`'s handling of `current_live_slots`/`committed_live_slots`/`insert_cursor` becomes `self.packer.begin()/commit()/rollback()`. In `savepoints.rs`, where `Savepoint` is built/restored, use `self.packer.snapshot()` / `self.packer.restore(...)` instead of cloning the fields directly. The `Savepoint` struct keeps its `live_slots`/`insert_cursor` fields (they hold the snapshot tuple's parts).

- [ ] **Step 5: Rewire stats/introspection** reads of `current_live_slots` to `self.packer.current_live_slots()` and the `tm.current_live_slots.is_empty()` test assertions to `tm.packer.is_current_empty()` (or a test accessor). grep `current_live_slots|committed_live_slots|insert_cursor` to find all sites.

- [ ] **Step 6: Green gate** (all four) + surface check (no external access to `SlotPacker`'s private fields).

- [ ] **Step 7: Commit.** `refactor: extract SlotPacker (R1 live-slot packing) as an owned unit`

---

### Task 4: Extract `FreemapRecycle` (the hardest)

Owned state struct for the structural-recycle cluster + the freemap commit/alloc paths.

**Files:**
- Modify: `src/transaction/freemap.rs` (define `FreemapRecycle` + move the freemap methods), `src/transaction/mod.rs` (replace 5 fields), `src/transaction/lifecycle.rs` (recycle lifecycle), `src/transaction/savepoints.rs` (rollback interaction), call sites in `staging.rs`/`mutate.rs`/`stats.rs`

- [ ] **Step 1: Define `FreemapRecycle`** in `freemap.rs`, owning the five fields:

```rust
pub(super) struct FreemapRecycle {
    hint: u64,
    structural_reuse: Vec<u64>,
    structural_superseded: Vec<u64>,
    pending_structural_frees: Vec<u64>,
    session_owned: FxHashSet<u64>,
}
```

- [ ] **Step 2: Move the freemap machinery into `impl FreemapRecycle`.** Move the bodies of `take_freemap_tree`, `put_freemap_tree`, `structural_extend` (the free fn), and `cow_alloc`'s freemap portion into methods on `FreemapRecycle` that take `&mut PageCache` and the roots' `{freemap_page, freemap_depth}` (mutating them via out-params or by taking `&mut Roots`). Interface:

```rust
impl FreemapRecycle {
    pub(super) fn new() -> Self;
    // Allocate a page id for COW work: reuse a free bit (drawing structural COW
    // targets from the recycle pool, never the bitmap) or extend. Updates the
    // roots' freemap_page/depth and the hint; accumulates supersedes internally.
    // (Body = today's cow_alloc + take/put_freemap_tree dance, now self-contained.)
    pub(super) fn allocate(&mut self, cache: &mut PageCache, roots: &mut Roots, reuse_enabled: bool) -> Result<u64>;
    // Mark a page free through the committed-path COW (body of
    // freemap_mark_free_committed_path).
    pub(super) fn mark_free_committed_path(&mut self, cache: &mut PageCache, roots: &mut Roots, id: u64) -> Result<()>;
    // Commit-time persist (body of persist_freemap): apply txn_freed_pages, COW
    // the touched leaves+spine via the pool, promote structural_superseded.
    pub(super) fn persist(&mut self, cache: &mut PageCache, roots: &mut Roots, txn_freed_pages: &[u64]) -> Result<()>;
    // Defrag orphan sweep (body of reclaim_freemap_orphans), with the
    // savepoints-active guard passed in.
    pub(super) fn reclaim_orphans(&mut self, cache: &mut PageCache, roots: &Roots, savepoint_active: bool, superblock_count: u32) -> Result<u64>;
    // Lifecycle:
    pub(super) fn begin(&mut self);     // structural_reuse = pending_structural_frees.clone(); session_owned.clear()
    pub(super) fn commit(&mut self);    // promote structural_superseded + leftover reuse -> pending_structural_frees
    pub(super) fn rollback(&mut self);  // structural_reuse back to pending; superseded.clear(); session_owned.clear()
    // The reuse pool, for the orphan-sweep exclusion set (read-only):
    pub(super) fn pool_ids(&self) -> impl Iterator<Item = u64> + '_;
}
```

- [ ] **Step 3: Replace the five fields** in `mod.rs` with `freemap: FreemapRecycle`, init `FreemapRecycle::new()` in both constructors. The `cow_alloc` free fn either becomes a `FreemapRecycle` method or a thin wrapper delegating to `self.freemap.allocate(...)`.

- [ ] **Step 4: Rewire the THREE allocation call sites** (`allocate_data_page`/`ht_insert`'s closure, the membership insert/remove sites in `staging.rs`). Each currently captures `let hint = &mut self.freemap_hint; let pool = &mut self.structural_reuse;` etc. — now they borrow `&mut self.freemap` (one field, disjoint from `self.handle_table`/`self.membership_index`/`self.cache`). Confirm the disjoint-borrow still satisfies the checker (it should — one field vs the structure handles). Update `allocate_data_page`, `ht_insert`, and the membership candidate/remove methods.

- [ ] **Step 5: Delegate lifecycle + rollback interaction.** `begin_inner`/`commit_inner`/`rollback_inner` call `self.freemap.begin()/commit()/rollback()` instead of the inline stream handling. The `reclaim_freemap_orphans` exclusion set uses `self.freemap.pool_ids()`. `persist_freemap` call in commit becomes `self.freemap.persist(&mut cache, &mut self.current_roots, &self.txn_freed_pages)`.

- [ ] **Step 6: Green gate** (all four — the recycle pins `structural_recycle_one_commit_defer`/`..rollback_resets_pools`/`..no_lost_or_double_free` and the orphan-sweep/savepoint tests are the load-bearing oracle here; they MUST stay green) + surface check.

- [ ] **Step 7: Commit.** `refactor: extract FreemapRecycle (structural recycle + persist/reclaim) as an owned unit`

---

### Task 5: Extract `CommitProtocol`

The `commit_inner` sequence into a behavior unit (function-module over a context; promote to an owned struct only if it reads cleanly).

**Files:**
- Modify: `src/transaction/commit.rs` (the CommitProtocol unit), `src/transaction/lifecycle.rs` (`commit_inner` delegates)

- [ ] **Step 1: Move the `commit_inner` body into `commit.rs`** as a function (or `CommitProtocol::run`) taking the context it needs:

```rust
// The 3-fsync commit sequence (I28 pre-drain flush -> FreemapRecycle::persist ->
// data fsync -> superblock build/write/fsync -> roots promotion). The
// data-fsync-before-superblock-fsync ordering and I18 allocate-before-merge are
// preserved verbatim. Operates over the manager's parts; owns no state.
pub(super) fn run_commit(
    cache: &RefCell<PageCache>,
    committed_roots: &mut Roots,
    current_roots: &mut Roots,
    freemap: &mut FreemapRecycle,
    packer: &mut SlotPacker,
    txn_freed_pages: &mut Vec<u64>,
    txn_counter: &mut u64,
    superblock_count: u32,
) -> Result<()>;
```
(Exact parameter set = whatever `commit_inner` touches; thread each as `&mut`/`&` rather than `self`. If the param list is unwieldy, a small `CommitCtx<'a>` struct bundling the `&mut`s is allowed.)

- [ ] **Step 2: `commit_inner` (lifecycle.rs) becomes a thin caller** of `run_commit(...)`, passing its fields. The poison-on-fatal wrapper (`commit` → `poison_on_fatal(commit_inner())`) stays on the manager.

- [ ] **Step 3: Green gate** (all four — the fsync-count integration test `tests/spillway_integration.rs` asserting `fsync_delta == 3` and the `persist_freemap_does_not_reuse_committed_live_pages` I18 guardrail are the load-bearing oracle).

- [ ] **Step 4: Commit.** `refactor: extract the commit protocol (3-fsync sequence) into commit.rs`

---

### Task 6: Extract `StagingTxn`

The BUG#2 atomic staging of `allocate_inner` into a behavior unit. Last (touches the most units).

**Files:**
- Modify: `src/transaction/staging.rs` (the StagingTxn unit), and the `allocate_inner` caller

- [ ] **Step 1: Move the staging into `staging.rs`** as a function/unit taking the context: the cache, both roots, `&mut HandleTable`, `&mut MembershipIndex`, `&mut FreemapRecycle`, `&mut SlotPacker`, `&mut txn_freed_pages`, and (test-only) `&FaultInjector`. Move the bodies of `handle_table_insert_candidate`, `membership_insert_candidate`, `membership_remove_candidate`, `abort_allocate_prepare`, and the PREPARE/INSTALL flow of `allocate_inner` here. Preserve verbatim: the compute-without-install discipline, the local `ht_freed`/`mi_freed` lists appended to `txn_freed_pages` only in the INSTALL phase, the bounded-residue-on-abort contract (documented in PR #75), and the `#[cfg(test)]` fault hooks (now via `&FaultInjector`).

```rust
pub(super) fn run_allocate(
    ctx: &mut StagingCtx<'_>,   // bundles cache, roots, handle_table, membership_index, freemap, packer, txn_freed_pages
    value: &[u8],
    tag: u32,
    #[cfg(test)] fault: &FaultInjector,
) -> Result<u64>;
```

- [ ] **Step 2: `allocate_inner` becomes a thin caller** assembling the context and delegating. `allocate`/`allocate_tagged` (the public wrappers with `check_alive` + `poison_on_fatal`) stay on the manager.

- [ ] **Step 3: Green gate** (all four — the staging oracle: `allocate_membership_failure_leaves_maps_consistent`, `allocate_handle_table_failure_leaves_maps_consistent`, `aborted_tagged_allocate_with_freemap_reuse_is_consistent_and_rollback_reclaims`, and the BUG#2 atomic-staging tests MUST stay green) + surface check.

- [ ] **Step 4: Commit.** `refactor: extract StagingTxn (BUG#2 atomic prepare/install) into staging.rs`

- [ ] **Step 5: Final whole-refactor review.** After all six, dispatch an adversarial review of `git diff <pre-refactor-main>..HEAD` confirming: zero behavior change (the suite is the proof), each unit's surface is narrow, the durability invariant comments survived the moves, and no field is `pub` that wasn't. Then `transaction/mod.rs` should be a slim orchestrator + struct, and the largest concern file should be a fraction of the original 5,377.

---

## Self-Review (against the spec)

**Spec coverage:**
- Module split by concern → Task 1. ✅
- State sub-structs own only their cluster + closures → Tasks 3 (SlotPacker), 4 (FreemapRecycle). ✅
- Behavior units over a context + function-module fallback → Tasks 5 (CommitProtocol), 6 (StagingTxn); the `CommitCtx`/`StagingCtx` bundle is the function-module form. ✅
- `#[cfg(test)]` FaultInjector (not trait object) → Task 2. ✅
- Savepoint snapshot/restore through the interface → Task 3 Step 4. ✅
- Low-risk-first order, each green → Tasks 1-6 in order; the green gate in every task. ✅
- Reads/mutations/named-roots/stats/config move to files but stay `impl TransactionManager` (no sub-struct) → Task 1's mapping; not extracted in 3-6. ✅
- #75-merge dependency → header + branch note. ✅
- Pure behavior-preserving (no new test behavior) → the refactor discipline section. ✅

**Placeholder scan:** the interface signatures are marked "illustrative"/"exact = whatever the method touches" where the precise param set is a mechanical read of the existing body — that is concrete guidance for a refactor (the bodies already exist), not a placeholder. No `TODO`/`TBD`.

**Type consistency:** `SlotPacker` (`insert`/`release`/`begin`/`commit`/`rollback`/`snapshot`/`restore`); `FreemapRecycle` (`allocate`/`mark_free_committed_path`/`persist`/`reclaim_orphans`/`begin`/`commit`/`rollback`/`pool_ids`); `FaultInjector` (the four `Cell`s); `run_commit`/`CommitCtx`; `run_allocate`/`StagingCtx`. Consistent across tasks.

> **Open risk (carried from the spec):** if an owned-struct extraction (esp. FreemapRecycle, Task 4) hits a borrow wall, fall back to the function-module form (decision 4) in the same file with the same narrow interface — do not force ownership. Note the choice in that task's commit message.
