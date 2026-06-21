# Per-Page Format Versioning — Design

**Status:** Approved 2026-06-21 (brainstorming). Pre-implementation.

**Goal:** Make the per-page format version byte (written since I31, never read) *load-bearing*,
and add its complementary file-level MINOR write-gate (I29). Together these are the mechanism by
which Chisel evolves an on-disk page layout *within a MAJOR version* — a new binary reads old pages
correctly, and an old binary refuses to write a file whose layout it doesn't fully understand. This
is built **ahead of need**: there is exactly one page version (`0`) today. The deliverable is the
seam and the conventions, not a framework, so the *first* real layout change is a localized,
checklist-driven diff.

**Companion docs:** `ARCHITECTURE.md` (layer mechanics + on-disk tables), `.codebase-memory/adr.md`
(the *why*; this updates **ADR-7**), `ISSUES.md` (issue log; closes the read half of **I31** and the
write half of **I29**).

**Non-goal (scope guard):** No version-decoder trait/registry, no `PageView` abstraction, no
eager-upgrade sweep, no synthetic shipped "version 1." YAGNI is the governing constraint — with only
version 0 in existence, the entire mechanism must add ~two functions and one open-time branch.

---

## 1. Context

Every non-superblock page carries a one-byte format version in its header (byte 1 for Data, Overflow,
FreeMap, MembershipInterior, MembershipLeaf; byte 2 for HandleTable, whose byte 1 holds the
leaf/interior flag). It is stamped at every `init_page` site as `PAGE_FORMAT_VERSION_CURRENT = 0` and
**read nowhere** — the reader function `page::page_format_version()` was deleted as dead code in PR #46
because no production path dispatched on it. The superblock separately carries a packed
`format_version: u32` (upper 16 = MAJOR, lower 16 = MINOR); the open-time gate compares MAJOR only.
`format_minor()` was likewise deleted in #46.

Two deferred mechanisms make this complete and are designed here together:

- **I31 (forward / read):** a new binary reading an old page must interpret it correctly.
- **I29 (backward / write):** an old binary must not *write* a file whose MINOR exceeds what it
  understands, or it would silently drop fields it can't see.

## 2. The model — what the version byte is *for*

The governing invariant (ADR-7) is **additive-only layout changes within a MAJOR**: new fields land in
reserved padding (the `8..16` common-header region, or type-specific reserved bytes), which is **zero
in every pre-existing page**. Reinterpreting an existing byte offset is *not* additive and requires a
MAJOR bump — out of scope here. This yields two flavors of change:

1. **Zero-default additive** — a new field whose **zero value is its "absent/default" meaning**. An old
   page's zeroed reserved bytes already read correctly as "absent," with no decode logic. **This needs
   no version change at all.** It is not hypothetical: the client byte (ADR-14) is exactly this pattern
   and deliberately kept `FORMAT_MINOR_VERSION = 1` ("byte-identical layout, nothing to gate on"). This
   path stays version-free forever and is untouched by this design.

2. **Version-requiring additive** — a new field where **zero is a legitimate value**, so a reader must
   distinguish "this byte is zero because the field was *absent* (old page)" from "...because the value
   is *genuinely zero* (new page)." **Disambiguating absent-vs-zero is the entire job of the per-page
   version byte.** The decoder reads `if page_version >= K { read field } else { it's absent → real default }`.

So the mechanism is *not* "decode N arbitrary layouts." It is a narrow, precise tool: the absent-vs-zero
disambiguator for additive fields that cannot self-describe through a zero sentinel.

The two gates fall directly out of the additive invariant:

- **Reads are always safe within a MAJOR.** Known fields sit at stable offsets regardless of minor, so
  a too-old binary can *read* a newer file (it reads what it knows, ignores newer reserved fields). The
  per-page version is consumed only where a decoder must apply a default for an absent field.
- **Writes are not safe across minors.** A binary at minor `M` writing a file at minor `M' > M` would
  stamp pages at `M` and could drop the `M'` fields. The **I29 file-MINOR write-gate** prevents this by
  forcing read-only when `file MINOR > binary MINOR`.

## 3. Components (the seam)

### 3.1 `page.rs` helpers (two new, one restored)

- **`pub const fn current_version(page_type: PageType) -> u8`** — the single source of truth for the
  version a freshly-initialized page of each type stamps. Returns `0` for every type today. When (e.g.)
  the data-page layout gains a version-requiring field, *only* its `PageType::Data =>` arm becomes `1`.
  This **retires the shared `PAGE_FORMAT_VERSION_CURRENT` constant** — versions are per-type from here on
  (ADR-7's promise that one page type's layout can evolve without touching others).
