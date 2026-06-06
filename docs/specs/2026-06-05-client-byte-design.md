# Client Byte — Design

**Status:** Approved 2026-06-05 (brainstorming). Pre-implementation.

**Goal:** Give each chunk a single client-owned `u8` ("client byte") that Chisel
persists and returns verbatim but never interprets — no search, no filter, no
indexing. It occupies the last reserved `HandleEntry` byte (`[15]`).

**Companion docs:** `ARCHITECTURE.md` (layer mechanics), `.codebase-memory/adr.md`
(the *why*; this feature becomes ADR-14), `ISSUES.md` (issue log).

---

## Motivation

The relational layer built on Chisel wants a tiny per-chunk scratch field for
opaque metadata — a record-kind nibble, a generation/state marker — that it can
flip cheaply without rewriting the value or spending a tag. The chunk-tags work
(ADR-12) committed 4 of the 5 reserved `HandleEntry` bytes to the `u32` tag,
leaving exactly one byte (`[15]`) reserved. This feature spends it.

The byte is deliberately *dumb*: Chisel stores and returns it and does nothing
else with it. That is the whole contract.

---

## Scope

**In scope**

- A mutable per-chunk `u8`, default `0`.
- A read accessor and a write mutator on `Chisel` (Rust) and the Python binding.
- Persistence across reopen; correct transactional behavior (commit / rollback /
  savepoint); preservation across `update()`.

**Out of scope (non-goals)**

- No searching, filtering, or indexing by client byte — it is opaque (contrast
  the tag's membership index, ADR-12).
- No allocate-time parameter. New chunks default to `0`; the client sets the byte
  afterward. (Rationale: "mutable, set anytime" was chosen over "both".)
- No on-disk format-version change (see "Format version — no change").
- No richer `Handle` return type. The handle stays `u64`; the byte is read via an
  accessor (chosen over a struct-valued handle).

---

## API (Rust)

Public methods on `Chisel` (`src/lib.rs`), each delegating to `TransactionManager`
(`src/transaction.rs`):

```rust
/// Return the client byte stored in `handle`'s handle-table entry.
/// Returns 0 for chunks that never set it (including every pre-feature chunk).
/// Opaque to Chisel — never searched or interpreted. Takes &self (F3 read path).
pub fn client_byte(&self, handle: u64) -> Result<u8>;

/// Set the client byte for `handle`. Requires an active transaction; the change
/// is durable on commit and reverted on rollback. Any u8 is valid (opaque).
/// Takes &mut self.
pub fn set_client_byte(&mut self, handle: u64, byte: u8) -> Result<()>;
```

**Errors.** Both return `ChiselError::InvalidHandle(handle)` for an unknown or
deleted handle, mirroring `tag()` / `update()`. `set_client_byte` additionally
surfaces the existing no-active-transaction error and poisons on a fatal IO error
during the leaf COW, exactly like other mutators. **No new error variant.**

---

## Data model

`HandleEntry` (`src/handle_table.rs`) gains one field:

```rust
pub struct HandleEntry {
    // existing: page_id (u64), slot_index (u16), flags (u8), tag (u32) ...
    /// Client-owned opaque byte; 0 = unset. Stored in the entry's last
    /// reserved byte [15]. Chisel never interprets it.
    pub client_byte: u8,
}
```

The 16-byte on-disk entry layout is **unchanged**. Byte `[15]`, previously written
as `0`, now carries `client_byte`:

- `read_entry`: `client_byte: buf[base + 15]`.
- `write_entry`: `buf[base + 15] = entry.client_byte;` (was hardcoded `0`).

Every `HandleEntry` construction site must set `client_byte` — `0` for
`allocate` / new / default entries, and carry-forward in `update` (see Semantics).

---

## Format version — no change

The on-disk layout is byte-identical: `[15]` has always existed and always been
written. Activating a *reserved* byte is not a format change — no reader needs to
gate on it. Therefore `FORMAT_MINOR_VERSION` stays `1` and `src/page.rs` is
untouched.

**Compatibility**

- *Backward:* a pre-feature database has `[15] == 0` everywhere, so it reads as
  `client_byte == 0`. No migration.
- *Forward (caveat, recorded not gated):* a pre-feature binary hardcodes
  `[15] = 0` on every entry rewrite, so opening a client-byte database with an
  older binary and then rewriting an entry (`update`, or a defrag move) silently
  clears that chunk's client byte. Acceptable pre-1.0 (no production databases,
  single-writer single-process, opaque metadata), and is exactly the case the
  deferred I29 minor-write gate would eventually catch. ADR-14 records this and
  the principle that spending a reserved byte is a non-versioned change.

---

## Semantics

- **Default:** `0` at allocation and for all legacy chunks.
- **Mutability:** changeable any number of times via `set_client_byte`, within a
  transaction.
