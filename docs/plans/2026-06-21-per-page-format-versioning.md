# Per-Page Format Versioning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the per-page format version byte load-bearing (I31 read-dispatch) and add the file-level MINOR write-gate (I29), as one minor-versioning mechanism — built ahead of need so the first real layout change is a localized diff.

**Architecture:** Two new `page.rs` helpers (`current_version`, `page_format_version`) plus a restored `format_minor`; every `init_page` site stamps `current_version(its_type)` instead of the shared constant; and `open_existing` forces the engine read-only when the file's MINOR exceeds this binary's. No version branches exist yet (only version 0), so reads and the cache load-path are unchanged. See `docs/specs/2026-06-21-per-page-format-versioning-design.md`.

**Tech Stack:** Rust 2021 (MSRV 1.82), `cargo test`/`clippy`/`fmt`. Engine crate `chisel` (`src/`). No new dependencies.

**Conventions:** Commits use no AI-referencing text. Run from repo root. `PAGE_FORMAT_VERSION_CURRENT` is **kept** (it is the canonical "version 0" value that `current_version` returns) — this minimizes churn; the spec's "retire" means it is no longer referenced directly by `init_page` sites.

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/page.rs` | Page-format constants + helpers | Add `current_version`, `page_format_version`, `format_minor` + unit tests |
| `src/data_page.rs`, `src/overflow.rs`, `src/freemap.rs`, `src/membership_index.rs`, `src/handle_table.rs` | Page-type `init_page` sites | Stamp `current_version(type)` instead of `PAGE_FORMAT_VERSION_CURRENT` |
| `src/page_io.rs` | Low-level page I/O + read-only enforcement | Add `force_read_only()` setter + test |
| `src/transaction.rs` | `open_existing` (the open gate) | Add the I29 MINOR write-gate + test |
| `ARCHITECTURE.md`, `ISSUES.md` | Docs | Make the `page_format_version` reference accurate; record I29/I31 |

---

## Task 1: Version-helper functions in `page.rs`

**Files:**
- Modify: `src/page.rs` (add `format_minor` after `format_major`; add `current_version` + `page_format_version` after the `PageType` enum; add tests in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/page.rs`:

```rust
    #[test]
    fn page_format_version_dispatches_offset_on_page_type() {
        // HandleTable: version at byte 2 (byte 1 holds the leaf/interior flag).
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = PageType::HandleTable as u8;
        buf[1] = 0x01; // FLAG_LEAF — must be ignored by the version reader
        buf[2] = 7;
        assert_eq!(page_format_version(&buf), 7);

        // Every other non-superblock type: version at byte 1.
        for pt in [
            PageType::Data,
            PageType::Overflow,
            PageType::FreeMap,
            PageType::MembershipInterior,
            PageType::MembershipLeaf,
        ] {
            let mut buf = [0u8; PAGE_SIZE];
            buf[0] = pt as u8;
            buf[1] = 5;
            buf[2] = 99; // type-specific byte must not leak into the version
            assert_eq!(page_format_version(&buf), 5, "type {pt:?}");
        }
    }

    #[test]
    fn current_version_is_zero_for_all_types_today() {
        for pt in [
            PageType::HandleTable,
            PageType::Data,
            PageType::Overflow,
            PageType::FreeMap,
            PageType::MembershipInterior,
            PageType::MembershipLeaf,
        ] {
            assert_eq!(current_version(pt), 0, "type {pt:?}");
        }
    }

    #[test]
    fn format_minor_extracts_low_16_bits() {
        let v = pack_format_version(3, 42);
        assert_eq!(format_major(v), 3);
        assert_eq!(format_minor(v), 42);
        assert_eq!(format_minor(FORMAT_VERSION), FORMAT_MINOR_VERSION);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib page::tests::page_format_version_dispatches_offset_on_page_type page::tests::current_version_is_zero_for_all_types_today page::tests::format_minor_extracts_low_16_bits`
Expected: FAIL to compile — `cannot find function page_format_version` / `current_version` / `format_minor`.

- [ ] **Step 3: Implement the three functions**

In `src/page.rs`, immediately after `format_major` (the `pub const fn format_major(...)` block) and before `pub const FORMAT_VERSION`, add:

```rust
/// Extract the minor-version pair from a packed `format_version`. Companion to
/// `format_major`; used only by the open-time I29 write-gate — a binary whose
/// MINOR is below the file's must not write the file (it would drop fields it
/// doesn't know). See ISSUES.md I29.
pub const fn format_minor(version: u32) -> u16 {
    (version & 0xFFFF) as u16
}
```

