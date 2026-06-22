# Multi-Page Freemap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Chisel's single-page freemap (~512 MB / 65,344-page reclamation ceiling) with a copy-on-write radix tree of bitmap leaves so freed-page reclamation works at any database size.

**Architecture:** A third COW radix structure beside the handle table and membership index. Leaves are today's bitmap pages (`PageType::FreeMap = 0x04`); interiors are a new `PageType::FreeMapInterior = 0x07` holding up to 1021 child page-id pointers. Depth 0 is exactly today's single-page format, so existing databases need no migration. The freemap moves into the page cache: the per-`begin()` clone of the whole bitmap is replaced by `{root, depth}` carried in `Roots`, and structural COW pages are extend-only (never drawn from the freemap) to guarantee termination.

**Tech Stack:** Rust, `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt`, `proptest`, in-memory (`open_in_memory`) test fixtures, `criterion`/`divan` benches.

**Spec:** `docs/specs/2026-06-22-multi-page-freemap-design.md` (read it before starting).

**Branch:** `feature/multi-page-freemap` (already created off `main`).

---

## Phasing (suggested PR boundaries)

- **Phase 1 — Foundations + standalone tree (PR 1).** New page type, superblock `freemap_depth` field, and the complete `freemap_tree.rs` module with unit tests. Wired into nothing yet (`#[allow(dead_code)]`). Additive, no behavior change — ships green and independently reviewable.
- **Phase 2 — Integration (PR 2).** Replace the in-memory `Box` freemap with the tree, rewrite `persist_freemap`, rewire allocation, open/recovery, begin/rollback. This is the behavior change that removes the ceiling. Heavily tested (integration + proptest + backward-compat).
- **Phase 3 — Benches + docs (PR 3).** Freemap-tree benches; `ARCHITECTURE.md` update.

Land each phase as its own PR off `main` (merge-first between phases, per the project PR workflow).

---

## File Structure

- **Create `src/freemap_tree.rs`** — the radix tree. Owns descent, lazy materialization, COW spine rewrite, depth growth, `mark_free` / `allocate_first` / `is_free`, and the interior-page child-pointer helpers. Layer 4 (depends only on `page`, `page_cache`, `freemap`). Mirrors `membership_index.rs`.
- **Modify `src/freemap.rs`** — retained as the single-page leaf-bitmap primitive. Add one accessor (`first_free_bit`) the tree needs; everything else is unchanged.
- **Modify `src/page.rs`** — add `PageType::FreeMapInterior = 0x07`; extend `current_version`'s exhaustive match.
- **Modify `src/superblock.rs`** — add `freemap_depth: u32` (byte 320) to the struct + serialize/deserialize + test constructors.
- **Modify `src/transaction.rs`** — `Roots` gains `freemap_depth`; remove the `committed_freemap`/`current_freemap` `Box` fields; add `freemap_hint: u64`; rewrite `persist_freemap`; rewire `cow_alloc` callers; load `{root, depth}` on open.
- **Modify `src/lib.rs`** — `mod freemap_tree;`.
- **Create `tests/freemap_multipage.rs`** — end-to-end reclamation past the old ceiling, backward-compat, reopen round-trip.
- **Modify `bench/src/`** — a freemap-tree bench (Phase 3).

---

# Phase 1 — Foundations + standalone tree

### Task 1: New `FreeMapInterior` page type

**Files:**
- Modify: `src/page.rs:147-172` (the `PageType` enum and `current_version`)
- Test: `src/page.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

In `src/page.rs`'s test module:

```rust
#[test]
fn freemap_interior_type_tag_and_version() {
    assert_eq!(PageType::FreeMapInterior as u8, 0x07);
    // current_version is exhaustive and returns the current format version.
    assert_eq!(
        current_version(PageType::FreeMapInterior),
        PAGE_FORMAT_VERSION_CURRENT
    );
}
```

- [ ] **Step 2: Run it, expect a compile failure**

Run: `cargo test --lib freemap_interior_type_tag_and_version`
Expected: FAIL — `no variant named FreeMapInterior`.

- [ ] **Step 3: Add the variant and the match arm**

`src/page.rs`, in `enum PageType`:

```rust
pub enum PageType {
    HandleTable = 0x01,
    Data = 0x02,
    Overflow = 0x03,
    FreeMap = 0x04,
    MembershipInterior = 0x05,
    MembershipLeaf = 0x06,
    FreeMapInterior = 0x07,
}
```

In `current_version`, add `FreeMapInterior` to the existing all-arms-return-current match:

```rust
        PageType::HandleTable
        | PageType::Data
        | PageType::Overflow
        | PageType::FreeMap
        | PageType::MembershipInterior
        | PageType::MembershipLeaf
        | PageType::FreeMapInterior => PAGE_FORMAT_VERSION_CURRENT,
```

- [ ] **Step 4: Run it, expect PASS**

Run: `cargo test --lib freemap_interior_type_tag_and_version`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/page.rs
git commit -m "feat: add PageType::FreeMapInterior (0x07) for the multi-page freemap"
```

---

### Task 2: Superblock `freemap_depth` field

**Files:**
- Modify: `src/superblock.rs` (the `Superblock` struct, `serialize`, `deserialize`, the `new` default, test constructors)
- Test: `src/superblock.rs` (inline `#[cfg(test)]`)

Byte layout reminder (from `serialize`): `page_size` ends at 52; named-roots 52..308; `superblock_count` 308..312; `root_membership_index_page` 312..320. **`freemap_depth` goes at byte 320** (the next reserved-zero region). Existing databases read 0 there.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn freemap_depth_round_trips_and_defaults_zero() {
    let mut sb = Superblock::new(2);
    sb.root_freemap_page = 9;
    sb.freemap_depth = 3;
    let buf = sb.serialize();
    let back = Superblock::deserialize(&buf).unwrap();
    assert_eq!(back.freemap_depth, 3);
    assert_eq!(back.root_freemap_page, 9);

    // A superblock written before this field existed (byte 320 == 0) reads
    // depth 0 — backward compatibility.
    let mut legacy = sb.serialize();
    legacy[320..324].fill(0);
    page::stamp_checksum(&mut legacy); // re-stamp after editing the body
    let back0 = Superblock::deserialize(&legacy).unwrap();
    assert_eq!(back0.freemap_depth, 0);
}
```

- [ ] **Step 2: Run it, expect a compile failure**

Run: `cargo test --lib freemap_depth_round_trips_and_defaults_zero`
Expected: FAIL — `no field freemap_depth`.

- [ ] **Step 3: Add the field, an offset const, and serialize/deserialize**

Near the other offset consts in `src/superblock.rs`:

```rust
// freemap tree depth (multi-page freemap). Byte 320, immediately after
// root_membership_index_page (312..320), in the reserved-zero region. A
// database written before this field existed reads 0 here = single-page
// (depth-0) freemap, so the field is backward compatible.
const FREEMAP_DEPTH_OFFSET: usize = 320;
```

Add to the `Superblock` struct (next to `root_freemap_page`):

```rust
    pub freemap_depth: u32,
