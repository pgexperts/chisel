# I112 Fault-Injection Layer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive real I/O syscall failures into a live Chisel engine so the poison/flush coupling and the fatal-`IoError` path are tested, and close the dependent gaps I114 (release-safe I20 invariant) and I115 (pin `CorruptSuperblock`, remove dead `InvalidMagic`).

**Architecture:** A `#[cfg(test)]` `Fault` cell on `PageIo` (mirroring the existing `fail_next_membership_op` pattern) makes `read_page`/`write_page`/`fsync` return `Err(IoError)` on command. Crate-internal unit tests arm faults via `tm.cache.borrow().io().arm_fault(...)` and assert the engine poisons. Byte-corruption tests (existing `recovery_tests.rs` helpers) close I115; a release-compiled accounting test closes I114. No public API is added; the dead `InvalidMagic` variant is removed.

**Tech Stack:** Rust (the `chisel` lib crate), PyO3 binding (`chisel-py`), `std::cell::Cell`, `std::io::Error`.

**Spec:** `docs/specs/2026-06-21-i112-fault-injection-design.md`

**Branch:** `i112-fault-injection` (already created off `main`).

**Standing gates** (run after every task that changes code):
- `cargo test` — 0 failures.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean (the `#[cfg(test)]` hooks compile under `--all-targets`, so they must be clippy-clean).
- `cargo fmt --check` — clean.

> **Note on line numbers:** all `file:line` refs are relative to `main`. Edit by *content* (the quoted code), not by line number — a prior edit in the same file shifts later lines.

---

### Task 1: The `Fault` hook in `PageIo`

The test-only injection machinery. Everything in Tasks 2–4 depends on it.

**Files:**
- Modify: `src/page_io.rs` (struct + 2 constructors + 3 I/O methods + new method + new test)

- [ ] **Step 1: Add the `Fault` enum.** In `src/page_io.rs`, after the existing `enum Backing { … }` block, add:

```rust
/// Test-only fault plan armed via `PageIo::arm_fault` (I112). `Copy` so it
/// lives in a `Cell` (the `io::Error` is synthesized at the failure site, not
/// stored). One fault is armed at a time; faults are one-shot except the fsync
/// countdown, which decrements on each fsync until it fires.
#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Fault {
    #[default]
    None,
    /// Fail an fsync via a countdown: `FailFsync(0)` fails the NEXT fsync;
    /// `FailFsync(n)` lets `n` fsyncs succeed (decrementing each) then fails the
    /// one after. Lets a test target a specific fsync in commit's three-fsync
    /// protocol (pre-drain / data-flush / superblock).
    FailFsync(u32),
    /// Fail `write_page` for exactly this page id (one-shot).
    FailWritePage(u64),
    /// Fail `read_page` for exactly this page id (one-shot).
    FailReadPage(u64),
}
```

- [ ] **Step 2: Add the field to `struct PageIo`.** After the `cached_page_count: Cell<u64>,` field (`page_io.rs:71`), add:

```rust
    // I112: test-only fault injector. Checked at the top of read_page/
    // write_page/fsync; cfg(test) so it is compiled out of production builds
    // entirely (same discipline as transaction.rs::fail_next_membership_op).
    #[cfg(test)]
    fault: Cell<Fault>,
```

- [ ] **Step 3: Initialize the field in both constructors.** In `PageIo::open` (`:107`) and `PageIo::open_in_memory` (`:124`), inside each `PageIo { … }` struct literal, after `cached_page_count: Cell::new(…),`, add:

```rust
            #[cfg(test)]
            fault: Cell::new(Fault::None),
```

- [ ] **Step 4: Add `arm_fault`.** In `impl PageIo`, after `fsync_count` (`:324-326`), add:

```rust
    /// Arm a test fault (I112). The next matching I/O op returns `IoError`.
    #[cfg(test)]
    pub(crate) fn arm_fault(&self, f: Fault) {
        self.fault.set(f);
    }
```

- [ ] **Step 5: Hook `read_page`.** In `read_page`, immediately after the `page_id >= page_count` bounds check (`:215-217`), before `match &mut self.backing`, insert:

```rust
        #[cfg(test)]
        if self.fault.get() == Fault::FailReadPage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected read failure",
            )));
        }
```

- [ ] **Step 6: Hook `write_page`.** In `write_page`, immediately after the `if self.read_only { return Err(ChiselError::ReadOnlyMode); }` guard (`:248-250`), insert:

