# Stable Chunk Iteration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Promote the already-true within-session repeatability of `Chisel::handles()` and `Chisel::handles_with_tag()` into a documented, tested contract — without changing any production logic.

**Architecture:** Contract-hardening. The radix walks backing both APIs are already deterministic functions of tree structure, so the guarantee already holds. The work is (1) adversarial *differential* tests that prove it, then (2) documentation that promises it. Tests come first: never document a guarantee that isn't tested.

**Tech Stack:** Rust; `cargo test`; Chisel's `tests/common` dual-backing harness (`dual_backing_test!`, `pastey`); in-memory + file backends.

**Spec:** `docs/specs/2026-06-04-stable-chunk-iteration-design.md`

---

## ⚠️ Inverted TDD — read before starting

This change adds **no production code**. The tests are *characterization tests* over existing behavior, so:

- **There is no red phase.** Every test must **PASS on first run**.
- **A failing test is a real finding, not a step.** It means the engine does not actually provide the within-session repeatability the spec claims. If that happens: **STOP**, do not write any documentation, and investigate the violation (it would be a genuine bug in the iteration or rollback/depth-recovery path).
- Therefore tests (Tasks 1–4) land and are proven green **before** the documentation (Tasks 5–6) is written.

## ⚠️ Commit Policy — commits are DEFERRED

The repo owner's standing rule: **commit/push only when explicitly asked.** Every "Stage commit" step below is **DEFERRED** — do the work and run verification, but do **not** run `git commit` until the owner authorizes it. Prepared Conventional-Commit messages are given so they're ready when authorized. The two logical commits are:

- **Commit A** (Tasks 1–4): `test(iteration): within-session stability tests for handles()/handles_with_tag()`
- **Commit B** (Tasks 5–6): `docs(iteration): document within-session iteration-stability contract`

Landing convention (feature branch + PR vs. direct to `main`) is the owner's call, to be confirmed at authorization time. Chisel history shows feature-branch + PR (e.g. `feature/chunk-tags`, PR #31).

---

## File Structure

- **Create** `tests/iteration_stability.rs` — the entire test suite (one focused integration-test binary; mirrors `tests/tag_ops.rs` / `tests/transactions.rs`). All 7 tests live here because they share one concern: the iteration contract.
- **Modify** `src/lib.rs` — doc comments on `handles` (≈556–562) and `handles_with_tag` (≈461–466). No code change.
- **Modify** `src/membership_index.rs` — doc comment on `RadixU64::iter` (≈254–256). No code change.
- **Modify** `README.md` — one example annotation (≈193) and two API-table rows (≈240, 246).
- **Modify** `ARCHITECTURE.md` — append a paragraph to the "Handle stability" section (≈483).

---

## Task 1: Test scaffold + back-to-back stability

**Files:**
- Create: `tests/iteration_stability.rs`

- [ ] **Step 1: Create the file with the module header, imports, and the two back-to-back tests**

