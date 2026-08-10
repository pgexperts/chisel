---
id: 0021
title: Linearize credential revocation with an fsync barrier before the sibling scrub
date: 2026-08-10
status: Accepted
summary: rewrite_crypto_header fsyncs after writing the target superblock slot and before scrubbing the stale key-slot table from the other N-1 slots, so a crash can never tear every slot at once.
---

# 0021. Linearize credential revocation with an fsync barrier before the sibling scrub

## Context

The key-slot table is cleartext at bytes 332..1356 of every superblock image and the
per-DB DEK never changes across a credential rotation. A sibling slot still carrying the
PRE-revocation table is therefore a live, unwrappable copy of the current DEK, usable by
anyone holding the revoked credential with only READ access. `rewrite_crypto_header` was
extended to scrub every sibling.

That scrub introduced a durability regression. `write_page_unit` is a positioned write
with no implicit flush, and the function issued a single fsync after ALL N slot writes.
Program order is not device order, so every slot was in flight simultaneously: a power
loss could tear all of them, `Superblock::select` would find nothing that validates, and
the database would be unopenable BY ANY KEY. That window did not exist before the scrub,
because the function never wrote more than one slot. The comment defending the ordering
asserted program order as if it were durability.

## Decision

An fsync separates the target-slot write from the sibling scrub. Before it returns, only
the target is in flight and every sibling remains durable and valid at its own counter,
so the worst outcome is a lost revocation. After it returns, the target is durable,
validates, and holds the strict maximum `txn_counter`, so `Superblock::select` picks it
regardless of what happens to the siblings — which is what lets all N-1 scrub writes
share the single trailing fsync.

Two fsyncs total, independent of N. `add_key` takes the same path as the revoking
operations rather than branching on intent.

## Alternatives considered

- **One fsync per sibling** — rejected: unnecessary. After the barrier the target
  already outranks every sibling, so the siblings are collectively expendable.
- **Write the full superblock unit to all N slots** (the shape the issue proposed) —
  rejected: it destroys the always-one-durable-valid-slot invariant outright, which is a
  strictly worse version of the bug being fixed.
- **Apply the barrier only to rotate_key/remove_key** — rejected: `rewrite_crypto_header`
  has no caller-intent flag, so the branch is new failure surface whose bug silently
  reinstates the leak; and a benign `add_key` is currently what clears residue left by an
  earlier rotation that crashed mid-scrub.

## Consequences

Key operations now cost three fsyncs (entry flush, barrier, post-scrub) instead of two.
These are rare administrative operations, so the cost is irrelevant.

The fsync indices are now part of the tested contract: fault-injection tests target each
by index, and one test pins the COUNT so the barrier cannot be deleted silently.

Not addressed: `overwrite_slot_table` skips a slot that fails `deserialize`, on the
grounds that "there is no stale table in it to scrub". Under the read-attacker threat
model that is not strictly true — a slot can fail its checksum while the table bytes
survive. Self-healing under N=2 alternation; filed separately.
