# Chunk Tags Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional, immutable `u32` chunk tag plus a copy-on-write membership index (`tag → {handles}`) so a relational layer can scan, drop, and single-delete by relation.

**Architecture:** The tag rides 4 of the 5 reserved bytes of each 16-byte `HandleEntry` (free forward storage, `O(1)` reads, self-maintaining deletes). The membership index is one generic copy-on-write radix tree (`RadixU64`: `u64` key → `u64` value, `0` = absent) used twice: an **outer** tree keyed by tag whose value bit-packs `(inner_depth:6 | inner_root:58)`, and per-tag **inner** trees keyed by handle storing value `1` for "present." The index root is anchored in the superblock's reserved region and threaded through `Roots` exactly like the freemap root. Untagged chunks (tag `0`) never touch the index.

**Tech Stack:** Rust (edition 2021, MSRV 1.82), the existing `PageCache` / shadow-paging engine, PyO3 0.24 for the Python binding, `tempfile` + the `dual_backing_test!` macro for tests. Spec: `docs/specs/2026-06-02-chunk-tags-design.md`.

**Reference patterns (read before starting):** `src/handle_table.rs` is the radix blueprint `RadixU64` mirrors; `src/transaction.rs` `allocate_inner`/`delete_inner` are the templates the tag operations extend; `src/superblock.rs` `serialize`/`deserialize` show the root-field pattern.

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `src/handle_table.rs` | modify | Add `tag: u32` to `HandleEntry` + its serialization (bytes `[11..15)`). |
| `src/page.rs` | modify | Add `MembershipInterior=0x05` / `MembershipLeaf=0x06` to `PageType` + `from_u8`. Bump `FORMAT_MINOR_VERSION` to 1. |
| `src/superblock.rs` | modify | Add `root_membership_index_page: u64` at bytes `[312..320)` + serialize/deserialize/new_empty. |
| `src/membership_index.rs` | **create** | `RadixU64` (generic COW radix) + `MembershipIndex` (two-level composition) + `TagDropProgress`. |
| `src/transaction.rs` | modify | `Roots.membership_index_page`; thread through begin/commit/open_existing/create_new; hold a `MembershipIndex`; the tag engine methods. |
| `src/error.rs` | modify | Add operational `TagMismatch { handle, expected, actual }`. |
| `src/lib.rs` | modify | Register `mod membership_index`; public `Chisel` tag methods; re-export `TagDropProgress`. |
| `python/src/db.rs`, `python/src/errors.rs`, `python/chisel/chisel.pyi` | modify | Python bindings + the new exception. |
| `tests/tag_ops.rs` | **create** | Integration tests (round-trip, drop, reopen, backward-compat, F1/I12 regression). |

**Phases (dependency-ordered, each independently testable):**
1. Format + roots scaffolding (compiles, round-trips, no behavior change — index always empty).
2. `RadixU64` generic COW radix (standalone unit-tested).
3. `MembershipIndex` two-level composition (standalone unit-tested).
4. Wire `MembershipIndex` into `TransactionManager` (roots recovery + helpers).
5. Engine tag methods (`allocate_tagged`, `tag`, `handles_with_tag`, `delete` self-maintain, `delete_tagged`, `delete_with_tag`).
6. Public `Chisel` surface.
7. Python binding.
8. Integration tests.

After **every** task: `cargo test -p chisel` green, `cargo fmt`, `cargo clippy --all-targets --workspace --exclude chisel-py -- -D warnings` clean before committing. Commit messages omit any tool-referencing text.

---

## Phase 1 — Format + roots scaffolding

Goal: every format type carries the new field, all literals compile, round-trips pass, and a freshly-created DB writes `tag = 0` / `root_membership_index_page = PAGE_ID_NONE` — i.e. no behavior change yet.

### Task 1.1: Add `tag: u32` to `HandleEntry`

**Files:**
- Modify: `src/handle_table.rs` (struct ~115-120, `read_entry`/`write_entry` ~649-669, tombstone literal ~294, test literals)
- Modify: `src/transaction.rs` (`HandleEntry` literals ~1141, 1148, 1295, 1302)

- [ ] **Step 1: Write the failing test** — add to `src/handle_table.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn handle_entry_tag_round_trips_through_a_leaf_slot() {
    let mut buf = [0u8; PAGE_SIZE];
    let entry = HandleEntry { page_id: 42, slot_index: 7, flags: HandleFlags::Live, tag: 0xDEADBEEF };
    HandleTable::write_entry(&mut buf, 3, &entry);
    let read = HandleTable::read_entry(&buf, 3);
    assert_eq!(read, entry);
    assert_eq!(read.tag, 0xDEADBEEF);
    // A zeroed slot reads as the untagged sentinel.
    let zeroed = HandleTable::read_entry(&[0u8; PAGE_SIZE], 0);
    assert_eq!(zeroed.tag, 0);
}
```

(Note: `write_entry`/`read_entry` are private associated fns; the test is in-module so `HandleTable::write_entry` resolves. If they are not already `Self::`-qualified-callable from tests, leave them as-is — the in-module test can call them.)

- [ ] **Step 2: Run test to verify it fails** — `cargo test -p chisel handle_entry_tag_round_trips -- --exact`. Expected: FAIL to compile (`HandleEntry` has no field `tag`).

- [ ] **Step 3: Add the field + serialization.** In `src/handle_table.rs`:

```rust
// struct (was 4 lines, add tag):
pub struct HandleEntry {
    pub page_id: u64,
    pub slot_index: u16,
    pub flags: HandleFlags,
    /// Immutable client-supplied grouping tag; 0 = untagged. Stored in the
    /// entry's reserved bytes [11..15). See docs/specs/2026-06-02-chunk-tags-design.md.
    pub tag: u32,
}
```

```rust
// read_entry: add the tag read.
fn read_entry(buf: &[u8; PAGE_SIZE], index: usize) -> HandleEntry {
    let base = DATA_PAGE_HEADER_SIZE + index * ENTRY_SIZE;
    HandleEntry {
        page_id: u64::from_le_bytes(buf[base..base + 8].try_into().unwrap()),
        slot_index: u16::from_le_bytes(buf[base + 8..base + 10].try_into().unwrap()),
        flags: HandleFlags::from_u8(buf[base + 10]),
        tag: u32::from_le_bytes(buf[base + 11..base + 15].try_into().unwrap()),
    }
}

// On-disk layout per 16-byte entry:
//   [0..8)   page_id (u64 LE)
//   [8..10)  slot_index (u16 LE)
//   [10]     flags (HandleFlags u8)
//   [11..15) tag (u32 LE) — 0 = untagged (immutable chunk tag)
//   [15]     reserved, always zeroed for forward compatibility
fn write_entry(buf: &mut [u8; PAGE_SIZE], index: usize, entry: &HandleEntry) {
    let base = DATA_PAGE_HEADER_SIZE + index * ENTRY_SIZE;
    buf[base..base + 8].copy_from_slice(&entry.page_id.to_le_bytes());
    buf[base + 8..base + 10].copy_from_slice(&entry.slot_index.to_le_bytes());
    buf[base + 10] = entry.flags.to_u8();
    buf[base + 11..base + 15].copy_from_slice(&entry.tag.to_le_bytes());
    buf[base + 15] = 0; // remaining reserved byte
}
```

- [ ] **Step 4: Fix every `HandleEntry { .. }` literal to add `tag`.** The tombstone in `delete_recursive` (~294) uses `tag: 0`. In `src/transaction.rs`, `allocate_inner`'s two literals (~1141 overflow, ~1148 inline) and `update`'s two literals (~1295, ~1302) get `tag: 0` for now (Phase 5 changes `allocate_inner` to carry the real tag and `update` to copy the existing tag forward). Every `HandleEntry { .. }` in `handle_table.rs`'s test module gets `tag: 0` unless the test asserts a tag.

```rust
// handle_table.rs ~294 tombstone:
let tombstone = HandleEntry { page_id: 0, slot_index: 0, flags: HandleFlags::Deleted, tag: 0 };
```