```rust
//! Within-session iteration-stability contract for `handles()` and
//! `handles_with_tag()` (see docs/specs/2026-06-04-stable-chunk-iteration-design.md).
//!
//! These are *differential* tests: a "repeatable, order-unspecified" guarantee
//! cannot be checked from a single scan, so each test scans, churns state the
//! contract permits to change (reads, an `update`, cache eviction, a rolled-back
//! transaction/savepoint), scans again, and asserts the two scans are
//! byte-identical. They assert repeatability + contents, NEVER a specific order
//! — asserting order would silently strengthen the contract beyond what the spec
//! promises.
//!
//! Deliberately NOT tested: order across close+reopen or across defrag/compact.
//! The single-session scope makes those non-guarantees; a test there would lock
//! behavior the engine is explicitly free to change.

mod common;

use chisel::Options;
use common::{open_chisel, open_chisel_with, Backing};

fn back_to_back_handles_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let mut expected = vec![
        db.allocate(b"untagged-a").unwrap(),
        db.allocate_tagged(b"t42-a", 42).unwrap(),
        db.allocate(b"untagged-b").unwrap(),
        db.allocate_tagged(b"t7-a", 7).unwrap(),
        db.allocate_tagged(b"t42-b", 42).unwrap(),
    ];
    db.commit().unwrap();

    // The contract: two scans with no mutation between them are byte-identical.
    let first = db.handles().unwrap();
    let second = db.handles().unwrap();
    assert_eq!(first, second, "repeated handles() must be identical");

    // Sanity that the scan covers the real data — order-normalized, so this
    // never asserts a particular order.
    let mut got = first.clone();
    got.sort_unstable();
    expected.sort_unstable();
    assert_eq!(got, expected, "handles() must return exactly the live set");

    db.close().unwrap();
}
dual_backing_test!(back_to_back_handles, back_to_back_handles_body);

fn back_to_back_handles_with_tag_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let t42_a = db.allocate_tagged(b"t42-a", 42).unwrap();
    let _t7 = db.allocate_tagged(b"t7", 7).unwrap();
    let t42_b = db.allocate_tagged(b"t42-b", 42).unwrap();
    let _untagged = db.allocate(b"untagged").unwrap();
    db.commit().unwrap();

    let first = db.handles_with_tag(42).unwrap();
    let second = db.handles_with_tag(42).unwrap();
    assert_eq!(first, second, "repeated handles_with_tag() must be identical");

    let mut got = first.clone();
    got.sort_unstable();
    let mut expected = vec![t42_a, t42_b];
    expected.sort_unstable();
    assert_eq!(got, expected, "handles_with_tag(42) must return exactly tag 42's members");

    db.close().unwrap();
}
dual_backing_test!(back_to_back_handles_with_tag, back_to_back_handles_with_tag_body);
```

- [ ] **Step 2: Run the new tests — expect PASS (no red phase)**

Run: `cargo test --test iteration_stability`
Expected: 4 tests pass — `back_to_back_handles_file`, `back_to_back_handles_memory`, `back_to_back_handles_with_tag_file`, `back_to_back_handles_with_tag_memory`.
If any FAIL: STOP — the repeatability claim is false; investigate before continuing.

- [ ] **Step 3: Stage commit (DEFERRED — do not run until authorized)**

Part of **Commit A**. Do not `git commit` yet.

---

## Task 2: Interleaved reads and updates do not perturb a scan

**Files:**
- Modify: `tests/iteration_stability.rs` (append two test bodies + macro invocations)

- [ ] **Step 1: Append the interleaved-reads and interleaved-update tests**

```rust
fn reads_between_scans_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handles: Vec<u64> = (0..16)
        .map(|i| db.allocate(format!("row-{i}").as_bytes()).unwrap())
        .collect();
    db.commit().unwrap();

    let first = db.handles().unwrap();

    // Reads take &self and must not perturb a later scan — including a read of
    // the most recently allocated handle.
    for &h in &handles {
        let _ = db.read(h).unwrap();
    }
    let _ = db.read(*handles.last().unwrap()).unwrap();

    let second = db.handles().unwrap();
    assert_eq!(first, second, "reads must not perturb handles() order or contents");

    db.close().unwrap();
}
dual_backing_test!(reads_between_scans, reads_between_scans_body);

fn update_between_scans_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h_small = db.allocate(b"small").unwrap();
    let h_tagged = db.allocate_tagged(b"tagged-small", 42).unwrap();
    let h_other = db.allocate(b"other").unwrap();
    db.commit().unwrap();

    let first_all = db.handles().unwrap();
    let first_t42 = db.handles_with_tag(42).unwrap();

    // `update` preserves the handle (and the immutable tag), so the live set —
    // and each handle's radix position — is unchanged. Grow one value past
    // MAX_INLINE_VALUE (8162 bytes) to force relocation to an overflow chain,
    // the most disruptive update path.
    db.begin().unwrap();
    db.update(h_small, &[0xABu8; 9000]).unwrap();
    db.update(h_tagged, &[0xCDu8; 9000]).unwrap();
    db.update(h_other, b"still-small").unwrap();
    db.commit().unwrap();

    let second_all = db.handles().unwrap();
    let second_t42 = db.handles_with_tag(42).unwrap();
    assert_eq!(first_all, second_all, "update must not perturb handles()");
    assert_eq!(first_t42, second_t42, "update must not perturb handles_with_tag()");

    db.close().unwrap();
}
dual_backing_test!(update_between_scans, update_between_scans_body);
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test --test iteration_stability`
Expected: 8 tests pass (the 4 from Task 1 plus `reads_between_scans_{file,memory}` and `update_between_scans_{file,memory}`).
If FAIL: STOP and investigate (a perturbation by a read or an update would be a real bug).

