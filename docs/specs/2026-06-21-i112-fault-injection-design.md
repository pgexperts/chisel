# I112 — Fault-injection test layer (subsumes I114 + I115)

**Status:** approved design, 2026-06-21
**Issues closed:** I112 (P2), I114 (P3), I115 (P3)

## Goal

Drive *real* I/O faults into a live engine so that two things currently only
*reasoned about* become *tested*:

1. **The poison/flush coupling.** `PageCache::flush` clears each page's dirty
   flag *before* the trailing fsync (`page_cache.rs:363-378`, the "durability
   window" comment). That is safe *only* because a failed fsync poisons the
   manager — the comment explicitly warns it breaks if poisoning is weakened.
   No test currently induces a flush/fsync failure and observes the poison.
2. **The fatal-`IoError` path.** The most-travelled fatal error is
   constructed-only: no test induces a real I/O fault and observes the engine
   returning `IoError` and poisoning. `fatal_error_outside_commit_also_poisons`
   is tautological — it calls `force_poison_for_test()` then asserts `Poisoned`.

The same fault layer closes the dependent gaps: **I115** (pin
`CorruptSuperblock`; remove the provably-dead `InvalidMagic`) and **I114**
(make the I20 dirty-page invariant release-safe).

## Non-goals (explicit scope boundary)

- **`set_page_count` is NOT faultable.** A failed truncate mid-rollback is a
  distinct fatal path; the three faulted ops below cover the stated I112
  targets. Deferred, not forgotten.
- **Open/bootstrap `IoError` faults are NOT in scope.** A read fault on a
  superblock slot during `open` would test an open-time `IoError`, but there is
  no manager yet to poison, and open-time `CorruptSuperblock` is already covered
  by the byte-corruption path (Part B). Deferred.
- **No public API surface** is added. The fault hooks are `#[cfg(test)]`; all new
  tests are crate-internal unit tests or existing `recovery_tests.rs`. The only
  public change is *removing* the dead `InvalidMagic` variant.

## Architecture

### Component 1 — the fault hook in `PageIo` (`src/page_io.rs`)

A `#[cfg(test)]` field on `PageIo`, mirroring the existing in-engine injection
pattern (`fail_next_membership_op` is a `#[cfg(test)] Cell` checked inline).

```rust
#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    /// Fail an fsync via a countdown: `FailFsync(0)` fails the NEXT fsync;
    /// `FailFsync(n)` lets `n` fsyncs succeed (decrementing each) then fails.
    /// The countdown is what lets a test target a specific fsync in commit's
    /// three-fsync protocol (pre-drain / data-flush / superblock).
    FailFsync(u32),
    /// Fail `write_page` for exactly this page id (one-shot).
    FailWritePage(u64),
    /// Fail `read_page` for exactly this page id (one-shot).
    FailReadPage(u64),
}
```

- New field: `#[cfg(test)] fault: Cell<Fault>` on `struct PageIo`
  (`page_io.rs:47-72`, after `cached_page_count`).
- Both struct literals — `open()` (`:107`) and `open_in_memory()` (`:124`) —
  gain `#[cfg(test)] fault: Cell::new(Fault::None),`. These are the only two
  constructors. (Non-test callers cannot name a `cfg(test)` field, so the
  module boundary enforces this; the compiler error on a missed literal is the
  clear "missing field `fault`".)
- `#[cfg(test)] pub(crate) fn arm_fault(&self, f: Fault) { self.fault.set(f); }`
  (after `fsync_count`, ~`:326`).
- Check sites (each under `#[cfg(test)]`, returning a synthesized
  `ChiselError::IoError(io::Error::new(ErrorKind::Other, "…"))` so `Fault`
  stays `Copy`):
  - `read_page` (`:213`): after the bounds check, before the backing match —
    fire on `FailReadPage(page_id)`, clear to `None`.
  - `write_page` (`:247`): **after** the `read_only` guard (`:250`), before the
    backing match — fire on `FailWritePage(page_id)`, clear to `None`.
  - `fsync` (`:298`): **after** the `read_only` guard (`:301`), before the
    backing match — on `FailFsync(0)` fire and clear; on `FailFsync(n)` set
    `FailFsync(n-1)` and proceed. (`fsync_calls` is only incremented on the
    real success path, so a faulted fsync correctly leaves the counter
    unchanged — matching fsyncgate semantics.)

The fault check is **before** the `File`/`Memory` match, so faults compose with
the in-memory backing — every poison test runs on the fast, deterministic
in-memory `PageIo`; no real file needed.

**Rationale for ordering** (`read_only` first, then fault): the operational
`ReadOnlyMode` guard is a real production guard; the fault is a test artifact.
Checking the real guard first means a test never accidentally masks a
`ReadOnlyMode` bug. Faults are armed only in tests, so the ordering has no
production effect.

### Component 2 — Part A tests (the poison/flush + `IoError` coverage)

All crate-internal `#[cfg(test)]` unit tests. Arming path:
`tm.cache.borrow().io().arm_fault(Fault::…)` — the test module already accesses
the private `cache` field (e.g. `tm.fail_next_membership_op.set(true)`), and
`PageCache::io(&self) -> &PageIo` (`page_cache.rs:675`) reaches the `PageIo`
through a shared borrow.

**(a) `page_cache.rs` — flush/fsync coupling, isolated.** Build a cache over a
faulty in-memory `PageIo` (`fresh_cache_with_spillway`, `:1142`), dirty some
pages, `arm_fault(FailFsync(0))`, call `flush()` (`:379`). Assert:
- `flush()` returns `Err(IoError)`;
- **`dirty_count() == 0` afterwards** — the dirty flags were already cleared
  (phase 1a, `:406`) before the failed fsync. This assertion *documents the
  hazard*: the cache now shows clean-but-non-durable pages, and the only thing
  making that safe is the manager poisoning (proven in test (b)). This is the
  single most important assertion in the whole change — it is the "durability
  window" the `page_cache.rs:363-378` comment warns about, made observable.

**(b) `transaction.rs` — each commit fsync poisons.** Begin → allocate →
`arm_fault(FailFsync(k))` → `commit()`, for `k ∈ {0, 1, 2}` targeting the
pre-drain (`:1007`), data-flush (`:1025`), and superblock (`:1072`) fsyncs
respectively. Each asserts `commit()` returns `Err(IoError)` AND
`is_poisoned()`, and that a follow-up call returns `Poisoned`. This is the real
fatal error driving the poison that the I112 problem statement asks for.

**(c) `transaction.rs` — write fault during commit poisons.** Begin → allocate →
`arm_fault(FailWritePage(p))` for a data page id `p` written during commit →
`commit()` → `Err(IoError)` + `is_poisoned()`.

**(d) `transaction.rs` — reimplement `fatal_error_outside_commit_also_poisons`
with a REAL fault.** Replace the `force_poison_for_test()` body: allocate +
commit a value, then `arm_fault(FailReadPage(p))` on that value's page and call
`read()` outside any transaction → assert `Err(IoError)` AND `is_poisoned()`.
This preserves the non-commit poison-path coverage (the "any fatal error
poisons" branch, `transaction.rs:958`) while removing the tautology.

**`force_poison_for_test` cleanup.** After (d), if `force_poison_for_test` has
no remaining callers, remove it (the real fault makes the synthetic crutch dead
code). The `Poisoned` flag itself stays — it is the poison state. The plan must
grep for other callers before removing.

### Component 3 — Part B = I115 (`src/recovery_tests.rs`, `src/error.rs`, Python)

Uses the existing byte-corruption helpers (`rewrite_page_with_valid_checksum`
`:46`, `active_superblock` `:69`) — no new machinery.

1. **Pin `CorruptSuperblock`.** The OR-arm at `recovery_tests.rs:543-545`
   (`InvalidPageId | CorruptSuperblock | FileSizeMismatch`) leaves
   `CorruptSuperblock` un-pinned. Add a test that corrupts the superblock to
   the specific shape that yields `CorruptSuperblock` and asserts it as the
   *sole* expected variant (`assert!(matches!(err, ChiselError::CorruptSuperblock))`).
2. **Prove `InvalidMagic` dead, then remove it.** Add a test that corrupts the
   magic bytes and asserts the surfaced variant is `CorruptSuperblock` (via
   `select`), NOT `InvalidMagic` — confirming the dead-by-construction finding.
   Then remove the variant across all **20 sites** (full inventory below).

**`InvalidMagic` removal inventory** (verified — never constructed; all
references are declarations, Display/category arms, exhaustiveness test arrays,
the Python binding, and docs):

| File | Site |
|---|---|
| `src/error.rs:111` | enum variant decl |
| `src/error.rs:175` | `is_fatal()` match arm |
| `src/error.rs:243` | `Display` arm |
| `src/error.rs:416` | `documented_is_fatal` exhaustiveness test (Fatal block) |
| `src/error.rs:454` | `all[]` array in classification test |
| `src/error.rs:473` | **tripwire: fatal-variant count 9 → 8** |
| `src/lib.rs:302` | doc comment listing fatal errors |
| `tests/error_and_format.rs:61` | `test_is_fatal_storage_integrity_variants_are_fatal` |
| `tests/error_and_format.rs:108` | `test_error_display_is_non_empty_for_every_variant` |
| `python/src/errors.rs:45` | class-hierarchy comment |
| `python/src/errors.rs:139` | `create_exception!` |
| `python/src/errors.rs:221` | `m.add("InvalidMagicError", …)` |
| `python/src/errors.rs:316` | `to_py_err()` match arm |
| `python/chisel/__init__.py:43` | import |
| `python/chisel/__init__.py:138` | `__all__` |
| `python/chisel/chisel.pyi:68` | type stub class |
| `python/tests/test_errors.py` | fatal-class enumeration test |
| `README.md:284` | fatal-variants doc |
| `python/README.md` | error table row |
| `ISSUES.md` (I115) | mark resolved |

### Component 4 — Part C = I114 (`src/page_cache.rs`)

The I20 invariant (`claim_page` must not be called on an already-dirty page) is
guarded by a `debug_assert!` (`:721-723`) and tested only under
`#[cfg(debug_assertions)]` (`:1246-1257`), so `cargo test --release` (the wheel
gate, `wheels.yml:17`) skips it.

Keep the `debug_assert!` (cheap dev guard). Add a **release-compiled** test
asserting the *observable* accounting consequence of the invariant: after
`claim_page` on a fresh page, `dirty_count() == entries-len` and the page reads
dirty. The desync this catches: `claim_page` removing a dirty entry without
decrementing `dirty_count`, then re-incrementing — leaving `dirty_count` one too
high. The accounting check fires in both profiles, giving uniform I20 coverage
across the wheel gate. (`dirty_count == entries.len()` is already asserted
elsewhere — `page_cache.rs:1201` — so the invariant is known-observable.)

## Error handling

The injected error is always `ChiselError::IoError`, which `is_fatal()` ⇒
poisons the manager. Tests assert both the returned variant and `is_poisoned()`.
No new error variants are introduced; one (`InvalidMagic`) is removed.

## Testing & verification

- `cargo test` — full suite (lib unit tests incl. the new Part A/B/C tests,
  integration, bench): 0 failures.
- **`cargo test --release`** — must also pass, specifically exercising the new
  release-safe Part C test (the whole point of I114).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean (the
  `#[cfg(test)]` field and hooks must not trip clippy in any profile).
- `cargo fmt --check` — clean.
- Python `pytest` — green after the `InvalidMagicError` removal (the binding,
  `__init__`, `.pyi`, and `test_errors.py` must all be updated together).

## File-change summary

| File | Change |
|---|---|
| `src/page_io.rs` | `Fault` enum + `#[cfg(test)]` field/hook/`arm_fault` (Component 1) |
| `src/page_cache.rs` | Part A(a) flush test; Part C release-safe I20 test |
| `src/transaction.rs` | Part A(b)(c)(d) tests; reimplement + possibly drop `force_poison_for_test` |
| `src/recovery_tests.rs` | Part B: pin `CorruptSuperblock`; bad-magic test |
| `src/error.rs` | remove `InvalidMagic` (6 sites incl. the 9→8 tripwire) |
| `src/lib.rs` | doc list (drop `InvalidMagic`) |
| `tests/error_and_format.rs` | drop `InvalidMagic` from 2 test arrays |
| `python/src/errors.rs` | remove `InvalidMagicError` (4 sites) |
| `python/chisel/__init__.py`, `.pyi`, `python/tests/test_errors.py` | remove `InvalidMagicError` |
| `README.md`, `python/README.md` | drop `InvalidMagic` doc references |
| `ISSUES.md` | mark I112/I114/I115 fixed |