- [ ] **Step 5: Run test to verify it passes** — `cargo test -p chisel handle_entry_tag_round_trips -- --exact`. Expected: PASS. Then `cargo test -p chisel` (whole crate) — Expected: PASS (all existing tests unaffected; tag defaults to 0).

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt && cargo clippy -p chisel --all-targets -- -D warnings
git add src/handle_table.rs src/transaction.rs
git commit -m "feat(handle-table): add immutable u32 tag field to HandleEntry

Stored in entry reserved bytes [11..15); 0 = untagged. read_entry/write_entry
round-trip it; all existing entries (zeroed bytes) read as tag 0. No behavior
change yet — allocate sets tag 0 until allocate_tagged lands."
```

### Task 1.2: Add membership-index page types

**Files:**
- Modify: `src/page.rs` (`PageType` enum ~159, `from_u8` ~172, the two `from_u8` tests ~384-397, `FORMAT_MINOR_VERSION` ~122)

- [ ] **Step 1: Write the failing test** — extend the existing `test_page_type_from_u8_known_variants` (do NOT add a new fn; modify the existing assertions):

```rust
#[test]
fn test_page_type_from_u8_known_variants() {
    assert_eq!(PageType::from_u8(0x01), Some(PageType::HandleTable));
    assert_eq!(PageType::from_u8(0x02), Some(PageType::Data));
    assert_eq!(PageType::from_u8(0x03), Some(PageType::Overflow));
    assert_eq!(PageType::from_u8(0x04), Some(PageType::FreeMap));
    assert_eq!(PageType::from_u8(0x05), Some(PageType::MembershipInterior));
    assert_eq!(PageType::from_u8(0x06), Some(PageType::MembershipLeaf));
}
```

And update `test_page_type_from_u8_rejects_zero_and_unknown` so the "unknown" probe is no longer `0x05`:

```rust
#[test]
fn test_page_type_from_u8_rejects_zero_and_unknown() {
    assert_eq!(PageType::from_u8(0x00), None);
    assert_eq!(PageType::from_u8(0x07), None);
    assert_eq!(PageType::from_u8(0xFF), None);
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p chisel test_page_type_from_u8 -- --exact`. Expected: FAIL to compile (no `MembershipInterior`).

- [ ] **Step 3: Add the variants.** In `src/page.rs`:

```rust
#[repr(u8)]
pub enum PageType {
    HandleTable = 0x01,
    Data = 0x02,
    Overflow = 0x03,
    FreeMap = 0x04,
    MembershipInterior = 0x05,
    MembershipLeaf = 0x06,
}

impl PageType {
    #[allow(dead_code)]
    pub fn from_u8(v: u8) -> Option<PageType> {
        match v {
            0x01 => Some(PageType::HandleTable),
            0x02 => Some(PageType::Data),
            0x03 => Some(PageType::Overflow),
            0x04 => Some(PageType::FreeMap),
            0x05 => Some(PageType::MembershipInterior),
            0x06 => Some(PageType::MembershipLeaf),
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Bump the minor format version** (the on-disk format gains additive fields this phase):

```rust
pub const FORMAT_MINOR_VERSION: u16 = 1;
```

- [ ] **Step 5: Run to verify it passes** — `cargo test -p chisel test_page_type_from_u8 -- --exact`. Expected: PASS. Then `cargo test -p chisel`. Expected: PASS. (The `page_format_version` dispatch test is unaffected: membership pages put their version at byte 1, which the `else` arm already returns.)

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt && cargo clippy -p chisel --all-targets -- -D warnings
git add src/page.rs
git commit -m "feat(page): add MembershipInterior/MembershipLeaf page types, bump format minor

0x05/0x06 for the membership-index radix pages; version byte stays at byte 1
(page_format_version's else arm). FORMAT_MINOR_VERSION 0 -> 1 for the additive
format changes this batch introduces."
```

### Task 1.3: Add the superblock membership-index root

**Files:**
- Modify: `src/superblock.rs` (offset const, struct ~137, serialize ~178, deserialize ~215, new_empty ~321, all test literals + proptest)
- Modify: `src/transaction.rs` (`Roots` struct ~75, `commit_inner` Superblock literal ~880, `open_existing` Roots literal ~401, `create_new` Roots literal ~264)

- [ ] **Step 1: Write the failing test** — add to `src/superblock.rs` tests:

```rust
#[test]
fn membership_root_round_trips_through_serialize() {
    let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
    assert_eq!(sb.root_membership_index_page, page::PAGE_ID_NONE); // default
    sb.root_membership_index_page = 1234;
    let buf = sb.serialize();
    let back = Superblock::deserialize(&buf).unwrap();
    assert_eq!(back.root_membership_index_page, 1234);
    // An old superblock (zeroed reserved region) decodes to page id 0, which we
    // treat as "no index" the same as PAGE_ID_NONE at the Roots layer (Task 4).
    let mut old = sb.serialize();
    old[312..320].fill(0);
    page::stamp_checksum(&mut old);
    assert_eq!(Superblock::deserialize(&old).unwrap().root_membership_index_page, 0);
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p chisel membership_root_round_trips -- --exact`. Expected: FAIL to compile (no field).

- [ ] **Step 3: Add the field + serialization.** In `src/superblock.rs`, near `SUPERBLOCK_COUNT_OFFSET`:

```rust
// Membership-index root (chunk-tags feature). 8 bytes at 312..320, the first
// free reserved bytes after superblock_count. PAGE_ID_NONE when no tagged
// chunk has ever been allocated.
const ROOT_MEMBERSHIP_INDEX_OFFSET: usize = 312;
```

Add to the struct (after `superblock_count`):

```rust
    pub root_membership_index_page: u64,
```

In `serialize()`, after the `superblock_count` write (before the reserved-region comment):

```rust
    buf[ROOT_MEMBERSHIP_INDEX_OFFSET..ROOT_MEMBERSHIP_INDEX_OFFSET + 8]
        .copy_from_slice(&self.root_membership_index_page.to_le_bytes());
```

In `deserialize()`, add to the returned struct literal:

```rust
        root_membership_index_page: u64::from_le_bytes(
            buf[ROOT_MEMBERSHIP_INDEX_OFFSET..ROOT_MEMBERSHIP_INDEX_OFFSET + 8]
                .try_into()
                .unwrap(),
        ),
```

In `new_empty()`:

```rust
        root_membership_index_page: page::PAGE_ID_NONE,
```

- [ ] **Step 4: Fix every `Superblock { .. }` literal.** In `superblock.rs` tests (the 6 literals + the proptest block) add `root_membership_index_page: <value>` (use `page::PAGE_ID_NONE` unless the test sets it). In the proptest round-trip, add a `prop_assert_eq!(back.root_membership_index_page, sb.root_membership_index_page);`.

- [ ] **Step 5: Thread it through `Roots` and the transaction.** In `src/transaction.rs`:

```rust
// Roots struct — add field:
struct Roots {
    handle_table_page: u64,
    freemap_page: u64,
    next_handle: u64,
    total_pages: u64,
    named_roots: [NamedRoot; NAMED_ROOT_COUNT],
    /// Root of the membership index (chunk-tags). PAGE_ID_NONE until the first
    /// tagged chunk is allocated. Threaded like freemap_page.
    membership_index_page: u64,
}
```

`create_new`'s `Roots { .. }` literal (~264): add `membership_index_page: PAGE_ID_NONE,`.
`open_existing`'s `Roots { .. }` literal (~401): add — but normalize page id 0 (old files) to `PAGE_ID_NONE` so the rest of the engine has a single "empty" sentinel:

```rust
        membership_index_page: if sb.root_membership_index_page == 0 {
            PAGE_ID_NONE
        } else {
            sb.root_membership_index_page
        },
```

`commit_inner`'s `Superblock { .. }` literal (~880): add `root_membership_index_page: self.current_roots.membership_index_page,`.

(`begin_inner`/`rollback`/savepoint already clone the whole `Roots`, so the new field is snapshotted automatically — no edits there.)

- [ ] **Step 6: Run the suite** — `cargo test -p chisel`. Expected: PASS (round-trip test green; existing durability/recovery tests unaffected because the field is always `PAGE_ID_NONE` and old files normalize 0→`PAGE_ID_NONE`).

- [ ] **Step 7: fmt + clippy + commit**

```bash
cargo fmt && cargo clippy -p chisel --all-targets -- -D warnings
git add src/superblock.rs src/transaction.rs
git commit -m "feat(superblock): add root_membership_index_page, thread through Roots

New u64 root at bytes 312..320 of the superblock, defaulting to PAGE_ID_NONE.
Threaded through Roots/commit_inner/open_existing/create_new exactly like the
freemap root; old files (zeroed bytes) normalize 0 -> PAGE_ID_NONE on open. No
index is built yet."
```

**Phase 1 done when:** `cargo test -p chisel` green, an existing database opens unchanged, and a fresh database round-trips a superblock carrying `root_membership_index_page`.

---

## Phase 2 — `RadixU64`: a generic copy-on-write radix

Goal: a standalone, unit-tested radix tree (`u64` key → `u64` value, `0` = absent) that mirrors `handle_table.rs`'s COW machinery. Leaf values and interior child pointers share an 8-byte slot layout, so `SLOTS_PER_PAGE` (= 1021) is the single fan-out. Pages use `MembershipLeaf`/`MembershipInterior`; depth is supplied by the caller (the index packs inner depth into the outer value), so this type never caches a long-lived depth except via the public `depth` field the caller sets.

### Task 2.1: Create the module with `RadixU64` (create_root, lookup, insert, delete, iter, any_present, recover_depth)

**Files:**
- Create: `src/membership_index.rs`
- Modify: `src/lib.rs` (register the module — `mod membership_index;` near the other `mod` lines ~22-35, `pub(crate)` so transaction.rs can use it)

- [ ] **Step 1: Register the module.** In `src/lib.rs` add (alongside the other engine modules):

```rust
pub(crate) mod membership_index;
```

- [ ] **Step 2: Write the module with `RadixU64` + its tests.** Create `src/membership_index.rs` with EXACTLY this content (the `MembershipIndex` half is added in Phase 3):

```rust
//! Membership index for chunk tags: maps a `u32` tag to the set of handles that
//! carry it. Built from one generic copy-on-write radix (`RadixU64`: u64 key ->
//! u64 value, 0 = absent) used twice — an outer tree keyed by tag whose value
//! bit-packs `(inner_depth:6 | inner_root:58)`, and per-tag inner trees keyed by
//! handle storing `1` for "present". See docs/specs/2026-06-02-chunk-tags-design.md.
//!
//! Layer dependency: page + page_cache only (strictly below transaction.rs).
//! Like the handle table, this module returns the new root page id after a COW
//! mutation; all page dirtiness lives in `PageCache`, flushed at commit.

use crate::error::Result;
use crate::page::{self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_ID_NONE, PAGE_SIZE};
use crate::page_cache::PageCache;

// Leaf values and interior child pointers are both 8-byte little-endian u64s,
// so one constant is the fan-out at every level. 1021 = (8184 - 16) / 8.
const SLOT_SIZE: usize = 8;
const SLOTS_PER_PAGE: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / SLOT_SIZE; // 1021

fn read_slot(buf: &[u8; PAGE_SIZE], index: usize) -> u64 {
    let off = DATA_PAGE_HEADER_SIZE + index * SLOT_SIZE;
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_slot(buf: &mut [u8; PAGE_SIZE], index: usize, value: u64) {
    let off = DATA_PAGE_HEADER_SIZE + index * SLOT_SIZE;
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// Initialize a fresh zeroed radix page of the given type and stamp it.
fn init_page(cache: &mut PageCache, page_type: PageType) -> Result<u64> {
    let id = cache.new_page()?;
    // Page id 0 is the "no child" sentinel in interior nodes; a real DB reserves
    // pages 0..N for superblocks so new_page() never returns 0. (mirrors I8)
    debug_assert_ne!(id, 0, "membership-index pages must not use page id 0");
    let buf = cache.get_mut(id)?;
    buf.fill(0);
    buf[0] = page_type as u8;
    buf[1] = page::PAGE_FORMAT_VERSION_CURRENT; // version at byte 1 (no FLAG byte)
    page::stamp_checksum(buf);
    Ok(id)
}

/// A copy-on-write radix tree: `u64` key -> `u64` value, where `0` means absent
/// (a zeroed leaf reads as all-absent, mirroring the handle table's tombstone
/// trick). The caller owns `depth` and the root page id.
pub(crate) struct RadixU64 {
    pub depth: u32,
}

impl RadixU64 {
    #[allow(dead_code)] // used by tests; production builds RadixU64 { depth } directly
    pub fn new() -> RadixU64 {
        RadixU64 { depth: 0 }
    }

    /// Create a new empty root leaf. Returns its page id.
    pub fn create_root(&mut self, cache: &mut PageCache) -> Result<u64> {
        self.depth = 0;
        init_page(cache, PageType::MembershipLeaf)
    }

    fn capacity(&self) -> u64 {
        let mut cap = SLOTS_PER_PAGE as u64;
        for _ in 0..self.depth {
            cap *= SLOTS_PER_PAGE as u64;
        }
        cap
    }

    fn span_at_level(&self, level: u32) -> u64 {
        let mut span = SLOTS_PER_PAGE as u64;
        for _ in 1..level {
            span *= SLOTS_PER_PAGE as u64;
        }
        span
    }

    fn find_leaf(&self, cache: &mut PageCache, root: u64, key: u64) -> Result<Option<(u64, usize)>> {
        if self.depth > 0 && key >= self.capacity() {
            return Ok(None);
        }
        if self.depth == 0 {
            return Ok(Some((root, (key % SLOTS_PER_PAGE as u64) as usize)));
        }
        let mut current = root;
        let mut remaining = key;
        for level in (1..=self.depth).rev() {
            let span = self.span_at_level(level);
            let child_idx = (remaining / span) as usize;
            remaining %= span;
            let child = read_slot(cache.get(current)?, child_idx);
            if child == 0 {
                return Ok(None);
            }
            current = child;
        }
        Ok(Some((current, (remaining % SLOTS_PER_PAGE as u64) as usize)))
    }

    /// Return the value for `key`, or `0` if absent.
    pub fn lookup(&self, cache: &mut PageCache, root: u64, key: u64) -> Result<u64> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let Some((leaf, idx)) = self.find_leaf(cache, root, key)? else {
            return Ok(0);
        };
        Ok(read_slot(cache.get(leaf)?, idx))
    }

    fn grow(&mut self, cache: &mut PageCache, old_root: u64) -> Result<u64> {
        let new_root = cache.new_page()?;
        debug_assert_ne!(new_root, 0);
        let buf = cache.get_mut(new_root)?;
        buf.fill(0);
        buf[0] = PageType::MembershipInterior as u8;
        buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
        write_slot(buf, 0, old_root); // old root becomes child 0
        page::stamp_checksum(buf);
        self.depth += 1;
        Ok(new_root)
    }

    /// Insert `value` (must be non-zero) at `key`. Returns the new root.
    pub fn insert(&mut self, cache: &mut PageCache, root: u64, key: u64, value: u64) -> Result<u64> {
        debug_assert_ne!(value, 0, "0 is the absent sentinel; cannot be stored");
        let mut current_root = root;
        while key >= self.capacity() {
            current_root = self.grow(cache, current_root)?;
        }
        self.insert_recursive(cache, current_root, key, value, self.depth)
    }

    fn insert_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        key: u64,
        value: u64,
        level: u32,
    ) -> Result<u64> {
        let new_page = cache.new_page()?;
        debug_assert_ne!(new_page, 0);
        {
            let old: [u8; PAGE_SIZE] = *cache.get(page)?;
            cache.get_mut(new_page)?.copy_from_slice(&old);
        }
        if level == 0 {
            let idx = (key % SLOTS_PER_PAGE as u64) as usize;
            let buf = cache.get_mut(new_page)?;
            write_slot(buf, idx, value);
            page::stamp_checksum(buf);
            Ok(new_page)
        } else {
            let span = self.span_at_level(level);
            let child_idx = (key / span) as usize;
            let child = read_slot(cache.get(new_page)?, child_idx);
            let actual_child = if child == 0 {
                let pt = if level == 1 {
                    PageType::MembershipLeaf
                } else {
                    PageType::MembershipInterior
                };
                init_page(cache, pt)?
            } else {
                child
            };
            let new_child = self.insert_recursive(cache, actual_child, key % span, value, level - 1)?;
            let buf = cache.get_mut(new_page)?;
            write_slot(buf, child_idx, new_child);
            page::stamp_checksum(buf);
            Ok(new_page)
        }
    }

    /// Set `key` to absent. Returns `(new_root, prev_value)`; `prev_value == 0`
    /// means it was already absent and no COW happened.
    pub fn delete(&mut self, cache: &mut PageCache, root: u64, key: u64) -> Result<(u64, u64)> {
        if root == PAGE_ID_NONE || key >= self.capacity() {
            return Ok((root, 0));
        }
        self.delete_recursive(cache, root, key, self.depth)
    }

    fn delete_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        key: u64,
        level: u32,
    ) -> Result<(u64, u64)> {
        if level == 0 {
            let idx = (key % SLOTS_PER_PAGE as u64) as usize;
            let prev = read_slot(cache.get(page)?, idx);
            if prev == 0 {
                return Ok((page, 0));
            }
            let new_leaf = cache.new_page()?;
            debug_assert_ne!(new_leaf, 0);
            {
                let old: [u8; PAGE_SIZE] = *cache.get(page)?;
                *cache.get_mut(new_leaf)? = old;
            }
            {
                let buf = cache.get_mut(new_leaf)?;
                write_slot(buf, idx, 0);
                page::stamp_checksum(buf);
            }
            Ok((new_leaf, prev))
        } else {
            let span = self.span_at_level(level);
            let child_idx = (key / span) as usize;
            let child = read_slot(cache.get(page)?, child_idx);
            if child == 0 {
                return Ok((page, 0));
            }
            let (new_child, prev) = self.delete_recursive(cache, child, key % span, level - 1)?;
            if prev == 0 {
                return Ok((page, 0));
            }
            let new_page = cache.new_page()?;
            debug_assert_ne!(new_page, 0);
            {
                let old: [u8; PAGE_SIZE] = *cache.get(page)?;
                *cache.get_mut(new_page)? = old;
            }
            {
                let buf = cache.get_mut(new_page)?;
                write_slot(buf, child_idx, new_child);
                page::stamp_checksum(buf);
            }
            Ok((new_page, prev))
        }
    }

    /// Enumerate all `(key, value)` pairs with a non-zero value. Order is
    /// unspecified.
    pub fn iter(&self, cache: &mut PageCache, root: u64) -> Result<Vec<(u64, u64)>> {
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.iter_recursive(cache, root, 0, self.depth, &mut out)?;
        Ok(out)
    }

    fn iter_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        base: u64,
        level: u32,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                let v = read_slot(buf, i);
                if v != 0 {
                    out.push((base + i as u64, v));
                }
            }
        } else {
            let span = self.span_at_level(level);
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| (i, read_slot(buf, i)))
                    .filter(|(_, c)| *c != 0)
                    .collect()
            };
            for (i, child) in children {
                self.iter_recursive(cache, child, base + i as u64 * span, level - 1, out)?;
            }
        }
        Ok(())
    }

    /// Like `iter` but stops after collecting `limit` pairs — touches at most
    /// ~`limit` leaves' worth of work, giving the caller a bounded-time pass.
    pub fn iter_bounded(
        &self,
        cache: &mut PageCache,
        root: u64,
        limit: usize,
    ) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        if root == PAGE_ID_NONE || limit == 0 {
            return Ok(out);
        }
        self.iter_bounded_recursive(cache, root, 0, self.depth, limit, &mut out)?;
        Ok(out)
    }

    fn iter_bounded_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        base: u64,
        level: u32,
        limit: usize,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        if out.len() >= limit {
            return Ok(());
        }
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                if out.len() >= limit {
                    break;
                }
                let v = read_slot(buf, i);
                if v != 0 {
                    out.push((base + i as u64, v));
                }
            }
        } else {
            let span = self.span_at_level(level);
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| (i, read_slot(buf, i)))
                    .filter(|(_, c)| *c != 0)
                    .collect()
            };
            for (i, child) in children {
                if out.len() >= limit {
                    break;
                }
                self.iter_bounded_recursive(cache, child, base + i as u64 * span, level - 1, limit, out)?;
            }
        }
        Ok(())
    }

    /// Early-exit emptiness check: `true` if any key has a non-zero value.
    /// Cheap when the tree is non-empty (stops at the first hit); `O(tree)` only
    /// when it is empty.
    pub fn any_present(&self, cache: &mut PageCache, root: u64) -> Result<bool> {
        if root == PAGE_ID_NONE {
            return Ok(false);
        }
        self.any_recursive(cache, root, self.depth)
    }

    fn any_recursive(&self, cache: &mut PageCache, page: u64, level: u32) -> Result<bool> {
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                if read_slot(buf, i) != 0 {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            let children: Vec<u64> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| read_slot(buf, i))
                    .filter(|c| *c != 0)
                    .collect()
            };
            for child in children {
                if self.any_recursive(cache, child, level - 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    /// Recover a tree's depth by walking the leftmost spine from `root` (mirrors
    /// the handle-table open-time depth recovery; relies on `grow` always
    /// installing the old root at child 0).
    pub fn recover_depth(cache: &mut PageCache, root: u64) -> Result<u32> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let mut depth = 0u32;
        let mut current = root;
        loop {
            let buf = cache.get(current)?;
            if buf[0] != PageType::MembershipInterior as u8 {
                break;
            }
            depth += 1;
            let child = read_slot(buf, 0);
            if child == 0 {
                break;
            }
            current = child;
        }
        Ok(depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    // Same fixture shape as handle_table.rs tests: reserve pages 0/1 so
    // new_page() never returns the zero-child sentinel.
    fn cache(max_pages: usize) -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut c = PageCache::new(
            io,
            max_pages as u64 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        c.set_next_page_id(2);
        c
    }

    #[test]
    fn insert_lookup_delete_single_level() {
        let mut c = cache(64);
        let mut t = RadixU64::new();
        let root = t.create_root(&mut c).unwrap();
        assert_eq!(t.lookup(&mut c, root, 5).unwrap(), 0);
        let r = t.insert(&mut c, root, 5, 99).unwrap();
        assert_eq!(t.lookup(&mut c, r, 5).unwrap(), 99);
        assert!(t.any_present(&mut c, r).unwrap());
        let (r2, prev) = t.delete(&mut c, r, 5).unwrap();
        assert_eq!(prev, 99);
        assert_eq!(t.lookup(&mut c, r2, 5).unwrap(), 0);
        assert!(!t.any_present(&mut c, r2).unwrap());
    }

    #[test]
    fn grows_and_iterates_across_levels() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        // Insert keys that force a grow past one leaf (SLOTS_PER_PAGE = 1021).
        for k in [0u64, 1, 1021, 2000, 1_000_000] {
            root = t.insert(&mut c, root, k, k + 7).unwrap();
        }
        assert!(t.depth >= 1, "tree should have grown");
        for k in [0u64, 1, 1021, 2000, 1_000_000] {
            assert_eq!(t.lookup(&mut c, root, k).unwrap(), k + 7);
        }
        let mut pairs = t.iter(&mut c, root).unwrap();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![(0, 7), (1, 8), (1021, 1028), (2000, 2007), (1_000_000, 1_000_007)]
        );
    }

    #[test]
    fn delete_absent_is_noop() {
        let mut c = cache(64);
        let mut t = RadixU64::new();
        let root = t.create_root(&mut c).unwrap();
        let (r, prev) = t.delete(&mut c, root, 42).unwrap();
        assert_eq!(prev, 0);
        assert_eq!(r, root, "no COW for an absent key");
    }

    #[test]
    fn recover_depth_matches_after_grow() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        root = t.insert(&mut c, root, 5_000_000, 1).unwrap();
        let recovered = RadixU64::recover_depth(&mut c, root).unwrap();
        assert_eq!(recovered, t.depth);
    }
}
```

- [ ] **Step 3: Run the tests** — `cargo test -p chisel membership_index::tests`. Expected: PASS (4 tests). If `cache()`'s `PageCache::new` signature differs, copy the exact fixture from `src/handle_table.rs`'s `ht_cache`.

- [ ] **Step 4: fmt + clippy + commit**

```bash
cargo fmt && cargo clippy -p chisel --all-targets -- -D warnings
git add src/membership_index.rs src/lib.rs
git commit -m "feat(membership-index): RadixU64 copy-on-write radix (u64 key -> u64 value)