- **`pub fn page_format_version(buf: &[u8; PAGE_SIZE]) -> u8`** — resurrected. Dispatches the offset on
  the type tag at `buf[0]`: byte 2 for `HandleTable`, byte 1 for all other non-superblock types. Returns
  the stamped version. This is what a decoder calls to learn which layout a page is in.
- **`pub const fn format_minor(version: u32) -> u16`** — restored (companion to the existing
  `format_major`). Used only by the open-time I29 gate.

### 3.2 Write convention

Every `init_page` site changes `buf[<off>] = PAGE_FORMAT_VERSION_CURRENT` →
`buf[<off>] = page::current_version(PageType::X)`. Sites: `data_page.rs`, `overflow.rs`, `freemap.rs`,
`membership_index.rs` (×N init paths), `handle_table.rs` (byte 2). The version a page is born with now
comes from one function, per type.

### 3.3 Decode convention (dormant until a real version exists)

When a page-type module reaches version `K` with a version-requiring field, its read path adds exactly
one branch: `if page::page_format_version(buf) >= K { read field at offset O } else { default }`. Until
that happens, **no module has any version branch** — the seam is just the helpers plus the write
convention. The spec's §6 worked example is the copy-pasteable template.

### 3.4 The I29 file-MINOR write-gate

At open (`open_existing`), after the existing MAJOR gate:

- `format_minor(sb.format_version) <= FORMAT_MINOR_VERSION` → normal read+write.
- `format_minor(sb.format_version) > FORMAT_MINOR_VERSION` → open **succeeds**, but the engine is forced
  **read-only**, reusing the existing `read_only` flag so any mutating call returns `ReadOnlyMode`.

A version-requiring change bumps **both** the page type's `current_version` arm **and**
`FORMAT_MINOR_VERSION` (one minor bump per release, shared across all page-type changes that release).
The superblock is rewritten on every commit with `FORMAT_VERSION = pack(MAJOR, MINOR)`, so a file's minor
tracks the last binary that committed to it: a new binary opening an old file upgrades the file's minor
on its next commit; an old binary opening a new file is gated to read-only.

## 4. Data flow

- **Open:** `Superblock::select` → MAJOR gate (unchanged: mismatch → `UnsupportedFormatVersion`) → **new**
  MINOR check → force `read_only` if `file MINOR > binary MINOR`.
- **Read (cache miss → `PageCache::load_page`):** **unchanged.** The cache stores raw `[u8; PAGE_SIZE]`
  and validates only the XXH3 checksum; it holds no version logic. Version dispatch lives in the per-module
  decoders via `page_format_version`, and with all types at version 0 there are **zero branches today**, so
  `load_page` does not change. (Deliberately **no `validate_version` gate** at load: within a MAJOR, reads
  are always safe by the additive invariant, and a corrupt version byte is already caught by the
  checksum — a load-time version gate would be machinery for a case the invariant already covers.)
- **Write (mutation → COW):** `init_page` stamps `current_version(type)`. An existing page being mutated is
  COW'd to a fresh page and re-stamped at current → **free lazy upgrade**, no separate migration pass. A
  mutating call while forced-read-only → `ReadOnlyMode`.
- **Commit:** superblock rewritten with `FORMAT_VERSION` → the file's minor advances to the writing
  binary's minor.

Net code touched **today**: two new functions + one restored (`format_minor`), the `init_page` one-liner
swap at each site, and one MINOR branch in `open_existing`. The decode convention is documented but adds
no branches until a real version exists.

## 5. Error handling

Almost entirely reuse — **no new error variants**:

- Open MAJOR mismatch → `UnsupportedFormatVersion { found, expected }` (existing).
- Open with file MINOR > binary MINOR → **not an error**; open succeeds, engine forced read-only; writes
  then return `ReadOnlyMode` (existing).
- An undecodable page cannot arise within the additive-MAJOR invariant; genuine corruption remains
  `CorruptPage` / `ChecksumMismatch`.

**Decided:** reuse `ReadOnlyMode` for the gated case rather than add a distinct error. The operability
nicety — letting a caller distinguish "forced read-only because my binary is too old for this file's
minor" from "opened read-only by request" — is left as a trivial future `read_only_reason`/stats field,
not added speculatively.

## 6. The "how to bump a page format" procedure (the payoff)