```rust
        #[cfg(test)]
        if self.fault.get() == Fault::FailWritePage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected write failure",
            )));
        }
```

- [ ] **Step 7: Hook `fsync`.** In `fsync`, immediately after the `if self.read_only { return Err(ChiselError::ReadOnlyMode); }` guard (`:299-301`), insert:

```rust
        #[cfg(test)]
        match self.fault.get() {
            Fault::FailFsync(0) => {
                self.fault.set(Fault::None);
                return Err(ChiselError::IoError(std::io::Error::other(
                    "fault-injected fsync failure",
                )));
            }
            Fault::FailFsync(n) => self.fault.set(Fault::FailFsync(n - 1)),
            _ => {}
        }
```

- [ ] **Step 8: Write the self-tests.** In the existing `#[cfg(test)] mod tests` of `src/page_io.rs`, add (these verify the hook itself — they FAIL to compile until Steps 1–7 land, then pass):

```rust
    #[test]
    fn armed_fsync_fault_returns_io_error_once() {
        let io = PageIo::open_in_memory().unwrap();
        io.arm_fault(Fault::FailFsync(0));
        assert!(matches!(io.fsync(), Err(ChiselError::IoError(_))));
        assert!(io.fsync().is_ok(), "one-shot: next fsync succeeds");
    }

    #[test]
    fn armed_fsync_countdown_fires_on_the_nth() {
        let io = PageIo::open_in_memory().unwrap();
        io.arm_fault(Fault::FailFsync(2));
        assert!(io.fsync().is_ok(), "1st succeeds (2 -> 1)");
        assert!(io.fsync().is_ok(), "2nd succeeds (1 -> 0)");
        assert!(matches!(io.fsync(), Err(ChiselError::IoError(_))), "3rd fires");
    }

    #[test]
    fn armed_write_fault_targets_one_page_then_clears() {
        let mut io = PageIo::open_in_memory().unwrap();
        let buf = [0u8; PAGE_SIZE];
        io.write_page(0, &buf).unwrap();
        io.arm_fault(Fault::FailWritePage(0));
        assert!(matches!(io.write_page(0, &buf), Err(ChiselError::IoError(_))));
        assert!(io.write_page(0, &buf).is_ok(), "one-shot cleared");
    }

    #[test]
    fn armed_read_fault_targets_one_page_then_clears() {
        let mut io = PageIo::open_in_memory().unwrap();
        let buf = [0u8; PAGE_SIZE];
        io.write_page(0, &buf).unwrap();
        io.arm_fault(Fault::FailReadPage(0));
        assert!(matches!(io.read_page(0), Err(ChiselError::IoError(_))));
        assert!(io.read_page(0).is_ok(), "one-shot cleared");
    }
```

- [ ] **Step 9: Run + verify.**

```bash
cargo test -p chisel --lib page_io::tests::armed_ 2>&1 | tail -8
```
Expected: 4 tests pass. Then `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo fmt --check` clean.

- [ ] **Step 10: Commit.**

```bash
git add src/page_io.rs
git commit -m "test(I112): fault-injection hook in PageIo (cfg(test))"
```

---

### Task 2: Part A(a) — flush/fsync coupling test

Isolates the exact "dirty flags cleared before the trailing fsync" window the `page_cache.rs:363-378` comment warns about.

**Files:**
- Modify: `src/page_cache.rs` (add one test + a `use` in the test module)

- [ ] **Step 1: Add the import.** In `page_cache.rs`'s `#[cfg(test)] mod tests`, add to its `use` block:

```rust
    use crate::page_io::Fault;
```

- [ ] **Step 2: Write the test.** Add to the same test module:

```rust
    #[test]
    fn failed_flush_fsync_leaves_pages_clean_but_nondurable() {
        // I112 (durability window — see the flush() comment): flush clears each
        // page's dirty flag in phase 1a BEFORE the trailing fsync. If that fsync
        // fails, the pages are now CLEAN in the cache yet NOT durable on disk —
        // a state that is safe ONLY because the transaction manager poisons on
        // the fatal IoError (proven in transaction.rs). This makes the hazard
        // observable. spillway_max_bytes=0 keeps flush to a single fsync.
        let (_dir, mut cache) = fresh_cache_with_spillway(64, 0);
        let id = cache.new_page().unwrap();
        assert!(cache.is_dirty(id));
        assert_eq!(cache.dirty_count, 1);

        cache.io().arm_fault(Fault::FailFsync(0));
        let result = cache.flush();
        assert!(
            matches!(result, Err(ChiselError::IoError(_))),
            "flush must surface the fsync IoError, got {result:?}"
        );
        // The dirty flags were already cleared (phase 1a) before the failed
        // fsync. This is the durability window; poison (asserted in
        // transaction.rs) is what makes it safe.
        assert_eq!(
            cache.dirty_count, 0,
            "phase-1a cleared dirty flags before the (failed) fsync"
        );
    }
```