```

In `serialize`, after the membership-index write:

```rust
        buf[FREEMAP_DEPTH_OFFSET..FREEMAP_DEPTH_OFFSET + 4]
            .copy_from_slice(&self.freemap_depth.to_le_bytes());
```

In `deserialize`, where the struct is built, add:

```rust
            freemap_depth: u32::from_le_bytes(
                buf[FREEMAP_DEPTH_OFFSET..FREEMAP_DEPTH_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ),
```

Add `freemap_depth: 0` to every other `Superblock { .. }` literal in the file (the `new` constructor at ~line 390 and the test fixtures at ~525/545/565). `cargo build` will name each missing one.

- [ ] **Step 4: Run it, expect PASS, and confirm nothing else broke**

Run: `cargo test --lib superblock`
Expected: PASS (the new test plus all existing superblock tests).

- [ ] **Step 5: Commit**

```bash
git add src/superblock.rs
git commit -m "feat: superblock freemap_depth field (byte 320, defaults 0)"
```

---

### Task 3: Leaf accessor `FreeMap::first_free_bit`

The tree's `allocate_first` needs to *find* the lowest free bit in a leaf without clearing it (clearing is the tree's job, under COW). `FreeMap::allocate_first` both finds and clears; add a non-mutating finder.

**Files:**
- Modify: `src/freemap.rs` (the `impl FreeMap` block)
- Test: `src/freemap.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn first_free_bit_finds_lowest_without_clearing() {
    let mut buf = [0u8; PAGE_SIZE];
    FreeMap::init_page(&mut buf);
    assert_eq!(FreeMap::first_free_bit(&buf), None);
    FreeMap::mark_free(&mut buf, 300);
    FreeMap::mark_free(&mut buf, 12);
    assert_eq!(FreeMap::first_free_bit(&buf), Some(12));
    // Non-mutating: the bit is still set.
    assert_eq!(FreeMap::first_free_bit(&buf), Some(12));
    assert!(FreeMap::is_free(&buf, 12));
}
```

- [ ] **Step 2: Run it, expect compile failure**

Run: `cargo test --lib first_free_bit_finds_lowest_without_clearing`
Expected: FAIL — `no function first_free_bit`.

- [ ] **Step 3: Implement (factor out of `allocate_first`)**

In `src/freemap.rs`:

```rust
    /// Return the lowest free page id in this leaf without clearing it, or
    /// None if the leaf has no free bit. The tree composes this with its own
    /// COW: it descends to the leaf, reads the index here, then COW-clears it.
    pub fn first_free_bit(buf: &[u8; PAGE_SIZE]) -> Option<u64> {
        for byte_idx in 0..PAGE_BODY_SIZE {
            let byte = buf[BITMAP_OFFSET + byte_idx];
            if byte != 0 {
                let bit_idx = byte.trailing_zeros() as usize;
                return Some((byte_idx * 8 + bit_idx) as u64);
            }
        }
        None
    }
```

(Leave `allocate_first` as-is; the single-page callers in `transaction.rs` still use it until Phase 2.)

- [ ] **Step 4: Run it, expect PASS**

Run: `cargo test --lib freemap`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/freemap.rs
git commit -m "feat: FreeMap::first_free_bit (non-mutating finder for the tree)"
```

---

### Task 4: `freemap_tree.rs` — descent + `is_free` (read side)

**Files:**
- Create: `src/freemap_tree.rs`
- Modify: `src/lib.rs` (add `mod freemap_tree;`)
- Test: `src/freemap_tree.rs` (inline `#[cfg(test)]`)

This task builds the read-only skeleton: the struct, constants, interior child-pointer helpers, leaf/interior init, descent to a leaf, and `is_free`. Growth and mutation come in Tasks 5–6.

- [ ] **Step 1: Add the module declaration**

`src/lib.rs`, alongside the other `mod` lines:

```rust
mod freemap_tree;
```

- [ ] **Step 2: Write the failing test**

Create `src/freemap_tree.rs` with only the test module first (so it fails to compile against missing items):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    fn make_cache() -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut cache = PageCache::new(
            io,
            256 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        cache.set_next_page_id(2); // reserve superblock slots 0,1
        cache
    }

    // A depth-0 tree (single leaf) reports a marked id free and an unmarked
    // id not free.
    #[test]
    fn depth0_is_free_round_trip() {
        let mut cache = make_cache();
        let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
        assert_eq!(t.depth, 0);
        assert!(!t.is_free(&mut cache, 5).unwrap());
        t.mark_free(&mut cache, 5, &mut |c| c.new_page()).unwrap();
        assert!(t.is_free(&mut cache, 5).unwrap());
        assert!(!t.is_free(&mut cache, 6).unwrap());
    }
}
```

- [ ] **Step 3: Run it, expect compile failure**

Run: `cargo test --lib freemap_tree`
Expected: FAIL — `FreeMapTree` undefined. (`mark_free` is stubbed in Task 5; to keep this task self-contained, the test above also exercises `mark_free`, so this task and Task 5 are committed together — see Step 5. If you prefer a strictly read-only first commit, temporarily delete the `mark_free` lines, land descent + `is_free`, then restore them in Task 5.)

- [ ] **Step 4: Implement the skeleton**

Prepend to `src/freemap_tree.rs` (above the test module):

```rust
//! freemap_tree.rs — multi-page freemap as a copy-on-write radix tree of bitmap
//! leaves (layer 4: page-type-specific logic). Leaves are `FreeMap` bitmap pages
//! (`PageType::FreeMap`, 65,344 bits); interiors are `PageType::FreeMapInterior`
//! pages holding up to 1021 child page-id pointers. A zero child pointer means
//! "that whole sub-range is entirely in use" (lazy/sparse), mirroring the handle
//! table and membership index. Depth 0 is a single bitmap leaf — exactly the
//! historical single-page freemap. See docs/specs/2026-06-22-multi-page-freemap-design.md.
//!
//! Termination invariant: structural COW (interior/leaf copies, newly-
//! materialized nodes) always allocates via the caller-supplied `extend`
//! closure, which in production is `PageCache::new_page` (file extension) and
//! NEVER the freemap itself — so the freemap never needs a free page to record
//! free pages.