Standalone generic radix mirroring the handle-table COW pattern: 1021-way
fan-out, grow/insert/delete/iter/any_present/recover_depth. 0 is the absent
sentinel. Used twice by the two-level membership index (next task)."
```

---

## Phase 3 — `MembershipIndex`: two-level composition

Goal: compose two `RadixU64` trees — an outer (tag → packed inner ref) and per-tag inner trees (handle → 1) — into the membership API the engine calls. The outer depth is tracked on the struct; each inner tree's depth is bit-packed into the outer value alongside its root.

### Task 3.1: Add `MembershipIndex` + `TagDropProgress`

**Files:**
- Modify: `src/membership_index.rs` (append below `RadixU64`)

- [ ] **Step 1: Write the failing tests** — append to the `#[cfg(test)] mod tests` in `src/membership_index.rs`:

```rust
    #[test]
    fn membership_insert_contains_remove() {
        let mut c = cache(8192);
        let mut idx = MembershipIndex::new();
        let mut root = PAGE_ID_NONE;
        root = idx.insert(&mut c, root, 7, 100).unwrap();
        root = idx.insert(&mut c, root, 7, 200).unwrap();
        root = idx.insert(&mut c, root, 9, 300).unwrap();
        assert!(idx.contains(&mut c, root, 7, 100).unwrap());
        assert!(idx.contains(&mut c, root, 7, 200).unwrap());
        assert!(!idx.contains(&mut c, root, 7, 300).unwrap());
        let mut h7 = idx.handles_for_tag(&mut c, root, 7).unwrap();
        h7.sort();
        assert_eq!(h7, vec![100, 200]);
        assert_eq!(idx.handles_for_tag(&mut c, root, 9).unwrap(), vec![300]);

        let (root2, removed) = idx.remove(&mut c, root, 7, 100).unwrap();
        assert!(removed);
        assert!(!idx.contains(&mut c, root2, 7, 100).unwrap());
        assert!(idx.contains(&mut c, root2, 7, 200).unwrap());
        let (_root3, removed_again) = idx.remove(&mut c, root2, 7, 100).unwrap();
        assert!(!removed_again, "removing an absent member reports false");
    }

    #[test]
    fn handles_for_tag_bounded_caps_results() {
        let mut c = cache(16384);
        let mut idx = MembershipIndex::new();
        let mut root = PAGE_ID_NONE;
        for h in 0..10u64 {
            root = idx.insert(&mut c, root, 3, 1000 + h).unwrap();
        }
        // Bounded enumeration returns at most `limit`.
        assert_eq!(idx.handles_for_tag_bounded(&mut c, root, 3, 4).unwrap().len(), 4);
        assert_eq!(idx.handles_for_tag_bounded(&mut c, root, 3, 100).unwrap().len(), 10);
        // Requesting max+1 lets the engine detect "more remain".
        assert_eq!(idx.handles_for_tag_bounded(&mut c, root, 3, 5).unwrap().len(), 5);
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p chisel membership_insert_contains_remove`. Expected: FAIL to compile (no `MembershipIndex`).

