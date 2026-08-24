// error.rs — FFI error surface. Role in system: the only place that
// translates the engine's `chisel::ChiselError` into the UniFFI `Error` enum
// Swift receives via `throws`. See
// docs/specs/2026-07-18-swift-binding-design.md §4.2: Swift enums cannot
// subclass, so Python's `ChiselError -> Operational|Fatal -> <specific>`
// class tree flattens to one enum here; `.category`/`.isPoisoning` are
// computed properties added on the Swift side (Extensions.swift), not
// represented in this type.
//
// Post-review fix: every variant now carries `message: String` (the engine
// error's `Display` text, captured in `From` below), so a Swift caller
// catching e.g. `.checksumMismatch` still gets "checksum mismatch on page
// 42" instead of a bare case name. This brings the FFI surface to parity
// with python/src/errors.rs, which has always attached `err.to_string()`.
// Structured fields (page ids, byte limits, expected/actual counts) are
// still NOT carried as typed per-case payloads — `message` is a
// forward-compatible stopgap; adding typed fields later is additive and can
// wait for post-1.0, per the project owner's call on this review finding.
//
// Two variants keep their pre-existing structured fields alongside the new
// `message`: `Io { kind, errno, message }` (I42 — lets callers distinguish
// e.g. ENOSPC from EACCES without string-parsing `message`). The two
// `*Savepoint` cases DROP their old `{ name }` field in favor of the
// uniform `{ message }` shape — the name is already embedded in the engine's
// Display text ("savepoint not found: foo"), so keeping a separate `name`
// field would be redundant with `message` for every other variant's shape.

/// Result alias for exported fallible methods.
pub type FfiResult<T> = Result<T, ChiselError>;

#[derive(Debug, uniffi::Error)]
pub enum ChiselError {
    // --- operational: database state intact ---
    InvalidHandle {
        message: String,
    },
    NoActiveTransaction {
        message: String,
    },
    TransactionAlreadyActive {
        message: String,
    },
    SavepointNotFound {
        message: String,
    },
    DuplicateSavepoint {
        message: String,
    },
    ReadOnlyMode {
        message: String,
    },
    DatabaseFileNotFound {
        message: String,
    },
    InvalidRootName {
        message: String,
    },
    RootNameTableFull {
        message: String,
    },
    InvalidSuperblockCount {
        message: String,
    },
    InvalidArgon2Params {
        message: String,
    },
    CacheFull {
        message: String,
    },
    SpillwayFull {
        message: String,
    },
    TransactionInProgress {
        message: String,
    },
    // NOT produced by `From<chisel::ChiselError>` below — the engine has no
    // equivalent variant. Raised directly by the `NativeDatabase` wrapper
    // (`Mutex<Option<Chisel>>`) via `closed_err()` in database.rs when a
    // method is called after `close()`, mirroring the Python binding's
    // `ClosedError` (ISSUES.md I25).
    Closed {
        message: String,
    },
    // Ditto: reserved for the transaction/savepoint wrapper's one-shot
    // "finished" guard, mirroring Python's `AlreadyFinishedError`
    // (ISSUES.md I22/I24).
    AlreadyFinished {
        message: String,
    },
    // FFI-only, like `Closed` and `AlreadyFinished`: the engine has no
    // equivalent variant because its `Tag` type makes zero unrepresentable.
    // UniFFI cannot express that — it lowers `Tag` to a bare `u32` alias — so
    // the check has to live at this boundary, mirroring the Python binding,
    // which validates in `python/src/db.rs` rather than relying on the lift.
    InvalidTag {
        message: String,
    },
    TagMismatch {
        message: String,
    },
    NoEncryptionKey {
        message: String,
    },
    InvalidEncryptionKey {
        message: String,
    },
    EncryptionNotSupported {
        message: String,
    },
    NoFreeKeySlot {
        message: String,
    },
    LastKeySlot {
        message: String,
    },
    // --- fatal: database poisoned / on-disk state suspect ---
    Io {
        kind: String,
        errno: Option<i32>,
        message: String,
    },
    ChecksumMismatch {
        message: String,
    },
    CorruptSuperblock {
        message: String,
    },
    FileSizeMismatch {
        message: String,
    },
    LockFailed {
        message: String,
    },
    UnsupportedFormatVersion {
        message: String,
    },
    UnsupportedPageSize {
        message: String,
    },
    CorruptPage {
        message: String,
    },
    InvalidPageId {
        message: String,
    },
    InvalidFreemapDepth {
        message: String,
    },
    InvalidNextHandle {
        message: String,
    },
    TxnCounterExhausted {
        message: String,
    },
    HandleSpaceExhausted {
        message: String,
    },
    Poisoned {
        message: String,
    },
    DecryptionFailed {
        message: String,
    },
    /// An engine variant added AFTER this crate was last updated. `fatal`
    /// carries the engine's own `is_fatal()` verdict, so the caller-facing
    /// tier is never guessed.
    ///
    /// This replaces a `_ => Poisoned` fallback that reported every unmapped
    /// variant as a fatal poisoning. That was wrong twice over: it mistiers
    /// operational variants (an out-of-range Argon2 parameter is not a
    /// corrupt database), and `Poisoned` additionally asserts an engine
    /// state — `is_poisoned() == true` — that is not true for a handle that
    /// merely rejected a bad argument.
    ///
    /// Reaching this case is not an error, but it does mean the mapping is
    /// stale; `every_engine_variant_has_an_explicit_ffi_arm` fails as soon as
    /// a variant exists with no arm of its own.
    Other {
        message: String,
        fatal: bool,
    },
}

