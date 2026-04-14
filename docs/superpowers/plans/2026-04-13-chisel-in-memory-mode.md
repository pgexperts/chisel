# Chisel In-Memory Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a non-durable, RAM-backed storage mode to Chisel so the engine can be benchmarked against SQLite `:memory:` without filesystem overhead.

**Architecture:** A new `Backing::Memory { pages: Vec<[u8; PAGE_SIZE]> }` variant is added to an enum inside `PageIo`. Every `PageIo` method dispatches on `self.backing`. Nothing above `page_io.rs` changes. `Chisel::open_in_memory()` / `open_in_memory_with_options(opts)` bootstrap a fresh memory-backed database using the same `TransactionManager::create_new` path as a fresh file-backed one.

**Tech Stack:** Rust 2021, Cargo, existing Chisel crate. Integration tests use `tempfile::NamedTempFile` (today) and will be parameterized over a `Backing` helper enum.

**Spec:** `docs/superpowers/specs/2026-04-13-chisel-in-memory-mode-design.md`

**Conventions assumed from CLAUDE.md:**
- Build/test: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt -- --check`.
- Bottom-up module graph — do NOT introduce upward references from `page_io.rs`.
- Comments explain *why* (invariants, tradeoffs), not *what*.

---

## File Map

Files created:
- `tests/common/mod.rs` — dual-backing test helper (Backing enum, `open_chisel`, `dual_backing_test!` macro).

Files modified:
- `src/page_io.rs` — introduce `Backing` enum; dispatch in every method; add `open_in_memory` constructor.
- `src/lib.rs` — add `Chisel::open_in_memory` and `Chisel::open_in_memory_with_options`.
- `tests/basic_ops.rs` — convert Chisel-level tests to dual-backing.
- `tests/transactions.rs` — convert to dual-backing.
- `tests/api_edge_cases.rs` — convert to dual-backing.
- `tests/defrag.rs` — convert to dual-backing.
- `tests/overflow.rs` — convert to dual-backing.
- `tests/stress.rs` — convert to dual-backing.
- `tests/error_and_format.rs` — partial: convert format/version tests; keep file-corruption tests file-only.
- `tests/options_validation.rs` — partial: dual-run the options that apply to memory mode.

Files unchanged:
- `tests/crash_recovery.rs` — file-only by nature.
- `src/page_io.rs` read-only-mode unit tests (`read_only_tests`) — file-only by nature.
- Every other `src/*` module.

---

## Task 1: Refactor `PageIo` into a `Backing` enum (file-only variant)

This is a pure refactor. Behavior must be bit-identical; the full existing test suite is the spec.

**Files:**
- Modify: `src/page_io.rs`

- [ ] **Step 1: Baseline — confirm the existing suite is green**

Run: `cargo test`
Expected: all tests pass. If anything is red before we start, stop and fix that first.

- [ ] **Step 2: Introduce the `Backing` enum with only a `File` variant, route all methods through it**

Replace the struct and impl in `src/page_io.rs`. The `Backing` enum is private; `PageIo` remains the only public type.

```rust
// New internal layout. The enum is private: callers still see only `PageIo`.
//
// Why an enum rather than a trait object: benchmark integrity. A `dyn PageIo`
// adds a vtable call per page read/write — exactly the cost we want excluded
// when comparing Chisel to SQLite `:memory:`. An enum branch is predictable
// and effectively free once the variant is hot. See the in-memory-mode spec.
enum Backing {
    File { file: File },
}

pub struct PageIo {
    backing: Backing,
    read_only: bool,
}

impl PageIo {
    pub fn open(path: &Path, read_only: bool) -> Result<PageIo> {
        let file = if read_only {
            OpenOptions::new().read(true).open(path)?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?
        };
        Self::try_lock(&file)?;
        Ok(PageIo {
            backing: Backing::File { file },
            read_only,
        })
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn try_lock(file: &File) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(ChiselError::LockFailed);
        }
        Ok(())
    }

    pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let page_count = self.page_count()?;
        if page_id >= page_count {
            return Err(ChiselError::InvalidPageId { page_id });
        }
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * PAGE_SIZE as u64;
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = [0u8; PAGE_SIZE];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
        }
    }

    pub fn write_page(&mut self, page_id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * PAGE_SIZE as u64;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(buf)?;
                Ok(())
            }
        }
    }

    pub fn fsync(&self) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        match &self.backing {
            Backing::File { file } => {
                file.sync_all()?;
                Ok(())
            }
        }
    }

    pub fn page_count(&mut self) -> Result<u64> {
        match &mut self.backing {
            Backing::File { file } => {
                let len = file.seek(SeekFrom::End(0))?;
                Ok(len / PAGE_SIZE as u64)
            }
        }
    }

    pub fn set_page_count(&mut self, n: u64) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        match &mut self.backing {
            Backing::File { file } => {
                file.set_len(n * PAGE_SIZE as u64)?;
                Ok(())
            }
        }
    }
}
```

Preserve all existing module-level and method-level doc comments. The only structural change is that each method body is wrapped in `match &self.backing { Backing::File { file } => { ... existing body ... } }`.

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: clean compile. Any error here is a mechanical refactor mistake.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test`
Expected: all tests pass. The refactor is behavior-preserving.

- [ ] **Step 5: Clippy + fmt**

Run: `cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: both clean. If clippy flags `match` with a single arm (`match_single_arm` or similar), suppress it locally with `#[allow(clippy::single_match_else)]` at the method level only where necessary — the second arm is coming in Task 2.

If clippy refuses the single-arm match and no local allow will satisfy it cleanly, add the Memory variant now as an empty skeleton:

```rust
enum Backing {
    File { file: File },
    Memory { pages: Vec<[u8; PAGE_SIZE]> },
}
```

and have every method add `Backing::Memory { .. } => unreachable!("Task 2 fills this in")` as the second arm. This keeps clippy happy and is replaced fully in Task 2.

- [ ] **Step 6: Commit**

```bash
git add src/page_io.rs
git commit -m "Refactor PageIo to dispatch through a Backing enum

No behavior change. Introduces a single-variant Backing enum inside
PageIo so the in-memory variant can be added without restructuring
the rest of the module stack."
```

---

## Task 2: Add `Backing::Memory` variant and its method implementations

**Files:**
- Modify: `src/page_io.rs`
- Test: inline module tests inside `src/page_io.rs` (follow the existing `read_only_tests` pattern).

- [ ] **Step 1: Write failing unit tests for memory backing**

Append to `src/page_io.rs`, after the existing `read_only_tests` module:

```rust
#[cfg(test)]
mod memory_backing_tests {
    use super::*;

    // These tests exercise the Memory variant through the PageIo surface.
    // They use the (Task 3) constructor `PageIo::open_in_memory`, written
    // against its final signature so that implementing Task 3 flips these
    // from failing-to-compile to passing.

    #[test]
    fn memory_starts_with_zero_pages() {
        let mut io = PageIo::open_in_memory().unwrap();
        assert_eq!(io.page_count().unwrap(), 0);
    }

    #[test]
    fn memory_write_then_read_roundtrip() {
        let mut io = PageIo::open_in_memory().unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0x42;
        buf[PAGE_SIZE - 1] = 0xFF;
        io.write_page(0, &buf).unwrap();
        let read = io.read_page(0).unwrap();
        assert_eq!(read, buf);
    }

    #[test]
    fn memory_write_extends_page_count() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
        io.write_page(1, &[0u8; PAGE_SIZE]).unwrap();
        io.write_page(2, &[0u8; PAGE_SIZE]).unwrap();
        assert_eq!(io.page_count().unwrap(), 3);
    }

    #[test]
    fn memory_write_beyond_end_grows_with_zero_fill() {
        // Writing to page 5 on an empty backing extends pages 0..=5.
        // Pages 0..=4 must be zero-filled; page 5 carries the written bytes.
        let mut io = PageIo::open_in_memory().unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        buf[42] = 0xAB;
        io.write_page(5, &buf).unwrap();
        assert_eq!(io.page_count().unwrap(), 6);
        for p in 0..5 {
            assert_eq!(io.read_page(p).unwrap(), [0u8; PAGE_SIZE]);
        }
        assert_eq!(io.read_page(5).unwrap(), buf);
    }

    #[test]
    fn memory_read_out_of_range_is_invalid_page_id() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
        let err = io.read_page(1).unwrap_err();
        assert!(
            matches!(err, ChiselError::InvalidPageId { page_id: 1 }),
            "expected InvalidPageId {{ 1 }}, got {err:?}"
        );
    }

    #[test]
    fn memory_fsync_is_noop() {
        let io = PageIo::open_in_memory().unwrap();
        io.fsync().unwrap();
    }

    #[test]
    fn memory_set_page_count_shrinks_and_grows() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(0, &[1u8; PAGE_SIZE]).unwrap();
        io.write_page(1, &[2u8; PAGE_SIZE]).unwrap();
        io.write_page(2, &[3u8; PAGE_SIZE]).unwrap();
        io.set_page_count(1).unwrap();
        assert_eq!(io.page_count().unwrap(), 1);
        assert_eq!(io.read_page(0).unwrap(), [1u8; PAGE_SIZE]);
        io.set_page_count(4).unwrap();
        assert_eq!(io.page_count().unwrap(), 4);
        // Pages 1..=3 are freshly zero-filled after re-growth.
        for p in 1..4 {
            assert_eq!(io.read_page(p).unwrap(), [0u8; PAGE_SIZE]);
        }
    }
}
```

- [ ] **Step 2: Confirm tests fail to compile (constructor not defined yet)**

Run: `cargo test memory_backing_tests`
Expected: compilation error — `no function or associated item named 'open_in_memory' found for struct 'PageIo'`. This confirms the tests would run if the constructor existed; they will become proper pass/fail in Step 4.

- [ ] **Step 3: Implement `Backing::Memory` and the `open_in_memory` constructor**

In `src/page_io.rs`:

Add a new variant to `Backing` (or replace the skeleton added at Task 1 Step 5):

```rust
enum Backing {
    File { file: File },
    // Memory-backed database for benchmarking against SQLite :memory:.
    // `pages.len() * PAGE_SIZE` is the on-disk "file size" equivalent;
    // allocating a new page is a `Vec::push` of a zero-filled array.
    // No fsync, no flock, no recovery — see the in-memory-mode spec.
    Memory { pages: Vec<[u8; PAGE_SIZE]> },
}
```

Add the constructor next to `open`:

```rust
/// Open a fresh memory-backed database. Non-durable by design: dropping
/// the returned `PageIo` discards all pages. Used for benchmark parity
/// with SQLite `:memory:`; not intended for durable workloads.
///
/// No `flock` is taken — a memory-backed database is single-client by
/// virtue of being owned by a single `PageIo` value. Never fallible in
/// the current implementation, but the `Result` return keeps the API
/// symmetric with `open` and leaves room for future fallible init.
pub fn open_in_memory() -> Result<PageIo> {
    Ok(PageIo {
        backing: Backing::Memory { pages: Vec::new() },
        read_only: false,
    })
}
```

Fill in the Memory arm of each method. Replace the unreachable skeletons (if Task 1 Step 5 used that shape), or add the new arms:

```rust
pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
    let page_count = self.page_count()?;
    if page_id >= page_count {
        return Err(ChiselError::InvalidPageId { page_id });
    }
    match &mut self.backing {
        Backing::File { file } => {
            let offset = page_id * PAGE_SIZE as u64;
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = [0u8; PAGE_SIZE];
            file.read_exact(&mut buf)?;
            Ok(buf)
        }
        Backing::Memory { pages } => Ok(pages[page_id as usize]),
    }
}

pub fn write_page(&mut self, page_id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
    if self.read_only {
        return Err(ChiselError::ReadOnlyMode);
    }
    match &mut self.backing {
        Backing::File { file } => {
            let offset = page_id * PAGE_SIZE as u64;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(buf)?;
            Ok(())
        }
        Backing::Memory { pages } => {
            // Match POSIX: writing past end extends, intermediate pages
            // are zero-filled. Shadow paging and PageCache::new_page
            // rely on this growth shape.
            let idx = page_id as usize;
            if idx >= pages.len() {
                pages.resize(idx + 1, [0u8; PAGE_SIZE]);
            }
            pages[idx] = *buf;
            Ok(())
        }
    }
}

pub fn fsync(&self) -> Result<()> {
    if self.read_only {
        return Err(ChiselError::ReadOnlyMode);
    }
    match &self.backing {
        Backing::File { file } => {
            file.sync_all()?;
            Ok(())
        }
        // No durable storage to flush. The commit protocol still calls
        // fsync twice per commit; that overhead (two method calls and
        // two matches) is preserved for benchmark fidelity.
        Backing::Memory { .. } => Ok(()),
    }
}

pub fn page_count(&mut self) -> Result<u64> {
    match &mut self.backing {
        Backing::File { file } => {
            let len = file.seek(SeekFrom::End(0))?;
            Ok(len / PAGE_SIZE as u64)
        }
        Backing::Memory { pages } => Ok(pages.len() as u64),
    }
}

pub fn set_page_count(&mut self, n: u64) -> Result<()> {
    if self.read_only {
        return Err(ChiselError::ReadOnlyMode);
    }
    match &mut self.backing {
        Backing::File { file } => {
            file.set_len(n * PAGE_SIZE as u64)?;
            Ok(())
        }
        Backing::Memory { pages } => {
            pages.resize(n as usize, [0u8; PAGE_SIZE]);
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Run memory-backing tests**

Run: `cargo test memory_backing_tests`
Expected: all seven tests pass.

- [ ] **Step 5: Run full suite + clippy + fmt**

Run: `cargo test && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add src/page_io.rs
git commit -m "Add Memory variant to PageIo Backing

Non-durable RAM-backed storage for benchmark parity with SQLite :memory:.
Every PageIo method gains a Memory arm: reads/writes copy to a Vec of
page buffers, fsync is a no-op, page allocation is Vec::push. No
filesystem, no flock, no recovery — the variant is strictly ephemeral."
```

---

## Task 3: Add `Chisel::open_in_memory` and `Chisel::open_in_memory_with_options`

**Files:**
- Modify: `src/lib.rs`
- Test: `tests/in_memory.rs` (new integration test file for mode-specific behavior; dual-backing conversion comes later).

- [ ] **Step 1: Write failing integration tests**

Create `tests/in_memory.rs`:

```rust
// Mode-specific tests for in-memory Chisel. Behavior parity with file-
// backed Chisel is verified by the dual-backing integration suite;
// these tests cover only what is specific to memory mode.

use chisel::{Chisel, Options};

#[test]
fn open_in_memory_round_trip_commit() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"hello".to_vec());
}

#[test]
fn open_in_memory_round_trip_rollback() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.rollback().unwrap();
    // A fresh transaction must not see rolled-back handles.
    db.begin().unwrap();
    assert!(db.read(h).is_err());
}

#[test]
fn open_in_memory_with_options_respects_superblock_count() {
    // Non-default superblock_count flows through to the memory bootstrap.
    // We cannot inspect the superblock count from the public API directly,
    // but an invalid value must still be rejected by the same validation
    // path used for file-backed open.
    let bad = Options {
        superblock_count: 1, // below MIN_SUPERBLOCKS (2)
        ..Options::default()
    };
    assert!(Chisel::open_in_memory_with_options(bad).is_err());

    let good = Options {
        superblock_count: 4,
        ..Options::default()
    };
    let mut db = Chisel::open_in_memory_with_options(good).unwrap();
    db.begin().unwrap();
    db.allocate(b"payload").unwrap();
    db.commit().unwrap();
}

#[test]
fn open_in_memory_rejects_read_only_option() {
    // A read-only, freshly-created memory database cannot bootstrap
    // (nothing to read, and the superblock-write step is blocked).
    // We surface this early as an explicit InvalidArgument-style error
    // rather than letting the bootstrap fail obliquely.
    let opts = Options {
        read_only: true,
        ..Options::default()
    };
    assert!(Chisel::open_in_memory_with_options(opts).is_err());
}

#[test]
fn dropping_in_memory_db_releases_backing() {
    // Smoke test: we can create, fill, drop, and recreate many times
    // without accumulating state. No fd leaks to check here; this mostly
    // guards against "accidentally leaked into a static" regressions.
    for _ in 0..8 {
        let mut db = Chisel::open_in_memory().unwrap();
        db.begin().unwrap();
        for i in 0..100u32 {
            db.allocate(&i.to_le_bytes()).unwrap();
        }
        db.commit().unwrap();
    }
}
```

- [ ] **Step 2: Confirm tests fail to compile**

Run: `cargo test --test in_memory`
Expected: compilation error — `open_in_memory` and `open_in_memory_with_options` do not exist on `Chisel`.

- [ ] **Step 3: Implement the constructors in `src/lib.rs`**

Add, after the existing `Chisel::open`:

```rust
/// Open a non-durable, memory-backed Chisel database. Intended for
/// benchmark comparisons against SQLite `:memory:` and for tests that
/// do not need filesystem persistence. All data is lost when the
/// returned `Chisel` is dropped.
///
/// Uses default `Options`. For a tuned cache size or superblock count,
/// use `open_in_memory_with_options`.
pub fn open_in_memory() -> Result<Chisel> {
    Self::open_in_memory_with_options(Options::default())
}

/// Open a memory-backed Chisel database with explicit options.
///
/// `options.read_only` must be `false`: a fresh memory database must
/// be writable for the initial superblock bootstrap, and there is no
/// prior file to reopen read-only. `options.create_if_missing` is
/// ignored — memory mode always creates a fresh database. All other
/// options (cache_size, superblock_count) flow through normally.
pub fn open_in_memory_with_options(options: Options) -> Result<Chisel> {
    if options.read_only {
        // Fail fast rather than bootstrapping and then blocking the
        // superblock write with ReadOnlyMode: the caller almost
        // certainly passed `read_only: true` by mistake.
        return Err(ChiselError::ReadOnlyMode);
    }
    if options.superblock_count < superblock::MIN_SUPERBLOCKS
        || options.superblock_count > superblock::MAX_SUPERBLOCKS
    {
        return Err(ChiselError::InvalidSuperblockCount {
            value: options.superblock_count,
        });
    }

    let io = PageIo::open_in_memory()?;
    let cache = PageCache::new(io, options.cache_size);
    let txm = TransactionManager::create_new(cache, options.superblock_count)?;
    Ok(Chisel { txm })
}
```

- [ ] **Step 4: Run the new test file**

Run: `cargo test --test in_memory`
Expected: all five tests pass.

- [ ] **Step 5: Full suite + clippy + fmt**

Run: `cargo test && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs tests/in_memory.rs
git commit -m "Add Chisel::open_in_memory{,_with_options}

Thin wrappers that bootstrap a fresh memory-backed Chisel via
TransactionManager::create_new on an in-memory PageIo. read_only=true
is rejected early with a clear error; create_if_missing is ignored
(memory mode always creates fresh)."
```

---

## Task 4: Dual-backing test harness

Provide a `Backing` enum, an `open_chisel` helper, and a `dual_backing_test!` macro that the integration suite can use to run a single test body against both file-backed and memory-backed Chisel.

**Files:**
- Create: `tests/common/mod.rs`
- Test: no dedicated test; this module is validated by its first consumer (Task 5).

- [ ] **Step 1: Create the harness**

Create `tests/common/mod.rs`:

```rust
// Dual-backing test harness.
//
// Usage:
//   mod common;
//   use common::{Backing, open_chisel};
//
//   fn my_test_body(b: &Backing) {
//       let mut db = open_chisel(b);
//       /* ... assertions ... */
//   }
//
//   common::dual_backing_test!(my_test, my_test_body);
//
// Expands to two #[test] fns: `my_test_file` (NamedTempFile) and
// `my_test_memory` (open_in_memory). The macro keeps each body single-
// source so behavior parity above the I/O layer is continuously verified.