- [ ] **Step 3: Run + verify.**

```bash
cargo test -p chisel --lib failed_flush_fsync 2>&1 | tail -6
```
Expected: 1 test passes. (If `dirty_count != 0`, flush rolls back dirty flags on error — that would itself be a finding to surface, since the spec asserts the no-rollback window.) Then clippy + fmt clean.

- [ ] **Step 4: Commit.**

```bash
git add src/page_cache.rs
git commit -m "test(I112): failed flush fsync leaves clean-but-nondurable pages"
```

---

### Task 3: Part A(b)+(c) — commit poison on fsync and write faults

Proves a real I/O fault anywhere in commit surfaces `IoError` and poisons.

**Files:**
- Modify: `src/transaction.rs` (add two tests + a `use` in the test module)

- [ ] **Step 1: Add the import.** In `transaction.rs`'s `#[cfg(test)] mod tests`, add to its `use` block (it already `use`s `crate::page_io::PageIo`):

```rust
    use crate::page_io::Fault;
```

- [ ] **Step 2: Write the fsync test.** Add to the test module:

```rust
    #[test]
    fn commit_fsync_failure_poisons_at_each_of_the_three_fsyncs() {
        // I112: commit performs THREE fsyncs (pre-drain, data-flush, superblock).
        // A real IoError at ANY of them must surface as IoError AND poison the
        // manager. The FailFsync countdown targets each in turn. A small inline
        // value keeps commit to exactly three fsyncs (no spillway: fresh_manager
        // sets spillway_max_bytes=0).
        for nth in 0..3u32 {
            let mut tm = fresh_manager();
            tm.begin().unwrap();
            tm.allocate(b"v").unwrap();
            tm.cache.borrow().io().arm_fault(Fault::FailFsync(nth));
            let result = tm.commit();
            assert!(
                matches!(result, Err(ChiselError::IoError(_))),
                "commit fsync #{} failure must surface IoError, got {result:?}",
                nth + 1
            );
            assert!(tm.is_poisoned(), "fsync #{} failure must poison", nth + 1);
            assert!(
                matches!(tm.read(0), Err(ChiselError::Poisoned)),
                "a poisoned manager rejects all further ops"
            );
        }
    }
```

- [ ] **Step 3: Write the write-fault test.** Add to the test module:

```rust
    #[test]
    fn commit_write_failure_poisons() {
        // I112: a real write_page IoError during commit must surface and poison.
        // Target the value's own data page, which is written during commit flush.
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate(b"v").unwrap();
        let pid = tm
            .handle_live_page_id(h)
            .unwrap()
            .expect("allocated value has a live data page");
        tm.cache.borrow().io().arm_fault(Fault::FailWritePage(pid));
        let result = tm.commit();
        assert!(
            matches!(result, Err(ChiselError::IoError(_))),
            "commit write failure must surface IoError, got {result:?}"
        );
        assert!(tm.is_poisoned(), "write failure during commit must poison");
    }
```

- [ ] **Step 4: Run + verify.**

```bash
cargo test -p chisel --lib commit_fsync_failure commit_write_failure 2>&1 | tail -8
```
Expected: 2 tests pass. Then clippy + fmt clean.

- [ ] **Step 5: Commit.**

```bash
git add src/transaction.rs
git commit -m "test(I112): commit fsync/write faults surface IoError and poison"
```

---

### Task 4: Part A(d) — non-commit poison via a real read fault, retire `force_poison_for_test`

Replaces the tautological `fatal_error_outside_commit_also_poisons` (which calls `force_poison_for_test()`) with a genuine injected fault, preserving the non-commit poison-path coverage.

**Files:**
- Modify: `src/transaction.rs` (rewrite one test; possibly remove `force_poison_for_test`)

- [ ] **Step 1: Replace the test body.** Find `fn fatal_error_outside_commit_also_poisons` (`transaction.rs:~2849`) and replace the whole test with the version below. It reopens over the committed file so `read` is a genuine cache **miss** that reaches `read_page` (a same-session read after commit would be a cache hit and never call `read_page`):

