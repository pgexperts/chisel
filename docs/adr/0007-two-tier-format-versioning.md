---
id: 0007
title: Two-tier format versioning
date: unknown
status: Accepted
---

# 0007. Two-tier format versioning

**Context:** On-disk format compatibility is a long-term promise. Binaries from any version should refuse to open files written by an incompatible binary, and ideally accept files written by any compatible binary without forcing migration. But "compatibility" has two granularities: file-level (does the high-level layout match?) and page-level (do individual page layouts match?). Conflating them forces a file-wide format bump for any per-page change.

**Decision:** Two-tier versioning. (a) The superblock carries a packed `format_version: u32` — upper 16 bits MAJOR, lower 16 bits MINOR. Open-time gate compares MAJOR only; same-major files open regardless of minor; different-major is rejected with `UnsupportedFormatVersion`. (b) Every non-superblock page carries a one-byte `page_format_version` in its header. This lets individual page layouts evolve within a major without a file-wide bump. Today every page reports version 0; future minor changes to a page-type's layout will bump that page-type's version while leaving others alone.

**Alternatives considered:**

- *File-level only.* Standard approach; works but every per-page layout tweak forces a file-wide migration. Rejected because Chisel has 4 page types (handle table interior/leaf, data, overflow, freemap) and they're likely to evolve at different rates.
- *Page-level only (no file-level gate).* Would lose the "completely incompatible binary, refuse to open" check. The MAJOR check provides a clear "this binary cannot read this file" failure mode.
- *Schema migration system.* Out of scope for embedded; would require migration scripts, version-jump testing, etc.

**Consequences:**

- *Positive:* The README's "sacred within a major version" promise is enforceable. A user upgrading from a 1.x binary to a 1.y binary (y > x) opens their existing files cleanly.
- *Positive:* Per-page-type evolution. A future change to data-page slot layout bumps only that page-type's version; existing handle-table pages, freemap pages, etc. remain untouched and unmigrated.
- *Positive:* Lazy migration is the default. Reads dispatch on the version byte; writes always produce the latest version. An opt-in eager upgrader (deferred to a future minor; see ISSUES.md I31) sweeps remaining old pages.
- *Negative:* Two version checks instead of one. The page-cache load path validates the per-page version on every miss; the cost is one byte of comparison per cache miss.
- *Pre-1.0 caveat:* The on-disk `format_version` constant may receive one final reset to 1 before release. No production databases exist yet (project-memory note `project_chisel_format_version_tentative`), so accumulating in-development bumps can be collapsed.

**Update (2026-06-02):** Chunk tags (ADR-12) drove the first MINOR bump, `0 → 1` — the two-tier scheme's first real exercise. A minor-0 (pre-tag) database opens cleanly under a minor-1 binary through the MAJOR-only gate and reads as fully untagged, exactly as the design promised. The per-page version byte was untouched: the new `MembershipInterior` (`0x05`) / `MembershipLeaf` (`0x06`) page types are born at `PAGE_FORMAT_VERSION_CURRENT = 0`, confirming an additive page-type change needs no per-page bump.

**Update (2026-06-21):** The per-page read-dispatch (I31) and the file-MINOR write-gate (I29) landed (`docs/specs/2026-06-21-per-page-format-versioning-design.md`). This corrects two over-statements in the Consequences above:

- The page-cache load path validates ONLY the XXH3 checksum — it does **not** read or validate the per-page version byte. There is no per-miss version check; that "Negative" consequence never described shipped behavior. Version dispatch is per-module and **decode-only** (a reader branches `if page_format_version(buf) >= K { read field } else { default }`), and is currently **dormant**: with only version 0 in existence, no read path branches on it, so "reads dispatch on the version byte" describes the mechanism, not present behavior.
- Writes always stamp `page::current_version(page_type)` (the single per-type write seam; every `init_page` site calls it), and COW makes upgrade-on-write free. The I29 gate forces a file whose MINOR exceeds the binary's into **read-only** at open (`PageIo::force_read_only`) rather than rejecting it — reads stay safe by the additive invariant; only writes are refused (`ReadOnlyMode`).

Refined model: a **zero-default additive** field (the ADR-14 client-byte pattern, where zero == absent == default) needs **no** version bump. The per-page version exists *solely* to disambiguate absent-vs-zero for additive fields where zero is a legitimate value. The eager-upgrade sweep remains deferred.

**Update (2026-06-30):** The MAJOR tier saw its first real bump — encrypted databases stamp **MAJOR=2** (`ENCRYPTED_FORMAT_VERSION = pack(2, 0)`; plaintext stays MAJOR=1). The open-time gate now computes the expected MAJOR from whether a crypto-header is present (`expected = if encrypted { 2 } else { 1 }`), so a new binary accepts both a MAJOR=1 plaintext file and a MAJOR=2 encrypted one, while an encryption-unaware old binary (which gates on MAJOR==1) refuses a MAJOR=2 file — the "completely incompatible binary refuses to open" guarantee working exactly as designed. See ADR-15.

See `ISSUES.md` I29 (file-level) and I31 (page-level).

---