impl ChiselError {
    /// The human-readable message every variant carries — either the
    /// engine's own `Display` text (`From` below) or, for the two
    /// FFI-only variants (`Closed`, `AlreadyFinished`), whatever the
    /// Swift wrapper set when it raised them directly.
    fn message(&self) -> &str {
        match self {
            Self::InvalidHandle { message }
            | Self::NoActiveTransaction { message }
            | Self::TransactionAlreadyActive { message }
            | Self::SavepointNotFound { message }
            | Self::DuplicateSavepoint { message }
            | Self::ReadOnlyMode { message }
            | Self::DatabaseFileNotFound { message }
            | Self::InvalidRootName { message }
            | Self::RootNameTableFull { message }
            | Self::InvalidSuperblockCount { message }
            | Self::InvalidArgon2Params { message }
            | Self::CacheFull { message }
            | Self::SpillwayFull { message }
            | Self::TransactionInProgress { message }
            | Self::Closed { message }
            | Self::AlreadyFinished { message }
            | Self::InvalidTag { message }
            | Self::TagMismatch { message }
            | Self::NoEncryptionKey { message }
            | Self::InvalidEncryptionKey { message }
            | Self::EncryptionNotSupported { message }
            | Self::NoFreeKeySlot { message }
            | Self::LastKeySlot { message }
            | Self::Io { message, .. }
            | Self::ChecksumMismatch { message }
            | Self::CorruptSuperblock { message }
            | Self::FileSizeMismatch { message }
            | Self::LockFailed { message }
            | Self::UnsupportedFormatVersion { message }
            | Self::UnsupportedPageSize { message }
            | Self::CorruptPage { message }
            | Self::InvalidPageId { message }
            | Self::InvalidFreemapDepth { message }
            | Self::InvalidNextHandle { message }
            | Self::TxnCounterExhausted { message }
            | Self::HandleSpaceExhausted { message }
            | Self::Poisoned { message }
            | Self::DecryptionFailed { message }
            | Self::Other { message, .. } => message,
        }
    }
}

impl std::fmt::Display for ChiselError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}
impl std::error::Error for ChiselError {}

