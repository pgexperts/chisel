// types.rs — FFI value types: records/enums UniFFI passes by value, the
// custom_type! bridges for the engine's Handle/Tag newtypes, and the
// conversions to/from the corresponding chisel:: engine types.
//
// Role in system: this is the ONLY place that knows both the engine's value
// shapes (chisel::Options, chisel::Stats, ...) and the UniFFI-exported
// shapes (Options, Stats, ...) below. database.rs (Task 5) consumes these
// exclusively through the `From`/`into_engine` conversions here rather than
// touching engine types directly, so a future engine field rename is a
// one-file fix.
//
// Naming collision is deliberate: `Options`/`Stats`/... shadow the engine's
// same-named types within this crate. Call sites disambiguate via the
// `chisel::` prefix on the engine side; UniFFI-generated Swift never sees
// the engine type at all.
use crate::error::ChiselError;
// custom_type! parses its first argument as a single-segment ident (it needs
// a bare name to generate the FFI type name string), so `chisel::Handle` /
// `chisel::Tag` must be in scope unqualified here. The trait impls the macro
// generates attach to the real resolved type either way, so `chisel::Handle`
// (fully qualified) elsewhere in this crate is still the identical bridged
// type.
use chisel::{Handle, Tag};

// `remote` is required, not optional decoration: Handle/Tag are defined in
// the `chisel` crate, and Rust's orphan rule blocks a blanket
// `impl<UT> FfiConverter<UT> for Handle` for a foreign type from this crate.
// `remote` switches the macro to implement the trait only for this crate's
// own (local) UniFfiTag, which orphan-checks fine — see
// uniffi_macros::custom::expand_custom_type's `is_remote()` branch.
//
// Handle bridges as a plain u64. The engine's `From<u64> for Handle` gives a
// blanket infallible `TryFrom<u64>` (std's `T: From<U> => U: TryInto<T>`
// impl), so the custom_type! shorthand's generated `try_into()` lift and
// `into()` lower both resolve without an explicit closure.
uniffi::custom_type!(Handle, u64, { remote });

// Tag bridges as u32. try_lift rejects 0 via the engine's real
// `TryFrom<u32> for Tag` (Err(ZeroTagError) on zero) — the shorthand's
// generated `try_into()` calls exactly that impl, so "0 is untagged" is
// enforced by the engine's own type, not reimplemented here.
uniffi::custom_type!(Tag, u32, { remote });

#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum DrainInsertion {
    LruTail,
    Mru,
}

impl From<DrainInsertion> for chisel::DrainInsertion {
    fn from(d: DrainInsertion) -> Self {
        match d {
            DrainInsertion::LruTail => chisel::DrainInsertion::LruTail,
            DrainInsertion::Mru => chisel::DrainInsertion::Mru,
        }
    }
}
impl From<chisel::DrainInsertion> for DrainInsertion {
    fn from(d: chisel::DrainInsertion) -> Self {
        match d {
            chisel::DrainInsertion::LruTail => DrainInsertion::LruTail,
            chisel::DrainInsertion::Mru => DrainInsertion::Mru,
            // chisel::DrainInsertion is #[non_exhaustive]; a future third
            // policy must fail closed to the documented default rather than
            // fail to compile here.
            _ => DrainInsertion::LruTail,
        }
    }
}

// No `Debug` derive and no `Clone`, both deliberate.
//
// `Debug` is hand-written below to redact the payload, mirroring the engine's
// own `impl Debug for crypto::Key` (src/crypto/mod.rs). Deriving it printed
// the plaintext secret, directly against the discipline the surrounding code
// keeps: `into_engine` wraps everything in `Zeroizing`, and the root manifest
// pins `zeroize` specifically to wipe key material on drop.
//
// `Clone` is dropped because a cloned `ChiselKey` holds a plain `Vec<u8>` /
// `String` that is dropped WITHOUT wiping — the same leak in the other
// direction. No call site needs it: every consumer takes the value and
// `into_engine(self)` consumes it.
#[derive(uniffi::Enum)]
pub enum ChiselKey {
    Raw { bytes: Vec<u8> },
    Passphrase { phrase: String },
}

