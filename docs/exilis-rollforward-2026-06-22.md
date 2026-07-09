# Chisel client roll-forward notes — 2026-06-22

**Audience:** whoever maintains **Exilis** (Chisel's one current client).
**Purpose:** roll Exilis forward to the current Chisel and adapt to the client-visible changes.
**Baseline:** these notes cover everything client-visible since Chisel commit `7daa69d`.

## TL;DR — does anything break?

**No.** Nothing here requires a code change for Exilis to keep compiling and running. The surface changes are one *additive* error type and a few *defensive* behavior fixes; the large changes (a new freemap format, a big internal refactor) are invisible to callers. Read on only to (a) opt into the new error/methods if useful, and (b) check the one behavior change that *could* matter if you use the spillway.

---

## 1. New error: `UnsupportedPageSize` (additive)

Chisel now validates, at `open()` time, that the database file's stored page size matches the page size the running build was compiled with. On mismatch it returns a new **fatal** error instead of silently misreading the file.

- **Rust:** new variant `ChiselError::UnsupportedPageSize { stored: u32, compiled: u32 }`. `ChiselError` is `#[non_exhaustive]`, so your existing `match` already has a wildcard arm and **keeps compiling** — the new variant falls through it. Add a dedicated arm only if you want to surface a specific message.
- **Python:** new exception class `UnsupportedPageSizeError`, a subclass of the existing `FatalError`. Any `except FatalError:` you already have **catches it**. Add `except UnsupportedPageSizeError:` only if you want to special-case it.

**Will it ever fire for you?** Not in practice today — `PAGE_SIZE` is a fixed compile-time constant (8192), so no file written by any current build can mismatch. It's a forward-compatibility guard: if a future Chisel build ever ships a different page size, opening an old file with it becomes a clean, diagnosable error instead of corruption. **Action: none required.**

## 2. Behavior fixes (no signature change)

These are corrections to existing behavior. None changes a type or method signature.

- **Out-of-range handles now error correctly.** Previously, calling `read` / `tag` / `client_byte` / `update` / `delete_tagged` with a handle fabricated out of range (e.g. via `Handle::from(raw_u64)` on a Rust side, or a bogus integer handle) could *wrap onto a live slot and return another handle's value*. It now returns `InvalidHandle` (Rust) / raises the invalid-handle error (Python). **Action: none** — unless Exilis ever constructed handles from raw integers it did *not* receive from `allocate`. It shouldn't; handles are opaque tokens. If it does, those calls now fail loudly instead of returning wrong data (which is what you want).

- **Spillway open failure is now fatal (the one change that could matter).** The spillway sidecar file is opened lazily. If that open fails (e.g. the parent directory vanished, a permissions problem), Chisel now surfaces it as the **fatal `IoError`** and *poisons* the manager — the only recovery is close + reopen. Previously a `NotFound`-class open error was demoted to the *operational* `FileNotFound`, which a caller might have treated as recoverable in place. **Action:** if Exilis uses the spillway *and* has a code path that catches a spillway-open error and continues using the same handle, change it to close + reopen. If you don't use the spillway, or already treat fatal errors as "reopen," no change.

- **Overflow pages are now type-validated.** A checksum-valid page of the wrong type reached via a stale/buggy handle entry now returns `CorruptPage` instead of being misparsed as overflow data. This is purely defensive (matches the long-standing data-page hardening) and only surfaces under actual corruption. **Action: none.**

## 3. New Python convenience methods (additive)

`Transaction` (the `PyTransaction` object you get from the DB handle) gained forwarding methods that previously lived only on the top-level DB object:

- `transaction.handles()` — list live handles
- `transaction.stats()` — database stats
- `transaction.counters()` — operation counters
- `transaction.defrag(options=None)` — run defragmentation

Nothing was removed or renamed. **Action: optional** — use them if convenient.

## 4. Multi-page freemap — no action, just good news

Chisel previously stopped reclaiming freed space past roughly **512 MB** of database file (freed pages beyond that leaked silently). The freemap is now a copy-on-write tree with no practical size ceiling, so space reclamation keeps working as the database grows.

This **changed the on-disk format**, but **backward-compatibly**:

- The file-level format indicator was **not** bumped. Existing Exilis databases open unchanged.
- A small database stays byte-identical to the old single-page layout (`freemap_depth = 0`); the tree only deepens once a database grows large enough to need it.

**Action: none.** Existing databases open and operate exactly as before; large databases now reclaim space they previously leaked.

---

## What did *not* change

- **The whole `transaction.rs` → `transaction/` module decomposition** (several internal refactor PRs) is **pure and behavior-preserving** — zero API or on-disk change. Mentioned only so you don't go looking for a behavior difference behind it.
- No public method signatures changed (`Chisel::open`, `allocate`, `read`, `update`, `delete`, `commit`, savepoints, named roots, etc. are all unchanged).
- The on-disk format version was deliberately **not** bumped; it will be reset to a clean baseline at Chisel's eventual public release, not mid-development.

## Recommended roll-forward steps for Exilis

1. Bump the Chisel dependency to the current commit and rebuild — **expect a clean build** (no breaking surface changes).
2. Run Exilis's existing test suite — **expect it to pass unchanged.**
3. *If* Exilis uses the spillway: review any catch-and-continue on a spillway-open error and switch it to close + reopen (§2).
4. *Optional:* add a dedicated handler for `UnsupportedPageSize` / `UnsupportedPageSizeError` (§1) and adopt the new `Transaction` methods (§3) if useful.