```rust
    #[test]
    fn fatal_error_outside_commit_also_poisons() {
        // I112: a REAL fatal IoError on a cold read OUTSIDE any transaction
        // poisons the manager (the non-commit fatal path, poison_on_fatal). This
        // replaces the old force_poison_for_test() tautology with an injected
        // fault. We reopen over the committed file so read(h) is a cache MISS
        // that actually reaches read_page(pid).
        let file = NamedTempFile::new().unwrap();
        let h;
        let pid;
        {
            let io = PageIo::open(file.path(), false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::Path(file.path().to_path_buf()),
            );
            let mut tm = TransactionManager::create_new(cache, 2).unwrap();
            tm.begin().unwrap();
            h = tm.allocate(b"durable").unwrap();
            tm.commit().unwrap();
            pid = tm
                .handle_live_page_id(h)
                .unwrap()
                .expect("live data page");
        }

        // Reopen: cold cache, so read(h) misses and calls read_page(pid).
        let io = PageIo::open(file.path(), false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::Path(file.path().to_path_buf()),
        );
        let tm = TransactionManager::open_existing(cache).unwrap();
        tm.cache.borrow().io().arm_fault(Fault::FailReadPage(pid));
        let result = tm.read(h);
        assert!(
            matches!(result, Err(ChiselError::IoError(_))),
            "cold read fault must surface IoError, got {result:?}"
        );
        assert!(
            tm.is_poisoned(),
            "a fatal read error outside commit must poison"
        );
    }
```

- [ ] **Step 2: Check whether `force_poison_for_test` is now dead.**

```bash
grep -rn "force_poison_for_test" src/
```
Expected: references only in its own definition (`transaction.rs:~655`). If that is the only remaining reference, proceed to Step 3; if other callers exist, SKIP Step 3 (leave the method) and note them.

- [ ] **Step 3: Remove the dead helper (only if Step 2 showed no other callers).** Delete the `#[cfg(test)] pub fn force_poison_for_test(&self) { … }` method and its doc comment (`transaction.rs:~650-657`). The `poisoned` flag itself stays — only the synthetic setter is removed.

- [ ] **Step 4: Run + verify.**

```bash
cargo test -p chisel --lib fatal_error_outside_commit 2>&1 | tail -6
```
Expected: passes. Then `cargo test` (full), clippy, fmt clean.

- [ ] **Step 5: Commit.**

```bash
git add src/transaction.rs
git commit -m "test(I112): non-commit poison via real read fault; drop force_poison_for_test"
```

---

### Task 5: Part C = I114 — release-safe I20 accounting test

The I20 `debug_assert!` in `claim_page` vanishes under `cargo test --release` (the wheel gate). Add a release-compiled test of the observable accounting consequence.

**Files:**
- Modify: `src/page_cache.rs` (add one test; keep the existing `debug_assert!` and the debug-only `claim_page_asserts_on_dirty_page`)

- [ ] **Step 1: Write the test.** Add to `page_cache.rs`'s `#[cfg(test)] mod tests` (NOT gated on `debug_assertions` — it must run in release):

```rust
    #[test]
    fn claim_page_keeps_dirty_count_consistent() {
        // I114: the I20 debug_assert! in claim_page is a no-op under
        // `cargo test --release` (the wheel gate, wheels.yml). This asserts the
        // OBSERVABLE accounting consequence that holds in EVERY profile: claiming
        // a CLEAN page (the legitimate freemap-reuse path) adds exactly one dirty
        // entry. A regression that mis-tracks dirty_count fails here in release
        // too — complementing the debug-only claim_page_asserts_on_dirty_page,
        // which catches the illegitimate (claim-a-dirty-page) path.
        let (_dir, mut cache) = fresh_cache(64);
        let id = cache.new_page().unwrap();
        cache.flush().unwrap(); // page becomes clean
        assert!(!cache.is_dirty(id), "precondition: page clean before reclaim");

        let before = cache.dirty_count;
        cache.claim_page(id).unwrap();
        assert!(cache.is_dirty(id), "reclaimed page is dirty");
        assert_eq!(
            cache.dirty_count,
            before + 1,
            "claim of a clean page must add exactly one dirty entry (I20 accounting)"
        );
    }
```

- [ ] **Step 2: Run + verify in BOTH profiles.**