impl std::fmt::Debug for ChiselKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The variant name is safe and useful (it says which credential KIND
        // was supplied); the payload never is. Byte length is deliberately
        // omitted too — key length is a fingerprint.
        match self {
            ChiselKey::Raw { .. } => f.write_str("ChiselKey::Raw(<redacted>)"),
            ChiselKey::Passphrase { .. } => f.write_str("ChiselKey::Passphrase(<redacted>)"),
        }
    }
}

impl ChiselKey {
    // chisel::crypto::Key is pub(crate) in the engine — chisel::Key is the
    // public re-export (src/lib.rs) and the only path reachable from here.
    //
    // Caller: Options::into_engine's encryption_key branch below, which
    // database.rs's NativeDatabase::new invokes via Chisel::open.
    pub(crate) fn into_engine(self) -> chisel::Key {
        use zeroize::Zeroizing;
        match self {
            ChiselKey::Raw { bytes } => chisel::Key::Raw(Zeroizing::new(bytes)),
            ChiselKey::Passphrase { phrase } => chisel::Key::Passphrase(Zeroizing::new(phrase)),
        }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Argon2Params {
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}
impl From<Argon2Params> for chisel::Argon2Params {
    fn from(p: Argon2Params) -> Self {
        // chisel::Argon2Params is a plain (non-#[non_exhaustive]) struct with
        // public fields, so a literal is the direct construction path.
        chisel::Argon2Params {
            m_cost: p.m_cost,
            t_cost: p.t_cost,
            p_cost: p.p_cost,
        }
    }
}

// `Debug` is hand-written below rather than derived: `Options` embeds
// `Option<ChiselKey>`, so a derived impl is only as safe as its fields and a
// future field could reintroduce the leak silently. Spelling it out makes the
// redaction a property of this type rather than an accident of its members.
//
// `Clone` is dropped for the same reason it left `ChiselKey`: a derived
// `Clone` on `Options` requires `ChiselKey: Clone`, and a cloned key holds an
// unwiped `Vec<u8>`/`String` copy of the secret. Nothing clones `Options`
// either — `into_engine(self)` consumes it and `NativeDatabase::new` takes it
// by value.
#[derive(uniffi::Record)]
pub struct Options {
    #[uniffi(default = 8388608)]
    pub cache_max_bytes: u64,
    #[uniffi(default = None)]
    pub spillway_max_bytes: Option<u64>,
    // No #[uniffi(default = ...)] here: UniFFI's record-default grammar
    // (uniffi_macros::default) only accepts literals / None / Some(...) / [],
    // not a bare enum-variant path like `LruTail` — that fails to parse as a
    // proc-macro attribute. Swift callers pass this field explicitly.
    pub drain_insertion: DrainInsertion,
    #[uniffi(default = true)]
    pub create_if_missing: bool,
    #[uniffi(default = false)]
    pub read_only: bool,
    #[uniffi(default = 2)]
    pub superblock_count: u32,
    #[uniffi(default = None)]
    pub encryption_key: Option<ChiselKey>,
    #[uniffi(default = None)]
    pub argon2_params: Option<Argon2Params>,
}

impl Options {
    // Test helper mirroring the UniFFI field defaults above (kept in sync by
    // hand; a drift here only weakens this test, it can't silently change
    // Swift-facing behavior since the #[uniffi(default...)] values are what
    // actually generate the Swift initializer).
    #[cfg(test)]
    pub(crate) fn default_ffi() -> Self {
        Options {
            cache_max_bytes: 8388608,
            spillway_max_bytes: None,
            drain_insertion: DrainInsertion::LruTail,
            create_if_missing: true,
            read_only: false,
            superblock_count: 2,
            encryption_key: None,
            argon2_params: None,
        }
    }

    /// Fold into the engine's `#[non_exhaustive]` `Options` builder via its
    /// chained setters — a struct literal is not available to this crate.
    /// Returns `Result` (rather than `chisel::Options` directly) to match
    /// `ChiselError` as the crate's one FFI-facing error type; there is no
    /// fallible step today (`chisel::Options::open` does its own
    /// `InvalidSuperblockCount` validation), but `database.rs`'s
    /// `NativeDatabase::new` uses `?` against this the same way it does
    /// every other engine call.
    pub(crate) fn into_engine(self) -> Result<chisel::Options, ChiselError> {
        let mut o = chisel::Options::default()
            .cache_max_bytes(self.cache_max_bytes)
            .drain_insertion(self.drain_insertion.into())
            .create_if_missing(self.create_if_missing)
            .read_only(self.read_only)
            .superblock_count(self.superblock_count);
        if let Some(s) = self.spillway_max_bytes {
            o = o.spillway_max_bytes(s);
        }
        if let Some(k) = self.encryption_key {
            o = o.encryption_key(k.into_engine());
        }
        if let Some(a) = self.argon2_params {
            o = o.argon2_params(a.into());
        }
        Ok(o)
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("cache_max_bytes", &self.cache_max_bytes)
            .field("spillway_max_bytes", &self.spillway_max_bytes)
            .field("drain_insertion", &self.drain_insertion)
            .field("create_if_missing", &self.create_if_missing)
            .field("read_only", &self.read_only)
            .field("superblock_count", &self.superblock_count)
            // Presence is operationally useful ("was this DB opened
            // encrypted?"); the credential itself is not printable.
            .field(
                "encryption_key",
                &self.encryption_key.as_ref().map(|_| "<redacted>"),
            )
            .field("argon2_params", &self.argon2_params)
            .finish()
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Stats {
    pub handle_count: u64,
    pub total_pages: u64,
    pub file_size_bytes: u64,
    pub spillway_logical_bytes: Option<u64>,
    pub spillway_max_bytes: Option<u64>,
}
impl From<chisel::Stats> for Stats {
    fn from(s: chisel::Stats) -> Self {
        Stats {
            handle_count: s.handle_count,
            total_pages: s.total_pages,
            file_size_bytes: s.file_size_bytes,
            spillway_logical_bytes: s.spillway_logical_bytes,
            spillway_max_bytes: s.spillway_max_bytes,
        }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Counters {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub pages_allocated: u64,
    pub fsync_calls: u64,
}
impl From<chisel::ChiselCounters> for Counters {
    fn from(c: chisel::ChiselCounters) -> Self {
        Counters {
            cache_hits: c.cache_hits,
            cache_misses: c.cache_misses,
            pages_allocated: c.pages_allocated,
            fsync_calls: c.fsync_calls,
        }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct DefragOptions {
    #[uniffi(default = 0.25)]
    pub sparse_threshold: f64,
    #[uniffi(default = 0)]
    pub max_values: u64, // usize never crosses the FFI boundary
}
impl From<DefragOptions> for chisel::DefragOptions {
    fn from(o: DefragOptions) -> Self {
        // chisel::DefragOptions is #[non_exhaustive]: a struct literal is
        // rejected from outside its crate even with every field named, so
        // this goes through its chained setters like Options above.
        chisel::DefragOptions::default()
            .sparse_threshold(o.sparse_threshold)
            .max_values(o.max_values as usize)
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct DefragStats {
    pub pages_examined: u64,
    pub pages_freed: u64,
    pub values_moved: u64,
    pub freemap_orphans_reclaimed: u64,
}
impl From<chisel::DefragStats> for DefragStats {
    fn from(s: chisel::DefragStats) -> Self {
        DefragStats {
            pages_examined: s.pages_examined,
            pages_freed: s.pages_freed,
            values_moved: s.values_moved,
            freemap_orphans_reclaimed: s.freemap_orphans_reclaimed,
        }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct TagDropProgress {
    pub deleted: Vec<chisel::Handle>,
    pub complete: bool,
}
impl From<chisel::TagDropProgress> for TagDropProgress {
    fn from(p: chisel::TagDropProgress) -> Self {
        TagDropProgress {
            deleted: p.deleted,
            complete: p.complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_maps_to_engine_defaults() {
        let o = Options::default_ffi();
        let e = o.into_engine().expect("valid options");
        // 8 MiB default cache, mirroring chisel::Options::default(). We can't
        // read private fields, so just assert the fold-in succeeds; the
        // resulting Options is exercised end-to-end in database.rs tests.
        let _ = e;
    }

    #[test]
    fn drain_insertion_round_trips() {
        assert_eq!(
            chisel::DrainInsertion::from(DrainInsertion::Mru),
            chisel::DrainInsertion::Mru
        );
        assert_eq!(
            DrainInsertion::from(chisel::DrainInsertion::Mru),
            DrainInsertion::Mru
        );
    }

    #[test]
    fn chisel_key_raw_and_passphrase_convert() {
        match (ChiselKey::Raw {
            bytes: vec![1, 2, 3],
        })
        .into_engine()
        {
            chisel::Key::Raw(b) => assert_eq!(&*b, &[1, 2, 3]),
            chisel::Key::Passphrase(_) => panic!("expected Raw"),
        }
        match (ChiselKey::Passphrase {
            phrase: "s3cret".into(),
        })
        .into_engine()
        {
            chisel::Key::Passphrase(p) => assert_eq!(&*p, "s3cret"),
            chisel::Key::Raw(_) => panic!("expected Passphrase"),
        }
    }

    #[test]
    fn argon2_params_convert() {
        let p = Argon2Params {
            m_cost: 1,
            t_cost: 2,
            p_cost: 3,
        };
        let e: chisel::Argon2Params = p.into();
        assert_eq!(e.m_cost, 1);
        assert_eq!(e.t_cost, 2);
        assert_eq!(e.p_cost, 3);
    }

    // Exercises the u64 -> usize cast in DefragOptions::from at both ends of
    // its range: 0 (no-op limit) and a large value that only differs from
    // its usize form if the cast silently truncated (e.g. on a 32-bit
    // usize). The prior version of this test used max_values: 42, which
    // isn't a boundary of anything.
    #[test]
    fn defrag_options_convert_usize_boundary() {
        let zero = DefragOptions {
            sparse_threshold: 0.5,
            max_values: 0,
        };
        let e: chisel::DefragOptions = zero.into();
        assert_eq!(e.sparse_threshold, 0.5);
        assert_eq!(e.max_values, 0usize);

        let large = DefragOptions {
            sparse_threshold: 0.5,
            max_values: 1_000_000,
        };
        let e: chisel::DefragOptions = large.into();
        assert_eq!(e.max_values, 1_000_000usize);
    }

    // No unit test for TagDropProgress::from: chisel::TagDropProgress is
    // #[non_exhaustive], so it cannot be constructed with a struct literal
    // from this crate. The 1:1 field copy is exercised whenever
    // delete_with_tag returns a real engine value (database.rs).

    #[test]
    fn stats_and_counters_convert_from_engine() {
        let db = chisel::Chisel::open_in_memory().expect("open");

        let s = db.stats().expect("stats");
        let ffi_stats: Stats = s.into();
        assert_eq!(ffi_stats.handle_count, 0);

        let c = db.counters().expect("counters");
        let ffi_counters: Counters = c.into();
        assert_eq!(ffi_counters.cache_hits, 0);
    }

    #[test]
    fn handle_and_tag_round_trip_through_engine_conversions() {
        let h = chisel::Handle::from(42u64);
        assert_eq!(u64::from(h), 42);

        let t = chisel::Tag::new(7).expect("nonzero");
        assert_eq!(u32::from(t), 7);
        assert!(chisel::Tag::new(0).is_none());
    }

    // The engine hand-writes Debug for crypto::Key to print `<redacted>`;
    // this crate derived it, so `{:?}` on a ChiselKey — or on any struct
    // embedding one, which Options does — printed the user's passphrase
    // verbatim. Nothing formats these today, which is what makes it a
    // standing trap rather than a live leak: any future tracing::debug!(?o),
    // assert_eq! failure message, or Debug derive on an enclosing type turns
    // it into one. On iOS those destinations are routinely uploaded.
    #[test]
    fn debug_never_prints_key_material() {
        let raw = ChiselKey::Raw {
            bytes: b"0123456789abcdef".to_vec(),
        };
        let printed = format!("{raw:?}");
        assert!(
            !printed.contains("0123456789abcdef"),
            "raw key leaked: {printed}"
        );
        assert!(
            printed.contains("redacted"),
            "expected a redaction marker: {printed}"
        );

        let phrase = ChiselKey::Passphrase {
            phrase: "correct horse battery staple".into(),
        };
        let printed = format!("{phrase:?}");
        assert!(
            !printed.contains("correct horse"),
            "passphrase leaked: {printed}"
        );

        let mut opts = Options::default_ffi();
        opts.encryption_key = Some(ChiselKey::Passphrase {
            phrase: "correct horse battery staple".into(),
        });
        let printed = format!("{opts:?}");
        assert!(
            !printed.contains("correct horse"),
            "passphrase leaked through Options: {printed}"
        );
    }
}