- [ ] **Step 3: Implement.** Append to `src/membership_index.rs` (above the `#[cfg(test)]` module):

```rust
// The outer tree's value bit-packs the inner tree's (depth, root): depth in the
// top 6 bits (max radix depth for u64 keys is < 7), root in the low 58 bits.
// Page ids never approach 2^58 (2^58 * 8 KiB ~= 2.3 ZiB), so this is lossless.
const INNER_ROOT_BITS: u32 = 58;
const INNER_ROOT_MASK: u64 = (1u64 << INNER_ROOT_BITS) - 1;

fn pack_inner(root: u64, depth: u32) -> u64 {
    debug_assert!(root <= INNER_ROOT_MASK, "page id exceeds 2^58");
    debug_assert!(depth < (1 << (64 - INNER_ROOT_BITS)), "inner depth too large");
    ((depth as u64) << INNER_ROOT_BITS) | root
}

fn unpack_inner(packed: u64) -> (u64, u32) {
    (packed & INNER_ROOT_MASK, (packed >> INNER_ROOT_BITS) as u32)
}

/// Progress report from a bounded `delete_with_tag` pass.
#[derive(Debug, Clone)]
pub struct TagDropProgress {
    /// Handles removed from the index in this pass (the engine deletes their
    /// chunks). May be fewer than `max` if the tag emptied first.
    pub deleted: Vec<u64>,
    /// `true` if the tag has no remaining members after this pass.
    pub complete: bool,
}

/// Two-level membership index: tag -> {handles}. Owns the outer tree's depth;
/// inner depths ride the outer values via `pack_inner`. The caller (transaction
/// layer) owns the outer root and threads it through `Roots`/superblock.
pub(crate) struct MembershipIndex {
    outer_depth: u32,
}

impl MembershipIndex {
    pub fn new() -> MembershipIndex {
        MembershipIndex { outer_depth: 0 }
    }

    /// Restore the outer depth on open (the engine calls `RadixU64::recover_depth`
    /// on the index root and passes it here).
    pub fn set_outer_depth(&mut self, depth: u32) {
        self.outer_depth = depth;
    }

    /// Insert `(tag, handle)`. `tag` must be non-zero (0 = untagged is filtered
    /// by the caller). Returns the new outer root.
    pub fn insert(
        &mut self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
    ) -> Result<u64> {
        let mut outer = RadixU64 { depth: self.outer_depth };
        let root = if outer_root == PAGE_ID_NONE {
            outer.create_root(cache)?
        } else {
            outer_root
        };
        let (mut inner_root, inner_depth) = unpack_inner(outer.lookup(cache, root, tag as u64)?);
        let mut inner = RadixU64 { depth: inner_depth };
        if inner_root == 0 {
            inner_root = inner.create_root(cache)?;
        }
        let new_inner_root = inner.insert(cache, inner_root, handle, 1)?;
        let packed = pack_inner(new_inner_root, inner.depth);
        let new_outer_root = outer.insert(cache, root, tag as u64, packed)?;
        self.outer_depth = outer.depth;
        Ok(new_outer_root)
    }

    /// Remove `(tag, handle)`. Returns `(new_outer_root, was_present)`. Reclaims
    /// the tag's outer entry when its last member is removed (cheap: the
    /// emptiness check early-exits unless the tag truly emptied).
    pub fn remove(
        &mut self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
    ) -> Result<(u64, bool)> {
        if outer_root == PAGE_ID_NONE {
            return Ok((outer_root, false));
        }
        let mut outer = RadixU64 { depth: self.outer_depth };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        if inner_root == 0 {
            return Ok((outer_root, false));
        }
        let mut inner = RadixU64 { depth: inner_depth };
        let (new_inner_root, prev) = inner.delete(cache, inner_root, handle)?;
        if prev == 0 {
            return Ok((outer_root, false));
        }
        let new_outer_root = if inner.any_present(cache, new_inner_root)? {
            outer.insert(cache, outer_root, tag as u64, pack_inner(new_inner_root, inner.depth))?
        } else {
            let (r, _) = outer.delete(cache, outer_root, tag as u64)?;
            r
        };
        self.outer_depth = outer.depth;
        Ok((new_outer_root, true))
    }

    #[allow(dead_code)] // index-completeness + tests; production reads tags via handle_table.lookup
    pub fn contains(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
    ) -> Result<bool> {
        if outer_root == PAGE_ID_NONE {
            return Ok(false);
        }
        let outer = RadixU64 { depth: self.outer_depth };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        if inner_root == 0 {
            return Ok(false);
        }
        let inner = RadixU64 { depth: inner_depth };
        Ok(inner.lookup(cache, inner_root, handle)? != 0)
    }

    pub fn handles_for_tag(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
    ) -> Result<Vec<u64>> {
        if outer_root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let outer = RadixU64 { depth: self.outer_depth };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        if inner_root == 0 {
            return Ok(Vec::new());
        }
        let inner = RadixU64 { depth: inner_depth };
        Ok(inner
            .iter(cache, inner_root)?
            .into_iter()
            .map(|(h, _)| h)
            .collect())
    }

    /// Return at most `limit` handles of `tag` (bounded enumeration). The engine
    /// loops `delete` over these, so each `delete_with_tag` pass is bounded-time;
    /// `complete` is derived by the engine (it requests `max + 1` and checks the
    /// count). The full drop (members + chunks) lives in the engine because
    /// deleting a member's chunk is the engine's responsibility, not the index's.
    pub fn handles_for_tag_bounded(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        limit: usize,
    ) -> Result<Vec<u64>> {
        if outer_root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let outer = RadixU64 { depth: self.outer_depth };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        if inner_root == 0 {
            return Ok(Vec::new());
        }
        let inner = RadixU64 { depth: inner_depth };
        Ok(inner
            .iter_bounded(cache, inner_root, limit)?
            .into_iter()
            .map(|(h, _)| h)
            .collect())
    }
}
```

