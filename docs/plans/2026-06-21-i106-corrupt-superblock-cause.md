# I106 — `CorruptSuperblock` cause/slot detail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enrich the nullary `ChiselError::CorruptSuperblock` with per-slot cause detail (`Vec<SlotDefect>`) so the engine's worst failure says which slot failed and why, without adding cost to the success path.

**Architecture:** A shared `validate(buf) -> Result<(), SuperblockDefect>` feeds both the unchanged `deserialize` (hot path) and a new cold-path `Superblock::diagnose`. The error variant gains a `Vec<SlotDefect>`, built lazily only when `select` finds no valid slot. Python stays message-only.

**Tech Stack:** Rust (`chisel` lib), PyO3 binding (`chisel-py`).

**Spec:** `docs/specs/2026-06-21-i106-corrupt-superblock-cause-design.md`
**Branch:** `i106-corrupt-superblock-cause` (off `main`, post-#61/#62).

**Standing gates** (after every code task): `cargo test` 0 failures · `cargo clippy --workspace --all-targets -- -D warnings` clean · `cargo fmt --check` clean.

> Edit by **content** (the quoted code), not line number.

---

### Task 1: `SuperblockDefect`, `SlotDefect`, `validate`, `diagnose` (additive)

Adds the new types and the shared/cold-path helpers, and refactors `deserialize` to use `validate` (behavior-preserving). Self-contained: compiles and tests pass without touching the error variant yet.

**Files:**
- Modify: `src/superblock.rs`

- [ ] **Step 1: Add the `fmt` import.** Near the top of `src/superblock.rs`, in the imports, add (if not already present):

```rust
use std::fmt;
```

- [ ] **Step 2: Add the types.** Add near the top of `src/superblock.rs` (after the imports, before `impl Superblock` is fine):

```rust
/// Why a superblock slot failed validation (I106). These are exactly the three
/// torn-slot causes `deserialize` checks. `Copy` and small so the failure-path
/// `Vec<SlotDefect>` is cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive] // project convention for public enums (I36); a 4th cause would
                  // mean a new deserialize check, addable without a break.
pub enum SuperblockDefect {
    BadChecksum,
    BadMagic,
    BadCount(u32), // the out-of-range superblock_count value
}

impl fmt::Display for SuperblockDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuperblockDefect::BadChecksum => write!(f, "bad checksum"),
            SuperblockDefect::BadMagic => write!(f, "bad magic"),
            SuperblockDefect::BadCount(n) => write!(f, "bad superblock_count {n}"),
        }
    }
}

/// A defect tagged with the candidate-slot index it was found at (I106).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotDefect {
    pub slot: u32,
    pub defect: SuperblockDefect,
}
```

- [ ] **Step 3: Add `validate` (free fn) and refactor `deserialize`.** In `src/superblock.rs`, add a module-level free fn and change `deserialize` to use it. Replace the three inline checks at the head of `deserialize` (the `verify_checksum` guard, the `magic != MAGIC` guard, and the `superblock_count` range guard) with a single `validate(buf).ok()?;` — **keep the `Some(Superblock { ...field parse... })` block exactly as it is**.

```rust
/// The three torn-slot rules, shared by the hot path (`deserialize`) and the
/// cold path (`diagnose`). Order is load-bearing: checksum first — a bad
/// checksum means the rest of the buffer (including magic) is untrusted, so it
/// is reported as `BadChecksum` even if the magic bytes also differ.
fn validate(buf: &[u8; PAGE_SIZE]) -> Result<(), SuperblockDefect> {
    if !page::verify_checksum(buf) {
        return Err(SuperblockDefect::BadChecksum);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(SuperblockDefect::BadMagic);
    }
    let count = u32::from_le_bytes(
        buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    if !(MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS).contains(&count) {
        return Err(SuperblockDefect::BadCount(count));
    }
    Ok(())
}
```

The refactored `deserialize` head becomes:

```rust
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<Superblock> {
        validate(buf).ok()?;
        // NOTE: format_version and page_size are read-not-validated here; callers
        // gate them (see open_existing / I15). Only the three validate() rules
        // make a slot torn-and-skippable.
        let mut named_roots = [NamedRoot::EMPTY; NAMED_ROOT_COUNT];
        // ...rest of the existing field-parse block, UNCHANGED, ending in
        //    Some(Superblock { ... })
    }
```

(Preserve every field-parse line; only the leading three guards are replaced by `validate(buf).ok()?;`. The `named_roots` loop and the final `Some(Superblock { ... })` are unchanged.)

- [ ] **Step 4: Add `Superblock::diagnose`.** Inside `impl Superblock`, near `select`, add:

```rust
    /// Explain why every candidate slot failed. Called only on the cold path,
    /// when `select` returned `None` — which means EVERY candidate failed
    /// `validate` (none deserialized), so this returns one `SlotDefect` per
    /// candidate. Bounded by `buffers.len()` (the open path probes at most
    /// MAX_SUPERBLOCKS, trimmed at EOF).
    pub(crate) fn diagnose(buffers: &[[u8; PAGE_SIZE]]) -> Vec<SlotDefect> {
        buffers
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                validate(b)
                    .err()
                    .map(|defect| SlotDefect { slot: i as u32, defect })
            })
            .collect()
    }
```

- [ ] **Step 5: Write the unit tests.** Add to `src/superblock.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn validate_classifies_each_defect() {
        let good = Superblock::new_empty(2).serialize();
        assert_eq!(validate(&good), Ok(()));

        // Bad checksum: flip a non-magic, non-count byte WITHOUT re-stamping.
        let mut bad_checksum = good;
        bad_checksum[16] ^= 0xFF;
        assert_eq!(validate(&bad_checksum), Err(SuperblockDefect::BadChecksum));

        // Bad magic: flip a magic byte and RE-STAMP so the checksum passes.
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        page::stamp_checksum(&mut bad_magic);
        assert_eq!(validate(&bad_magic), Err(SuperblockDefect::BadMagic));

        // Bad count: write an out-of-range count and re-stamp.
        let mut bad_count = good;
        bad_count[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .copy_from_slice(&99u32.to_le_bytes());
        page::stamp_checksum(&mut bad_count);
        assert_eq!(validate(&bad_count), Err(SuperblockDefect::BadCount(99)));
    }

    #[test]
    fn diagnose_reports_each_slots_defect() {
        let good = Superblock::new_empty(2).serialize();
        let mut slot0 = good; // bad checksum (no re-stamp)
        slot0[16] ^= 0xFF;
        let mut slot1 = good; // bad magic (re-stamped)
        slot1[0] ^= 0xFF;
        page::stamp_checksum(&mut slot1);
        assert_eq!(
            Superblock::diagnose(&[slot0, slot1]),
            vec![
                SlotDefect { slot: 0, defect: SuperblockDefect::BadChecksum },
                SlotDefect { slot: 1, defect: SuperblockDefect::BadMagic },
            ]
        );
    }
```

- [ ] **Step 6: Run + verify.**

```bash
cargo test -p chisel --lib superblock:: 2>&1 | grep -E "test result:|FAILED"
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -2
cargo fmt --check && echo "fmt clean"
```
Expected: all superblock tests pass (existing + 2 new), clippy/fmt clean. The existing `deserialize_*` tests must still pass — that confirms the `validate` refactor preserved behavior.

- [ ] **Step 7: Commit.**

```bash
git add src/superblock.rs
git commit -m "feat(I106): SuperblockDefect/SlotDefect + validate/diagnose helpers"
```

---

### Task 2: Reshape `CorruptSuperblock` and wire `diagnose` (atomic)

The enum-shape change breaks every match/construct site, so this is ONE atomic commit. Let the compiler guide you: change the decl, then fix each error.

**Files:** `src/error.rs`, `src/transaction.rs`, `src/lib.rs`, `tests/error_and_format.rs`, `src/recovery_tests.rs`, `python/src/errors.rs`

- [ ] **Step 1: Re-export the new types.** In `src/lib.rs`, extend the existing `pub use superblock::{ ... };` re-export to add `SuperblockDefect, SlotDefect`. For example:

```rust
pub use superblock::{
    DEFAULT_SUPERBLOCK_COUNT, MAX_SUPERBLOCKS, MIN_SUPERBLOCKS, NAMED_ROOT_COUNT,
    NAMED_ROOT_NAME_LEN, SlotDefect, SuperblockDefect,
};
```

- [ ] **Step 2: Reshape the variant.** In `src/error.rs`, add `use crate::superblock::SlotDefect;` to the imports, and change the enum variant:

```rust
    CorruptSuperblock { defects: Vec<SlotDefect> },
```

- [ ] **Step 3: Fix `is_fatal`.** In `src/error.rs::is_fatal`, change the arm `| ChiselError::CorruptSuperblock` to `| ChiselError::CorruptSuperblock { .. }`.

- [ ] **Step 4: Reshape `Display`.** In `src/error.rs`'s `Display` impl, replace the arm
`ChiselError::CorruptSuperblock => write!(f, "no valid superblock found"),`
with:

```rust
            ChiselError::CorruptSuperblock { defects } => {
                write!(f, "no valid superblock found")?;
                for (i, sd) in defects.iter().enumerate() {
                    let sep = if i == 0 { ":" } else { ";" };
                    write!(f, "{sep} slot {}: {}", sd.slot, sd.defect)?;
                }
                Ok(())
            }
```

(Empty `defects` → `"no valid superblock found"`; populated → `"no valid superblock found: slot 0: bad checksum; slot 1: bad magic"`.)

- [ ] **Step 5: Fix the two error.rs test sites.** In `src/error.rs`'s test module:
  - In `non_io_variants_have_no_source`, change `ChiselError::CorruptSuperblock,` to `ChiselError::CorruptSuperblock { defects: vec![] },`.
  - In `is_fatal_matches_documented_classification_for_every_variant`: the `documented_is_fatal` Fatal block arm `| ChiselError::CorruptSuperblock` → `| ChiselError::CorruptSuperblock { .. }`, and the `all[]` array entry `ChiselError::CorruptSuperblock,` → `ChiselError::CorruptSuperblock { defects: vec![] },`.

- [ ] **Step 6: Wire `diagnose` at the raise site.** In `src/transaction.rs::open_existing`, change:
`let sb = Superblock::select(&candidates).ok_or(ChiselError::CorruptSuperblock)?;`
to:

```rust
        let sb = Superblock::select(&candidates).ok_or_else(|| {
            ChiselError::CorruptSuperblock {
                defects: Superblock::diagnose(&candidates),
            }
        })?;
```

(`ok_or_else` keeps `diagnose` off the success path.)

- [ ] **Step 7: Fix `tests/error_and_format.rs`.** Change both `ChiselError::CorruptSuperblock,` entries (in the two test arrays) to `ChiselError::CorruptSuperblock { defects: vec![] },`.

- [ ] **Step 8: Fix the recovery_tests OR-arm.** In `src/recovery_tests.rs`, the truncation-test OR-arm `| ChiselError::CorruptSuperblock` → `| ChiselError::CorruptSuperblock { .. }`.

- [ ] **Step 9: Strengthen the I115 bad-magic test.** In `src/recovery_tests.rs::corrupt_magic_surfaces_as_corrupt_superblock_not_invalid_magic`, replace the `match Chisel::open(...)` block with:

```rust
    match Chisel::open(&path, Default::default()) {
        Err(ChiselError::CorruptSuperblock { defects }) => {
            // I106: the real superblock slots (0 and 1) must be reported as
            // BadMagic — we corrupted the magic while keeping the checksum valid.
            for slot in 0..crate::superblock::DEFAULT_SUPERBLOCK_COUNT {
                assert!(
                    defects.iter().any(|d| d.slot == slot
                        && d.defect == crate::superblock::SuperblockDefect::BadMagic),
                    "slot {slot} should be reported BadMagic, got {defects:?}"
                );
            }
        }
        Err(e) => panic!("bad magic must surface as CorruptSuperblock, got {e:?}"),
        Ok(_) => panic!("Chisel::open accepted a fully-corrupted-magic file"),
    };
```

- [ ] **Step 10: Fix the Python binding (message-only).** In `python/src/errors.rs`, change the `to_py_err` arm
`RustChiselError::CorruptSuperblock => CorruptSuperblockError::new_err(msg),`
to:

```rust
        RustChiselError::CorruptSuperblock { .. } => CorruptSuperblockError::new_err(msg),
```

(The `CorruptSuperblockError` type declaration/registration is unchanged; `msg` comes from `Display`, which now includes the per-slot defects.)

- [ ] **Step 11: Build + full gate.**

```bash
cargo test 2>&1 | grep -E "test result:|error\[|FAILED" | grep -v "0 failed" || echo "RUST OK"
cargo test --release -p chisel --lib 2>&1 | tail -3
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --check && echo "fmt clean"
```
Then the Python suite:
```bash
cd python && (source .venv/bin/activate 2>/dev/null || (python3 -m venv .venv && source .venv/bin/activate))
pip -q install maturin pytest hypothesis >/dev/null 2>&1
maturin develop -q 2>&1 | tail -1 && python -m pytest -q 2>&1 | tail -3
cd ..
```
Expected: 0 Rust failures (debug + release), clippy/fmt clean, Python all pass.

- [ ] **Step 12: Commit.**

```bash
git add -A
git commit -m "feat(I106): CorruptSuperblock carries per-slot defect detail"
```

---

### Task 3: Close out ISSUES.md

**Files:** `ISSUES.md`

- [ ] **Step 1: Mark I106 fixed.** Add `✅ FIXED 2026-06-21` to the I106 entry title and a one-paragraph resolution describing: the `validate`/`diagnose` split (hot path untouched), `CorruptSuperblock { defects: Vec<SlotDefect> }` with `SuperblockDefect { BadChecksum, BadMagic, BadCount(u32) }`, lazy construction via `ok_or_else`, message-only Python, and the strengthened I115 bad-magic test (now a cause-level pin). Do NOT touch the shared handoff-note paragraph (kept stable to avoid cross-PR churn).

- [ ] **Step 2: Commit.**

```bash
git add ISSUES.md
git commit -m "docs: close I106 in ISSUES.md"
```

---

## Self-Review

**Spec coverage:**
- `validate` + `SuperblockDefect`/`SlotDefect` + `diagnose` + deserialize refactor → Task 1. ✓
- `CorruptSuperblock { defects }` + lazy raise + Display + is_fatal + exhaustiveness + re-export → Task 2. ✓
- Python message-only → Task 2 Step 10. ✓
- Strengthened I115 test → Task 2 Step 9. ✓
- `superblock.rs` unit tests (validate/diagnose) → Task 1 Step 5. ✓
- Non-goals (no Result-deserialize, no structured Python, page_size/format_version untouched) → respected (no task does them). ✓
- ISSUES.md → Task 3. ✓

**Placeholder scan:** The only ellipsis is Task 1 Step 3's "keep the existing field-parse block" — a preserve-existing-code instruction with the exact head shown, not a gap.

**Type consistency:** `SuperblockDefect { BadChecksum | BadMagic | BadCount(u32) }`, `SlotDefect { slot: u32, defect: SuperblockDefect }`, `validate(&[u8; PAGE_SIZE]) -> Result<(), SuperblockDefect>`, `Superblock::diagnose(&[[u8; PAGE_SIZE]]) -> Vec<SlotDefect>`, `CorruptSuperblock { defects: Vec<SlotDefect> }` — used consistently across Tasks 1–2 and matched against the real `serialize`/`new_empty`/`stamp_checksum`/`verify_checksum` signatures read from source.
