---
id: 0013
title: Within-session iteration-stability contract
date: 2026-06-04
status: Accepted
---

# 0013. Within-session iteration-stability contract

**Context:** `handles()` and `handles_with_tag()` materialize a `Vec<u64>` by walking radix trees, and their doc comments explicitly disclaimed any ordering ("order is unspecified"). The relational client wants to scan a relation, do work, and scan it again expecting identical results when it has not mutated the store — to re-drive a query, resume an interrupted pass, or cross-check — without defensively sorting or snapshotting. The implementation already produced a deterministic order (the radix walk is a pure function of tree structure), but the *contract* promised nothing, so a consumer could not rely on it.

**Decision:** Promote the already-true behavior to a documented, tested guarantee, scoped deliberately narrow: within a single open instance, repeated `handles()` / `handles_with_tag(tag)` calls return an identical `Vec` (same elements, same order) as long as the relevant live set is unchanged between calls and no `defrag` has run. The *order itself stays unspecified* — this is a *repeatability* guarantee, not an ordering one. The guarantee is single-session only (it does not survive close+reopen or `defrag`). No production code changed: the radix walks already satisfy it. The work is the contract (doc comments on `Chisel::handles`, `Chisel::handles_with_tag`, and `RadixU64::iter`) plus adversarial differential tests (`tests/iteration_stability.rs`).

**Alternatives considered:**

- *Guarantee a specific order (ascending handle).* Strongest, and already what the implementation produces. Rejected: it would commit the public contract to a particular order, constraining any future change to the index internals. "Repeatable but opaque" preserves that freedom.
- *Snapshot / MVCC isolation* so a scan stays stable even across concurrent mutation. Rejected as out of scope — Chisel is single-writer (ADR-2), and the requirement explicitly excluded mutation between scans.
- *Wider scope* (the guarantee survives reopen and/or `defrag`). Rejected in favor of single-session, which constrains future internals the least while still serving the client's scan-twice use case.
- *Internal `debug_assert!` sortedness canary* as a second enforcement mechanism. Rejected — it would couple internal code to the ascending behavior we explicitly declined to promise; a future intentional reorder would have to delete it.

**Consequences:**

- *Positive:* The scan layer can rely on within-session repeatability without defensive sorting or snapshotting.
- *Positive:* Minimal commitment. The order stays opaque and the scope is single-session, so the radix order, reopen layout, and `defrag` reordering all remain free to change.
- *Positive:* The differential tests double as a regression guard for the radix-depth re-derivation invariant (ISSUES.md I99 / C1): a rolled-back grow that failed to restore tree depth would mis-enumerate a later scan, which the rollback/savepoint tests catch from the iteration angle.
- *Negative (subtlety):* "Repeatable" cannot be checked from a single call — only differentially (scan, churn the state the contract permits to vary, scan again, compare). Enforcement is therefore test-only; there is no compile-time or single-call guard.
- *Reversibility:* Easy in code (the guarantee is documentation over existing behavior), but it is now a public contract — removing it would be a breaking change for consumers that rely on within-session scan repeatability.

Spec: `docs/specs/2026-06-04-stable-chunk-iteration-design.md`. Plan: `docs/plans/2026-06-04-stable-chunk-iteration.md`. Tests: `tests/iteration_stability.rs`. Hardens the tagged-iteration API from ADR-12.

---