- [ ] **Step 4: Run the tests** — `cargo test -p chisel membership_index::tests`. Expected: PASS (6 tests total).

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt && cargo clippy -p chisel --all-targets -- -D warnings
git add src/membership_index.rs
git commit -m "feat(membership-index): MembershipIndex two-level composition + TagDropProgress

Outer RadixU64 (tag -> packed inner ref) over per-tag inner RadixU64
(handle -> 1). insert/remove/contains/handles_for_tag plus bounded
handles_for_tag_bounded; TagDropProgress type for the engine drop. Reclaims an
emptied tag's outer entry via an early-exit emptiness check."
```

**Phase 3 done when:** the membership index round-trips inserts/removes against a bare `PageCache` with no transaction layer involved, and bounded enumeration caps results.

---

## Phase 4 — Wire `MembershipIndex` into `TransactionManager`

Goal: the engine owns a `MembershipIndex`, recovers its outer depth on open, and the field is snapshotted by the existing `Roots` clones (already done in Phase 1). No tag operations yet — this phase just makes the index a live engine member.

### Task 4.1: Add the `membership_index` field + open-time depth recovery

**Files:**
- Modify: `src/transaction.rs` (`use` line; `TransactionManager` struct; `create_new` + `open_existing` `TransactionManager { .. }` literals)

- [ ] **Step 1: Import the types.** Near the other `use crate::...` lines in `src/transaction.rs`:

```rust
use crate::membership_index::{MembershipIndex, RadixU64, TagDropProgress};
```

- [ ] **Step 2: Add the struct field.** In the `TransactionManager` struct, after `handle_table: HandleTable,`:

```rust
    /// In-memory state for the membership index (chunk tags). Owns only the
    /// outer tree's depth; the root lives in current/committed `Roots`.
    membership_index: MembershipIndex,