- [ ] **Step 3: Stage commit (DEFERRED)** — part of **Commit A**.

---

## Task 3: Cache eviction does not perturb a scan

**Files:**
- Modify: `tests/iteration_stability.rs` (append one test body + macro invocation)

- [ ] **Step 1: Append the cache-eviction test**

```rust
fn cache_eviction_between_scans_body(b: &Backing) {
    // A small cache with the spillway LEFT ENABLED (so the bulk insert
    // succeeds). The point: force the handle-table page(s) out of the LRU
    // between the two scans and prove the reload reproduces the scan exactly —
    // i.e. the order is structural, not cache-residency-dependent.
    const N: usize = 300;
    let opts = Options::default().cache_max_bytes(32 * 8192); // 32 pages
    let mut db = open_chisel_with(b, opts);

    db.begin().unwrap();
    // ~4 KiB values → ~two per data page → ~150 data pages, far exceeding the
    // 32-page cache, so reading them all evicts the handle-table page(s).
    for i in 0..N {
        let mut v = vec![0u8; 4000];
        v[0] = i as u8;
        db.allocate(&v).unwrap();
    }
    db.commit().unwrap();

    let first = db.handles().unwrap();
    assert_eq!(first.len(), N, "baseline scan should see every chunk");

    // Churn the cache: reading every value loads ~150 distinct data pages
    // through a 32-page cache, evicting the handle-table page(s).
    for &h in &first {
        let _ = db.read(h).unwrap();
    }

    let second = db.handles().unwrap();
    assert_eq!(first, second, "cache eviction must not perturb handles()");

    db.close().unwrap();
}
dual_backing_test!(cache_eviction_between_scans, cache_eviction_between_scans_body);
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test --test iteration_stability`
Expected: 10 tests pass (adds `cache_eviction_between_scans_{file,memory}`).
If FAIL: STOP — an eviction-order dependence would be a real bug in the page cache / walk.

- [ ] **Step 3: Stage commit (DEFERRED)** — part of **Commit A**.

---

## Task 4: Rolled-back transaction and savepoint do not perturb a scan

**Files:**
- Modify: `tests/iteration_stability.rs` (append two test bodies + macro invocations)

- [ ] **Step 1: Append the rolled-back-transaction and savepoint-rollback tests**

