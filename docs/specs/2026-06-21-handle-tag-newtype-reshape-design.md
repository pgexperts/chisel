# `Handle` / `Tag` Newtype Reshape — Design

**Status:** Approved 2026-06-21 (brainstorming). Pre-implementation.

**Goal:** Replace the primitive-obsessed public surface — handles as raw `u64`, tags as raw `u32`,
"no tag" as the in-band sentinel `0` — with opaque `Handle` / `Tag` newtypes at the API skin, so
that `delete_tagged(handle, tag)`'s two integer arguments become un-transposable, "untagged" becomes
expressible in the type system (`Option<Tag>` / `Tag(NonZeroU32)`), and the misleading
`DefragOptions::max_pages` name is corrected. This closes **I120** (newtype), **I126** (the tag-`0`
sentinel overload), and **I122** (the `max_pages` → `max_values` rename) in one coherent change.

**Companion docs:** `ISSUES.md` (closes I120, I126, I122), `ARCHITECTURE.md` (public-API surface),
`docs/specs/2026-06-05-client-byte-design.md` and `docs/specs/2026-06-02-chunk-tags-design.md` (the
tag/client-byte features being reshaped).

**Non-goal (scope guard):** This is a **skin reshape**, not an engine change. No on-disk format byte
moves. The handle-table radix math, the `HandleEntry` layout, the membership-index keys, and
`next_handle` all stay raw integers. No `Handle`/`Tag` type penetrates below `lib.rs` / `python/src`.
No streaming-iterator variant (that is the orthogonal I97/I100). No `ClientByte` newtype. No Python
`Handle`/`Tag` wrapper classes. No new `ChiselError` variant.

---

## 1. Context

The public surface (`Chisel` in `src/lib.rs`, delegating to `TransactionManager` in
`src/transaction.rs`) exposes its most-used values as bare primitives:

- **Handles** are `u64`, minted monotonically from `next_handle` (starts at 1; `0` is the reserved
  "no handle" sentinel). Returned by `allocate*`, accepted by `read`/`update`/`delete`/`tag`/
  `client_byte`/`set_client_byte`/`set_root_name`/`delete_many`, enumerated by `handles`/
  `handles_with_tag`/`get_root_name`/`delete_with_tag`.
- **Tags** are `u32`. `delete_tagged(handle: u64, tag: u32)` can be called with its two integer
  arguments transposed and the compiler will not object.
- **Tag `0`** doubles as a legal value *and* the "untagged" sentinel. The rule "the membership index
  is not updated for tag 0" is enforced by two bare `if tag != 0` guards in `transaction.rs`
  (allocate at ~line 1546, delete at ~line 1996) and documented only in prose. `tag()` returns `0`
  for untagged handles; `allocate_tagged(value, 0)` silently stores an untagged value.
- **`DefragOptions::max_pages`** counts *values relocated*, not pages — a name carried over from
  pre-R3 defrag and papered over by a "DESPITE THE NAME" comment plus a Python `__init__.py` note.

The **structural floor** (confirmed by inventory; must NOT change):

- `HandleEntry` is a 16-byte on-disk record: `page_id: u64`, `slot_index: u16`, `flags`,
  `tag: u32` at bytes `[11..15)`, `client_byte: u8` at byte `[15]`.
- Handles index the handle-table radix tree by arithmetic: `handle % ENTRIES_PER_LEAF`,
  `handle / child_span`, `handle >= capacity()`.
- The membership index uses the `u32` tag (cast to `u64`) as an outer-tree key and the `u64` handle
  as an inner-tree key; neither is stored as a payload, only as radix structure.

Therefore the newtypes can only live at the **public boundary**. The engine continues to traffic in
raw integers; `lib.rs` and `python/src` convert at the edge. This is both the lazy-correct choice and
the architecturally-forced one — and the boundary is exactly where the transposition hazard lives.

## 2. The model — opaque IDs with pragmatic ergonomics

Two newtypes, defined in a new public-layer module `src/handle.rs`, re-exported from the crate root.
They are deliberately **not** the maximally-strict form: per the brainstorming decision they carry
`PartialEq` against their raw primitive and `Display`, so the bulk of the ~485 existing
test/bench call sites — which compare (`assert_eq!(h, 1)`) and print handles — keep compiling.
Transposition is still blocked, because `Handle` and `Tag` are distinct types.