```

- [ ] **Step 3: Initialize in `create_new`.** In `create_new`'s `TransactionManager { .. }` literal add:

```rust
            membership_index: MembershipIndex::new(),
```

- [ ] **Step 4: Recover depth in `open_existing`.** After the `Roots` literal is built and before the `TransactionManager { .. }` literal, add (mirroring the handle-table depth recovery just above it):

```rust
        let mut membership_index = MembershipIndex::new();
        if roots.membership_index_page != PAGE_ID_NONE {
            let depth = {
                let mut cache = cache; // existing &mut PageCache in scope; reuse the same binding name used above
                RadixU64::recover_depth(&mut cache, roots.membership_index_page)?
            };
            membership_index.set_outer_depth(depth);
        }
```

> If `cache` is already moved into the `TransactionManager` literal at this point, perform the `recover_depth` call earlier, right after the `Roots` literal where `cache` is still a live `&mut`. The exact binding lives next to the handle-table depth-walk (`ht.set_depth(depth)`); place this block immediately after it and add `membership_index` to the `TransactionManager` literal.

- [ ] **Step 5: Add to the `open_existing` `TransactionManager { .. }` literal:**

```rust
            membership_index,
```

- [ ] **Step 6: Run + commit** — `cargo test -p chisel` (Expected: PASS — no behavior change; the index is empty everywhere). Then fmt + clippy.

```bash
cargo fmt && cargo clippy -p chisel --all-targets -- -D warnings
git add src/transaction.rs
git commit -m "feat(transaction): hold a MembershipIndex, recover its depth on open

The engine owns a MembershipIndex; open_existing rebuilds the outer depth via
RadixU64::recover_depth. The root is already threaded through Roots (Phase 1).
No tag operations yet."
```

---

## Phase 5 — Engine tag operations

Goal: the six engine methods. `allocate`/`delete` keep their signatures; `delete` becomes self-maintaining.

### Task 5.1: `allocate_tagged` (refactor `allocate_inner` to take a tag)

**Files:** Modify `src/transaction.rs` (`allocate`, `allocate_inner` ~1122-1168)

- [ ] **Step 1: Write the failing test** in `transaction.rs` tests:

```rust
    #[test]
    fn allocate_tagged_then_tag_and_handles_with_tag() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"row", 42).unwrap();
        let u = tm.allocate(b"untagged").unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.tag(h).unwrap(), 42);
        assert_eq!(tm.tag(u).unwrap(), 0);
        assert_eq!(tm.handles_with_tag(42).unwrap(), vec![h]);
        assert_eq!(tm.handles_with_tag(99).unwrap(), Vec::<u64>::new());
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p chisel allocate_tagged_then_tag`. Expected: FAIL (no `allocate_tagged`/`tag`/`handles_with_tag`).

- [ ] **Step 3: Refactor + add.** Replace `allocate`/`allocate_inner` with:

```rust
    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value, 0);
        self.poison_on_fatal(result)
    }

    pub fn allocate_tagged(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value, tag);
        self.poison_on_fatal(result)
    }

    fn allocate_inner(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let handle = self.current_roots.next_handle;
        self.current_roots.next_handle += 1;

        let entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry { page_id: first_page, slot_index: 0, flags: HandleFlags::Overflow, tag }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            HandleEntry { page_id: data_page_id, slot_index: slot, flags: HandleFlags::Live, tag }
        };

        self.ensure_handle_table()?;
        let new_root = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .insert(&mut cache, self.current_roots.handle_table_page, handle, &entry)?
        };
        self.current_roots.handle_table_page = new_root;

        if tag != 0 {
            let new_index_root = {
                let mut cache = self.cache.borrow_mut();
                self.membership_index.insert(
                    &mut cache,
                    self.current_roots.membership_index_page,
                    tag,
                    handle,
                )?
            };
            self.current_roots.membership_index_page = new_index_root;
        }
        Ok(handle)
    }
```

(This also replaces the two `tag: 0` placeholders from Task 1.1 with the real `tag`.)

- [ ] **Step 4: Add `tag` + `handles_with_tag`** (same file):

```rust
    pub fn tag(&self, handle: u64) -> Result<u32> {
        self.check_alive()?;
        let result = self.tag_inner(handle);
        self.poison_on_fatal(result)
    }

    fn tag_inner(&self, handle: u64) -> Result<u32> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };
        if root == PAGE_ID_NONE {
            return Err(ChiselError::InvalidHandle(handle));
        }
        let mut cache = self.cache.borrow_mut();
        let entry = self
            .handle_table
            .lookup(&mut cache, root, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;
        Ok(entry.tag)
    }

    pub fn handles_with_tag(&self, tag: u32) -> Result<Vec<u64>> {
        self.check_alive()?;
        let result = self.handles_with_tag_inner(tag);
        self.poison_on_fatal(result)
    }

    fn handles_with_tag_inner(&self, tag: u32) -> Result<Vec<u64>> {
        let root = if self.active_txn {
            self.current_roots.membership_index_page
        } else {
            self.committed_roots.membership_index_page
        };
        let mut cache = self.cache.borrow_mut();
        self.membership_index.handles_for_tag(&mut cache, root, tag)
    }
```

- [ ] **Step 5: Run to verify it passes** — `cargo test -p chisel allocate_tagged_then_tag`. Expected: PASS. Then `cargo test -p chisel`. Expected: PASS.

- [ ] **Step 6: fmt + clippy + commit** (`feat(transaction): allocate_tagged, tag, handles_with_tag`).

### Task 5.2: Make `delete` self-maintain the index

**Files:** Modify `src/transaction.rs` (`delete_inner` ~1331-1393)

- [ ] **Step 1: Write the failing test:**

```rust
    #[test]
    fn delete_removes_tagged_chunk_from_index() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"row", 7).unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
        tm.begin().unwrap();
        tm.delete(h).unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(7).unwrap(), Vec::<u64>::new());
    }