```rust
// Allocating this many same-tag chunks inside a transaction grows BOTH radix
// trees past a level: > 510 entries grows the handle-table leaf, and > 1021
// same-tag members grows that tag's inner membership tree. Rolling back must
// then restore the roots AND re-derive tree depth (the I99/C1 invariant), or a
// later scan would mis-descend and mis-enumerate.
const GROW: usize = 1100;

fn rolled_back_transaction_body(b: &Backing) {
    const TAG: u32 = 99;
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let mut baseline_tag = vec![
        db.allocate_tagged(b"base-1", TAG).unwrap(),
        db.allocate_tagged(b"base-2", TAG).unwrap(),
        db.allocate_tagged(b"base-3", TAG).unwrap(),
    ];
    let _base_untagged = db.allocate(b"base-untagged").unwrap();
    db.commit().unwrap();

    let baseline_all = db.handles().unwrap();
    let baseline_t = db.handles_with_tag(TAG).unwrap();

    db.begin().unwrap();
    for i in 0..GROW {
        db.allocate_tagged(format!("ephemeral-{i}").as_bytes(), TAG).unwrap();
    }
    db.rollback().unwrap();

    // After rollback both scans must reproduce the committed baseline exactly.
    assert_eq!(db.handles().unwrap(), baseline_all, "rollback must restore handles()");
    assert_eq!(
        db.handles_with_tag(TAG).unwrap(),
        baseline_t,
        "rollback must restore handles_with_tag()"
    );

    // Order-normalized: the tag retains exactly its three committed members.
    let mut got = db.handles_with_tag(TAG).unwrap();
    got.sort_unstable();
    baseline_tag.sort_unstable();
    assert_eq!(got, baseline_tag);

    db.close().unwrap();
}
dual_backing_test!(rolled_back_transaction, rolled_back_transaction_body);

fn savepoint_rollback_body(b: &Backing) {
    const TAG: u32 = 123;
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let mut baseline_tag = vec![
        db.allocate_tagged(b"keep-1", TAG).unwrap(),
        db.allocate_tagged(b"keep-2", TAG).unwrap(),
    ];
    db.commit().unwrap();

    let baseline_all = db.handles().unwrap();
    let baseline_t = db.handles_with_tag(TAG).unwrap();

    // Grow both trees inside a savepoint, then roll the savepoint back. Like a
    // full rollback, rollback_to must re-derive depth (I99/C1).
    db.begin().unwrap();
    db.savepoint("grow").unwrap();
    for i in 0..GROW {
        db.allocate_tagged(format!("ephemeral-{i}").as_bytes(), TAG).unwrap();
    }
    db.rollback_to("grow").unwrap();
    db.commit().unwrap();

    assert_eq!(db.handles().unwrap(), baseline_all, "savepoint rollback must restore handles()");
    assert_eq!(
        db.handles_with_tag(TAG).unwrap(),
        baseline_t,
        "savepoint rollback must restore handles_with_tag()"
    );

    let mut got = db.handles_with_tag(TAG).unwrap();
    got.sort_unstable();
    baseline_tag.sort_unstable();
    assert_eq!(got, baseline_tag);

    db.close().unwrap();
}
dual_backing_test!(savepoint_rollback, savepoint_rollback_body);
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test --test iteration_stability`
Expected: 14 tests pass (adds `rolled_back_transaction_{file,memory}` and `savepoint_rollback_{file,memory}`).
If FAIL: STOP — a post-rollback scan mismatch is a real I99/C1 depth-recovery regression, not a test bug.

- [ ] **Step 3: Stage commit (DEFERRED)** — completes **Commit A** (`test(iteration): within-session stability tests for handles()/handles_with_tag()`).

---

## Task 5: Document the contract on the API doc comments

**Files:**
- Modify: `src/lib.rs` (doc comments only — no code change)
- Modify: `src/membership_index.rs` (doc comment only — no code change)

- [ ] **Step 1: Replace the `handles` doc comment in `src/lib.rs`**

Find (≈556–559):

```rust
    /// Enumerate all live handles. Walks the handle-table radix tree; cost
    /// is proportional to the number of live handles, not to the historical
    /// maximum. Order is unspecified and callers must not depend on it.
    /// Takes `&self` for the same reason `read` does (F3).
```

Replace with:

```rust
    /// Enumerate all live handles. Walks the handle-table radix tree; cost
    /// is proportional to the number of live handles, not to the historical
    /// maximum. Takes `&self` for the same reason `read` does (F3).
    ///
    /// Stability: within a single open instance, repeated calls return an
    /// identical `Vec` — the same handles in the same order — as long as the
    /// live set is unchanged between calls (changed only by `allocate*` /
    /// `delete*`; `read` and `update` do not change it) and no `defrag` has run.
    /// The order itself is unspecified: it is not sorted, not insertion order,
    /// and may differ after a reopen or `defrag`, or across Chisel versions.
    /// Rely on within-session repeatability; do not rely on the order.
```

- [ ] **Step 2: Replace the `handles_with_tag` doc comment in `src/lib.rs`**

Find (≈461–463):

```rust
    /// Enumerate all live handles that carry `tag`. Returns an empty Vec if
    /// no handles with that tag exist. Tag 0 always returns an empty Vec
    /// (the membership index is not updated for untagged values). Takes `&self` (F3).
```

Replace with:

```rust
    /// Enumerate all live handles that carry `tag`. Returns an empty Vec if
    /// no handles with that tag exist. Tag 0 always returns an empty Vec
    /// (the membership index is not updated for untagged values). Takes `&self` (F3).
    ///
    /// Stability: the same within-session repeatability contract as `handles` —
    /// repeated calls return an identical `Vec` while the set of live handles
    /// carrying `tag` is unchanged and no `defrag` has run. The order is
    /// unspecified and may differ after a reopen or `defrag`.
```

- [ ] **Step 3: Replace the `RadixU64::iter` doc comment in `src/membership_index.rs`**

Find (≈254–255):

```rust
    /// Enumerate all `(key, value)` pairs with a non-zero value. Order is
    /// unspecified.
```

Replace with:

```rust
    /// Enumerate all `(key, value)` pairs with a non-zero value. The walk is a
    /// deterministic function of the tree's structure: for a fixed tree it
    /// returns the same pairs in the same order on every call, which is what
    /// backs the public within-session iteration-stability contract on
    /// `Chisel::handles_with_tag`. The order itself (currently ascending key) is
    /// unspecified and must not be relied upon.
```

- [ ] **Step 4: Verify the crate still builds and docs render**

Run: `cargo build && cargo doc --no-deps`
Expected: clean build; no rustdoc warnings about the edited items. (`cargo test --test iteration_stability` is unaffected — doc-only change — but re-run it if you want to confirm: still 14 passing.)

- [ ] **Step 5: Stage commit (DEFERRED)** — part of **Commit B**.

---

## Task 6: Reconcile README and ARCHITECTURE

**Files:**
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1: Update the README example annotation (≈193)**

Find:

```
let members = db.handles_with_tag(42)?; // both handles, order unspecified
```

Replace with:

```
let members = db.handles_with_tag(42)?; // both handles; order unspecified, but repeatable within a session
```

- [ ] **Step 2: Update the two README API-table rows (≈240, 246)**

Find:

```
| `handles_with_tag(tag)` | Enumerate live handles carrying `tag` (takes `&self`) |
```

Replace with:

```
| `handles_with_tag(tag)` | Enumerate live handles carrying `tag`; repeatable within a session, order unspecified (takes `&self`) |
```

Find:

```
| `handles()` | Enumerate all live handles (takes `&self`) |
```

Replace with:

```
| `handles()` | Enumerate all live handles; repeatable within a session, order unspecified (takes `&self`) |
```

- [ ] **Step 3: Append the iteration-stability paragraph to ARCHITECTURE.md "Handle stability" (after ≈483)**

Find (the paragraph that ends the "Handle stability" section):

```
The radix-tree indirection means values can move freely on disk — `update()` to a larger value, `defrag()` consolidation, future page-format upgrades — without changing the handle the caller holds.
```

Replace with (same paragraph, then a new one):

