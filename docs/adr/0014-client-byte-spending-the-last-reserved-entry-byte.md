---
id: 0014
title: Client byte — spending the last reserved entry byte
date: 2026-06-05
status: Accepted
---

# 0014. Client byte — spending the last reserved entry byte

**Context:** Chunk tags (ADR-12) committed 4 of the 5 reserved `HandleEntry`
bytes to the `u32` tag, leaving one byte (`[15]`). The relational client wanted a
small per-chunk scratch value it could set and read without rewriting the chunk's
value or spending a tag — opaque metadata Chisel stores but never interprets.

**Decision:** Expose a per-chunk `u8` "client byte" stored in entry byte `[15]`.
It is mutable (`set_client_byte(handle, u8)`, a transactional handle-table
mutation that COWs only the leaf) and readable (`client_byte(handle) -> u8`,
mirroring `tag()`'s read path). Default `0`. Opaque: no search, no filter, no
index — contrast the tag's membership index (ADR-12). The byte rides every value
`update()` via the same entry carry-forward that preserves the tag, and reverts
with the transaction on rollback. Deleted handles return `InvalidHandle`
(following `read()`).

Crucially, **no on-disk format change**: byte `[15]` has always been part of the
16-byte entry and always written (as `0`). Activating a *reserved* byte is not a
versioned change — there is nothing for a reader to gate on — so
`FORMAT_MINOR_VERSION` stays `1`. This refines ADR-7: reserved bytes are part of
the format from creation; only new structures or semantics a reader must gate on
warrant a version bump.

**Alternatives considered:**

- *Store the byte with the value (data page).* Rejected: forces a full value
  rewrite per change and re-couples metadata to value bytes. Entry-resident
  storage makes a flip cost one handle-table leaf COW, independent of value size.
- *Immutable / set-at-allocation only (like the tag).* Rejected: the client needs
  to change it in place; immutability would force delete + reallocate.
- *Richer `Handle { id, tag, client_byte }` return type.* Rejected: a breaking
  change to every handle-returning signature; an accessor keeps `handle: u64`.
- *MINOR bump `1 -> 2` for record-keeping.* Rejected: the layout is byte-identical,
  so there is nothing to gate on (see Decision).

**Consequences:**

- *Positive:* Cheap, value-size-independent per-chunk metadata with zero format
  cost — the payoff of pre-allocating reserved bytes in the original layout.
- *Positive:* No migration; pre-feature databases read byte `[15]` as `0`.
- *Negative (caveat, recorded not gated):* a pre-feature binary hardcodes
  `[15] = 0` on every entry rewrite, so opening a client-byte database with an
  older binary and rewriting an entry (`update`, defrag) silently clears that
  chunk's client byte. Acceptable pre-1.0 (no production databases, single-writer
  single-process, opaque metadata); it is exactly the case the deferred I29
  minor-write gate would catch.
- *Note:* `client_byte` / `set_client_byte` reject deleted handles with
  `InvalidHandle` (following `read()`), stricter than `tag()`'s current unguarded
  read of a tombstone — a pre-existing `tag()` quirk tracked separately.

Spec: `docs/specs/2026-06-05-client-byte-design.md`.

---