```

- [ ] **Step 2: Run to verify it fails** — Expected: the assertion fails (index still contains `h`) because `delete_inner` does not yet touch the index.

- [ ] **Step 3: Add the index removal at the end of `delete_inner`,** right before `Ok(())` (after `self.current_roots.handle_table_page = new_root;`):

```rust
        if entry.tag != 0 {
            let (new_index_root, _removed) = {
                let mut cache = self.cache.borrow_mut();
                self.membership_index.remove(
                    &mut cache,
                    self.current_roots.membership_index_page,
                    entry.tag,
                    handle,
                )?
            };
            self.current_roots.membership_index_page = new_index_root;
        }
```

- [ ] **Step 4: Run + commit** — Expected: PASS. fmt + clippy. (`feat(transaction): delete self-maintains the membership index`).

### Task 5.3: `delete_tagged` + `TagMismatch` error

**Files:** Modify `src/error.rs` (operational variant + Display), `src/transaction.rs` (`delete_tagged`)

- [ ] **Step 1: Write the failing test:**

```rust
    #[test]
    fn delete_tagged_rejects_wrong_tag() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"row", 5).unwrap();
        // Wrong tag: error, nothing deleted, index intact.
        let err = tm.delete_tagged(h, 6).unwrap_err();
        assert!(matches!(err, ChiselError::TagMismatch { handle, expected: 6, actual: 5 } if handle == h));
        assert_eq!(tm.handles_with_tag(5).unwrap(), vec![h]);
        // Right tag: deletes.
        tm.delete_tagged(h, 5).unwrap();
        assert_eq!(tm.handles_with_tag(5).unwrap(), Vec::<u64>::new());
        tm.commit().unwrap();
    }
```

- [ ] **Step 2: Run to verify it fails** — Expected: FAIL to compile (no `TagMismatch`/`delete_tagged`).

- [ ] **Step 3: Add the error variant.** In `src/error.rs`'s Operational block:

```rust
    /// `delete_tagged` was given a tag that does not match the chunk's actual
    /// tag. Operational: the caller passed the wrong tag; chunk and index are
    /// untouched, so the transaction may continue.
    TagMismatch { handle: u64, expected: u32, actual: u32 },
```

In the `Display` impl's match:

```rust
            ChiselError::TagMismatch { handle, expected, actual } => {
                write!(f, "handle {handle} has tag {actual}, not the expected {expected}")
            }
```

(`is_fatal()` needs no change — operational variants are simply absent from its fatal `matches!` list. `source()`/`From` need no change.)

- [ ] **Step 4: Add `delete_tagged`** in `src/transaction.rs`:

```rust
    pub fn delete_tagged(&mut self, handle: u64, tag: u32) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_tagged_inner(handle, tag);
        self.poison_on_fatal(result)
    }

    fn delete_tagged_inner(&mut self, handle: u64, tag: u32) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        // Lookup-then-delete: verify the tag before mutating anything.
        let actual = {
            let root = self.current_roots.handle_table_page;
            if root == PAGE_ID_NONE {
                return Err(ChiselError::InvalidHandle(handle));
            }
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .lookup(&mut cache, root, handle)?
                .ok_or(ChiselError::InvalidHandle(handle))?
                .tag
        };
        if actual != tag {
            return Err(ChiselError::TagMismatch { handle, expected: tag, actual });
        }
        self.delete_inner(handle)
    }
```

- [ ] **Step 5: Run + commit** — Expected: PASS. fmt + clippy. (`feat: delete_tagged with tag verification (TagMismatch)`).

### Task 5.4: `delete_with_tag` (bounded relation drop)

**Files:** Modify `src/transaction.rs` (`delete_with_tag`)

- [ ] **Step 1: Write the failing test:**

```rust
    #[test]
    fn delete_with_tag_drops_in_bounded_batches() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let mut hs = Vec::new();
        for i in 0..10u64 {
            hs.push(tm.allocate_tagged(format!("row{i}").as_bytes(), 3).unwrap());
        }
        tm.commit().unwrap();
        tm.begin().unwrap();
        let p1 = tm.delete_with_tag(3, 4).unwrap();
        assert_eq!(p1.deleted.len(), 4);
        assert!(!p1.complete);
        let p2 = tm.delete_with_tag(3, 100).unwrap();
        assert_eq!(p2.deleted.len(), 6);
        assert!(p2.complete);
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(3).unwrap(), Vec::<u64>::new());
        // The chunks themselves are gone too.
        for h in hs {
            assert!(tm.read(h).is_err());
        }
    }
```

- [ ] **Step 2: Run to verify it fails** — Expected: FAIL to compile (no `delete_with_tag`).

- [ ] **Step 3: Add `delete_with_tag`** in `src/transaction.rs`:

```rust
    pub fn delete_with_tag(&mut self, tag: u32, max: usize) -> Result<TagDropProgress> {
        self.check_alive()?;
        let result = self.delete_with_tag_inner(tag, max);
        self.poison_on_fatal(result)
    }

    fn delete_with_tag_inner(&mut self, tag: u32, max: usize) -> Result<TagDropProgress> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if max == 0 {
            return Ok(TagDropProgress { deleted: Vec::new(), complete: false });
        }
        // Bounded enumeration: ask for max+1 to learn whether more remain.
        let members = {
            let root = self.current_roots.membership_index_page;
            let mut cache = self.cache.borrow_mut();
            self.membership_index
                .handles_for_tag_bounded(&mut cache, root, tag, max + 1)?
        };
        let complete = members.len() <= max;
        let take: Vec<u64> = members.into_iter().take(max).collect();
        for &h in &take {
            self.delete_inner(h)?; // self-maintains the index and frees the chunk
        }
        Ok(TagDropProgress { deleted: take, complete })
    }
```

- [ ] **Step 4: Cover the poison invariant.** In `poisoned_manager_rejects_every_public_entry_point` (~1956), add calls asserting `Err(ChiselError::Poisoned)` for `allocate_tagged`, `tag`, `handles_with_tag`, `delete_tagged`, `delete_with_tag`.

- [ ] **Step 5: Run + commit** — `cargo test -p chisel`. Expected: PASS. fmt + clippy. (`feat: delete_with_tag bounded relation drop`).

**Phase 5 done when:** all engine tag ops pass at the `TransactionManager` level, including the poison-rejection coverage.

---

## Phase 6 — Public `Chisel` surface

**Files:** Modify `src/lib.rs` (re-export + 5 delegating methods)

- [ ] **Step 1: Re-export the progress type.** Near the other `pub use` lines:

```rust
pub use membership_index::TagDropProgress;
```

And mark the type `#[non_exhaustive]` in `src/membership_index.rs` (I36 convention for public types — external callers read its fields but only Chisel constructs it):

```rust
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TagDropProgress {
    pub deleted: Vec<u64>,
    pub complete: bool,
}
```

- [ ] **Step 2: Add the delegating methods** to `impl Chisel` (with doc comments mirroring `allocate`/`delete`):

```rust
    /// Allocate a chunk carrying an immutable `tag` (0 = untagged). Tagged
    /// chunks are indexed so they can be enumerated and bulk-dropped by tag.
    pub fn allocate_tagged(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        self.txm.allocate_tagged(value, tag)
    }

    /// The chunk's immutable tag (0 = untagged). `O(1)`.
    pub fn tag(&self, handle: u64) -> Result<u32> {
        self.txm.tag(handle)
    }

    /// Handles carrying `tag`. Order is unspecified.
    pub fn handles_with_tag(&self, tag: u32) -> Result<Vec<u64>> {
        self.txm.handles_with_tag(tag)
    }

    /// Delete a chunk, asserting it carries `tag` (errors `TagMismatch`
    /// otherwise). The unchecked fast path is `delete`.
    pub fn delete_tagged(&mut self, handle: u64, tag: u32) -> Result<()> {
        self.txm.delete_tagged(handle, tag)
    }

    /// Delete up to `max` chunks carrying `tag`, returning progress. Loop until
    /// `complete` for an incremental, bounded-time relation drop.
    pub fn delete_with_tag(&mut self, tag: u32, max: usize) -> Result<TagDropProgress> {
        self.txm.delete_with_tag(tag, max)
    }
```