#![allow(dead_code)] // Helpers may be unused in individual test files.

use chisel::{Chisel, Options};
use tempfile::NamedTempFile;

pub enum Backing {
    File(NamedTempFile),
    Memory,
}

impl Backing {
    pub fn fresh_file() -> Backing {
        Backing::File(NamedTempFile::new().expect("tempfile"))
    }
}

pub fn open_chisel(b: &Backing) -> Chisel {
    open_chisel_with(b, Options::default())
}

pub fn open_chisel_with(b: &Backing, opts: Options) -> Chisel {
    match b {
        Backing::File(f) => Chisel::open(f.path(), opts).unwrap(),
        Backing::Memory => Chisel::open_in_memory_with_options(opts).unwrap(),
    }
}

#[macro_export]
macro_rules! dual_backing_test {
    ($name:ident, $body:path) => {
        paste::paste! {
            #[test]
            fn [<$name _file>]() {
                let b = $crate::common::Backing::fresh_file();
                $body(&b);
            }

            #[test]
            fn [<$name _memory>]() {
                let b = $crate::common::Backing::Memory;
                $body(&b);
            }
        }
    };
}
```

The macro uses the `paste` crate to concatenate identifiers. Add it as a dev-dependency:

```toml
# Cargo.toml — under [dev-dependencies]
paste = "1"
```

(If `paste` is already present, skip the Cargo.toml edit.)

- [ ] **Step 2: Build**

Run: `cargo build --tests`
Expected: clean. If the `paste` crate was newly added, it will be downloaded.

- [ ] **Step 3: Sanity-check that the macro works by adding a throwaway test inside `tests/in_memory.rs`**

Append temporarily to `tests/in_memory.rs` (to be removed in Task 5 once a real dual test exists):

```rust
#[cfg(test)]
mod harness_sanity {
    mod common {
        include!("../common/mod.rs");
    }