```bash
cargo test -p chisel --lib claim_page_keeps_dirty_count_consistent 2>&1 | tail -5
cargo test --release -p chisel --lib claim_page_keeps_dirty_count_consistent 2>&1 | tail -5
```
Expected: passes in both debug and release. Then clippy + fmt clean.

- [ ] **Step 3: Commit.**

```bash
git add src/page_cache.rs
git commit -m "test(I114): release-safe I20 dirty-count accounting check"
```

---

### Task 6: Part B = I115 (pin) — bad magic surfaces as `CorruptSuperblock`

One test that both pins `CorruptSuperblock` as the *sole* expected variant AND proves `InvalidMagic` is unreachable. Uses the existing byte-corruption helper.

**Files:**
- Modify: `src/recovery_tests.rs` (add one test; confirm/add `use crate::ChiselError;`)

- [ ] **Step 1: Confirm imports.** Ensure `recovery_tests.rs` has `ChiselError` and `TempDir` in scope (it already uses `Chisel`, `Superblock`, `page`, `fs`, `SeekFrom`). If `ChiselError` or `tempfile::TempDir` are not imported, add:

```rust
use crate::ChiselError;
use tempfile::TempDir;
```

- [ ] **Step 2: Write the test.** Add to `recovery_tests.rs`:

```rust
// I115: corrupt the 4-byte magic (superblock offset 0..4) in EVERY superblock
// slot, re-stamping the checksum so the magic check (not the checksum check) is
// the failing gate. Superblock::deserialize then returns None for every slot,
// Superblock::select returns None, and open surfaces CorruptSuperblock. This
// PINS CorruptSuperblock as the sole expected variant AND proves InvalidMagic is
// unreachable (a bad magic never produces it) — the basis for removing the dead
// variant in the next task.
#[test]
fn corrupt_magic_surfaces_as_corrupt_superblock_not_invalid_magic() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.chisel");
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"x").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    for slot in 0..crate::superblock::DEFAULT_SUPERBLOCK_COUNT as u64 {
        rewrite_page_with_valid_checksum(&path, slot, |buf| {
            buf[0] ^= 0xFF; // flip a magic byte; magic lives at bytes 0..4
        });
    }
    let err = Chisel::open(&path, Default::default()).unwrap_err();
    assert!(
        matches!(err, ChiselError::CorruptSuperblock),
        "bad magic must surface as CorruptSuperblock (sole variant), got {err:?}"
    );
}
```

- [ ] **Step 3: Run + verify.**

```bash
cargo test -p chisel --lib corrupt_magic_surfaces 2>&1 | tail -6
```
Expected: passes (confirming bad magic → `CorruptSuperblock`, never `InvalidMagic`). Then clippy + fmt clean.

- [ ] **Step 4: Commit.**

```bash
git add src/recovery_tests.rs
git commit -m "test(I115): pin CorruptSuperblock; prove bad magic never yields InvalidMagic"
```

---

### Task 7: Part B = I115 (remove) — delete the dead `InvalidMagic` variant

With Task 6 proving it unreachable, remove the variant across all sites. This is one atomic cross-crate change (the workspace build includes the Python crate). Let the compiler guide you: removing the variant breaks every match/array that names it.

**Files (20 sites):**
- Modify: `src/error.rs`, `src/lib.rs`, `tests/error_and_format.rs`, `python/src/errors.rs`, `python/chisel/__init__.py`, `python/chisel/chisel.pyi`, `python/tests/test_errors.py`, `README.md`, `python/README.md`

- [ ] **Step 1: Remove from `src/error.rs` (6 sites).**
  - `:111` — delete the `InvalidMagic,` enum variant.
  - `:175` — delete `| ChiselError::InvalidMagic` from the `is_fatal()` match.
  - `:243` — delete the `ChiselError::InvalidMagic => write!(f, "invalid magic number"),` `Display` arm.
  - `:416` — delete `| ChiselError::InvalidMagic` from the `documented_is_fatal` exhaustiveness-test Fatal block.
  - `:454` — delete `ChiselError::InvalidMagic,` from the `all[]` array.
  - `:473` — the tripwire that asserts the fatal-variant count: change the expected count from **9 to 8** (read the surrounding assertion to confirm the literal and adjust it; e.g. `assert_eq!(fatal_count, 8, …)`).

- [ ] **Step 2: Remove from `src/lib.rs` (1 site).**
  - `:302`-ish — in the doc comment listing fatal errors, drop `InvalidMagic` from the list. (Search the file for `InvalidMagic`.)