- [ ] **Step 3: Run + commit** — `cargo test -p chisel` + clippy + fmt. (`feat: public Chisel tag API`).

---

## Phase 7 — Python binding

**Files:** Modify `python/src/db.rs`, `python/src/errors.rs`, `python/chisel/chisel.pyi`. (Build with `cd python && maturin develop`; test with `pytest`.)

### Task 7.1: Bind the methods + the TagMismatch exception

- [ ] **Step 1: Add the `#[pymethods]` wrappers** in `python/src/db.rs` (mirroring `delete`/`handles`; no `allow_threads`, the same `with_inner_*` pattern):

```rust
    fn allocate_tagged(&self, py: Python<'_>, value: &Bound<'_, PyAny>, tag: u32) -> PyResult<u64> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(py, |c| c.allocate_tagged(&bytes, tag))
    }

    fn tag(&self, py: Python<'_>, handle: u64) -> PyResult<u32> {
        self.with_inner_io(py, |c| c.tag(handle))
    }

    fn handles_with_tag(&self, py: Python<'_>, tag: u32) -> PyResult<Vec<u64>> {
        self.with_inner_io(py, |c| c.handles_with_tag(tag))
    }

    fn delete_tagged(&self, py: Python<'_>, handle: u64, tag: u32) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.delete_tagged(handle, tag))
    }

    /// Returns (deleted: list[int], complete: bool).
    fn delete_with_tag(&self, py: Python<'_>, tag: u32, max: usize) -> PyResult<(Vec<u64>, bool)> {
        self.with_inner_mut_io(py, |c| {
            c.delete_with_tag(tag, max).map(|p| (p.deleted, p.complete))
        })
    }
```

(Returning a `(list, bool)` tuple avoids materializing a dataclass; the `.pyi` types it precisely. Match `coerce_value`/import paths to the existing `allocate` wrapper in this file.)

- [ ] **Step 2: Add the exception** in `python/src/errors.rs`: (1) `create_exception!(_chisel, TagMismatchError, OperationalError);`; (2) register it in `register()`; (3) add a `to_py_err` arm: `RustChiselError::TagMismatch { .. } => TagMismatchError::new_err(msg),` (before the `_ =>` catchall).

- [ ] **Step 3: Add `.pyi` stubs** in `python/chisel/chisel.pyi` (in the `Chisel` class and the exceptions section):

```python
    def allocate_tagged(self, value: Buffer, tag: int) -> int: ...
    def tag(self, handle: int) -> int: ...
    def handles_with_tag(self, tag: int) -> list[int]: ...
    def delete_tagged(self, handle: int, tag: int) -> None: ...
    def delete_with_tag(self, tag: int, max: int) -> tuple[list[int], bool]: ...
```
```python
class TagMismatchError(OperationalError): ...
```

- [ ] **Step 4: Write a pytest** in `python/tests/` (mirror an existing test file):

```python
def test_chunk_tags_roundtrip(tmp_path):
    import chisel
    db = chisel.Chisel.open(str(tmp_path / "t.chisel"))
    db.begin()
    h = db.allocate_tagged(b"row", 42)
    db.commit()
    assert db.tag(h) == 42
    assert db.handles_with_tag(42) == [h]
    db.begin()
    deleted, complete = db.delete_with_tag(42, 100)
    db.commit()
    assert deleted == [h] and complete
    assert db.handles_with_tag(42) == []
    db.close()
```

- [ ] **Step 5: Build + test + commit** — `cd python && maturin develop && pytest tests/ -q`. Expected: PASS. Then commit (`feat(python): bind chunk-tag methods + TagMismatchError`).

---

## Phase 8 — Integration tests

**Files:** Create `tests/tag_ops.rs` (uses `mod common;` + `dual_backing_test!`)

### Task 8.1: Durability, backward-compat, and the F1/I12 drop regression

- [ ] **Step 1: Write the tests.** Create `tests/tag_ops.rs`:

```rust
mod common;
use chisel::Chisel;
use common::{open_chisel, Backing};
use tempfile::NamedTempFile;

fn tag_survives_reopen_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate_tagged(b"relation row", 77).unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap(), 77);
    db.close().unwrap();
}
dual_backing_test!(tag_survives_in_session, tag_survives_reopen_body);

#[test]
fn tag_and_index_survive_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h = db.allocate_tagged(b"survive", 9).unwrap();
        db.allocate_tagged(b"survive2", 9).unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.tag(h).unwrap(), 9);
        assert_eq!(db.handles_with_tag(9).unwrap().len(), 2);
        db.close().unwrap();
    }
}

#[test]
fn dropping_a_relation_frees_pages_no_leak() {
    // F1 / I12 regression: delete_with_tag must not leak handles or pages.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let baseline;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        let _anchor = db.allocate(b"anchor").unwrap();
        db.commit().unwrap();
        baseline = db.stats().unwrap().file_size_bytes;
        // Create a relation, then drop it entirely.
        db.begin().unwrap();
        for i in 0..200u64 {
            db.allocate_tagged(format!("row {i}").as_bytes(), 1).unwrap();
        }
        db.commit().unwrap();
        db.begin().unwrap();
        loop {
            let p = db.delete_with_tag(1, 64).unwrap();
            if p.complete {
                break;
            }
        }
        db.commit().unwrap();
        assert_eq!(db.handles_with_tag(1).unwrap(), Vec::<u64>::new());
        // Pages freed by the drop are reclaimable: a follow-up defrag should not
        // grow the file, and re-allocating reuses freed pages rather than
        // extending unboundedly.
        db.begin().unwrap();
        for i in 0..200u64 {
            db.allocate_tagged(format!("again {i}").as_bytes(), 2).unwrap();
        }
        db.commit().unwrap();
        let after = db.stats().unwrap().file_size_bytes;
        // Re-allocating the same volume after a drop should reuse freed pages,
        // so the file should not have grown by a full second relation's worth.
        assert!(after < baseline + 200 * 2 * chisel::PAGE_SIZE as u64,
            "freed pages were not reused (leak): baseline={baseline} after={after}");
        db.close().unwrap();
    }
}

#[test]
fn old_database_opens_with_all_untagged() {
    // A database created before tags (simulated: only plain allocate) opens with
    // tag 0 everywhere and an empty index.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h = db.allocate(b"legacy").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.tag(h).unwrap(), 0);
        assert_eq!(db.handles_with_tag(0).unwrap(), Vec::<u64>::new());
        db.close().unwrap();
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p chisel --test tag_ops`. Expected: PASS. If the `dropping_a_relation_frees_pages_no_leak` threshold is too tight for slot-packing/COW overhead, relax the bound but keep it strictly less than "baseline + a full second relation with no reuse."

- [ ] **Step 3: Full suite + workspace clippy + commit**

```bash
cargo test
cargo clippy --all-targets --workspace --exclude chisel-py -- -D warnings
cargo fmt --check
git add tests/tag_ops.rs
git commit -m "test: chunk-tags integration (durability, backward-compat, F1/I12 drop)

Reopen preserves tags + index; old databases open all-untagged; dropping a
relation via delete_with_tag frees pages for reuse (the original F1/I12 leak)."
```

**Phase 8 done when:** `cargo test` (workspace), `cargo clippy --all-targets --workspace --exclude chisel-py -- -D warnings`, and `cargo fmt --check` are all clean, and the Python suite passes.

---

## Closing checklist

- [ ] Update `docs/specs/2026-06-02-chunk-tags-design.md` status to "implemented" and note the one v1 simplification recorded during planning: `delete_with_tag` enumerates members bounded (`iter_bounded`) rather than the spec's unbounded sketch, to honor the bounded-time requirement.
- [ ] File `ISSUES.md` entries for the deferred refinements named in the spec's "Out of scope": callback enumeration (`for_each_handle_with_tag`, after I97), bitmap inner sets if profiling shows dense ranges, and a streaming drop for relations far larger than one `max` batch.
- [ ] Re-record bench baselines if any tagged path enters the bench grid (currently none — the feature is additive and untagged workloads are unaffected).
- [ ] Confirm `FORMAT_MINOR_VERSION` bump is called out in release notes (per the on-disk format change).