In `src/page.rs`, immediately after the `pub enum PageType { ... }` block, add:

```rust
/// The per-page format version a freshly-initialized page of `page_type`
/// stamps — the single source of truth for the write side (every `init_page`
/// site calls this). Returns 0 for every type today. When a page type's layout
/// gains a version-requiring field, ONLY that type's arm changes; others stay
/// put (ADR-7: per-type evolution). The match is exhaustive on purpose — adding
/// a `PageType` variant forces a decision here rather than defaulting silently.
/// See ISSUES.md I31.
pub const fn current_version(page_type: PageType) -> u8 {
    match page_type {
        PageType::HandleTable
        | PageType::Data
        | PageType::Overflow
        | PageType::FreeMap
        | PageType::MembershipInterior
        | PageType::MembershipLeaf => PAGE_FORMAT_VERSION_CURRENT,
    }
}

/// Read the per-page format version stamped in `buf`. Dispatches the byte
/// offset on the page-type tag at byte 0: HandleTable keeps byte 1 for its
/// leaf/interior flag and stores the version at byte 2; every other
/// non-superblock page stores it at byte 1. A reader that must distinguish
/// layouts (an additive field where zero is a legitimate value) branches on
/// this: `if page_format_version(buf) >= K { read field } else { default }`.
/// See ISSUES.md I31 and docs/specs/2026-06-21-per-page-format-versioning-design.md.
pub fn page_format_version(buf: &[u8; PAGE_SIZE]) -> u8 {
    if buf[0] == PageType::HandleTable as u8 {
        buf[2]
    } else {
        buf[1]
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib page::tests::page_format_version_dispatches_offset_on_page_type page::tests::current_version_is_zero_for_all_types_today page::tests::format_minor_extracts_low_16_bits`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add src/page.rs
git commit -m "feat(format): add per-page version read/write helpers + restore format_minor"
```

---

## Task 2: Write convention — `init_page` sites stamp `current_version(type)`

This is a mechanical refactor (no behavior change — `current_version` returns 0, exactly what the sites stamped before). The safety net is the existing `fresh_pages_report_current_version` test in `page.rs` plus Task 1's `current_version` test. The transform at each site: replace `<prefix>::PAGE_FORMAT_VERSION_CURRENT` with `<prefix>::current_version(<the same PageType expression written to buf[0] at that site>)`, preserving the existing `page::` / `crate::page::` prefix and the byte offset.

**Files & exact production sites:**

| Site | Current line | Replace with |
|------|--------------|--------------|
| `src/data_page.rs:100` | `buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;` | `buf[1] = page::current_version(page::PageType::Data);` |
| `src/overflow.rs:120` | `buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;` | `buf[1] = page::current_version(page::PageType::Overflow);` |
| `src/freemap.rs:90` | `buf[1] = crate::page::PAGE_FORMAT_VERSION_CURRENT;` | `buf[1] = crate::page::current_version(crate::page::PageType::FreeMap);` |
| `src/membership_index.rs:47` | `buf[1] = page::PAGE_FORMAT_VERSION_CURRENT; // version at byte 1 (no FLAG byte)` | `buf[1] = page::current_version(page_type); // version at byte 1 (no FLAG byte)` |
| `src/membership_index.rs:160` | `buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;` | `buf[1] = page::current_version(PageType::MembershipInterior);` |
| `src/membership_index.rs:235` | `buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;` | `buf[1] = page::current_version(pt);` |
| `src/handle_table.rs:191` | `buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31` | `buf[2] = page::current_version(page::PageType::HandleTable); // I31` |
| `src/handle_table.rs:515` | `buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31` | `buf[2] = page::current_version(page::PageType::HandleTable); // I31` |
| `src/handle_table.rs:602` | `buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31` | `buf[2] = page::current_version(page::PageType::HandleTable); // I31` |
| `src/handle_table.rs:612` | `buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31` | `buf[2] = page::current_version(page::PageType::HandleTable); // I31` |

(Note: `membership_index.rs:47` is inside a fn whose `page_type` parameter is what `buf[0] = page_type as u8` uses; `:235` uses the local `pt`. Leave any `PAGE_FORMAT_VERSION_CURRENT` occurrences that are inside `#[cfg(test)]` modules — e.g. `membership_index.rs` ~950/~1007 and any in `handle_table.rs` test fixtures — untouched: they build test pages and the constant remains valid. Confirm with `grep -n PAGE_FORMAT_VERSION_CURRENT src/handle_table.rs` whether a site sits in a `#[cfg(test)]` block before editing.)

- [ ] **Step 1: Apply the ten production-site edits above.**