```
The radix-tree indirection means values can move freely on disk — `update()` to a larger value, `defrag()` consolidation, future page-format upgrades — without changing the handle the caller holds.

Within-session iteration stability follows from that same handle identity. `handles()` and `handles_with_tag()` walk arithmetic radix trees in a structure-only traversal, so within one open instance repeated scans return an identical `Vec` — same handles, same order — as long as the live set is unchanged and no `defrag` has run. This is a *repeatability* guarantee only: the order itself is unspecified (it is not promised to be sorted, and may differ after a reopen or `defrag`, or across versions), which keeps the index internals free to change. The guarantee is deliberately scoped to a single session and does not survive reopen or `defrag`; it rests on the radix-depth re-derivation invariant (see [In-memory radix depth is re-derived from the root](#in-memory-radix-depth-is-re-derived-from-the-root-never-stored)) — a rolled-back grow must restore depth or a later scan would mis-enumerate.
```

(If the cross-reference anchor does not match, grep `ARCHITECTURE.md` for the heading containing "radix depth" and use its exact slug. A wrong anchor only breaks the link, not the build.)

- [ ] **Step 4: Verify**

Run: `cargo test --test iteration_stability` (unchanged: 14 passing) and visually confirm the README/ARCHITECTURE renders.

- [ ] **Step 5: Stage commit (DEFERRED)** — completes **Commit B** (`docs(iteration): document within-session iteration-stability contract`).

---

## Task 7: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Confirm the exact CI lint/test invocation**

Run: `cat .github/workflows/*.yml | grep -nE "cargo (fmt|clippy|test|doc)"`
Use whatever flags CI uses; the steps below are the standard fallback.

- [ ] **Step 2: Format check**

Run: `cargo fmt --all -- --check`
Expected: no output (clean). If it reports diffs, run `cargo fmt --all` and re-check.

- [ ] **Step 3: Clippy (all targets, warnings as errors)**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean. Watch for `clippy::useless_vec` in the eviction test (the `vec![0u8; 4000]` there is mutated, so it is legitimate) and unused-import warnings in the test file.

- [ ] **Step 4: Full test suite**

Run: `cargo test`
Expected: the whole suite passes, including all 14 `iteration_stability` variants. (`dual_backing_test!` names each `<name>_file` and `<name>_memory`.)

- [ ] **Step 5: Docs**

Run: `cargo doc --no-deps`
Expected: clean.

- [ ] **Step 6: Request commit authorization (DEFERRED commits)**

Do NOT commit unprompted. Report verification results and ask the owner to authorize **Commit A** then **Commit B** (messages above), and confirm the landing convention (feature branch + PR vs. direct to `main`).

---

## Plan self-review

- **Spec coverage:** Guarantee + scope → Tasks 5–6 (docs) and Tasks 1–4 (tests). The 7 spec tests map 1:1 — back-to-back all (T1), back-to-back tagged (T1), interleaved reads (T2), interleaved update (T2), cache eviction (T3), rolled-back transaction (T4), savepoint rollback (T4). "Deliberately not tested cross-reopen/defrag" → module-doc comment in Task 1 Step 1. Doc-comment / README / ARCHITECTURE changes → Tasks 5–6. No spec requirement is unmapped.
- **Spec deviation (improvement):** the spec said "in-memory backend"; the plan uses `dual_backing_test!`, which covers in-memory **and** file — this is how `tag_ops.rs`/`transactions.rs` are written, so it both mirrors them and exceeds the spec. Not a gap.
- **No production code:** confirmed — every `src/` step is a doc-comment replacement.
- **Type/name consistency:** all API names match the verified surface — `open_chisel`/`open_chisel_with`/`Backing`/`dual_backing_test!` (tests/common), `begin`/`commit`/`rollback`/`savepoint`/`rollback_to`, `allocate`/`allocate_tagged`/`read`/`update`/`handles`/`handles_with_tag`, `Options::default().cache_max_bytes(...)`. `read` returns owned `Vec<u8>`; mutations require an active transaction (no autocommit).
- **No placeholders:** every step has complete code or an exact command + expected output.