- **Transactional:** durable on `commit`; reverted on `rollback` and on
  `rollback_to(savepoint)` through the existing watermark/COW machinery (the
  entry's prior leaf is restored — no special-casing).
- **Preserved across `update()`:** `update_inner` rewrites the entry when a value
  moves pages; it already carries `tag` forward and MUST carry `client_byte`
  forward at the same two rewrite sites. Missing one makes `update()` a silent
  client-byte-loss path (same bug class as I99). This is the single most important
  correctness obligation in the feature.
- **Independent of tag:** orthogonal fields in the same entry; setting one never
  affects the other.
- **Uniform across value backings:** the byte lives in the entry, so it behaves
  identically for inline, overflow, and spillway-backed values.
- **Cost:** `set_client_byte` COWs only the handle-table leaf root-to-leaf path;
  it never touches the data page, overflow/spillway, or the membership index.
- **Delete:** the byte goes with the entry; a reallocated handle starts at `0`.

---

## Concurrency

Consistent with ADR-2 (single-writer, `&mut self`): `set_client_byte` is
`&mut self`; `client_byte` is `&self` and rides the `RefCell<PageCache>` read path
like `tag()` / `read()`.

---

## Python binding

Mirror the tag surface exactly:

- `client_byte(handle) -> int` and `set_client_byte(handle, byte)` on both
  `chisel.Database` (`python/src/db.rs`) and the `Transaction` context manager
  (`python/src/transaction.rs`).
- `client_byte_internal` / `set_client_byte_internal` helpers in `db.rs`.
- `byte` is extracted as `u8`; out-of-range ints raise `OverflowError` (PyO3
  default) — documented in the method docstring.

---

## Testing

**Rust integration** (`tests/client_byte.rs`; dual-backing where applicable):

1. In-session set + read returns the value (both backings).
2. Default `0` for a freshly `allocate`d chunk and an `allocate`-only legacy chunk.
3. Durability across close / reopen.
4. Revert on `rollback`; revert on `rollback_to(savepoint)`.
5. Preserved across `update()`, including a value that grows inline → overflow/spillway.
6. Preserved with an overflow/spillway-sized value across reopen.
7. Independence from tag: `allocate_tagged` + `set_client_byte`, verify both
   survive an `update()` and a reopen.
8. Bad / deleted handle → `InvalidHandle`.
9. `set_client_byte` on a poisoned manager → `Poisoned` (mirror the tag test).

**Rust unit** (`src/handle_table.rs`): `write_entry ∘ read_entry` round-trips
`client_byte` over byte `[15]`.

**Python** (`python/tests/`): set/read via both `Database` and `Transaction`;
reopen persistence; out-of-range int raises `OverflowError`.

---

## Documentation impact

- `ARCHITECTURE.md`: update the `HandleEntry` layout description (`[15]` is the
  client byte now, not "reserved") and add the `client_byte` / `set_client_byte`
  ops.
- `README.md` + `python/README.md`: add the two ops.
- `.codebase-memory/adr.md`: new **ADR-14** ("Client byte — spending the last
  reserved entry byte") + ADR-0 register row. Record the no-version-bump decision,
  the reserved-byte-vs-format-change principle, and the forward-compat caveat.
  Update text in ADR-7 / ADR-12 and the `handle_table.rs` comment that currently
  call `[15]` "the one remaining reserved byte."
- `ISSUES.md`: file any deferred follow-ups surfaced during implementation.

---

## File touch list

- `src/handle_table.rs` — `HandleEntry.client_byte` field; `read_entry` /
  `write_entry` (`[15]`); all entry constructors; the `[15]` comment; round-trip
  unit test.
- `src/transaction.rs` — `client_byte` (+ `_inner`), `set_client_byte`
  (+ `_inner`); carry `client_byte` at `update_inner`'s two entry-rewrite sites;
  `client_byte: 0` at the `tag: 0` construction sites; tests.
- `src/lib.rs` — public `client_byte` / `set_client_byte` delegators + doc comments.
- `python/src/db.rs` — `client_byte` / `set_client_byte` + `_internal` helpers.
- `python/src/transaction.rs` — `client_byte` / `set_client_byte` on the context manager.
- `tests/client_byte.rs` — integration tests (new file).
- `python/tests/` — Python tests.
- Docs as above (done last, after the API is final).

---

## Rejected alternatives

- **Store the byte with the value (data page).** Forces a full value rewrite per
  change and re-couples metadata to value bytes; loses the cheap-flip property.
- **Richer `Handle { id, tag, client_byte }` return type.** Settled by the
  "Accessor, handle stays `u64`" decision — a non-breaking accessor instead of a
  breaking signature change across every handle-returning method.
- **Immutable / allocate-time only.** Settled by the "mutable, set anytime"
  decision; immutability would force delete + reallocate to change the byte.
- **MINOR version bump `1 → 2`.** Considered for record-keeping, but the layout is
  byte-identical so there is nothing for a reader to gate on; rejected in favor of
  treating reserved-byte activation as a non-versioned change (ADR-14).