Use the table verbatim. Re-grep first in case line numbers drifted: `grep -rn "PAGE_FORMAT_VERSION_CURRENT" src/ --include='*.rs'` — match each production write-site to its row by the surrounding `buf[0] = <type>` assignment.

- [ ] **Step 2: Run the full suite to verify no regression**

Run: `cargo test`
Expected: PASS — the change is behavior-preserving (`current_version` returns the same 0). In particular `page::tests::fresh_pages_report_current_version` still passes (fresh pages stamp `current_version(type) == 0`).

- [ ] **Step 3: Commit**

```bash
git add src/data_page.rs src/overflow.rs src/freemap.rs src/membership_index.rs src/handle_table.rs
git commit -m "refactor(format): init sites stamp current_version(type), establishing the per-type write seam"
```

---

## Task 3: `PageIo::force_read_only()` setter

**Files:**
- Modify: `src/page_io.rs` (add `force_read_only` near `is_read_only` at ~line 137; add a test in the `read_only_tests` module)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod read_only_tests` block in `src/page_io.rs` (it already imports `NamedTempFile` and `PAGE_SIZE`):

```rust
    #[test]
    fn force_read_only_blocks_writes_after_a_read_write_open() {
        let tmp = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(tmp.path(), false).unwrap(); // opened read-WRITE
        assert!(!io.is_read_only());

        io.force_read_only();

        assert!(io.is_read_only());
        let buf = [0u8; PAGE_SIZE];
        assert!(matches!(io.write_page(0, &buf), Err(ChiselError::ReadOnlyMode)));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib page_io::read_only_tests::force_read_only_blocks_writes_after_a_read_write_open`
Expected: FAIL to compile — `no method named force_read_only`.

- [ ] **Step 3: Implement the setter**

In `src/page_io.rs`, immediately after the `pub fn is_read_only(&self) -> bool { self.read_only }` method, add:

```rust
    /// Force this handle read-only after open. Used by the I29 format-MINOR
    /// write-gate: a file whose MINOR exceeds this binary's may be READ (within
    /// a MAJOR all layout changes are additive, so known fields are at stable
    /// offsets) but must not be WRITTEN, since this binary would stamp pages at
    /// its older minor and drop fields it cannot see. Idempotent. The OS file
    /// handle is unchanged (still O_RDWR) — this only flips the in-memory guard
    /// that `write_page` / `fsync` / `set_page_count` already honor. See I29.
    pub fn force_read_only(&mut self) {
        self.read_only = true;
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib page_io::read_only_tests::force_read_only_blocks_writes_after_a_read_write_open`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/page_io.rs
git commit -m "feat(io): add PageIo::force_read_only for the I29 format-minor write-gate"
```

---

## Task 4: I29 — open-gate MINOR write-gate in `open_existing`

**Files:**
- Modify: `src/transaction.rs` (add the gate in `open_existing` right after the MAJOR gate; add a test in the same `#[cfg(test)]` module as `format_version_gate_is_major_only`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `src/transaction.rs`, next to `format_version_gate_is_major_only`:

```rust
    // I29 write-gate: a file whose MINOR exceeds this binary's opens READ-ONLY,
    // not rejected — within a MAJOR every layout change is additive, so reads
    // are safe, but writing would drop fields this binary can't see. A
    // same-or-older minor file opens read-write as normal.
    #[test]
    fn file_minor_newer_than_binary_is_forced_read_only() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        // Fresh DB so there are 2 superblock slots (pages 0 and 1) to patch.
        {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let _ = TransactionManager::create_new(cache, 2).unwrap();
        }

        // Patch every slot to (current MAJOR, MINOR + 1) and re-stamp checksums.
        let newer_minor = page::pack_format_version(
            page::FORMAT_MAJOR_VERSION,
            page::FORMAT_MINOR_VERSION + 1,
        );
        let mut bytes = std::fs::read(&path).unwrap();
        for slot in 0..2 {
            let offset = slot * PAGE_SIZE;
            bytes[offset + 4..offset + 8].copy_from_slice(&newer_minor.to_le_bytes());
            let page_arr: &mut [u8; PAGE_SIZE] =
                (&mut bytes[offset..offset + PAGE_SIZE]).try_into().unwrap();
            page::stamp_checksum(page_arr);
        }
        std::fs::write(&path, &bytes).unwrap();

        // Reopen read-WRITE; the gate must force read-only, so begin() fails.
        let io = PageIo::open(&path, false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm = TransactionManager::open_existing(cache)
            .expect("a newer-minor file must still OPEN (reads are additive-safe)");
        assert!(
            matches!(tm.begin(), Err(ChiselError::ReadOnlyMode)),
            "a newer-minor file must be forced read-only"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib file_minor_newer_than_binary_is_forced_read_only`
Expected: FAIL — `tm.begin()` returns `Ok` (no gate yet), so the `matches!(... ReadOnlyMode)` assertion fails.

- [ ] **Step 3: Implement the gate**

In `src/transaction.rs`, in `open_existing`, immediately after the MAJOR-gate block (the `if page::format_major(sb.format_version) != page::FORMAT_MAJOR_VERSION { ... }` that ends near line 462), add:

```rust
        // I29 write-gate: a file whose MINOR exceeds this binary's may contain
        // version-requiring page layouts we cannot safely write — we would
        // stamp pages at our older minor and drop the newer fields. Reads ARE
        // safe (within a MAJOR all layout changes are additive, so known fields
        // sit at stable offsets), so we open the file but force it read-only;
        // mutations then return ReadOnlyMode. The complementary I31 per-page
        // read-dispatch lets a newer binary read these older pages.
        // See docs/specs/2026-06-21-per-page-format-versioning-design.md.
        if page::format_minor(sb.format_version) > page::FORMAT_MINOR_VERSION {
            cache.io_mut().force_read_only();
        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib file_minor_newer_than_binary_is_forced_read_only`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/transaction.rs
git commit -m "feat(format): I29 file-minor write-gate forces read-only on a newer-minor file"
```

---

## Task 5: Docs — make the format-versioning docs accurate

**Files:**
- Modify: `ARCHITECTURE.md` (the `page::page_format_version()` reference is live again; the load path does not validate the version)
- Modify: `ISSUES.md` (record that I31 read-dispatch + I29 write-gate landed; eager-upgrade sweep remains deferred)

- [ ] **Step 1: Fix the ARCHITECTURE.md reference**

Find the line `See page::page_format_version() for the dispatch.` (around `ARCHITECTURE.md:246`) and replace it with:

```
See `page::page_format_version()` for the read-side dispatch (offset by page
type). The cache load path validates only the checksum — per-page version
dispatch is per-module decode-only; see
docs/specs/2026-06-21-per-page-format-versioning-design.md.
```

- [ ] **Step 2: Add an ISSUES.md note**

Append to the I31 and I29 entries in `ISSUES.md` (or add a dated sub-note) that the per-page version read-dispatch helpers (`page_format_version`, `current_version`) and the file-MINOR write-gate (forced read-only via `PageIo::force_read_only`) landed on 2026-06-21; the eager-upgrade sweep remains deferred. Match the file's existing entry style (do not invent a new format).

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md ISSUES.md
git commit -m "docs: record per-page version dispatch (I31) + minor write-gate (I29)"
```

(ADR-7's stale "reads dispatch / load validates the per-page version on every miss" text should also be corrected in `.codebase-memory/adr.md` via the codebase-memory `manage_adr` tool — that store is untracked, so it is a separate, non-git step; note it for the human reviewer rather than committing.)

---

## Task 6: Final verification

- [ ] **Step 1: Format, lint, build, test (the CI gates)**

Run:
```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```
Expected: fmt makes no changes (or only trivial); clippy clean (no warnings); all tests pass, including the three new ones (`page_format_version_dispatches_offset_on_page_type`, `force_read_only_blocks_writes_after_a_read_write_open`, `file_minor_newer_than_binary_is_forced_read_only`).

- [ ] **Step 2: Confirm the seam is dormant (no behavior change today)**

Run: `grep -rn "page_format_version(" src/ --include='*.rs' | grep -v "fn page_format_version\|tests\|//"`
Expected: **no production caller** — the read-dispatch is documented but unused until the first real version bump (per the spec). This confirms the change adds the seam without changing any read path.

- [ ] **Step 3: Commit any fmt fixups (if cargo fmt changed anything)**

```bash
git add -A
git commit -m "style: cargo fmt" --allow-empty
```

---

## Notes for the implementer

- **TDD discipline:** each task's test is written and seen to fail before the implementation. Task 2 is the one exception — it is a behavior-preserving refactor whose safety net is the existing suite (run it before and after; both green).
- **Do not** add a `validate_version` gate in `load_page`, a version-decoder trait/registry, an eager-upgrade sweep, or a synthetic shipped "version 1" — all explicitly out of scope (spec §8). The first *real* version bump follows the "how to bump" procedure (spec §6), which makes the decode-branch test mandatory at that time.
- **PR:** this is on branch `design/per-page-format-versioning` (which already carries the spec commit). Open a PR with `--base main` when the plan is complete and green.
</content>
