# Handle/Tag Newtype Reshape Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the primitive-obsessed public surface (handles as `u64`, tags as `u32`, "no tag" as the in-band sentinel `0`, and the misnamed `DefragOptions::max_pages`) with opaque `Handle`/`Tag` newtypes at the API skin — closing ISSUES.md I120, I126, and I122.

**Architecture:** A new public-layer module `src/handle.rs` defines `Handle(u64)` (`#[repr(transparent)]`) and `Tag(NonZeroU32)` with pragmatic ergonomics (`From`, `PartialEq` against the raw primitive, `Display`). The newtypes live ABOVE the engine — `lib.rs` and `python/src` convert at the edge via `.get()`/`From`, and nothing in `transaction.rs`/`handle_table.rs`/`membership_index.rs` ever sees a newtype. The on-disk format and the radix-tree key arithmetic stay raw integers. See `docs/specs/2026-06-21-handle-tag-newtype-reshape-design.md`.

**Tech Stack:** Rust 2021 (MSRV 1.82), engine crate `chisel` (`src/`). PyO3 binding crate `chisel-py` (`python/`), built with maturin. `cargo test`/`clippy`/`fmt`; `pytest`/`hypothesis`. No new dependencies (`NonZeroU32` is std).

**Conventions:** Run from repo root. Commits use no AI-referencing text. The CI gate this plan must keep green: `cargo build`, `cargo test`, `cargo clippy --workspace -- -D warnings`, `cargo fmt -- --check`, and (from `python/`, in a venv) `maturin develop --release` + `pytest -v`. Each task ends green and is committed; the public-API flip (Task 3) is unavoidably one atomic commit because the newtype change breaks compilation crate-wide until the call-site sweep restores it.

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/handle.rs` | Public newtypes `Handle`, `Tag`; relocated `TagDropProgress` | **Create** (Task 2: newtypes; Task 3: `TagDropProgress`) |
| `src/lib.rs` | Public `Chisel` facade | Add `pub(crate) mod handle;` + re-exports; flip every handle/tag method signature; convert at the edge |
| `src/transaction.rs` | `TransactionManager` engine layer | `delete_with_tag`/`_inner` return `(Vec<u64>, bool)` instead of `TagDropProgress`. Everything else stays `u64`/`u32` |
| `src/membership_index.rs` | Membership index engine layer | Remove the `TagDropProgress` struct definition (relocated to `handle.rs`) |
| `src/defrag.rs` | `DefragOptions` | Rename `max_pages` → `max_values` (field, builder, `Default`, consumer, doc) |
| `python/src/db.rs` | PyO3 `PyChisel` methods | Convert `u64`↔`Handle`/`u32`↔`Tag` at the edge; `require_tag` helper; `tag()`→`Option<u32>`; `getattr("max_values")` |
| `python/src/transaction.rs` | PyO3 `Transaction` wrappers | Match changed return signatures (`tag()`→`Option<u32>`); delegate unchanged otherwise |
| `python/chisel/__init__.py` | Python dataclasses + docstrings | `DefragOptions.max_values`; doc the tag-0 `ValueError` |
| `python/chisel/chisel.pyi` | Type stubs | `tag(...) -> int | None` (both classes); `max_values` |
| `tests/*.rs`, `src/recovery_tests.rs`, `bench/**` | Rust call sites | Mechanical sweep (compiler-driven) + new behavior tests |
| `python/tests/*.py` | Python call sites | `max_values` rename; new tag-0 `ValueError` + `tag()`→`None` tests |
| `ISSUES.md` | Issue log | Mark I120, I126, I122 resolved |

---

## Task 1: Rename `DefragOptions::max_pages` → `max_values` (I122)

Independent of the newtypes; do it first as a self-contained green commit. The field has counted *values relocated* (not pages) since R3; the name is now honest.

**Files:**
- Modify: `src/defrag.rs:60-105` (doc comment, struct field, `Default`, builder method)
- Modify: `src/defrag.rs:209` (the one consumer)
- Modify: `tests/defrag.rs` (the `.max_pages(2)` call site)
- Modify: `python/chisel/__init__.py:94-106` (dataclass field + docstring)
- Modify: `python/chisel/chisel.pyi` (the `max_pages: int = 0` stub line)
- Modify: `python/src/db.rs:413-418` (the `getattr` + builder call)
- Modify: `python/tests/test_stats_defrag.py:80` (the `max_pages=10` call site)

- [ ] **Step 1: Rename in `src/defrag.rs`**

In the doc comment (lines 65-70), replace the `` `max_pages` `` paragraph with:

```rust
/// `max_values`: soft cap on work per call, so a very large database can
/// be defragged incrementally across several transactions. `0` means no
/// limit. The cap counts values relocated, not pages touched. Breaking
/// the loop early leaves the transaction in a valid state; the caller
/// chooses commit vs rollback.
```

In the struct (line 81) `pub max_pages: usize,` → `pub max_values: usize,`.
In `Default` (line 88) `max_pages: 0,` → `max_values: 0,`.
In the builder (lines 101-104):

```rust
    pub fn max_values(mut self, cap: usize) -> Self {
        self.max_values = cap;
        self
    }
```

At the consumer (line 209):

```rust
            if options.max_values > 0 && stats.values_moved >= options.max_values as u64 {
```

Also update the builder usage comment at line 95 (`...max_pages(100)` → `...max_values(100)`).

- [ ] **Step 2: Update the Rust test call site**

In `tests/defrag.rs`, change `DefragOptions::default().sparse_threshold(0.25).max_pages(2)` → `...max_values(2)` (and any other `.max_pages(` occurrence in that file).

- [ ] **Step 3: Run the Rust gate**

Run: `cargo test --test defrag && cargo clippy --workspace -- -D warnings`
Expected: PASS / no warnings. (`cargo build` first if you want a faster failure.)

- [ ] **Step 4: Rename on the Python side**

`python/chisel/__init__.py` (lines 100-106): rename the field and rewrite the docstring:

```python
    max_values: cap on VALUES relocated in one pass; 0 means no limit.
        The current implementation caps value moves, which is the useful
        knob for bounding pass cost. Default 0.
    """
    sparse_threshold: float = 0.25
    max_values: int = 0
```

`python/chisel/chisel.pyi`: change the `max_pages: int = 0` line in the `DefragOptions` stub to `max_values: int = 0`.

`python/src/db.rs` (lines 413-418): `getattr("max_pages")` → `getattr("max_values")`, the local `max_pages` binding → `max_values`, and `.max_pages(max_pages)` → `.max_values(max_values)`.

`python/tests/test_stats_defrag.py:80`: `chisel.DefragOptions(sparse_threshold=0.5, max_pages=10)` → `max_values=10`.

- [ ] **Step 5: Run the Python gate**

Run (from `python/`, in the project venv): `maturin develop --release && pytest -v tests/test_stats_defrag.py`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/defrag.rs tests/defrag.rs python/chisel/__init__.py python/chisel/chisel.pyi python/src/db.rs python/tests/test_stats_defrag.py
git commit -m "refactor: rename DefragOptions::max_pages to max_values (I122)"
```

---

## Task 2: `Handle` and `Tag` newtypes in `src/handle.rs`

Self-contained, unit-tested. The types are defined and re-exported but not yet used by the public API (that flip is Task 3), so the crate stays green.

**Files:**
- Create: `src/handle.rs`
- Modify: `src/lib.rs` (add `pub(crate) mod handle;` near the other module decls, ~line 36; add `pub use handle::{Handle, Tag};` near the other re-exports, ~line 62)

- [ ] **Step 1: Write `src/handle.rs` with the newtypes and unit tests**

```rust
// src/handle.rs — public newtypes for the Chisel API surface.
//
// Role in system: the public boundary's value types. `Handle` and `Tag` wrap
// the engine's raw `u64`/`u32` ids so the public API is not primitive-obsessed
// — `delete_tagged(Handle, Tag)` cannot be called with its arguments
// transposed, and "no tag" is the absence of a `Tag` (`Option<Tag>`), never the
// in-band sentinel `0`. These types live ABOVE the engine: nothing in
// transaction.rs / handle_table.rs / membership_index.rs knows about them;
// lib.rs converts at the edge via `.get()` / `From`. The on-disk format and the
// radix-tree key arithmetic stay raw integers (ISSUES.md I120/I126).
//
// Ergonomics decision (ISSUES.md I120): the newtypes carry `PartialEq` against
// their raw primitive and `Display`, so existing call sites that compare or
// print ids keep compiling. Distinctness — not opacity of comparison — is what
// blocks transposition.

use std::fmt;
use std::num::NonZeroU32;

/// A stable, opaque chunk handle. Newtype over the engine's `u64` id.
///
/// `#[repr(transparent)]` is load-bearing, not decoration: the bench adapter
/// reinterprets a `&[u64]` slice as `&[Handle]` without copying, which is sound
/// only because `Handle` has identical layout to `u64`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(u64);

impl Handle {
    /// The underlying raw id. Handles are minted from `1` (the engine reserves
    /// `0` as the "no handle" sentinel); this carrier type does not itself
    /// enforce non-zero.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for Handle {
    fn from(v: u64) -> Self {
        Handle(v)
    }
}

impl From<Handle> for u64 {
    fn from(h: Handle) -> Self {
        h.0
    }
}

// PartialEq against the raw primitive in both directions so `assert_eq!(h, 1)`
// and `assert_eq!(1, h)` both compile. The integer literal infers as `u64`
// because that is the only integer type `Handle` compares against.
impl PartialEq<u64> for Handle {
    fn eq(&self, other: &u64) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Handle> for u64 {
    fn eq(&self, other: &Handle) -> bool {
        *self == other.0
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A non-zero chunk tag. "No tag" is the ABSENCE of a `Tag` (`Option<Tag>`) —
/// `Tag(0)` is unconstructable, which makes the untagged sentinel expressible
/// in the type (ISSUES.md I126). The engine still stores the tag as a `u32`
/// with `0` meaning untagged; the `0 <-> None` mapping happens at the lib.rs
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag(NonZeroU32);

impl Tag {
    /// Construct a `Tag`, returning `None` if `v == 0`. Mirrors
    /// `NonZeroU32::new`.
    #[must_use]
    pub const fn new(v: u32) -> Option<Tag> {
        match NonZeroU32::new(v) {
            Some(nz) => Some(Tag(nz)),
            None => None,
        }
    }

    /// The underlying raw tag value (always `>= 1`).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Error from `Tag::try_from(0)`. Zero-size; deliberately NOT a `ChiselError`
/// variant — tag construction is fallible only at the API boundary, never
/// inside the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroTagError;

impl fmt::Display for ZeroTagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tag must be non-zero (0 is the untagged sentinel)")
    }
}

impl std::error::Error for ZeroTagError {}

impl TryFrom<u32> for Tag {
    type Error = ZeroTagError;
    fn try_from(v: u32) -> Result<Tag, ZeroTagError> {
        Tag::new(v).ok_or(ZeroTagError)
    }
}

impl From<Tag> for u32 {
    fn from(t: Tag) -> Self {
        t.0.get()
    }
}

impl PartialEq<u32> for Tag {
    fn eq(&self, other: &u32) -> bool {
        self.get() == *other
    }
}

impl PartialEq<Tag> for u32 {
    fn eq(&self, other: &Tag) -> bool {
        *self == other.get()
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_round_trips_u64() {
        let h = Handle::from(42);
        assert_eq!(h.get(), 42);
        assert_eq!(u64::from(h), 42);
        assert_eq!(h, 42u64); // PartialEq<u64>
        assert_eq!(42u64, h); // reverse direction
    }

    #[test]
    fn handle_displays_as_raw() {
        assert_eq!(format!("{}", Handle::from(7)), "7");
    }

    #[test]
    fn tag_new_rejects_zero() {
        assert_eq!(Tag::new(0), None);
        assert_eq!(Tag::new(5).unwrap().get(), 5);
    }

    #[test]
    fn tag_try_from_zero_errors() {
        assert!(Tag::try_from(0).is_err());
        assert_eq!(Tag::try_from(9).unwrap().get(), 9);
    }

    #[test]
    fn tag_eq_and_display() {
        let t = Tag::new(3).unwrap();
        assert_eq!(t, 3u32);
        assert_eq!(3u32, t);
        assert_eq!(format!("{t}"), "3");
        assert_eq!(u32::from(t), 3);
    }
}
```

- [ ] **Step 2: Wire the module into `src/lib.rs`**

Near the other `pub(crate) mod` declarations (around line 36), add:

```rust
pub(crate) mod handle;
```

Near the other `pub use` re-exports (around line 62, beside `pub use defrag::{DefragOptions, DefragStats};`), add:

```rust
pub use handle::{Handle, Tag};
```

(`TagDropProgress` is added to this re-export in Task 3.)

- [ ] **Step 3: Run the gate**

Run: `cargo test --lib handle:: && cargo clippy --workspace -- -D warnings && cargo fmt -- --check`
Expected: 5 handle tests PASS; no clippy warnings; fmt clean.

- [ ] **Step 4: Commit**

```bash
git add src/handle.rs src/lib.rs
git commit -m "feat: add Handle/Tag newtypes (I120, I126) — types only, not yet wired"
```

---

## Task 3: Flip the public `Chisel` surface + relocate `TagDropProgress` (Rust)

This is the atomic change. Flipping `lib.rs` breaks every Rust caller until the sweep restores compilation, so steps 1-7 land as **one commit**. Strategy is compiler-driven: make the type changes, then run `cargo build` and fix exactly what it flags using the transform rules in Step 5.

**Files:**
- Modify: `src/handle.rs` (add `TagDropProgress`)
- Modify: `src/membership_index.rs:529-540` (remove `TagDropProgress`)
- Modify: `src/transaction.rs:2087-2135` (`delete_with_tag`/`_inner` return `(Vec<u64>, bool)`)
- Modify: `src/lib.rs:440-609` (flip the `Chisel` method block) + re-export
- Modify: `tests/*.rs`, `src/recovery_tests.rs`, `bench/src/chisel_engine.rs`, `bench/tests/*.rs` (sweep)
- Modify: `tests/tag_ops.rs` (new behavior tests)

- [ ] **Step 1: Relocate `TagDropProgress` to `src/handle.rs`**

Append to `src/handle.rs` (after the `Tag` impls, before `mod tests`):

```rust
/// Progress report from `Chisel::delete_with_tag`. `deleted` lists the handles
/// removed in this pass (the engine deletes their chunks); `complete` is `true`
/// when the tag has no remaining members. Produced only on the success path —
/// a mid-pass error returns `Err` and no progress (the per-delete state stays
/// consistent; only the reporting is lost).
// #[must_use] (ISSUES.md I121): `complete` is the field a resumable drop loops on.
#[must_use = "TagDropProgress.complete tells you whether the tag drop finished; loop on it or bind with `let _`"]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TagDropProgress {
    /// Handles removed from the index in this pass. May be fewer than `max` if
    /// the tag emptied first.
    pub deleted: Vec<Handle>,
    /// `true` if the tag has no remaining members after this pass.
    pub complete: bool,
}
```

In `src/membership_index.rs`, delete the `TagDropProgress` struct (lines ~525-540: the doc comment block, the `#[must_use]`/`#[non_exhaustive]`/`#[derive]` attributes, and the `pub struct TagDropProgress { ... }`). Leave the surrounding `MembershipIndex` code intact. Remove any now-unused doc lines referring only to that struct.

In `src/lib.rs`, change the re-export `pub use membership_index::TagDropProgress;` (line 63) — delete it — and extend the Task-2 re-export to:

```rust
pub use handle::{Handle, Tag, TagDropProgress};
```

- [ ] **Step 2: Change the engine `delete_with_tag` to return a tuple**

In `src/transaction.rs`, the engine layer must not reference `TagDropProgress` (it now lives above the engine). Rewrite `delete_with_tag` and `delete_with_tag_inner` (lines 2087-2135) to return `(Vec<u64>, bool)`:

```rust
    pub fn delete_with_tag(&mut self, tag: u32, max: usize) -> Result<(Vec<u64>, bool)> {
        self.check_alive()?;
        let result = self.delete_with_tag_inner(tag, max);
        self.poison_on_fatal(result)
    }

    fn delete_with_tag_inner(&mut self, tag: u32, max: usize) -> Result<(Vec<u64>, bool)> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if max == 0 {
            return Ok((Vec::new(), false));
        }
        // (keep the existing bounded-enumeration comment block here unchanged)
        let members = {
            let root = self.current_roots.membership_index_page;
            let mut cache = self.cache.borrow_mut();
            self.membership_index.handles_for_tag_bounded(
                &mut cache,
                root,
                tag,
                max.saturating_add(1),
            )?
        };
        let complete = members.len() <= max;
        let take: Vec<u64> = members.into_iter().take(max).collect();
        for &h in &take {
            self.delete_inner(h)?;
        }
        Ok((take, complete))
    }
```

Remove the now-unused `TagDropProgress` import in `transaction.rs` if one exists (search for `TagDropProgress` in the file).

- [ ] **Step 3: Flip the `Chisel` method signatures in `src/lib.rs`**

Replace the bodies of the handle/tag methods (the block spanning roughly lines 451-609) with the newtyped versions below. Engine delegations are otherwise unchanged. Update each method's doc comment where it says "u64 identifiers" / "Returns 0 for untagged" to describe `Handle`/`Option<Tag>` (keep the surrounding prose).

```rust
    pub fn allocate(&mut self, value: &[u8]) -> Result<Handle> {
        self.txm.allocate(value).map(Handle::from)
    }

    pub fn allocate_tagged(&mut self, value: &[u8], tag: Tag) -> Result<Handle> {
        self.txm.allocate_tagged(value, tag.get()).map(Handle::from)
    }

    /// Return the tag stored for `handle`, or `None` if the handle is untagged.
    pub fn tag(&self, handle: Handle) -> Result<Option<Tag>> {
        self.txm.tag(handle.get()).map(Tag::new) // stored 0 -> None
    }

    pub fn client_byte(&self, handle: Handle) -> Result<u8> {
        self.txm.client_byte(handle.get())
    }

    pub fn set_client_byte(&mut self, handle: Handle, byte: u8) -> Result<()> {
        self.txm.set_client_byte(handle.get(), byte)
    }

    pub fn handles_with_tag(&self, tag: Tag) -> Result<Vec<Handle>> {
        Ok(self
            .txm
            .handles_with_tag(tag.get())?
            .into_iter()
            .map(Handle::from)
            .collect())
    }

    pub fn read(&self, handle: Handle) -> Result<Vec<u8>> {
        self.txm.read(handle.get())
    }

    pub fn update(&mut self, handle: Handle, value: &[u8]) -> Result<()> {
        self.txm.update(handle.get(), value)
    }

    pub fn delete(&mut self, handle: Handle) -> Result<()> {
        self.txm.delete(handle.get())
    }

    pub fn delete_tagged(&mut self, handle: Handle, tag: Tag) -> Result<()> {
        self.txm.delete_tagged(handle.get(), tag.get())
    }

    pub fn delete_with_tag(&mut self, tag: Tag, max: usize) -> Result<TagDropProgress> {
        let (ids, complete) = self.txm.delete_with_tag(tag.get(), max)?;
        Ok(TagDropProgress {
            deleted: ids.into_iter().map(Handle::from).collect(),
            complete,
        })
    }

    pub fn delete_many(&mut self, handles: &[Handle]) -> Result<()> {
        // Copy to a raw Vec; bulk delete is far below the fsync floor, so the
        // allocation is immaterial. (The bench adapter does the zero-copy
        // reinterpret where it matters; the engine API stays simple here.)
        let raw: Vec<u64> = handles.iter().map(|h| h.get()).collect();
        self.txm.delete_many(&raw)
    }

    pub fn set_root_name(&mut self, name: &str, handle: Handle) -> Result<()> {
        self.txm.set_root_name(name, handle.get())
    }

    pub fn get_root_name(&self, name: &str) -> Result<Option<Handle>> {
        Ok(self.txm.get_root_name(name)?.map(Handle::from))
    }

    pub fn handles(&self) -> Result<Vec<Handle>> {
        Ok(self.txm.handles()?.into_iter().map(Handle::from).collect())
    }
```

Note `stats()` (lib.rs ~616) calls `self.txm.handles()` directly (engine layer, still `Vec<u64>`) for `handle_count` — that is unaffected and must NOT be routed through the newtyped `Chisel::handles()`. Leave `stats()` as-is.

- [ ] **Step 4: Run `cargo build` to enumerate the call-site breakage**

Run: `cargo build 2>&1 | tee /tmp/reshape-errors.txt`
Expected: FAIL with type-mismatch errors across `tests/`, `src/recovery_tests.rs`, and `bench/`. This list IS the sweep worklist.

- [ ] **Step 5: Sweep Rust call sites using these transform rules**

Apply per the compiler errors. Most sites compile **untouched** (a `Handle` flows straight back into the next call, and `assert_eq!(h, 1)` still works via `PartialEq<u64>`). Only these patterns break:

1. **Tag construction** — any literal/`u32` tag argument:
   `allocate_tagged(v, 42)` → `allocate_tagged(v, Tag::new(42).unwrap())`;
   likewise `handles_with_tag(42)`, `delete_tagged(h, 42)`, `delete_with_tag(42, max)`.
   Add `use chisel::Tag;` (integration tests) / `use crate::Tag;` (`recovery_tests.rs`) where needed.
2. **`tag()` return is now `Option<Tag>`**:
   `assert_eq!(db.tag(h)?, 42)` → `assert_eq!(db.tag(h)?.unwrap(), 42)` (tagged case; `Tag: PartialEq<u32>`);
   untagged assertion → `assert_eq!(db.tag(h)?, None)`.
3. **`Vec<u64>` of handles** (4 sites: `tests/defrag.rs`, `tests/iteration_stability.rs`, `tests/page_reclamation.rs`, `src/recovery_tests.rs`):
   change the annotation to `Vec<Handle>`. If the vec is compared to an **integer-literal vec** (`assert_eq!(db.handles()?, vec![1, 2, 3])`), map to raw first: `let got: Vec<u64> = db.handles()?.iter().map(|h| h.get()).collect();` then compare to `vec![1u64, 2, 3]`. (Comparing two `Vec<Handle>` to each other — the iteration-stability repeatability checks — works unchanged.)
4. **`HashMap<u64, _>` keyed by handle** (2 sites in `tests/page_reclamation.rs`, ~line 150 and the `HashMap<u32, Vec<u64>>` at ~240): change the key type to `Handle` (it derives `Hash`+`Eq`); the tag-keyed map's *value* `Vec<u64>` → `Vec<Handle>`. Inserts/lookups with `Handle` keys then compile.
5. **Handle arithmetic** (`src/recovery_tests.rs:172` `committed_handle + 1`): `Handle::from(committed_handle.get() + 1)`.
6. **Bench adapter** (`bench/src/chisel_engine.rs`): `allocate()` now returns `Handle` — store it via `.get()` into the adapter's `Identifier(u64)`, and convert back with `Handle::from(id.0)` on `read`/`update`/`delete`. For the `delete_many` zero-copy path (the existing transmute, ~lines 98-100): build `let raw: Vec<Handle> = ids.iter().map(|i| Handle::from(i.0)).collect(); db.delete_many(&raw);` — or, if preserving the zero-copy reinterpret, transmute `&[Identifier]` → `&[Handle]` (both `#[repr(transparent)]` over `u64`; document the SAFETY). Prefer the simple collect unless a bench regression shows it matters.

Re-run `cargo build` after each file; iterate until it compiles.

- [ ] **Step 6: Add new behavior tests in `tests/tag_ops.rs`**

```rust
#[test]
fn tag_of_untagged_handle_is_none() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"plain").unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap(), None);
}

#[test]
fn tag_of_tagged_handle_is_some() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate_tagged(b"row", Tag::new(42).unwrap()).unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap(), Some(Tag::new(42).unwrap()));
    assert_eq!(db.tag(h).unwrap().unwrap(), 42); // PartialEq<u32> ergonomic
}
```

Ensure `tests/tag_ops.rs` imports `Tag` (`use chisel::Tag;` or via the existing glob). The transposition guarantee (`delete_tagged(handle, tag)` can't swap args) is enforced by the type system at compile time and needs no runtime test.

- [ ] **Step 7: Run the full Rust gate, then commit**

Run: `cargo test && cargo clippy --workspace -- -D warnings && cargo fmt -- --check`
Expected: entire suite PASS (note: full `cargo test`, not `--lib` — the integration tests in `tests/` are where the sweep landed); no clippy warnings; fmt clean.

```bash
git add -A
git commit -m "feat: reshape public Chisel API to Handle/Tag newtypes (I120, I126)"
```

---

## Task 4: Python boundary reshape (tag-0 → `ValueError`, `tag()` → `int | None`)

The Rust core now takes `Tag` / returns `Handle`/`Option<Tag>`, so the PyO3 layer must convert at the edge. Handles stay Python `int`; tags stay `int` (or `int | None`). This is a separate compile unit from the engine, so it is its own green commit.

**Files:**
- Modify: `python/src/db.rs` (the `PyChisel` handle/tag methods + a `require_tag` helper)
- Modify: `python/src/transaction.rs` (match the changed `tag()` return signature)
- Modify: `python/chisel/chisel.pyi` (`tag(...) -> int | None` on both classes)
- Modify: `python/chisel/__init__.py` (doc the tag-0 `ValueError`)
- Modify: `python/tests/test_tags.py` (new `ValueError` + `None` tests)

- [ ] **Step 1: Add the `require_tag` helper and convert the `PyChisel` methods in `python/src/db.rs`**

Add near the top of the `impl PyChisel` block (or as a free helper in the module):

```rust
// Convert a Python-supplied u32 tag into a non-zero `chisel::Tag`, raising
// Python ValueError on 0. Tag 0 is no longer a valid value — "untagged" is
// expressed by calling `allocate` instead of `allocate_tagged` (ISSUES.md I126).
fn require_tag(tag: u32) -> PyResult<chisel::Tag> {
    chisel::Tag::new(tag)
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("tag must be non-zero"))
}
```

Convert every `PyChisel` method that crosses a handle or tag (`u64`↔`Handle`, `u32`↔`Tag`). The engine closures now receive/return newtypes:

```rust
    pub(crate) fn allocate(&self, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(|c| c.allocate(&bytes).map(|h| h.get()))
    }

    pub(crate) fn allocate_tagged(&self, value: &Bound<'_, PyAny>, tag: u32) -> PyResult<u64> {
        let tag = require_tag(tag)?;
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(|c| c.allocate_tagged(&bytes, tag).map(|h| h.get()))
    }

    pub(crate) fn read<'py>(&self, py: Python<'py>, handle: u64) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        // (unchanged body except the engine call:)
        // ... self.with_inner_io(|c| c.read(chisel::Handle::from(handle))) ...
    }

    pub(crate) fn update(&self, handle: u64, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(|c| c.update(chisel::Handle::from(handle), &bytes))
    }

    pub(crate) fn delete(&self, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.delete(chisel::Handle::from(handle)))
    }

    pub(crate) fn delete_many(&self, handles: Vec<u64>) -> PyResult<()> {
        let hs: Vec<chisel::Handle> = handles.into_iter().map(chisel::Handle::from).collect();
        self.with_inner_mut_io(|c| c.delete_many(&hs))
    }

    fn handles(&self) -> PyResult<Vec<u64>> {
        self.with_inner_io(|c| c.handles().map(|v| v.into_iter().map(|h| h.get()).collect()))
    }

    pub(crate) fn tag(&self, handle: u64) -> PyResult<Option<u32>> {
        self.with_inner_io(|c| c.tag(chisel::Handle::from(handle)).map(|o| o.map(|t| t.get())))
    }

    pub(crate) fn handles_with_tag(&self, tag: u32) -> PyResult<Vec<u64>> {
        let tag = require_tag(tag)?;
        self.with_inner_io(|c| c.handles_with_tag(tag).map(|v| v.into_iter().map(|h| h.get()).collect()))
    }

    pub(crate) fn client_byte(&self, handle: u64) -> PyResult<u8> {
        self.with_inner_io(|c| c.client_byte(chisel::Handle::from(handle)))
    }

    pub(crate) fn set_client_byte(&self, handle: u64, byte: u8) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_client_byte(chisel::Handle::from(handle), byte))
    }

    pub(crate) fn delete_tagged(&self, handle: u64, tag: u32) -> PyResult<()> {
        let tag = require_tag(tag)?;
        self.with_inner_mut_io(|c| c.delete_tagged(chisel::Handle::from(handle), tag))
    }

    /// Returns (deleted: list[int], complete: bool).
    pub(crate) fn delete_with_tag(&self, tag: u32, max: usize) -> PyResult<(Vec<u64>, bool)> {
        let tag = require_tag(tag)?;
        self.with_inner_mut_io(|c| {
            c.delete_with_tag(tag, max)
                .map(|p| (p.deleted.into_iter().map(|h| h.get()).collect(), p.complete))
        })
    }

    pub(crate) fn set_root_name(&self, name: &str, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_root_name(name, chisel::Handle::from(handle)))
    }

    pub(crate) fn get_root_name(&self, name: &str) -> PyResult<Option<u64>> {
        self.with_inner_io(|c| c.get_root_name(name).map(|o| o.map(|h| h.get())))
    }
```

(Match the exact existing `read` signature/body — only its engine call changes to `chisel::Handle::from(handle)`. Confirm `chisel::Handle`/`chisel::Tag` are reachable; they are re-exported from the crate root by Task 2/3.)

- [ ] **Step 2: Update the `Transaction` wrapper signature in `python/src/transaction.rs`**

The wrappers delegate to `PyChisel`. Only the `tag` wrapper's return type changes:

```rust
    fn tag(&self, py: Python<'_>, handle: u64) -> PyResult<Option<u32>> {
        self.db.bind(py).borrow().tag(handle)
    }
```

All other wrappers (`allocate_tagged`, `handles_with_tag`, `delete_tagged`, `delete_with_tag`, `set_root_name`, `get_root_name`, etc.) keep their current signatures — the validation/conversion lives in `PyChisel`, which they already call.

- [ ] **Step 3: Update the stubs in `python/chisel/chisel.pyi`**

Both the `Chisel` and `Transaction` classes: `def tag(self, handle: int) -> int: ...` → `def tag(self, handle: int) -> int | None: ...`. Leave all other handle/tag stubs as `int`/`list[int]`. Optionally add a one-line note on `allocate_tagged`/`delete_tagged`/`handles_with_tag`/`delete_with_tag` that `tag` must be `>= 1` (raises `ValueError`).

- [ ] **Step 4: Document the tag-0 contract in `python/chisel/__init__.py`**

Where the tag API is described (module or class docstring), add: "Tags are integers `>= 1`. `0` is not a valid tag — an untagged value is created with `allocate` (not `allocate_tagged`), and `tag()` returns `None` for it. Passing `tag=0` raises `ValueError`."

- [ ] **Step 5: Add Python behavior tests in `python/tests/test_tags.py`**

```python
import pytest


def test_allocate_tagged_rejects_zero(mem_db):
    with pytest.raises(ValueError):
        mem_db.allocate_tagged(b"row", 0)


def test_tag_of_untagged_is_none(mem_db):
    h = mem_db.allocate(b"plain")
    assert mem_db.tag(h) is None


def test_tagged_handle_reports_tag(mem_db):
    h = mem_db.allocate_tagged(b"row", 9)
    assert mem_db.tag(h) == 9


def test_handles_with_tag_rejects_zero(mem_db):
    with pytest.raises(ValueError):
        mem_db.handles_with_tag(0)
```

(Use the existing `mem_db` fixture from the test module/conftest; match its name. If `allocate`/`tag` outside a transaction need an implicit txn in these tests, follow the pattern already used by the other tests in `test_tags.py`.)

- [ ] **Step 6: Build and run the Python gate**

Run (from `python/`, in the project venv): `maturin develop --release && pytest -v`
Expected: all Python tests PASS, including the four new ones.

- [ ] **Step 7: Run clippy on the binding and commit**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt -- --check`
Expected: no warnings (this lints `python/src` via `--workspace`, per the I108 CI note).

```bash
git add python/
git commit -m "feat: Python boundary for Tag(NonZeroU32) — tag()->int|None, tag 0 raises ValueError (I126)"
```

---

## Task 5: Close the issues and run the complete gate

**Files:**
- Modify: `ISSUES.md` (I120, I126, I122 entries)

- [ ] **Step 1: Mark the three issues resolved in `ISSUES.md`**

On the `#### I120.` heading add `✅ FIXED 2026-06-21` and a one-line resolution note (`Handle`/`Tag` newtypes at the lib.rs + PyO3 skin; engine stays raw integers; `delete_tagged` un-transposable). Same for `#### I126.` (`Tag(NonZeroU32)` + `Option<Tag>`; tag 0 unconstructable in Rust, `ValueError` in Python) and `#### I122.` (`max_pages` → `max_values`). Update the 2026-06-21 handoff status note near the top to move I120/I126/I122 from "STILL OPEN" to done.

- [ ] **Step 2: Run the entire CI gate locally**

Run:
```bash
cargo build && cargo test && cargo clippy --workspace -- -D warnings && cargo fmt -- --check
```
Then from `python/` in the venv:
```bash
maturin develop --release && pytest -v
```
Expected: everything green.

- [ ] **Step 3: Commit**

```bash
git add ISSUES.md
git commit -m "docs: mark I120/I126/I122 resolved (Handle/Tag newtype reshape)"
```

- [ ] **Step 4: Open the PR**

```bash
git push -u origin newtype-reshape
gh pr create --base main --title "Handle/Tag newtype reshape (I120, I126, I122)" --body "$(cat <<'EOF'
Replaces the primitive-obsessed public surface with opaque `Handle`/`Tag` newtypes at the API skin.

- **I120** — `Handle(u64)` / `Tag(NonZeroU32)` newtypes; `delete_tagged(Handle, Tag)` is un-transposable; `handles()` returns `Vec<Handle>`.
- **I126** — `Option<Tag>` replaces the tag-0 untagged sentinel; `Tag(0)` is unconstructable (Rust), tag 0 raises `ValueError` (Python).
- **I122** — `DefragOptions::max_pages` renamed to `max_values`.

Engine internals (`transaction.rs`, `handle_table.rs`, `membership_index.rs`, on-disk format, radix math) are unchanged — this is a boundary reshape. Python handles/tags stay plain `int`/`int | None`.

Spec: `docs/specs/2026-06-21-handle-tag-newtype-reshape-design.md`
Plan: `docs/plans/2026-06-21-handle-tag-newtype-reshape.md`
EOF
)"
```

---

## Self-review notes (for the implementer)

- **Spec coverage:** every spec section maps to a task — newtypes (T2), `Chisel` flip table (T3 §3), `TagDropProgress` relocation (T3 §1-2), tag-0/`Option<Tag>` (T3 + T4), Python plain-int boundary (T4), `max_values` (T1), testing/gate (every task's gate step + T5).
- **The atomic-commit reality:** Task 3 cannot be split into smaller green commits because `allocate`'s output type feeds `read`'s input type — flipping one method without the others fails to compile. Keep Task 3 as one commit; use `cargo build` as the worklist generator (Step 4).
- **Why `assert_eq!(h, 1)` survives:** `Handle: PartialEq<u64>` + the literal inferring as `u64` means the ~hundreds of literal-compare asserts compile untouched. The real edits are tag construction (~40), the 4 `Vec<u64>` + 2 `HashMap<u64>` sites, one arithmetic site, the bench adapter, and the tag-read `Option` asserts. Don't try to pre-edit every file — let the compiler drive.
- **No new `ChiselError`:** confirmed — Rust tag methods receive an already-valid `Tag`; the only 0-rejection points are `Tag::new`/`TryFrom` (construction) and `require_tag` (Python edge).