impl From<chisel::ChiselError> for ChiselError {
    fn from(e: chisel::ChiselError) -> Self {
        use chisel::ChiselError as E;
        // Captured before the match moves fields out of `e` (e.g. IoError's
        // io::Error, CorruptSuperblock's Vec<SlotDefect>) — Display only
        // needs `&e`, so this borrow is dropped before the match takes
        // ownership.
        let message = e.to_string();
        match e {
            E::InvalidHandle(_) => Self::InvalidHandle { message },
            E::NoActiveTransaction => Self::NoActiveTransaction { message },
            E::TransactionAlreadyActive => Self::TransactionAlreadyActive { message },
            E::SavepointNotFound(_) => Self::SavepointNotFound { message },
            E::DuplicateSavepoint(_) => Self::DuplicateSavepoint { message },
            E::ReadOnlyMode => Self::ReadOnlyMode { message },
            // Engine name is `FileNotFound`; FFI keeps the more explicit
            // `DatabaseFileNotFound` name from the Swift design spec.
            E::FileNotFound => Self::DatabaseFileNotFound { message },
            E::InvalidRootName => Self::InvalidRootName { message },
            E::RootNameTableFull => Self::RootNameTableFull { message },
            E::InvalidSuperblockCount { .. } => Self::InvalidSuperblockCount { message },
            E::InvalidArgon2Params { .. } => Self::InvalidArgon2Params { message },
            E::CacheFull { .. } => Self::CacheFull { message },
            E::SpillwayFull { .. } => Self::SpillwayFull { message },
            E::TransactionInProgress => Self::TransactionInProgress { message },
            E::TagMismatch { .. } => Self::TagMismatch { message },
            E::NoEncryptionKey => Self::NoEncryptionKey { message },
            E::InvalidEncryptionKey => Self::InvalidEncryptionKey { message },
            E::EncryptionNotSupported => Self::EncryptionNotSupported { message },
            E::NoFreeKeySlot => Self::NoFreeKeySlot { message },
            E::LastKeySlot => Self::LastKeySlot { message },
            E::IoError(io) => Self::Io {
                kind: format!("{:?}", io.kind()),
                errno: io.raw_os_error(),
                message,
            },
            E::ChecksumMismatch { .. } => Self::ChecksumMismatch { message },
            E::CorruptSuperblock { .. } => Self::CorruptSuperblock { message },
            E::FileSizeMismatch { .. } => Self::FileSizeMismatch { message },
            E::LockFailed => Self::LockFailed { message },
            E::UnsupportedFormatVersion { .. } => Self::UnsupportedFormatVersion { message },
            E::UnsupportedPageSize { .. } => Self::UnsupportedPageSize { message },
            E::CorruptPage { .. } => Self::CorruptPage { message },
            E::InvalidPageId { .. } => Self::InvalidPageId { message },
            E::InvalidFreemapDepth { .. } => Self::InvalidFreemapDepth { message },
            E::InvalidNextHandle { .. } => Self::InvalidNextHandle { message },
            E::TxnCounterExhausted { .. } => Self::TxnCounterExhausted { message },
            E::HandleSpaceExhausted { .. } => Self::HandleSpaceExhausted { message },
            E::Poisoned => Self::Poisoned { message },
            E::DecryptionFailed { .. } => Self::DecryptionFailed { message },
            // `chisel::ChiselError` is `#[non_exhaustive]`, so this match
            // needs a wildcard even though every variant that exists today is
            // listed explicitly above.
            //
            // It does NOT guess the tier. The previous version routed
            // everything here to `Poisoned` on a fail-toward-fatal argument,
            // which mistiered every future operational variant and claimed an
            // engine state that was not true. `is_fatal()` is the engine's own
            // answer to exactly this question, so ask it — the same fix the
            // Python binding made for the same fail-open class (I138).
            //
            // `message` (captured above from the still-real `Display`) is
            // preserved, so a not-yet-mapped variant still surfaces its actual
            // diagnostic text.
            other => Self::Other {
                fatal: other.is_fatal(),
                message,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_maps_and_carries_kind() {
        let e = chisel::ChiselError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ));
        match ChiselError::from(e) {
            ChiselError::Io {
                kind,
                errno,
                message,
            } => {
                assert_eq!(kind, "PermissionDenied");
                // Synthetic (non-OS-backed) io::Error, so no raw errno — the
                // field still exists and round-trips None rather than panicking.
                assert_eq!(errno, None);
                assert!(!message.is_empty(), "message must be non-empty");
                assert!(message.contains("I/O error"));
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn invalid_handle_maps() {
        // Engine's InvalidHandle carries the offending handle id (u64); the
        // FFI variant drops the typed id but keeps it in `message` via the
        // engine's Display (see module doc comment) so any id round-trips to
        // the same case with its diagnostic text intact.
        match ChiselError::from(chisel::ChiselError::InvalidHandle(7)) {
            ChiselError::InvalidHandle { message } => assert_eq!(message, "invalid handle: 7"),
            other => panic!("expected InvalidHandle, got {other:?}"),
        }
    }

    // Discriminant name of an FFI ChiselError. Exhaustive with NO wildcard, so
    // adding an FFI ChiselError variant fails to compile here until it is named
    // — a compile-time tripwire complementing the runtime coverage below.
    fn ffi_name(e: &ChiselError) -> &'static str {
        match e {
            ChiselError::InvalidHandle { .. } => "InvalidHandle",
            ChiselError::NoActiveTransaction { .. } => "NoActiveTransaction",
            ChiselError::TransactionAlreadyActive { .. } => "TransactionAlreadyActive",
            ChiselError::SavepointNotFound { .. } => "SavepointNotFound",
            ChiselError::DuplicateSavepoint { .. } => "DuplicateSavepoint",
            ChiselError::ReadOnlyMode { .. } => "ReadOnlyMode",
            ChiselError::DatabaseFileNotFound { .. } => "DatabaseFileNotFound",
            ChiselError::InvalidRootName { .. } => "InvalidRootName",
            ChiselError::RootNameTableFull { .. } => "RootNameTableFull",
            ChiselError::InvalidSuperblockCount { .. } => "InvalidSuperblockCount",
            ChiselError::InvalidArgon2Params { .. } => "InvalidArgon2Params",
            ChiselError::CacheFull { .. } => "CacheFull",
            ChiselError::SpillwayFull { .. } => "SpillwayFull",
            ChiselError::TransactionInProgress { .. } => "TransactionInProgress",
            ChiselError::Closed { .. } => "Closed",
            ChiselError::AlreadyFinished { .. } => "AlreadyFinished",
            ChiselError::InvalidTag { .. } => "InvalidTag",
            ChiselError::TagMismatch { .. } => "TagMismatch",
            ChiselError::NoEncryptionKey { .. } => "NoEncryptionKey",
            ChiselError::InvalidEncryptionKey { .. } => "InvalidEncryptionKey",
            ChiselError::EncryptionNotSupported { .. } => "EncryptionNotSupported",
            ChiselError::NoFreeKeySlot { .. } => "NoFreeKeySlot",
            ChiselError::LastKeySlot { .. } => "LastKeySlot",
            ChiselError::Io { .. } => "Io",
            ChiselError::ChecksumMismatch { .. } => "ChecksumMismatch",
            ChiselError::CorruptSuperblock { .. } => "CorruptSuperblock",
            ChiselError::FileSizeMismatch { .. } => "FileSizeMismatch",
            ChiselError::LockFailed { .. } => "LockFailed",
            ChiselError::UnsupportedFormatVersion { .. } => "UnsupportedFormatVersion",
            ChiselError::UnsupportedPageSize { .. } => "UnsupportedPageSize",
            ChiselError::CorruptPage { .. } => "CorruptPage",
            ChiselError::InvalidPageId { .. } => "InvalidPageId",
            ChiselError::InvalidFreemapDepth { .. } => "InvalidFreemapDepth",
            ChiselError::InvalidNextHandle { .. } => "InvalidNextHandle",
            ChiselError::TxnCounterExhausted { .. } => "TxnCounterExhausted",
            ChiselError::HandleSpaceExhausted { .. } => "HandleSpaceExhausted",
            ChiselError::Poisoned { .. } => "Poisoned",
            ChiselError::DecryptionFailed { .. } => "DecryptionFailed",
            ChiselError::Other { .. } => "Other",
        }
    }

    // Spec §7.1: "every chisel::ChiselError variant maps to a distinct
    // ChiselError UniFFI case ... the mapping must be total". Constructs one
    // instance of EVERY current engine variant (template: src/error.rs's
    // is_fatal_matches_documented_classification_for_every_variant) and asserts
    // `From` yields the exact expected FFI case. Because the engine enum is
    // #[non_exhaustive], the `From` match ends in a `_ => Other` wildcard, so
    // a deleted arm, a duplicated arm, or a variant silently falling through the
    // wildcard would all be invisible to the compiler — this test is the only
    // CI signal that catches them for every known variant.
    #[test]
    fn every_engine_variant_maps_to_its_ffi_case() {
        use chisel::ChiselError as E;

        // (engine variant instance, expected FFI case name). Covers all 35
        // current engine variants. `Poisoned` is the only pair whose target is
        // "Poisoned" — every OTHER variant mapping to "Poisoned" would mean it
        // fell through the non_exhaustive wildcard, which the exact-name assert
        // (and the explicit non-Poisoned guard) below flags.
        let cases: Vec<(E, &'static str)> = vec![
            (E::InvalidHandle(0), "InvalidHandle"),
            (E::NoActiveTransaction, "NoActiveTransaction"),
            (E::TransactionAlreadyActive, "TransactionAlreadyActive"),
            (E::SavepointNotFound("s".into()), "SavepointNotFound"),
            (E::DuplicateSavepoint("s".into()), "DuplicateSavepoint"),
            (E::ReadOnlyMode, "ReadOnlyMode"),
            (E::FileNotFound, "DatabaseFileNotFound"),
            (E::InvalidRootName, "InvalidRootName"),
            (E::RootNameTableFull, "RootNameTableFull"),
            (
                E::InvalidSuperblockCount { value: 0 },
                "InvalidSuperblockCount",
            ),
            (E::CacheFull { limit: 0 }, "CacheFull"),
            (E::SpillwayFull { limit_bytes: 0 }, "SpillwayFull"),
            (E::TransactionInProgress, "TransactionInProgress"),
            (
                E::TagMismatch {
                    handle: 0,
                    expected: 0,
                    actual: 0,
                },
                "TagMismatch",
            ),
            (E::NoEncryptionKey, "NoEncryptionKey"),
            (E::InvalidEncryptionKey, "InvalidEncryptionKey"),
            (E::EncryptionNotSupported, "EncryptionNotSupported"),
            (E::NoFreeKeySlot, "NoFreeKeySlot"),
            (E::LastKeySlot, "LastKeySlot"),
            (E::IoError(std::io::Error::other("io")), "Io"),
            (E::ChecksumMismatch { page_id: 0 }, "ChecksumMismatch"),
            (
                E::CorruptSuperblock { defects: vec![] },
                "CorruptSuperblock",
            ),
            (
                E::FileSizeMismatch {
                    expected: 0,
                    actual: 0,
                },
                "FileSizeMismatch",
            ),
            (E::LockFailed, "LockFailed"),
            (
                E::UnsupportedFormatVersion {
                    found: 0,
                    expected: 0,
                },
                "UnsupportedFormatVersion",
            ),
            (
                E::UnsupportedPageSize {
                    stored: 0,
                    compiled: 0,
                },
                "UnsupportedPageSize",
            ),
            (E::CorruptPage { page_id: 0 }, "CorruptPage"),
            (E::InvalidPageId { page_id: 0 }, "InvalidPageId"),
            (
                E::InvalidArgon2Params {
                    m_cost: 1,
                    t_cost: 1,
                    p_cost: 1,
                },
                "InvalidArgon2Params",
            ),
            (
                E::InvalidFreemapDepth { stored: 9, max: 3 },
                "InvalidFreemapDepth",
            ),
            (
                E::InvalidNextHandle { stored: 9, max: 3 },
                "InvalidNextHandle",
            ),
            (E::TxnCounterExhausted { current: 7 }, "TxnCounterExhausted"),
            (
                E::HandleSpaceExhausted { current: 7 },
                "HandleSpaceExhausted",
            ),
            (E::Poisoned, "Poisoned"),
            (E::DecryptionFailed { page_id: 0 }, "DecryptionFailed"),
        ];

        for (engine, expected) in cases {
            let got = ChiselError::from(engine);
            let name = ffi_name(&got);
            assert_eq!(
                name, expected,
                "engine variant expected to map to {expected}, got {name}"
            );
            // Every variant must carry the engine's diagnostic text now — a
            // blank message would mean the `message` field was constructed
            // wrong for this arm (e.g. from a fresh default instead of
            // `e.to_string()`).
            assert!(
                !got.message().is_empty(),
                "{expected} produced an empty message"
            );
            // Spec §7.1 "no silently-dropped error": any variant whose intended
            // target is NOT Poisoned must not surface as Poisoned — i.e. must
            // not have fallen through the non_exhaustive `_ => Poisoned` arm.
            // The wildcard's target is now `Other`, never `Poisoned`. Any
            // variant surfacing as `Other` fell through instead of reaching
            // its own arm — which is exactly what this test exists to catch.
            assert_ne!(
                name, "Other",
                "{expected} fell through to the wildcard instead of its own arm"
            );
        }
    }

    // SWIFT-13. The engine's `is_fatal()` is the authority on whether a
    // variant means "the database is intact" or "integrity is in question".
    // Before this fix all five of these fell through `_ => Poisoned`, which
    // reports fatal unconditionally — right by luck for four of them and
    // flatly wrong for InvalidArgon2Params, an out-of-range cost parameter
    // rejected before the file is touched.
    #[test]
    fn new_engine_variants_map_to_their_own_cases_at_the_right_tier() {
        use chisel::ChiselError as E;

        let operational = E::InvalidArgon2Params {
            m_cost: 1,
            t_cost: 1,
            p_cost: 1,
        };
        assert!(!operational.is_fatal(), "engine tiering changed under us");
        assert_eq!(
            ffi_name(&ChiselError::from(operational)),
            "InvalidArgon2Params"
        );

        let fatal: Vec<(E, &str)> = vec![
            (
                E::InvalidFreemapDepth { stored: 9, max: 3 },
                "InvalidFreemapDepth",
            ),
            (
                E::InvalidNextHandle { stored: 9, max: 3 },
                "InvalidNextHandle",
            ),
            (E::TxnCounterExhausted { current: 7 }, "TxnCounterExhausted"),
            (
                E::HandleSpaceExhausted { current: 7 },
                "HandleSpaceExhausted",
            ),
        ];
        for (engine, expected) in fatal {
            assert!(
                engine.is_fatal(),
                "{expected} should be fatal per the engine"
            );
            assert_eq!(ffi_name(&ChiselError::from(engine)), expected);
        }
    }

    // SWIFT-13. `chisel::ChiselError` is `#[non_exhaustive]`, so no downstream
    // crate may match it exhaustively and the compiler cannot enforce this.
    // The list-based test above can only check variants it already names —
    // which is precisely how five variants went unmapped.
    //
    // `chisel-ffi` is `publish = false` and workspace-only, so the engine's
    // source is always at a fixed path relative to this manifest. Parsing it
    // is the only way to learn the true variant set without adding a
    // test-only helper to the engine's public API.
    //
    // A parse that finds implausibly few variants is itself a failure, so a
    // reformatting of `src/error.rs` breaks this loudly rather than quietly
    // passing on an empty set.
    #[test]
    fn every_engine_variant_has_an_explicit_ffi_arm() {
        let engine_src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/error.rs"))
                .expect("engine src/error.rs must be readable from within the workspace");

        let enum_body = engine_src
            .split_once("pub enum ChiselError {")
            .expect("engine error.rs must declare `pub enum ChiselError {`")
            .1
            .split_once("\n}")
            .expect("the ChiselError enum must end with a line-start closing brace")
            .0;

        // Variant lines are exactly those at 4-space indent starting with an
        // uppercase ASCII letter. Doc comments (`///`), attributes (`#[`),
        // and field lines (8-space indent, lowercase) are all excluded.
        let variants: Vec<String> = enum_body
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                line.len() - trimmed.len() == 4
                    && trimmed.starts_with(|c: char| c.is_ascii_uppercase())
            })
            .map(|line| {
                line.trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect()
            })
            .collect();

        assert!(
            variants.len() >= 35,
            "parsed only {} variants out of src/error.rs — the parser is \
             broken, not the enum. Fix this test before trusting it.",
            variants.len()
        );

        // Our own source, so the check tracks the real `From` impl rather
        // than a second list that can drift from it.
        let from_impl = include_str!("error.rs")
            .split_once("impl From<chisel::ChiselError> for ChiselError {")
            .expect("this file must contain the From impl")
            .1
            .split_once("\n}")
            .expect("the From impl must end with a line-start closing brace")
            .0;

        // Strip whole-line `//` comments before searching, so a variant named
        // only in a commented-out arm or in prose like "E::Foo is handled
        // above" does not count as mapped. This is a partial defense, not a
        // full one, and the two gaps are accepted deliberately rather than
        // hidden: a *trailing* comment on a code line (`E::Foo => ..., //
        // mentions E::Bar`) and a `/* */` block comment both survive this
        // filter and could still exempt a variant permanently. Handling
        // either properly needs a real comment-aware tokenizer, which is out
        // of scope for a workspace-only (`publish = false`), single-file test
        // helper — this crate's `From` impl does not use either form today,
        // and a reviewer adding one would be the one introducing the gap.
        let from_impl_code: String = from_impl
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // Plain `.contains(needle)` would also pass a variant whose name is a
        // strict prefix of a mapped one (an `Io` variant would be satisfied
        // by the text `E::IoError`), since the substring match does not care
        // what follows it. Require the byte right after `E::{variant}` to not
        // be an identifier-continuation character (alphanumeric or `_`) —
        // i.e. the match must end the identifier — or to be the end of the
        // impl body. `is_ascii_alphanumeric`/`_` matches how `variants` above
        // was tokenized (`take_while(is_ascii_alphanumeric)`), so the check
        // uses the same notion of "identifier character".
        let unmapped: Vec<&String> = variants
            .iter()
            .filter(|v| {
                let needle = format!("E::{v}");
                !from_impl_code.match_indices(&needle).any(|(idx, _)| {
                    from_impl_code[idx + needle.len()..]
                        .as_bytes()
                        .first()
                        .map(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
                        .unwrap_or(true)
                })
            })
            .collect();

        assert!(
            unmapped.is_empty(),
            "engine ChiselError variants with no explicit arm in \
             `From<chisel::ChiselError>`: {unmapped:?}\n\
             Add an arm for each, plus a matching FFI case, a `message()` \
             arm, an `ffi_name` arm, and a `category` arm in \
             swift/Sources/Chisel/Extensions.swift. The `Other` fallback \
             exists for variants added after this crate was last updated — \
             it is not a resting place for one that already exists."
        );
    }
}