```rust
/// A stable, opaque chunk handle. Newtype over the engine's `u64` handle id.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(u64);

impl Handle {
    #[must_use] pub const fn get(self) -> u64 { self.0 }
}
impl From<u64> for Handle { fn from(v: u64) -> Self { Handle(v) } }
impl From<Handle> for u64 { fn from(h: Handle) -> Self { h.0 } }
impl PartialEq<u64> for Handle { /* self.0 == *other */ }
impl PartialEq<Handle> for u64 { /* *self == other.0 */ }   // both directions for assert_eq!
impl std::fmt::Display for Handle { /* writes self.0 */ }
```

```rust
/// A non-zero chunk tag. "No tag" is the ABSENCE of a `Tag` (`Option<Tag>`), never `Tag(0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag(NonZeroU32);

impl Tag {
    /// `None` if `v == 0`. Mirrors `NonZeroU32::new`.
    #[must_use] pub const fn new(v: u32) -> Option<Tag> { /* NonZeroU32::new(v).map(Tag) */ }
    #[must_use] pub const fn get(self) -> u32 { self.0.get() }
}
impl TryFrom<u32> for Tag { type Error = /* ZeroTag ZST */; /* Err on 0 */ }
impl From<Tag> for u32 { fn from(t: Tag) -> Self { t.0.get() } }
impl PartialEq<u32> for Tag { /* self.get() == *other */ }
impl PartialEq<Tag> for u32 { /* *self == other.get() */ }
impl std::fmt::Display for Tag { /* writes self.get() */ }
```

Design notes:

- **`#[repr(transparent)]` on `Handle`** keeps the engine equivalence-bench adapter's existing
  zero-copy `&[u64]` ↔ `&[Handle]` reinterpretation sound (`bench/src/chisel_engine.rs` already does
  a transmute-style cast for `delete_many` to avoid a `Vec` allocation). `Tag` is not `repr(transparent)`
  and needs no such guarantee.
- **No `From<u32> for Tag`** — construction from a raw `u32` is fallible (`Tag::new -> Option`,
  `TryFrom`). This is the irreducible friction that makes `Tag(0)` unconstructable; tagged-*input*
  call sites pay it (`Tag::new(42).unwrap()`), which is the point of I126.
- **No new `ChiselError` variant.** Every Rust tag-taking method receives an already-valid `Tag`, so
  there is no in-method fallible tag path. The only `0`-rejection points are `Tag::new`/`TryFrom`
  (construction) and the Python boundary (below).
- **`ZeroTag`** is a tiny zero-size error type for `TryFrom<u32>`'s `Error` associated type, local to
  `handle.rs` (it does not enter `ChiselError`). If a `TryFrom` error type proves unergonomic, the
  fallback is to expose only `Tag::new -> Option` and drop the `TryFrom` impl — `Tag::new` is the
  primary constructor regardless.

## 3. Public `Chisel` surface (lib.rs) — convert at the edge, engine unchanged

`TransactionManager`'s signatures stay `u64`/`u32`. `Chisel`'s methods newtype the boundary and
delegate. The complete reshaped surface:

| Method (after) | Engine delegation (unchanged signature) |
|---|---|
| `allocate(&mut, value) -> Result<Handle>` | `txm.allocate(value).map(Handle::from)` |
| `allocate_tagged(&mut, value, tag: Tag) -> Result<Handle>` | `txm.allocate_tagged(value, tag.get()).map(Handle::from)` |
| `tag(&, h: Handle) -> Result<Option<Tag>>` | `txm.tag(h.get()).map(Tag::new)`  (`0 → None`) |
| `client_byte(&, h: Handle) -> Result<u8>` | `txm.client_byte(h.get())` |
| `set_client_byte(&mut, h: Handle, byte: u8) -> Result<()>` | `txm.set_client_byte(h.get(), byte)` |
| `handles_with_tag(&, tag: Tag) -> Result<Vec<Handle>>` | map-collect `Handle::from` |
| `read(&, h: Handle) -> Result<Vec<u8>>` | `txm.read(h.get())` |
| `update(&mut, h: Handle, value) -> Result<()>` | `txm.update(h.get(), value)` |
| `delete(&mut, h: Handle) -> Result<()>` | `txm.delete(h.get())` |
| `delete_tagged(&mut, h: Handle, tag: Tag) -> Result<()>` | `txm.delete_tagged(h.get(), tag.get())` — **un-transposable** |
| `delete_with_tag(&mut, tag: Tag, max) -> Result<TagDropProgress>` | wrap (see §4) |
| `delete_many(&mut, handles: &[Handle]) -> Result<()>` | reborrow `&[u64]` (see below) |
| `handles(&) -> Result<Vec<Handle>>` | map-collect `Handle::from` |
| `set_root_name(&mut, name, h: Handle) -> Result<()>` | `txm.set_root_name(name, h.get())` |
| `get_root_name(&, name) -> Result<Option<Handle>>` | `.map(|o| o.map(Handle::from))` |

- `client_byte` / `set_client_byte` newtype only the **handle** argument; the `u8` byte is unchanged
  (client byte is genuinely uninterpreted; `0` is a default, not a functional sentinel; no membership
  coupling).
- **`delete_many(&[Handle])`**: because `Handle` is `#[repr(transparent)]` over `u64`, the slice can
  be reinterpreted as `&[u64]` without a copy. The conversion lives in `lib.rs` behind a single
  documented `// SAFETY: Handle is #[repr(transparent)] over u64` cast, OR — simpler and copy-cheap —
  `handles.iter().map(|h| h.get()).collect::<Vec<u64>>()`. Default to the safe collect unless a bench
  shows it matters; bulk delete is far below the fsync floor either way.
- `handle_live_page_id` (used internally by `defrag.rs`) is `pub` on `TransactionManager` but is **not**
  exposed on `Chisel`; it stays `u64` and is untouched.

## 4. `TagDropProgress` relocation

`TagDropProgress { deleted: Vec<u64>, complete: bool }` is currently defined in
`src/membership_index.rs` and re-exported (`pub use membership_index::TagDropProgress`). For a
consistent public surface its `deleted` field should be `Vec<Handle>` — but `membership_index` is an
engine layer that must not know `Handle`. Resolution:

- **Move the struct to `src/handle.rs`** as `pub struct TagDropProgress { pub deleted: Vec<Handle>,
  pub complete: bool }`, preserving its `#[must_use]` attribute and doc comment.
- The engine's `TransactionManager::delete_with_tag` (and `delete_with_tag_inner`) returns a plain
  `(Vec<u64>, bool)` instead of constructing the struct.
- `Chisel::delete_with_tag` wraps: `let (ids, complete) = self.txm.delete_with_tag(tag.get(), max)?;
  Ok(TagDropProgress { deleted: ids.into_iter().map(Handle::from).collect(), complete })`.
- `lib.rs` re-export changes from `membership_index::TagDropProgress` to `handle::TagDropProgress`.
- `membership_index.rs` drops the struct definition and its `#[must_use]`.

## 5. Tag-`0` semantics (I126)

The on-disk and engine representation is unchanged: the `HandleEntry.tag` field stays `u32` with `0`
meaning untagged, and the two `if tag != 0` membership guards in `transaction.rs` stay exactly as they
are (they operate on the storage-level representation). The newtype boundary is the only thing that
changes:

- "No tag" is expressed by calling `allocate()` (not `allocate_tagged()`); `allocate_tagged` requires
  a real `Tag` and therefore cannot be called with `0`.
- `tag(handle)` returns `Option<Tag>`: stored `0 → None`, `n → Some(Tag(n))`.
- This is the I125-adjacent quirk boundary too (`tag()` currently reads through a tombstone). I125 is
  **not** in scope here; `tag()` keeps its current read-through behavior, now returning `Option<Tag>`.
  Document the unchanged behavior; do not fix I125 in this PR.

## 6. Python boundary (PyO3 + stubs) — handles/tags stay plain `int`

Per the brainstorming decision, Python does **not** get wrapper classes — handles remain `int`, tags
`int` / `int | None`. The reshape surfaces only as the I126 semantics change and the I122 rename:

- **`python/src/db.rs` + `python/src/transaction.rs`:** the pyo3 methods keep `u64`/`u32` parameters
  (PyO3 marshals Python `int` ↔ `u64`/`u32` directly). For tag-taking methods, validate at the edge:
  `let tag = NonZeroU32::new(tag).ok_or_else(|| PyValueError::new_err("tag must be non-zero"))?;`
  then call the Rust core with `Tag(tag)`. For `tag()`, map `Option<Tag>` → `Option<u32>` → Python
  `int | None`.
- Affected pyo3 methods: `allocate_tagged`, `delete_tagged`, `handles_with_tag`, `delete_with_tag`
  (raise `ValueError` on tag `0`); `tag` (returns `int | None`). Handle-only methods are unchanged at
  the Python boundary (still `int`).
- **`python/chisel/chisel.pyi`:** `def tag(self, handle: int) -> int | None: ...` (both `Chisel` and
  `Transaction`). All other handle/tag stubs stay `int`/`list[int]`. Update docstrings noting tag `0`
  raises `ValueError`.
- No Python test relies on `tag() == 0` or passes `tag=0` (confirmed by inventory), so this is safe.

## 7. I122 rename `max_pages` → `max_values`

A clean break — `DefragOptions` is `#[non_exhaustive]`, pre-1.0, no production users.

- **Rust (`src/defrag.rs`):** rename the field, the builder method `max_pages()` → `max_values()`,
  the `Default` initializer, and the one consumer at `defrag.rs:209`. Delete the "DESPITE THE NAME"
  comment; the name is now honest.
- **Python (`python/chisel/__init__.py`):** rename the `DefragOptions` dataclass field and rewrite the
  legacy-name docstring to plainly describe `max_values`.
- **Python (`python/src/db.rs`):** the duck-typed `obj.getattr("max_pages")` becomes
  `getattr("max_values")`.
- **`python/chisel/chisel.pyi`:** rename the field stub.

## 8. Testing & rollout

This is one PR. The engine internals do not move, so the diff is: one new module, a skin reshape in
`lib.rs`, a small `TagDropProgress` relocation, the Python-edge validation, the `max_values` rename,
and a mechanical call-site sweep.

- **New unit tests in `src/handle.rs`:** `Handle` round-trips `From<u64>`/`into u64`/`.get()`;
  `PartialEq<u64>` both directions; `Display`; `Tag::new(0) == None`; `Tag::new(n).unwrap().get() == n`;
  `Tag::try_from(0)` errors; `Tag` `PartialEq<u32>` and `Display`.
- **New behavior tests:** `tag()` of an untagged handle returns `None`; a tagged handle returns
  `Some(Tag)`; `delete_tagged` still rejects a mismatched tag; `DefragOptions::max_values` bounds a
  pass (port the existing `max_pages` test).
- **New Python tests:** `allocate_tagged(b"x", 0)` raises `ValueError`; `tag()` of an untagged handle
  returns `None`; `DefragOptions(max_values=...)` works.
- **Mechanical sweep:** ~485 Rust call sites (tests/bench/recovery) + ~25 Python. Most handle flows are
  untouched (`Handle → Handle`). Real edits cluster at: tagged-input construction (`Tag::new(..).unwrap()`),
  the `Vec<u64>`/`HashMap<u64>` handle collections (4 + 2 sites), the one `committed_handle + 1`
  arithmetic site (`(committed_handle.get() + 1)` or `Handle::from(committed_handle.get() + 1)`), the
  `bench/src/chisel_engine.rs` adapter (`Identifier(u64)` / `delete_many` cast), and `tests/defrag.rs`
  (`max_pages` → `max_values`).
- **Gate (all green before any PR):** `cargo test` (full suite, not `--lib` — integration tests in
  `tests/` are where most call sites live), `cargo clippy --all-targets -- -D warnings` (the Python
  subcrate is excluded from `default-members`, so also `cargo clippy -p chisel-py --all-targets`),
  and the Python `pytest` suite via the maturin-built extension.

## 9. Issue closure

- **I120** — `Handle`/`Tag` newtypes land; `delete_tagged` is un-transposable; `handles()` returns
  `Vec<Handle>`.
- **I126** — `Tag(NonZeroU32)` + `Option<Tag>`; tag `0` is unconstructable; Python raises on `0`.
- **I122** — `max_pages` → `max_values` across Rust + Python.

Mark all three resolved in `ISSUES.md` with the PR reference once merged. I124 (`# Errors`/`# Panics`
rustdoc) is explicitly deferred to *after* this reshape so it documents the final signatures (per the
ISSUES.md handoff note).