- [ ] **Step 3: Remove from `tests/error_and_format.rs` (2 sites).**
  - `:61` and `:108` — delete the `ChiselError::InvalidMagic,` entries from the two test arrays.

- [ ] **Step 4: Remove from the Python binding (`python/src/errors.rs`, 4 sites).**
  - `:45` — delete the `//         InvalidMagicError` comment line.
  - `:139` — delete `create_exception!(_chisel, InvalidMagicError, FatalError);`.
  - `:221` — delete `m.add("InvalidMagicError", py.get_type::<InvalidMagicError>())?;`.
  - `:316` — delete the `RustChiselError::InvalidMagic => InvalidMagicError::new_err(msg),` match arm.

- [ ] **Step 5: Remove from the Python package.**
  - `python/chisel/__init__.py:43` — delete `InvalidMagicError,` from the import.
  - `python/chisel/__init__.py:138` — delete `"InvalidMagicError",` from `__all__`.
  - `python/chisel/chisel.pyi:68` — delete the `class InvalidMagicError(FatalError): ...` stub.
  - `python/tests/test_errors.py` — search for `InvalidMagicError` and delete it from the fatal-class enumeration list.

- [ ] **Step 6: Remove from docs.**
  - `README.md:284` — drop `InvalidMagic` from the fatal-variants list.
  - `python/README.md` — delete the `| InvalidMagicError | … |` error-table row.

- [ ] **Step 7: Build + verify the full gate.**

```bash
cargo test 2>&1 | grep -E "test result:|error\[|FAILED" | grep -v "0 failed" || echo "RUST: 0 failures"
cargo test --release -p chisel --lib 2>&1 | tail -3
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --check && echo "fmt clean"
```
Expected: 0 Rust failures (debug + release), clippy clean, fmt clean. Then the Python suite:

```bash
cd python && source .venv/bin/activate 2>/dev/null || python3 -m venv .venv && source .venv/bin/activate
pip -q install maturin pytest hypothesis >/dev/null 2>&1
maturin develop -q 2>&1 | tail -1 && python -m pytest -q 2>&1 | tail -3
```
Expected: all Python tests pass (the binding, `__init__`, `.pyi`, and `test_errors.py` updated together).

- [ ] **Step 8: Commit.**

```bash
git add -A
git commit -m "fix(I115): remove provably-dead InvalidMagic error variant"
```

---

### Task 8: Close out ISSUES.md

**Files:**
- Modify: `ISSUES.md`

- [ ] **Step 1: Mark I112, I114, I115 fixed.** Add a `✅ FIXED 2026-06-21` marker and a one-paragraph resolution to each of the I112, I114, I115 entries, and update the backlog handoff status note (the `Status note` line near the top) to record them done and name the next priority items (I106, I107, I113, I139, I118).

- [ ] **Step 2: Commit.**

```bash
git add ISSUES.md
git commit -m "docs: close I112/I114/I115 in ISSUES.md"
```

---

## Self-Review

**Spec coverage:**
- Component 1 (fault hook) → Task 1. ✓
- Component 2 Part A(a) flush coupling → Task 2. ✓
- Part A(b) three-fsync poison + A(c) write fault → Task 3. ✓
- Part A(d) non-commit read fault + `force_poison_for_test` cleanup → Task 4. ✓
- Component 4 Part C / I114 release-safe test → Task 5. ✓
- Component 3 Part B / I115 pin → Task 6; remove → Task 7 (all 20 sites). ✓
- ISSUES.md closure → Task 8. ✓
- Non-goals (`set_page_count`, open-time faults) → correctly absent. ✓

**Placeholder scan:** No TBD/TODO. The only conditional is Task 4 Step 3 (remove `force_poison_for_test` iff dead), with an explicit grep criterion — not a placeholder. The `error.rs:473` count change names the concrete 9→8 with an instruction to confirm the literal.

**Type consistency:** `Fault` (variants `None`/`FailFsync(u32)`/`FailWritePage(u64)`/`FailReadPage(u64)`), `arm_fault(&self, Fault)`, `tm.cache.borrow().io().arm_fault(...)`, `handle_live_page_id(h) -> Result<Option<u64>>`, `fresh_manager()`, `fresh_cache(usize)`, `fresh_cache_with_spillway(usize, u64)`, `cache.dirty_count` (field), `cache.is_dirty(u64)`, `cache.flush()`, `cache.new_page()`, `cache.claim_page(u64)` — all used consistently across tasks and matched against the real signatures read from source.