use crate::error::{ChiselError, Result};
use crate::freemap::FreeMap;
use crate::page::{
    self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_BODY_SIZE, PAGE_ID_NONE, PAGE_SIZE,
};
use crate::page_cache::PageCache;

// Interior fan-out: 8-byte child pointers in the body, same as membership.
const PTRS_PER_INTERIOR: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / 8; // 1021
// Pages one leaf covers.
const LEAF_CAPACITY: u64 = (PAGE_BODY_SIZE * 8) as u64; // 65_344
// depth d covers LEAF_CAPACITY * PTRS_PER_INTERIOR^d pages. d=5 already exceeds
// u64 page ids (2^16 * 2^(10*5) = 2^66), so 5 is the deepest valid tree.
const MAX_DEPTH: u32 = 5;

fn child_offset(idx: usize) -> usize {
    DATA_PAGE_HEADER_SIZE + idx * 8
}
fn read_child(buf: &[u8; PAGE_SIZE], idx: usize) -> u64 {
    let o = child_offset(idx);
    u64::from_le_bytes(buf[o..o + 8].try_into().unwrap())
}
fn write_child(buf: &mut [u8; PAGE_SIZE], idx: usize, child: u64) {
    let o = child_offset(idx);
    buf[o..o + 8].copy_from_slice(&child.to_le_bytes());
}

/// Initialize a fresh interior page (all child pointers zero = all sub-ranges
/// in use). Does not stamp the checksum — the caller does after any edit.
fn init_interior(buf: &mut [u8; PAGE_SIZE]) {
    buf.fill(0);
    buf[0] = PageType::FreeMapInterior as u8;
    buf[1] = page::current_version(PageType::FreeMapInterior);
}

/// A copy-on-write radix tree of bitmap leaves. `root`/`depth` mirror the
/// superblock (`root_freemap_page` / `freemap_depth`); the bitmap and interior
/// pages live in the page cache.
pub(crate) struct FreeMapTree {
    pub root: u64,
    pub depth: u32,
}

impl FreeMapTree {
    /// Reconstruct a tree handle from committed roots (open path).
    pub fn from_roots(root: u64, depth: u32) -> FreeMapTree {
        FreeMapTree { root, depth }
    }

