// src/handle.rs — public newtypes for the Chisel API surface.
//
// Role in system: the public boundary's value types. `Handle` and `Tag` wrap
// the engine's raw `u64`/`u32` ids so the public API is not primitive-obsessed
// — `delete_tagged(Handle, Tag)` cannot be called with its arguments
// transposed, and "no tag" is the absence of a `Tag` (`Option<Tag>`), never the
// in-band sentinel `0`. These types live ABOVE the engine: nothing in
// transaction.rs / handle_table.rs / membership_index.rs knows about them;
// lib.rs converts at the edge via `.get()` / `From`. The on-disk format and the
// radix-tree key arithmetic stay raw integers (ISSUES.md I120/I126).
//
// Ergonomics decision (ISSUES.md I120): the newtypes carry `PartialEq` against
// their raw primitive and `Display`, so existing call sites that compare or
// print ids keep compiling. Distinctness — not opacity of comparison — is what
// blocks transposition.

use std::fmt;
use std::num::NonZeroU32;

/// A stable, opaque chunk handle. Newtype over the engine's `u64` id.
///
/// `#[repr(transparent)]` is load-bearing, not decoration: the bench adapter
/// reinterprets a `&[u64]` slice as `&[Handle]` without copying, which is sound
/// only because `Handle` has identical layout to `u64`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(u64);

impl Handle {
    /// The underlying raw id. Handles are minted from `1` (the engine reserves
    /// `0` as the "no handle" sentinel); this carrier type does not itself
    /// enforce non-zero.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for Handle {
    fn from(v: u64) -> Self {
        Handle(v)
    }
}

impl From<Handle> for u64 {
    fn from(h: Handle) -> Self {
        h.0
    }
}

// PartialEq against the raw primitive in both directions so `assert_eq!(h, 1)`
// and `assert_eq!(1, h)` both compile. The integer literal infers as `u64`
// because that is the only integer type `Handle` compares against.
impl PartialEq<u64> for Handle {
    fn eq(&self, other: &u64) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Handle> for u64 {
    fn eq(&self, other: &Handle) -> bool {
        *self == other.0
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A non-zero chunk tag. "No tag" is the ABSENCE of a `Tag` (`Option<Tag>`) —
/// `Tag(0)` is unconstructable, which makes the untagged sentinel expressible
/// in the type (ISSUES.md I126). The engine still stores the tag as a `u32`
/// with `0` meaning untagged; the `0 <-> None` mapping happens at the lib.rs
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag(NonZeroU32);

impl Tag {
    /// Construct a `Tag`, returning `None` if `v == 0`. Mirrors
    /// `NonZeroU32::new`.
    #[must_use]
    pub const fn new(v: u32) -> Option<Tag> {
        match NonZeroU32::new(v) {
            Some(nz) => Some(Tag(nz)),
            None => None,
        }
    }

    /// The underlying raw tag value (always `>= 1`).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Error from `Tag::try_from(0)`. Zero-size; deliberately NOT a `ChiselError`
/// variant — tag construction is fallible only at the API boundary, never
/// inside the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroTagError;

impl fmt::Display for ZeroTagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tag must be non-zero (0 is the untagged sentinel)")
    }
}

impl std::error::Error for ZeroTagError {}

impl TryFrom<u32> for Tag {
    type Error = ZeroTagError;
    fn try_from(v: u32) -> Result<Tag, ZeroTagError> {
        Tag::new(v).ok_or(ZeroTagError)
    }
}

impl From<Tag> for u32 {
    fn from(t: Tag) -> Self {
        t.0.get()
    }
}

impl PartialEq<u32> for Tag {
    fn eq(&self, other: &u32) -> bool {
        self.get() == *other
    }
}

impl PartialEq<Tag> for u32 {
    fn eq(&self, other: &Tag) -> bool {
        *self == other.get()
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

/// Progress report from `Chisel::delete_with_tag`. `deleted` lists the handles
/// removed in this pass (the engine deletes their chunks); `complete` is `true`
/// when the tag has no remaining members. Produced only on the success path —
/// a mid-pass error returns `Err` and no progress (the per-delete state stays
/// consistent; only the reporting is lost).
//
// Lives here, above the engine, because its `deleted` field is `Vec<Handle>` —
// a public-surface type. The engine's `delete_with_tag` returns a raw
// `(Vec<u64>, bool)`; `Chisel::delete_with_tag` wraps it into this.
// #[must_use] (ISSUES.md I121): `complete` is the field a resumable drop loops on.
#[must_use = "TagDropProgress.complete tells you whether the tag drop finished; loop on it or bind with `let _`"]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TagDropProgress {
    /// Handles removed from the index in this pass. May be fewer than `max` if
    /// the tag emptied first.
    pub deleted: Vec<Handle>,
    /// `true` if the tag has no remaining members after this pass.
    pub complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_round_trips_u64() {
        let h = Handle::from(42);
        assert_eq!(h.get(), 42);
        assert_eq!(u64::from(h), 42);
        assert_eq!(h, 42u64); // PartialEq<u64>
        assert_eq!(42u64, h); // reverse direction
    }

    #[test]
    fn handle_displays_as_raw() {
        assert_eq!(format!("{}", Handle::from(7)), "7");
    }

    #[test]
    fn tag_new_rejects_zero() {
        assert_eq!(Tag::new(0), None);
        assert_eq!(Tag::new(5).unwrap().get(), 5);
    }

    #[test]
    fn tag_try_from_zero_errors() {
        assert!(Tag::try_from(0).is_err());
        assert_eq!(Tag::try_from(9).unwrap().get(), 9);
    }

    #[test]
    fn tag_eq_and_display() {
        let t = Tag::new(3).unwrap();
        assert_eq!(t, 3u32);
        assert_eq!(3u32, t);
        assert_eq!(format!("{t}"), "3");
        assert_eq!(u32::from(t), 3);
    }
}