    fn body(b: &common::Backing) {
        let mut db = common::open_chisel(b);
        db.begin().unwrap();
        let h = db.allocate(b"ok").unwrap();
        db.commit().unwrap();
        assert_eq!(db.read(h).unwrap(), b"ok".to_vec());
    }

    crate::dual_backing_test!(harness_sanity, body);
}
```

Note: `tests/*.rs` files in Cargo are each their own crate, so the standard pattern for sharing `tests/common` is either `mod common;` inside each test file or an `include!` — the macro above uses the `$crate::common::` path, so each test file must declare `mod common;` at its root. Replace the `include!` form above with a proper `mod common;` at the crate root before running.

Corrected sanity block (top of `tests/in_memory.rs`):

```rust
mod common;

// ... existing tests ...

fn sanity_body(b: &common::Backing) {
    let mut db = common::open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"ok").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"ok".to_vec());
}

dual_backing_test!(harness_sanity, sanity_body);
```

- [ ] **Step 4: Run the sanity test**

Run: `cargo test --test in_memory harness_sanity`
Expected: two tests pass — `harness_sanity_file` and `harness_sanity_memory`.

- [ ] **Step 5: Full suite + clippy + fmt**

Run: `cargo test && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add tests/common/mod.rs tests/in_memory.rs Cargo.toml Cargo.lock
git commit -m "Add dual-backing test harness

tests/common/mod.rs exposes Backing + open_chisel + dual_backing_test!.
Each test body written against it produces two #[test] fns (file and
memory), so behavior parity above page_io.rs is verified continuously."
```

---

## Task 5: Convert `tests/basic_ops.rs` Chisel-level tests to dual-backing

**Files:**
- Modify: `tests/basic_ops.rs`

The non-Chisel tests in this file (page checksum, superblock serialization, PageCache unit tests) are backing-agnostic by construction — they operate on raw buffers or on `PageIo` directly. Leave those untouched. Convert only the tests that open a `Chisel` via `NamedTempFile + Chisel::open`.

- [ ] **Step 1: Identify conversion candidates**

Run: `grep -n 'Chisel::open' tests/basic_ops.rs`
Expected: a list of line numbers where `Chisel::open(...)` is called. Each such test is a conversion candidate. Tests that use `Chisel::open` combined with behavior that requires an on-disk file (e.g., reopening the same path, checking file size via `std::fs::metadata`) stay file-only — note them but don't convert.

- [ ] **Step 2: Add `mod common;` at the top of the file**

```rust
mod common;
use common::{Backing, open_chisel};
```

Remove the `use tempfile::NamedTempFile;` import only if no remaining test uses it directly. Leave it otherwise.

- [ ] **Step 3: For each Chisel-level test, refactor its body into a `fn body(b: &Backing)` and invoke `dual_backing_test!`**

Pattern (applied to every candidate identified in Step 1):

Before:
```rust
#[test]
fn test_basic_allocate_and_read() {
    let tmp = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(tmp.path(), Options::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"hello".to_vec());
}
```

After:
```rust
fn test_basic_allocate_and_read_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"hello".to_vec());
}

dual_backing_test!(test_basic_allocate_and_read, test_basic_allocate_and_read_body);
```

For tests that take custom Options, use `common::open_chisel_with(b, opts)` instead.

For tests that deliberately close and reopen the same `Chisel` (e.g., persistence checks), leave them file-only — they have no memory-mode counterpart and must stay as `Chisel::open(tmp.path(), ...)`.

- [ ] **Step 4: Run the file**

Run: `cargo test --test basic_ops`
Expected: every converted test now has a `_file` and `_memory` variant, and all pass.

- [ ] **Step 5: Clippy + fmt + commit**

```bash
cargo clippy -- -D warnings && cargo fmt -- --check
git add tests/basic_ops.rs
git commit -m "Convert basic_ops Chisel-level tests to dual-backing

Tests that open a Chisel via NamedTempFile now run against both file
and memory backings via dual_backing_test!. Raw-buffer and PageIo-
level unit tests in the same file are unchanged."
```

---

## Task 6: Convert `tests/transactions.rs` to dual-backing

**Files:**
- Modify: `tests/transactions.rs`

Same pattern as Task 5: identify Chisel-level tests, refactor each body into a `fn body(b: &Backing)`, and apply `dual_backing_test!`. Tests that specifically test file persistence (open, write, drop, reopen same path) stay file-only.

- [ ] **Step 1: Identify candidates**

Run: `grep -n 'Chisel::open' tests/transactions.rs`
Expected: list of call sites.

- [ ] **Step 2: Add `mod common;` + imports**

```rust
mod common;
use common::{Backing, open_chisel, open_chisel_with};
```

- [ ] **Step 3: Convert each candidate using the Task 5 pattern**

For each identified test, apply the before/after transformation from Task 5 Step 3. Use `open_chisel_with(b, opts)` when the original test passes custom `Options`.

- [ ] **Step 4: Run the file**

Run: `cargo test --test transactions`
Expected: every converted test has `_file` and `_memory` variants, all pass.

- [ ] **Step 5: Clippy + fmt + commit**

```bash
cargo clippy -- -D warnings && cargo fmt -- --check
git add tests/transactions.rs
git commit -m "Convert transactions tests to dual-backing"
```

---

## Task 7: Convert `tests/api_edge_cases.rs` to dual-backing

**Files:**
- Modify: `tests/api_edge_cases.rs`

Same pattern as Task 5.

- [ ] **Step 1:** `grep -n 'Chisel::open' tests/api_edge_cases.rs`.
- [ ] **Step 2:** Add `mod common; use common::{Backing, open_chisel, open_chisel_with};`.
- [ ] **Step 3:** Convert each Chisel-opening test via the Task 5 Step 3 pattern.
- [ ] **Step 4:** Run `cargo test --test api_edge_cases`. Expect all pass with `_file` and `_memory` variants.
- [ ] **Step 5:** `cargo clippy -- -D warnings && cargo fmt -- --check`, then commit:

```bash
git add tests/api_edge_cases.rs
git commit -m "Convert api_edge_cases tests to dual-backing"
```

---

## Task 8: Convert `tests/defrag.rs` to dual-backing

**Files:**
- Modify: `tests/defrag.rs`

Same pattern as Task 5. Defrag is backing-agnostic.

- [ ] **Step 1:** `grep -n 'Chisel::open' tests/defrag.rs`.
- [ ] **Step 2:** Add `mod common; use common::{Backing, open_chisel, open_chisel_with};`.
- [ ] **Step 3:** Convert each Chisel-opening test via the Task 5 Step 3 pattern.
- [ ] **Step 4:** Run `cargo test --test defrag`. Expect all pass with `_file` and `_memory` variants.
- [ ] **Step 5:** `cargo clippy -- -D warnings && cargo fmt -- --check`, then commit:

```bash
git add tests/defrag.rs
git commit -m "Convert defrag tests to dual-backing"
```

---

## Task 9: Convert `tests/overflow.rs` to dual-backing

**Files:**
- Modify: `tests/overflow.rs`

Same pattern.

- [ ] **Step 1:** `grep -n 'Chisel::open' tests/overflow.rs`.
- [ ] **Step 2:** Add `mod common; use common::{Backing, open_chisel, open_chisel_with};`.
- [ ] **Step 3:** Convert each Chisel-opening test via the Task 5 Step 3 pattern.
- [ ] **Step 4:** Run `cargo test --test overflow`. Expect all pass with `_file` and `_memory` variants.
- [ ] **Step 5:** `cargo clippy -- -D warnings && cargo fmt -- --check`, then commit:

```bash
git add tests/overflow.rs
git commit -m "Convert overflow tests to dual-backing"
```

---

## Task 10: Convert `tests/stress.rs` to dual-backing

**Files:**
- Modify: `tests/stress.rs`

Same pattern. Stress tests may be slow; running both backings roughly doubles wallclock for this file (memory is faster, but not negligible). That's acceptable.

- [ ] **Step 1:** `grep -n 'Chisel::open' tests/stress.rs`.
- [ ] **Step 2:** Add `mod common; use common::{Backing, open_chisel, open_chisel_with};`.
- [ ] **Step 3:** Convert each Chisel-opening test via the Task 5 Step 3 pattern.
- [ ] **Step 4:** Run `cargo test --test stress`. Expect all pass with `_file` and `_memory` variants.
- [ ] **Step 5:** `cargo clippy -- -D warnings && cargo fmt -- --check`, then commit:

```bash
git add tests/stress.rs
git commit -m "Convert stress tests to dual-backing"
```

---

## Task 11: Partial conversion of `tests/error_and_format.rs` and `tests/options_validation.rs`

Some tests in these files are inherently file-only (write corrupt bytes via `std::fs::write`, read back the file for layout inspection, test `read_only` flow, test `create_if_missing`). Others — format-version mismatch detection, superblock checksum mismatch triggered *through* the API rather than via on-disk poking, options validation that does not depend on the filesystem — apply equally in memory mode.

**Files:**
- Modify: `tests/error_and_format.rs`
- Modify: `tests/options_validation.rs`

- [ ] **Step 1: Audit `tests/error_and_format.rs`**

Run: `grep -n -E 'Chisel::open|std::fs::write|std::fs::read' tests/error_and_format.rs`
Expected: a list of tests. Classify each:

- **Convert to dual-backing:** tests that only use the Chisel API surface to trigger an error (e.g., calling a method after poisoning via normal API, verifying an error from an invalid handle).
- **Keep file-only:** tests that write or read the backing file directly (`std::fs::write`, `std::fs::read`, `std::fs::OpenOptions`), that exercise the flock behavior, that corrupt the superblock on disk, or that test format-version mismatch by reopening a file whose header was edited externally.

- [ ] **Step 2: Apply dual-backing to the API-only tests**

Use the Task 5 Step 3 pattern. Add `mod common;` and imports at the top of the file.

- [ ] **Step 3: Audit `tests/options_validation.rs`**

Run: `grep -n 'Chisel::open' tests/options_validation.rs`
Expected: a list of tests. Classify each:

- **Convert to dual-backing (use `open_chisel_with`):** `superblock_count` validation (memory mode applies the same check), `cache_size` behavior (applies equally), any option whose validation happens before I/O.
- **Keep file-only:** `create_if_missing` (no file concept in memory), `read_only` opening an existing file (no file to reopen), any test that relies on file persistence.

- [ ] **Step 4: Apply dual-backing to the memory-compatible options tests**

Use `common::open_chisel_with(b, opts)` so each test can supply its custom options through the harness.

For tests that assert the Options are rejected (e.g., `superblock_count: 1`), duplicate the negative-path assertion explicitly against both `Chisel::open` and `Chisel::open_in_memory_with_options` rather than using the macro — the macro expects a successful `open` on both backings.

Example negative-path test:

```rust
#[test]
fn superblock_count_below_min_is_rejected() {
    let bad = Options { superblock_count: 1, ..Options::default() };

    // File-backed: rejection happens before the file is opened.
    let tmp = NamedTempFile::new().unwrap();
    assert!(matches!(
        Chisel::open(tmp.path(), bad.clone()).unwrap_err(),
        ChiselError::InvalidSuperblockCount { value: 1 }
    ));

    // Memory-backed: same validation path.
    assert!(matches!(
        Chisel::open_in_memory_with_options(bad).unwrap_err(),
        ChiselError::InvalidSuperblockCount { value: 1 }
    ));
}
```

- [ ] **Step 5: Run both files**

Run: `cargo test --test error_and_format --test options_validation`
Expected: every converted test has `_file` and `_memory` variants; file-only tests still pass as before.

- [ ] **Step 6: Final full-suite green light**

Run: `cargo test && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: all green. At this point the memory backing is exercised by every applicable behavior test in the suite.

- [ ] **Step 7: Commit**

```bash
git add tests/error_and_format.rs tests/options_validation.rs
git commit -m "Dual-backing conversion for error_and_format and options_validation

Converts tests that exercise the Chisel API surface only; file-
corruption and file-persistence tests remain file-only as designed."
```

---

## Post-implementation checklist

- [ ] `cargo test` passes.
- [ ] `cargo clippy -- -D warnings` passes.
- [ ] `cargo fmt -- --check` passes.
- [ ] `tests/crash_recovery.rs` remains file-only (confirmed by grep: no `mod common` line, no `dual_backing_test!` calls).
- [ ] `src/page_io.rs::read_only_tests` remains file-only.
- [ ] No new public API beyond `Chisel::open_in_memory` and `Chisel::open_in_memory_with_options`.
- [ ] CLAUDE.md is updated if any design decision changed during implementation (otherwise no-op).
