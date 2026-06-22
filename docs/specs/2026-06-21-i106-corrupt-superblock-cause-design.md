# I106 — `CorruptSuperblock` cause/slot detail

**Status:** approved design, 2026-06-21
**Issue:** I106 (P3)
**Branch:** `i106-corrupt-superblock-cause` (off `main` after #61 + #62 merged)

## Goal

`CorruptSuperblock` is a nullary fatal variant that folds three distinct causes
(bad checksum, bad magic, out-of-range `superblock_count`) into one opaque error
— on the engine's worst failure, an operator can't tell which slot failed or
why. Attach per-slot cause detail without adding any cost to the success path.

## Mechanism (hot path untouched)

The three causes are exactly the torn-slot checks `Superblock::deserialize`
already performs (`superblock.rs:215` checksum, `:219` magic, `:255` count).
Extract them into a shared helper; add a cold-path diagnoser that only runs when
`select` finds no valid slot.

```rust
// superblock.rs — pub, re-exported from the crate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive] // matches the project convention for public enums (I36)
pub enum SuperblockDefect {
    BadChecksum,
    BadMagic,
    BadCount(u32), // the out-of-range value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotDefect {
    pub slot: u32,
    pub defect: SuperblockDefect,
}

// The single source of the three torn-slot rules, shared by the hot path
// (deserialize) and the cold path (diagnose). Order matters: checksum first —
// a bad checksum means the rest of the buffer (including magic) is untrusted,
// so it is reported as BadChecksum even if the magic bytes also differ.
fn validate(buf: &[u8; PAGE_SIZE]) -> Result<(), SuperblockDefect> {
    if !page::verify_checksum(buf) {
        return Err(SuperblockDefect::BadChecksum);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(SuperblockDefect::BadMagic);
    }
    let count = u32::from_le_bytes(
        buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4].try_into().unwrap(),
    );
    if !(MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS).contains(&count) {
        return Err(SuperblockDefect::BadCount(count));
    }
    Ok(())
}

pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<Superblock> {
    validate(buf).ok()?;          // identical behavior + signature to today
    Some(Superblock { /* ...existing field parse, unchanged... */ })
}

// Cold path: select() returned None, so EVERY candidate failed validate (none
// deserialized). Map each to its defect. Bounded by candidates.len() (<= the
// MAX_SUPERBLOCKS probe, naturally trimmed at EOF by the open path).
pub(crate) fn diagnose(buffers: &[[u8; PAGE_SIZE]]) -> Vec<SlotDefect> {
    buffers
        .iter()
        .enumerate()
        .filter_map(|(i, b)| validate(b).err().map(|defect| SlotDefect { slot: i as u32, defect }))
        .collect()
}
```

`select` is **unchanged** (`filter_map(deserialize).max_by_key(txn_counter)`).
`deserialize`'s signature and behavior are unchanged — `validate(buf).ok()?`
runs the identical checks in the identical order, so the success path costs the
same. No new allocation or branch on a successful open.

## Error variant

`CorruptSuperblock` → `CorruptSuperblock { defects: Vec<SlotDefect> }`. Raised
**lazily** so `diagnose` only runs on the failure path:

```rust
// transaction.rs:454
let sb = Superblock::select(&candidates)
    .ok_or_else(|| ChiselError::CorruptSuperblock {
        defects: Superblock::diagnose(&candidates),
    })?;
```

`ChiselError` does not derive `Clone` (it wraps `io::Error`), so the added
`Vec<SlotDefect>` field is fine. The variant stays **fatal** (`is_fatal`
unchanged in classification; only the match pattern becomes `{ .. }`).

## Display

`SuperblockDefect` gets a `Display` (`"bad checksum"` / `"bad magic"` /
`"bad superblock_count {n}"`). `CorruptSuperblock`'s `Display` summarizes the
slots, handling the empty-Vec case (test constructors pass `vec![]`):

```text
no valid superblock found                                  // defects empty
no valid superblock found: slot 0: bad checksum; slot 1: bad magic   // populated
```

Trailing data-page slots legitimately report `bad magic` (they aren't
superblocks); this is honest and the diagnostic value is in the low-index
slots that *should* be superblocks.

## Python (message-only)

`python/src/errors.rs:311` keeps mapping to the existing `CorruptSuperblockError`
type; only the match pattern changes to `CorruptSuperblock { .. }`. The Python
exception carries the enriched `Display` message — no new structured Python type,
no marshaling of the defect Vec. Rationale: this is a fatal "drop and reopen"
error read in logs, not inspected programmatically by the single client. The
`CorruptSuperblockError` declaration/registration (`:43`, `:136`, `:212-213`)
is unchanged.

## Tests

- **`superblock.rs` unit tests** (new): build a checksum-valid superblock buffer
  via the existing helpers, then assert `validate`/`diagnose` classify each
  cause:
  - corrupt a byte without re-stamping the checksum → `BadChecksum`;
  - re-stamp after flipping a magic byte → `BadMagic`;
  - re-stamp after writing an out-of-range count (e.g. 99) → `BadCount(99)`;
  - a `diagnose` test over a 2-slot mixed set asserting `[SlotDefect{0,BadChecksum}, SlotDefect{1,BadMagic}]`.
- **`recovery_tests.rs::corrupt_magic_surfaces_*`** (strengthen): the I115 test
  currently matches `CorruptSuperblock` (nullary). Update to
  `CorruptSuperblock { defects }` and assert the real superblock slots (0 and 1)
  carry `BadMagic` — upgrading a variant-level pin to a cause-level pin.
- **`recovery_tests.rs:544`** truncation OR-arm: `CorruptSuperblock` →
  `CorruptSuperblock { .. }`.
- **`error.rs` / `tests/error_and_format.rs`** arrays: construct
  `CorruptSuperblock { defects: vec![] }`; exhaustiveness/`is_fatal` arms use the
  `{ .. }` pattern.

## File-change summary (7 files)

| File | Change |
|---|---|
| `src/superblock.rs` | `SuperblockDefect` + `SlotDefect` + `validate` + `diagnose`; `deserialize` refactored to call `validate`; new unit tests |
| `src/error.rs` | `CorruptSuperblock { defects }`; `Display` (+ `SuperblockDefect` Display); `is_fatal` arm `{ .. }`; exhaustiveness arrays/arms |
| `src/transaction.rs` | `:454` lazy construct via `diagnose`; `:435` doc touch |
| `src/lib.rs` | `pub use superblock::{SuperblockDefect, SlotDefect};` |
| `tests/error_and_format.rs` | `:56`, `:102` construct `{ defects: vec![] }` |
| `src/recovery_tests.rs` | strengthen `corrupt_magic` test; `:544` OR-arm pattern |
| `python/src/errors.rs` | `:311` match pattern `{ .. }` (message-only; type unchanged) |

## Non-goals

- `page_size` / `format_version` mismatches stay their own caller-raised errors
  (they are validated outside `deserialize`/`select`), not folded into
  `CorruptSuperblock`.
- No structured Python `.defects` accessor (message-only, per above).
- `select` and `deserialize` keep their `Option`-based contract; this does not
  adopt the deepdive's alternative "`deserialize -> Result`" sketch (it would
  churn the success path for no failure-path gain).

## Verification gate

`cargo test` (incl. the strengthened recovery test) + `cargo test --release -p chisel --lib`
+ `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --check`
+ Python `pytest` (the `to_py_err` arm change must keep the binding green).