To be documented in this spec and ADR-7, so the *first* real change is a checklist:

1. **Is it zero-default additive** (zero == absent == default)? → **no version change at all** (ADR-14
   client-byte path). Stop.
2. **Version-requiring** (zero is a real value that must be disambiguated from absent):
   1. Add the field at a stable offset in a reserved region.
   2. Bump `current_version(PageType::X)`'s arm to `K`.
   3. Bump `FORMAT_MINOR_VERSION` (one bump per release, shared across all page-type changes that release).
   4. In `X`'s read path: `if page_format_version(buf) >= K { read field } else { default }`.
   5. Writes need no change (init stamps current; current-version pages always write the field).
   6. Add the two required tests: an old-version fixture reads the field as its default; a new-version
      fixture reads the value; and an old page upgrades to `K` on COW.
3. **Never reinterpret an existing offset within a MAJOR** — that is a MAJOR bump, outside this mechanism.

**Worked decode template** (the copy-pasteable form of step 2.4), for a hypothetical `foo: u32` added at
data-page version 1 in reserved bytes `[O..O+4]` where `0` is a valid value:

```rust
// In DataPage::read (or the relevant accessor):
let foo = if page::page_format_version(buf) >= 1 {
    u32::from_le_bytes(buf[O..O + 4].try_into().unwrap())
} else {
    FOO_DEFAULT // the field was absent in v0; do NOT read the zeroed reserved bytes as a value
};
```

## 7. Testing

We test everything that exists now; the decode branch is tested when it exists (made mandatory by §6
step 2.6). No fake field is shipped.

- **Helper dispatch** (resurrects the byte-offset tests #46 deleted, now over live code):
  `page_format_version` returns the byte at the correct per-type offset (byte 2 for HandleTable, byte 1
  for the rest); `current_version` returns 0 for all types; a fresh page of each type stamps
  `current_version`.
- **I29 gate (concrete, high-value):** open a db; hand-edit the superblock's `format_version` to a higher
  minor and re-stamp its checksum; reopen → assert the engine is forced read-only and a mutating call
  returns `ReadOnlyMode`; and that an equal-or-older minor opens read-write. Cover both file backings.
- **`format_minor` round-trip** + the `pack_format_version` / `format_major` / `format_minor` algebra.
- **Worked decode example** lives in this spec (not shipped code), so step 2.4 is copy-pasteable.

## 8. Non-goals / deferred

- **Eager-upgrade sweep:** deferred. It slots in later as a defrag-like `&mut self` *bounded* pass that
  COW-rewrites old-version pages (reusing this read-dispatch decode), letting old decoders eventually be
  retired. Not needed until there are ≥2 versions *and* an intent to drop the oldest.
- **MAJOR / reinterpreting changes:** out of scope (additive-minor only).
- **Superblock versioning:** the superblock carries its own packed `format_version` and is not a
  `PageType`, so it is outside this per-page scheme.
- **The ADR-14 zero-default path stays version-free** — untouched by this design.

## 9. ISSUES.md / ADR impact

- Closes the **read** half of **I31** (per-page version dispatch) and the **write** half of **I29**
  (file-MINOR write-gate). The eager-upgrade half of I31 remains deferred (§8).
- **ADR-7** is updated: its "reads dispatch on the version byte / the page-cache load path validates the
  per-page version on every miss" consequence text is corrected (load does *not* validate the version;
  dispatch is per-module and decode-only), and the absent-vs-zero model + the "how to bump" procedure are
  recorded. (The 2026-06-21 review flagged that text as describing an unimplemented mechanism; this design
  is what makes the corrected description true.)
- ARCHITECTURE.md's stale `page::page_format_version()` reference becomes live again.

## 10. Decided questions (resolved during brainstorming)

- **Migration model:** lazy — read-dispatch decode + write-always-current; upgrade is free on the next COW.
  (Rejected: upgrade-in-place-on-read, which fights `&self` reads; eager-only-at-open, which stalls open.)
- **Scope:** both halves — I31 read-dispatch *and* the I29 write-gate — as one coherent minor-versioning
  mechanism.
- **Dispatch structure:** lightweight convention + helpers (rejected: a decoder-trait framework and a
  `PageView` wrapper, both speculative for versions that don't exist).
- **`validate_version` at load:** omitted (the additive invariant + checksum already cover it).
- **Read-only-reason surfacing:** reuse `ReadOnlyMode`; defer a distinct signal.
</content>