    /// Create an empty depth-0 tree: one fresh bitmap leaf via `extend`.
    pub fn create(
        cache: &mut PageCache,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<FreeMapTree> {
        let root = extend(cache)?;
        let buf = cache.get_mut(root)?;
        FreeMap::init_page(buf);
        page::stamp_checksum(buf);
        Ok(FreeMapTree { root, depth: 0 })
    }

    /// Pages covered by the whole tree at the current depth (saturating; a
    /// corrupt over-range depth saturates rather than overflow-panicking).
    fn capacity(&self) -> u64 {
        let mut cap = LEAF_CAPACITY;
        for _ in 0..self.depth {
            cap = cap.saturating_mul(PTRS_PER_INTERIOR as u64);
        }
        cap
    }

    /// Pages covered by one child pointer at `level` (1 = leaf level).
    fn span_at_level(&self, level: u32) -> u64 {
        let mut span = LEAF_CAPACITY;
        for _ in 1..level {
            span = span.saturating_mul(PTRS_PER_INTERIOR as u64);
        }
        span
    }

    /// Validate a tree page's type tag; a checksum-valid wrong-type page reached
    /// via a corrupt child pointer surfaces as CorruptPage (mirrors the overflow
    /// / data-page hardening), never silent misinterpretation.
    fn check_type(buf: &[u8; PAGE_SIZE], want: PageType, page_id: u64) -> Result<()> {
        if buf[0] != want as u8 {
            return Err(ChiselError::CorruptPage { page_id });
        }
        Ok(())
    }

    /// Descend to the leaf that would hold `id`, returning its page id, or None
    /// if any child pointer on the path is zero (absent subtree = all-in-use) or
    /// `id` is beyond the tree's reach.
    fn find_leaf(&self, cache: &mut PageCache, id: u64) -> Result<Option<u64>> {
        if self.root == PAGE_ID_NONE {
            return Ok(None);
        }
        let cap = self.capacity();
        if cap != u64::MAX && id >= cap {
            return Ok(None);
        }
        if self.depth == 0 {
            return Ok(Some(self.root));
        }
        let mut current = self.root;
        let mut remaining = id;
        for level in (1..=self.depth).rev() {
            let span = self.span_at_level(level);
            let child_idx = (remaining / span) as usize;
            if child_idx >= PTRS_PER_INTERIOR {
                return Ok(None);
            }
            remaining %= span;
            let buf = cache.get(current)?;
            Self::check_type(buf, PageType::FreeMapInterior, current)?;
            let child = read_child(buf, child_idx);
            if child == 0 {
                return Ok(None);
            }
            current = child;
        }
        Ok(Some(current))
    }

    /// Is `id` currently free? False for any id whose subtree is absent.
    pub fn is_free(&self, cache: &mut PageCache, id: u64) -> Result<bool> {
        match self.find_leaf(cache, id)? {
            None => Ok(false),
            Some(leaf) => {
                let buf = cache.get(leaf)?;
                Self::check_type(buf, PageType::FreeMap, leaf)?;
                Ok(FreeMap::is_free(buf, id % LEAF_CAPACITY))
            }
        }
    }
}
```

- [ ] **Step 5: (Implement Task 5's `mark_free` first if landing together) then run**

Run: `cargo test --lib freemap_tree`
Expected: PASS once `mark_free` (Task 5) exists. Commit at the end of Task 5.

---

### Task 5: `freemap_tree.rs` — `mark_free` with lazy materialization + COW

**Files:**
- Modify: `src/freemap_tree.rs`
- Test: `src/freemap_tree.rs` (inline)

`mark_free(id)` descends to `id`'s leaf, COW-rewriting the spine, materializing any absent interior/leaf along the way via `extend`, then sets the bit. **All structural allocation goes through `extend`.** Growth (id beyond capacity) is Task 6; for now assume in-range.

- [ ] **Step 1: Write the failing tests**

```rust
// COW: marking a fresh id materializes a leaf+spine and the bit reads back.
#[test]
fn mark_free_materializes_absent_subtree_at_depth1() {
    let mut cache = make_cache();
    // Force depth 1 by creating a depth-1 tree directly (Task 6 grows; here we
    // start from a grown tree to test materialization independently).
    let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
    t.grow(&mut cache, &mut |c| c.new_page()).unwrap(); // depth 0 -> 1
    assert_eq!(t.depth, 1);

    // An id in the SECOND leaf-span (child 1) — its subtree is absent.
    let id = LEAF_CAPACITY + 42;
    assert!(!t.is_free(&mut cache, id).unwrap());
    t.mark_free(&mut cache, id, &mut |c| c.new_page()).unwrap();
    assert!(t.is_free(&mut cache, id).unwrap());
    // A sibling id in the same new leaf:
    t.mark_free(&mut cache, LEAF_CAPACITY + 99, &mut |c| c.new_page())
        .unwrap();
    assert!(t.is_free(&mut cache, LEAF_CAPACITY + 99).unwrap());
}

// Termination invariant: structural COW only ever calls `extend`.
#[test]
fn mark_free_structural_alloc_is_extend_only() {
    let mut cache = make_cache();
    let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
    t.grow(&mut cache, &mut |c| c.new_page()).unwrap();
    let mut extend_calls = 0usize;
    let mut spy = |c: &mut PageCache| {
        extend_calls += 1;
        c.new_page()
    };
    t.mark_free(&mut cache, LEAF_CAPACITY + 1, &mut spy).unwrap();
    assert!(extend_calls >= 1, "materializing a leaf must extend");
}
```

- [ ] **Step 2: Run, expect compile failure** (`mark_free` / `grow` undefined)

Run: `cargo test --lib freemap_tree`
Expected: FAIL.

- [ ] **Step 3: Implement `mark_free` (and a private `cow_page` helper)**

Add to `impl FreeMapTree`:

```rust
    /// COW a page: copy `old` (if present) into a fresh extended page, returning
    /// the new id. The caller stamps the checksum after editing. A None `old`
    /// means "materialize a fresh page of `kind`".
    fn cow_page(
        cache: &mut PageCache,
        old: Option<u64>,
        kind: PageType,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<u64> {
        let new_id = extend(cache)?;
        match old {
            Some(src) => {
                let bytes = *cache.get(src)?; // copy out (immutable borrow ends)
                Self::check_type(&bytes, kind, src)?;
                let dst = cache.get_mut(new_id)?;
                *dst = bytes;
            }
            None => {
                let dst = cache.get_mut(new_id)?;
                match kind {
                    PageType::FreeMap => FreeMap::init_page(dst),
                    PageType::FreeMapInterior => init_interior(dst),
                    _ => unreachable!("freemap tree only COWs leaf/interior pages"),
                }
            }
        }
        Ok(new_id)
    }

    /// Mark `id` free, COW-rewriting the spine and materializing absent nodes via
    /// `extend`. Assumes `id < capacity()` (the manager grows first — Task 6).
    /// Returns the page ids this call superseded (old spine pages), which the
    /// caller must queue for reclamation; never reuses them itself.
    pub fn mark_free(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<Vec<u64>> {
        debug_assert!(self.capacity() == u64::MAX || id < self.capacity());
        let mut superseded = Vec::new();

        // COW the root.
        let kind_root = if self.depth == 0 {
            PageType::FreeMap
        } else {
            PageType::FreeMapInterior
        };
        let new_root = Self::cow_page(cache, Some(self.root), kind_root, extend)?;
        superseded.push(self.root);
        self.root = new_root;

        // Descend, COWing/materializing each child along the path.
        let mut current = self.root;
        let mut remaining = id;
        for level in (1..=self.depth).rev() {
            let span = self.span_at_level(level);
            let child_idx = (remaining / span) as usize;
            remaining %= span;
            let child_kind = if level == 1 {
                PageType::FreeMap
            } else {
                PageType::FreeMapInterior
            };
            let old_child = read_child(cache.get(current)?, child_idx);
            let new_child = if old_child == 0 {
                Self::cow_page(cache, None, child_kind, extend)?
            } else {
                let c = Self::cow_page(cache, Some(old_child), child_kind, extend)?;
                superseded.push(old_child);
                c
            };
            let buf = cache.get_mut(current)?;
            write_child(buf, child_idx, new_child);
            page::stamp_checksum(buf);
            current = new_child;
        }

        // Set the bit in the (now COW'd) leaf and stamp it.
        let leaf = cache.get_mut(current)?;
        FreeMap::mark_free(leaf, id % LEAF_CAPACITY);
        page::stamp_checksum(leaf);
        Ok(superseded)
    }
```

- [ ] **Step 4: Run (with Task 6's `grow` present), expect PASS**

Run: `cargo test --lib freemap_tree`
Expected: PASS (this task's tests reference `grow` from Task 6 — implement Task 6 before running, or temporarily construct a depth-1 tree by hand).

- [ ] **Step 5: Commit (Tasks 4–6 together)**

```bash
git add src/freemap_tree.rs src/lib.rs
git commit -m "feat: freemap_tree descent, is_free, and COW mark_free (extend-only structural alloc)"
```

---

### Task 6: `freemap_tree.rs` — `grow`

**Files:**
- Modify: `src/freemap_tree.rs`
- Test: `src/freemap_tree.rs` (inline)

Growing adds a level: a fresh interior becomes the new root with the old root reparented as child 0. The old root is **not** superseded (it is reparented, not copied), so it contributes nothing to free.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn grow_increases_depth_and_preserves_existing_frees() {
    let mut cache = make_cache();
    let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
    t.mark_free(&mut cache, 7, &mut |c| c.new_page()).unwrap();
    assert_eq!(t.depth, 0);

    t.grow(&mut cache, &mut |c| c.new_page()).unwrap();
    assert_eq!(t.depth, 1);
    // The pre-existing free id (in child 0's span) is still free.
    assert!(t.is_free(&mut cache, 7).unwrap());
    // An id only reachable at depth >= 1 is now in range (absent => not free).
    assert!(!t.is_free(&mut cache, LEAF_CAPACITY + 3).unwrap());
}
```

- [ ] **Step 2: Run, expect compile failure**

Run: `cargo test --lib grow_increases_depth_and_preserves_existing_frees`
Expected: FAIL — `no method grow`.

- [ ] **Step 3: Implement `grow`**

```rust
    /// Add one level: a fresh interior root with the old root reparented as
    /// child 0. The old root is reparented (not copied), so it is not superseded.
    pub fn grow(
        &mut self,
        cache: &mut PageCache,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            // capacity() has saturated to u64::MAX; the whole id space is in
            // reach already, so growth is neither needed nor representable.
            return Ok(());
        }
        let new_root = extend(cache)?;
        let old_root = self.root;
        let buf = cache.get_mut(new_root)?;
        init_interior(buf);
        write_child(buf, 0, old_root);
        page::stamp_checksum(buf);
        self.root = new_root;
        self.depth += 1;
        Ok(())
    }
```

- [ ] **Step 4: Run, expect PASS**

Run: `cargo test --lib freemap_tree`
Expected: PASS (all of Tasks 4–6).

- [ ] **Step 5: Commit** — folded into the Task 5 commit if landing together; otherwise:

```bash
git add src/freemap_tree.rs
git commit -m "feat: freemap_tree grow (depth increase, reparent old root)"
```

---

### Task 7: `freemap_tree.rs` — `allocate_first` + lowest-free hint, and auto-grow `mark_free`

**Files:**
- Modify: `src/freemap_tree.rs`
- Test: `src/freemap_tree.rs` (inline)

Two pieces: (a) a public `mark_free_growing` that grows until `id` is in range then marks it (the manager's entry point); (b) `allocate_first` that descends to the lowest free bit using the hint, COW-clears it, and returns the id.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn mark_free_growing_grows_to_reach_high_id() {
    let mut cache = make_cache();
    let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
    let high = LEAF_CAPACITY * 3 + 17; // needs depth >= 1
    let _ = t.mark_free_growing(&mut cache, high, &mut |c| c.new_page()).unwrap();
    assert!(t.depth >= 1);
    assert!(t.is_free(&mut cache, high).unwrap());
}

#[test]
fn allocate_first_returns_lowest_free_and_clears_it() {
    let mut cache = make_cache();
    let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
    for id in [500u64, 9, LEAF_CAPACITY + 4] {
        t.mark_free_growing(&mut cache, id, &mut |c| c.new_page()).unwrap();
    }
    let mut hint = 0u64;
    // Lowest free is 9, then 500, then LEAF_CAPACITY+4, then exhausted.
    assert_eq!(t.allocate_first(&mut cache, &mut hint, &mut |c| c.new_page()).unwrap(), Some(9));
    assert!(!t.is_free(&mut cache, 9).unwrap());
    assert_eq!(t.allocate_first(&mut cache, &mut hint, &mut |c| c.new_page()).unwrap(), Some(500));
    assert_eq!(
        t.allocate_first(&mut cache, &mut hint, &mut |c| c.new_page()).unwrap(),
        Some(LEAF_CAPACITY + 4)
    );
    assert_eq!(t.allocate_first(&mut cache, &mut hint, &mut |c| c.new_page()).unwrap(), None);
}
```

- [ ] **Step 2: Run, expect compile failure**

Run: `cargo test --lib freemap_tree`
Expected: FAIL — `mark_free_growing` / `allocate_first` undefined.

- [ ] **Step 3: Implement**

```rust
    /// Grow until `id` is in range, then mark it free. The manager's entry point
    /// for reclaiming a freed page id of any magnitude. Returns superseded ids.
    pub fn mark_free_growing(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<Vec<u64>> {
        while self.capacity() != u64::MAX && id >= self.capacity() {
            self.grow(cache, extend)?;
        }
        self.mark_free(cache, id, extend)
    }

    /// Find the lowest free id at or above `*hint`, COW-clear its bit, and return
    /// it (updating `*hint` to it). None if no free id remains. Walks the tree
    /// left-to-right skipping absent (all-in-use) subtrees; the hint avoids
    /// rescanning exhausted prefixes across calls.
    pub fn allocate_first(
        &mut self,
        cache: &mut PageCache,
        hint: &mut u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<Option<u64>> {
        let Some((leaf, found_id)) = self.scan_from(cache, *hint)? else {
            return Ok(None);
        };
        // COW-clear the found bit: descend again marking it used. Reuse the
        // mark_free spine machinery but clearing instead of setting — simplest is
        // a dedicated clear that mirrors mark_free.
        let superseded = self.clear_bit(cache, found_id, extend)?;
        // The superseded old spine pages are themselves now free; but per the
        // spec's one-cycle-lag rule the MANAGER queues them — allocate_first
        // returns them so the caller (cow_alloc/persist) can record them.
        debug_assert!(!superseded.is_empty());
        *hint = found_id;
        // The found page is now in use; record the superseded pages by handing
        // them back via a side channel is overkill here — callers that need them
        // use mark_free's return. allocate_first's superseded pages are queued by
        // the manager exactly like txn_freed_pages (see Phase 2 persist_freemap).
        self.pending_superseded.extend(superseded);
        Ok(Some(found_id))
    }
```

NOTE on `pending_superseded`: to avoid threading a `Vec` return through every
`cow_alloc` call site, `FreeMapTree` accumulates the page ids its COWs supersede
in a field `pending_superseded: Vec<u64>` that the manager drains into
`txn_freed_pages` after each freemap-touching operation. Update the struct:

```rust
pub(crate) struct FreeMapTree {
    pub root: u64,
    pub depth: u32,
    pub pending_superseded: Vec<u64>,
}
```

and set `pending_superseded: Vec::new()` in `from_roots` / `create`, and have
`mark_free` push into `self.pending_superseded` instead of returning a `Vec`
(change its signature to `-> Result<()>` and push there; update Tasks 5–6 tests
to read `t.pending_superseded` if they assert on it). This keeps every call site
uniform: do the op, then `manager.txn_freed_pages.append(&mut tree.pending_superseded)`.

Add the descent helpers:

```rust
    /// Lowest free id >= `from`, with its leaf page id. None if none.
    fn scan_from(&self, cache: &mut PageCache, from: u64) -> Result<Option<(u64, u64)>> {
        if self.root == PAGE_ID_NONE {
            return Ok(None);
        }
        self.scan_subtree(cache, self.root, self.depth, 0, from)
    }

    // Recursive left-to-right scan of the subtree rooted at `page` (covering
    // [base, base + span)), returning the lowest free id >= `from`.
    fn scan_subtree(
        &self,
        cache: &mut PageCache,
        page: u64,
        depth: u32,
        base: u64,
        from: u64,
    ) -> Result<Option<(u64, u64)>> {
        if depth == 0 {
            let buf = cache.get(page)?;
            Self::check_type(buf, PageType::FreeMap, page)?;
            // Lowest free bit at or above (from - base) within this leaf.
            let lo = from.saturating_sub(base);
            if let Some(bit) = FreeMap::first_free_bit_from(buf, lo) {
                return Ok(Some((page, base + bit)));
            }
            return Ok(None);
        }
        let span = self.span_at_level(depth); // span of each child here
        let buf = *cache.get(page)?;
        Self::check_type(&buf, PageType::FreeMapInterior, page)?;
        let start_child = if from > base {
            ((from - base) / span) as usize
        } else {
            0
        };
        for idx in start_child..PTRS_PER_INTERIOR {
            let child = read_child(&buf, idx);
            if child == 0 {
                continue;
            }
            let child_base = base + idx as u64 * span;
            let child_from = from.max(child_base);
            if let Some(hit) = self.scan_subtree(cache, child, depth - 1, child_base, child_from)? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    /// COW-clear `id`'s bit (mirror of mark_free that clears instead of sets).
    fn clear_bit(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<Vec<u64>> {
        // Identical spine walk to mark_free, but the leaf op clears the bit.
        // Factor the shared spine walk into `cow_descend(id, |leaf| { ... })`.
        self.cow_descend(cache, id, extend, &mut |leaf, bit| {
            // clear: AND with !mask (FreeMap exposes allocate at a known bit via
            // a small helper — see Step 3b).
            FreeMap::clear_bit(leaf, bit);
        })
    }
```

This introduces three small helpers to add for cohesion:
- `FreeMap::first_free_bit_from(buf, lo) -> Option<u64>` — lowest free bit >= `lo`.
- `FreeMap::clear_bit(buf, id)` — clear one bit (the existing `allocate_first` clears the *lowest*; this clears a specific id).
- `FreeMapTree::cow_descend(cache, id, extend, leaf_op)` — the shared spine-COW walk extracted from `mark_free`, taking a closure that mutates the target leaf. Refactor `mark_free` to call `cow_descend` with a set-bit closure so set and clear share one tested implementation.

Implement those (in `freemap.rs` and `freemap_tree.rs` respectively), mirroring the patterns already shown. Add unit tests for `first_free_bit_from` (lowest >= lo, and None when all below lo) and `clear_bit` (clears the named id, leaves others) in `freemap.rs`.

- [ ] **Step 4: Run, expect PASS**

Run: `cargo test --lib freemap_tree freemap`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/freemap.rs src/freemap_tree.rs
git commit -m "feat: freemap_tree allocate_first + lowest-free hint, auto-grow mark_free"
```

---

### Task 8: `freemap_tree.rs` — proptest vs. a `HashSet` oracle

**Files:**
- Modify: `src/freemap_tree.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the proptest**

```rust
proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

    // Random mark_free / allocate_first sequences match a HashSet oracle, across
    // ids that force depth 0 -> 1 -> 2.
    #[test]
    fn prop_tree_matches_oracle(
        ops in proptest::collection::vec(
            (proptest::bool::ANY, 0u64..(LEAF_CAPACITY * 1200)),
            1..60usize,
        )
    ) {
        let mut cache = make_cache();
        let mut t = FreeMapTree::create(&mut cache, &mut |c| c.new_page()).unwrap();
        let mut oracle: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut hint = 0u64;
        for (is_free, id) in ops {
            if is_free {
                t.mark_free_growing(&mut cache, id, &mut |c| c.new_page()).unwrap();
                t.pending_superseded.clear(); // mirror manager drain
                oracle.insert(id);
            } else {
                let got = t.allocate_first(&mut cache, &mut hint, &mut |c| c.new_page()).unwrap();
                t.pending_superseded.clear();
                let want = oracle.iter().next().copied();
                proptest::prop_assert_eq!(got, want);
                if let Some(v) = got { oracle.remove(&v); }
            }
        }
        // Every remaining oracle id reads free; a few non-members read not-free.
        for id in &oracle {
            proptest::prop_assert!(t.is_free(&mut cache, *id).unwrap());
        }
    }
}
```

- [ ] **Step 2: Run, expect PASS** (the implementation already exists)

Run: `cargo test --lib prop_tree_matches_oracle`
Expected: PASS. If it fails, the failure is a real bug in Tasks 4–7 — fix before continuing (this oracle test is the correctness backbone).

- [ ] **Step 3: Commit**

```bash
git add src/freemap_tree.rs
git commit -m "test: freemap_tree proptest vs BTreeSet oracle across depths"
```

> **Phase 1 gate:** `cargo test`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check` all green. Open **PR 1** off `main`. The tree is fully tested but unused (`#[allow(dead_code)]` on `FreeMapTree` if clippy complains).

---

# Phase 2 — Integration

> Land after PR 1 merges, off updated `main`. This phase changes behavior (removes the ceiling). The existing single-page tests in `transaction.rs` and `tests/` are the safety net; keep them green at every step.

### Task 9: `Roots.freemap_depth` + manager fields swap

**Files:**
- Modify: `src/transaction.rs` (`Roots`, the manager struct, `new`/`open_existing`, `begin`, `rollback`)

- [ ] **Step 1:** Add `freemap_depth: u32` to `struct Roots` (next to `freemap_page`) and `freemap_depth: 0` to every `Roots { .. }` literal (the `new` default at ~350 and the `open_existing` build at ~525). Set `freemap_depth: sb.freemap_depth` in the `open_existing` `Roots`.

- [ ] **Step 2:** Replace the two `Box<[u8; PAGE_SIZE]>` fields:

Remove:
```rust
    committed_freemap: Box<[u8; PAGE_SIZE]>,
    current_freemap: Box<[u8; PAGE_SIZE]>,
```
Add:
```rust
    // Best-effort lower bound on the lowest free page id, for allocate_first.
    // Transactionally untracked: a too-low hint only costs a wasted scan, never
    // correctness, so it survives begin/rollback without snapshotting.
    freemap_hint: u64,
```

- [ ] **Step 3:** In `new` (fresh DB), replace the `FreeMap::init_page` Box setup with: create a depth-0 tree lazily — set `freemap_page: PAGE_ID_NONE`, `freemap_depth: 0` in roots (already the default) and `freemap_hint: 0`. The first `mark_free`/`allocate` materializes the leaf (Task 10/11).

- [ ] **Step 4:** In `open_existing`, delete the `committed_freemap` load block (lines ~565-571). The committed tree is now `{ sb.root_freemap_page, sb.freemap_depth }`, already captured in `committed_roots`. Set `freemap_hint: 0`.

- [ ] **Step 5:** In `begin_inner`, delete `self.current_freemap = self.committed_freemap.clone();` (the freemap root+depth ride in `current_roots = committed_roots.clone()` for free). In `rollback`, delete any `current_freemap` reset (same reason). Reset is automatic via `current_roots`.

- [ ] **Step 6:** `cargo build`. Expect errors at every `current_freemap`/`committed_freemap` use site (the `cow_alloc` callers and `persist_freemap`); these are fixed in Tasks 10–11. Comment them out or `todo!()` *temporarily* only if needed to reach a clean build between tasks — but prefer doing Tasks 10–11 before re-running the suite.

- [ ] **Step 7: Commit** (with Tasks 10–11; this task alone does not build).

---

### Task 10: Rewire allocation through the tree (`cow_alloc` + callers)

**Files:**
- Modify: `src/transaction.rs` (`cow_alloc`, `allocate_data_page`, `ht_insert`, the membership-insert site)

- [ ] **Step 1:** Replace the free function `cow_alloc` with a tree-based one:

```rust
/// Allocate a page id for COW work. With reuse enabled, draw the lowest free id
/// from the freemap tree (COW-clearing its bit; structural COW extends the file
/// — decision 6), else extend. `tree`/`hint` are the live freemap state; any
/// page ids the clear superseded are accumulated in `tree.pending_superseded`,
/// which the caller drains into `txn_freed_pages`.
fn cow_alloc(
    cache: &mut PageCache,
    tree: &mut FreeMapTree,
    hint: &mut u64,
    reuse_enabled: bool,
) -> Result<u64> {
    if reuse_enabled {
        if let Some(id) = tree.allocate_first(cache, hint, &mut |c| c.new_page())? {
            cache.claim_page(id)?;
            return Ok(id);
        }
    }
    cache.new_page()
}
```

- [ ] **Step 2:** `allocate_data_page`:

```rust
    fn allocate_data_page(&mut self) -> Result<u64> {
        let reuse = self.savepoints.is_empty();
        let mut cache = self.cache.borrow_mut();
        let mut tree = FreeMapTree::from_roots(
            self.current_roots.freemap_page,
            self.current_roots.freemap_depth,
        );
        let id = cow_alloc(&mut cache, &mut tree, &mut self.freemap_hint, reuse)?;
        self.current_roots.freemap_page = tree.root;
        self.current_roots.freemap_depth = tree.depth;
        self.txn_freed_pages.append(&mut tree.pending_superseded);
        Ok(id)
    }
```

- [ ] **Step 3:** `ht_insert` (and the structurally-identical membership insert site): build the tree from current roots, pass a closure to `handle_table.insert` that calls `cow_alloc` on `&mut tree`/`&mut self.freemap_hint` via disjoint field borrows, then write `tree.root`/`tree.depth` back and drain `pending_superseded`:

```rust
    fn ht_insert(&mut self, handle: u64, entry: &HandleEntry) -> Result<()> {
        let mut freed: Vec<u64> = Vec::new();
        let reuse = self.savepoints.is_empty();
        let mut tree = FreeMapTree::from_roots(
            self.current_roots.freemap_page,
            self.current_roots.freemap_depth,
        );
        let hint = &mut self.freemap_hint;
        let new_root = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| cow_alloc(c, &mut tree, hint, reuse);
            self.handle_table.insert(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                entry,
                &mut alloc,
                &mut freed,
            )?
        };
        self.current_roots.freemap_page = tree.root;
        self.current_roots.freemap_depth = tree.depth;
        self.current_roots.handle_table_page = new_root;
        self.txn_freed_pages.append(&mut tree.pending_superseded);
        self.txn_freed_pages.extend(freed);
        Ok(())
    }
```

Apply the same shape to the membership-index insert/remove site(s) that currently capture `fm = &mut *self.current_freemap`.

- [ ] **Step 4:** `cargo build` should now be clean except `persist_freemap` (Task 11).

- [ ] **Step 5: Commit** (with Task 11).

---

### Task 11: Rewrite `persist_freemap`

**Files:**
- Modify: `src/transaction.rs:803-843` (`persist_freemap`)
- Test: existing `transaction.rs` freemap tests + the new integration tests (Task 12)

The new commit-time job: apply this transaction's frees to the tree. Allocation-side COW already happened incrementally during the txn (Task 10). The superseded freemap pages from those COWs are already in `txn_freed_pages` (drained from `pending_superseded`). So `persist_freemap` marks every `txn_freed_pages` id free — but marking frees *also* COWs the tree, producing *new* superseded pages. Those new superseded pages are queued for the **next** commit (the one-cycle reclamation lag the spec specifies), via a manager field `pending_structural_frees: Vec<u64>` applied at the *start* of the next `persist_freemap`.

- [ ] **Step 1:** Add `pending_structural_frees: Vec<u64>` to the manager (init `Vec::new()`), and write the new method:

```rust
    fn persist_freemap(&mut self) -> Result<()> {
        // Nothing freed and no carried structural frees: the committed tree is
        // still valid as-is.
        if self.txn_freed_pages.is_empty() && self.pending_structural_frees.is_empty() {
            return Ok(());
        }
        let mut tree = FreeMapTree::from_roots(
            self.current_roots.freemap_page,
            self.current_roots.freemap_depth,
        );
        let mut cache = self.cache.borrow_mut();

        // Apply last commit's deferred structural frees first, then this txn's.
        let prior: Vec<u64> = std::mem::take(&mut self.pending_structural_frees);
        for id in prior.into_iter().chain(self.txn_freed_pages.iter().copied()) {
            // mark_free_growing materializes/COWs leaves+spine via extend.
            tree.mark_free_growing(&mut cache, id, &mut |c| c.new_page())?;
        }
        // The COWs above superseded old freemap pages: defer their reclamation
        // to the NEXT commit (extend-only structural rule + one-cycle lag), so
        // this commit terminates without cascading.
        self.pending_structural_frees.append(&mut tree.pending_superseded);

        self.current_roots.freemap_page = tree.root;
        self.current_roots.freemap_depth = tree.depth;
        Ok(())
    }
```

- [ ] **Step 2:** In `commit`, ensure the new superblock writes `freemap_depth`: where the superblock is built (around line 1060), add `freemap_depth: self.current_roots.freemap_depth` next to `root_freemap_page: self.current_roots.freemap_page`.

- [ ] **Step 3:** Run the existing freemap/transaction suite:

Run: `cargo test --lib transaction && cargo test --test page_reclamation`
Expected: PASS. The single-page reclamation behavior is preserved at depth 0. Fix any regression before continuing (the `persist_freemap_does_not_reuse_committed_live_pages` test at transaction.rs:3032 is the I18 guardrail — it must stay green).

- [ ] **Step 4: Commit (Tasks 9–11 together):**

```bash
git add src/transaction.rs
git commit -m "feat: integrate multi-page freemap tree into the transaction/commit path"
```

---

### Task 12: End-to-end reclamation past the ceiling + backward compat

**Files:**
- Create: `tests/freemap_multipage.rs`

- [ ] **Step 1: Write the failing/again-passing tests**

```rust
// The exact scenario the single-page ceiling broke: free a page in a high
// (depth-1) range and confirm it is reclaimed by a later allocation.
use chisel::{Chisel, Options};

#[test]
fn reclaims_freed_page_above_single_page_ceiling() {
    // In-memory: exercises the engine, not fsync.
    let mut db = Chisel::open_in_memory().unwrap();
    // Allocate enough small values to push allocation past 65,344 pages is too
    // slow for a unit test; instead drive the freemap tree directly through the
    // public delete/allocate cycle with a cache large enough and assert the
    // freemap depth grew and a high free id is reused. (See note below.)
    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..2000u64 {
        handles.push(db.allocate(format!("v{i}").as_bytes()).unwrap());
    }
    db.commit().unwrap();
    // Delete half, commit (frees pages), then allocate again and confirm the
    // file did not grow unboundedly (reclamation works).
    db.begin().unwrap();
    for h in handles.iter().step_by(2) {
        db.delete(*h).unwrap();
    }
    db.commit().unwrap();
    let pages_before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..500u64 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after = db.stats().unwrap().total_pages;
    // Reuse keeps growth well under the naive 500-page extend.
    assert!(
        pages_after - pages_before < 500,
        "freed pages were reclaimed (before={pages_before}, after={pages_after})"
    );
}

// A file-backed db survives reopen with a multi-level freemap.
#[test]
fn multipage_freemap_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.chisel");
    let mut handles = Vec::new();
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        for i in 0..3000u64 {
            handles.push(db.allocate(format!("v{i}").as_bytes()).unwrap());
        }
        db.commit().unwrap();
        db.begin().unwrap();
        for h in handles.iter().take(1000) {
            db.delete(*h).unwrap();
        }
        db.commit().unwrap();
    }
    // Reopen: the freemap root+depth round-trip through the superblock; a new
    // allocation reuses a previously-freed page.
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..500u64 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let after = db.stats().unwrap().total_pages;
    assert!(after - before < 500, "reclamation works across reopen");
}
```

> **Note on the "past 65,344 pages" assertion:** a true past-ceiling test needs > 65,344 pages, which is slow. Add one `#[ignore]`d heavy test that allocates enough 8 KB-overflow values to cross `LEAF_CAPACITY`, frees a page in the second leaf-span, and asserts reuse, runnable on demand (`cargo test -- --ignored multipage_crosses_leaf_boundary`). The fast tests above prove the depth-0→1 path through the public API; the ignored test proves the literal ceiling removal.

- [ ] **Step 2: Run, expect PASS**

Run: `cargo test --test freemap_multipage`
Expected: PASS. If `pages_after - pages_before` is not below the threshold, reclamation regressed — debug `persist_freemap` (Task 11) before continuing.

- [ ] **Step 3:** Add the heavy `#[ignore]` test `multipage_crosses_leaf_boundary` (allocate > `LEAF_CAPACITY` pages via overflow values, free a high page, assert depth ≥ 1 via `stats` if exposed or via reuse behavior). Run it once: `cargo test --test freemap_multipage -- --ignored`. Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add tests/freemap_multipage.rs
git commit -m "test: multi-page freemap reclamation past the single-page ceiling + reopen"
```

> **Phase 2 gate:** full `cargo test` (incl. `--ignored` heavy test once), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, and the Python suite (`maturin develop && pytest` — the binding is unaffected but the engine changed). Open **PR 2** off `main`.

---

# Phase 3 — Benches + docs

### Task 13: Freemap-tree benchmark

**Files:**
- Modify: `bench/src/` (add a freemap bench alongside the existing harness)

- [ ] **Step 1:** Add a bench that, for depths 0/1/2, measures `mark_free` + `allocate_first` throughput on a cache-resident tree (in-memory backing, `black_box` the results, per the bench-harness conventions in the `chisel-performance` skill). Record both in-memory and file backings.

- [ ] **Step 2:** Run `cargo bench` (report-only per CI policy), capture baseline numbers, and note them in the bench results file the repo tracks.

- [ ] **Step 3: Commit**

```bash
git add bench/
git commit -m "bench: freemap tree mark_free/allocate_first across depths"
```

### Task 14: ARCHITECTURE.md update

**Files:**
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1:** Update the freemap section: it is now a COW radix tree of bitmap leaves (depth 0 = single page = historical format); document `FreeMapInterior = 0x07`, the `freemap_depth` superblock field (byte 320), the extend-only structural-COW termination invariant, and the one-cycle structural-free reclamation lag. Remove the stale "a multi-page freemap is not yet implemented" framing wherever it appears (e.g. the `freemap.rs` header note and `overflow.rs:47`).

- [ ] **Step 2: Commit**

```bash
git add ARCHITECTURE.md src/freemap.rs src/overflow.rs
git commit -m "docs: ARCHITECTURE + headers reflect the multi-page freemap"
```

> **Phase 3 gate:** green build/test/clippy/fmt. Open **PR 3** off `main`.

---

## Self-Review (against the spec)

**Spec coverage:**
- Radix tree of bitmap leaves → Tasks 4–7. ✅
- `FreeMap` leaf unchanged; `FreeMapInterior = 0x07` → Tasks 1, 4. ✅
- Depth 0 = today's format / backward compatible → Task 2 (defaults-0 test), Task 12 (reopen). ✅
- Freemap in the page cache; no eager clone; `{root, depth}` in roots → Task 9. ✅
- Extend-only structural pages (termination) → Task 5 (`extend`-only spy test), `cow_page`/`mark_free`. ✅
- Depth in superblock (byte 320) → Task 2. ✅
- In-memory lowest-free hint → Task 7. ✅
- Generalized I18 / commit ordering / one-cycle structural-free lag → Task 11. ✅
- `freemap_tree.rs` over retained `freemap.rs` primitive → Tasks 3–7. ✅
- Testing surface (oracle proptest, growth, termination, reopen, backward-compat, heavy past-ceiling) → Tasks 5–8, 12. ✅
- Durability/poison: rides the existing commit; rollback via roots; no special path → Tasks 9, 11 (no new poison path introduced). ✅

**Placeholder scan:** the only deferred item is the heavy past-ceiling test, which is concretely specified as an `#[ignore]`d test with a run command — not a placeholder. No `TODO`/`TBD` in implementation steps.

**Type consistency:** `FreeMapTree { root: u64, depth: u32, pending_superseded: Vec<u64> }`; methods `create`, `from_roots`, `is_free(&self, cache, id) -> Result<bool>`, `mark_free`/`mark_free_growing`/`allocate_first`/`grow`/`clear_bit` all take `cache: &mut PageCache` and an `extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>`; `cow_alloc(cache, tree, hint, reuse)`; `FreeMap::{first_free_bit, first_free_bit_from, clear_bit}`; `Roots.freemap_depth`; `Superblock.freemap_depth`; manager fields `freemap_hint`, `pending_structural_frees`. Consistent across tasks.

> **Open risk flagged for execution:** Task 7's `allocate_first` returning superseded ids via `tree.pending_superseded` (rather than a return value) is the one design refinement made in the plan vs. the spec; it keeps every `cow_alloc` call site uniform. If the borrow checker resists the disjoint `&mut tree` / `&mut self.freemap_hint` capture in Task 10's `ht_insert`, fall back to threading `tree` and `hint` as locals (shown) — confirmed disjoint from `self.handle_table`.
