---
id: 0000
title: Decision register (overview)
date: 2026-05-04
status: Accepted
---

# 0000. Decision register (overview)

Chisel is a single-writer embedded transactional storage engine in Rust. The decisions below are the ones that, if reversed, would require rewriting substantial parts of the engine. Smaller decisions (specific bit layouts, error message wording, individual issue resolutions) live in `ISSUES.md`.

| # | Decision | Status | Reversibility |
|---|---|---|---|
| 1 | Shadow paging, not WAL | Accepted | Hard — touches commit protocol, recovery, every page-mutation path |
| 2 | Single-writer enforced by `&mut self` | Accepted | Hard — every API signature would change |
| 3 | Per-module COW (no centralized abstraction) | Accepted | Medium — affects 5 modules |
| 4 | N rotating superblocks (configurable 2..=16) | Accepted | Hard — recovery and commit both depend |
| 5 | Spillway sidecar file over hard ceiling | Accepted (2026-05-04) | Medium — supersedes `HARD_CEILING_MULTIPLIER` |
| 6 | Poison model on fatal errors | Accepted | Easy — could be relaxed, but Linux fsyncgate semantics make retry unsafe regardless |
| 7 | Two-tier format versioning (file MAJOR/MINOR + per-page byte) | Accepted | Hard — affects every page header |
| 8 | In-memory mode via `Vec<u8>`-backed PageIo | Accepted | Easy — additive; could be removed |
| 9 | Counter instrumentation via `Chisel::counters()` | Accepted (PR 1, bench-suite) | Easy — additive, `#[non_exhaustive]` |
| 10 | Bench-suite series (cross-engine comparison + dedicated machine foundation) | Accepted (PRs 1-8 shipped 2026-04-30 → 2026-05-04) | Easy — bench/ is a sibling crate; no engine impact |
| 11 | macOS-fsync fairness via `PRAGMA fullfsync=ON` on SqliteEngine | Accepted (PR 8, 2026-05-04) | Easy — bench-side only |
| 12 | Chunk tags + reverse membership index | Accepted (2026-06-02) | Medium — new on-disk subsystem; additive format (MINOR) |
| 13 | Within-session iteration-stability contract | Accepted (2026-06-04) | Easy — documents existing behavior; public API contract |
| 14 | Client byte — opaque per-chunk u8 in the last reserved entry byte | Accepted (2026-06-05) | Easy — additive; reuses reserved byte [15], no format change |
| 15 | On-disk encryption (XChaCha20-Poly1305, envelope DEK/KEK, MAJOR=2) | Accepted (2026-06-30) | Hard — first MAJOR format bump; encrypted-stride + sealed superblock + page-I/O seal seam |

The body of this ADR walks each decision in turn.

---
