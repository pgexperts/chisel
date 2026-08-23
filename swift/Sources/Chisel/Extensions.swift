// Extensions.swift — hand-written Swift ergonomics layer over the UniFFI-
// generated bindings (Generated/chisel_ffi.swift). This is the first file in
// that layer: `ChiselError.category` / `.isPoisoning` / `.message`, so
// callers can react to an error without special-casing all 38 generated
// cases individually.
//
// Classification source of truth: the CALLER-FACING tiering in the Python
// binding, `python/src/errors.rs:101-172` (`OperationalError` vs
// `FatalError`). This whole Swift binding exists for parity with Python, so
// its two error tiers are authoritative.
//
// This is deliberately NOT the engine's `chisel::ChiselError::is_fatal()`
// (src/error.rs): that is a manager-INTERNAL predicate answering "does this
// error cause a NEW poisoning transition?", which returns FALSE for
// `Poisoned` because an already-poisoned manager isn't a fresh event
// (src/error.rs:204, :554). For a caller, a poisoned DB is precisely a
// drop-and-reopen (fatal) condition — so `Poisoned` is FATAL here, matching
// Python (`python/src/errors.rs:155-172`, PoisonedError : FatalError) and
// diverging from `is_fatal()` by that one case.
//
// IMPORTANT — do not collapse the switches below back into comma-grouped
// `case .A, .B, .C:` arms. That reads cleaner and DID compile, but crashes
// with SIGSEGV at runtime in DEBUG builds only (`swift test` / `swift
// build` without `-c release`) once `ChiselError` grew a `message: String`
// payload on every case (32 cases total, one with 3 fields). Confirmed via
// `swift test -c release`, which passes; `-Onone` codegen for a grouped
// multi-pattern switch over a large mixed-arity enum appears to be a Swift
// 6.0.3 compiler bug. One `case:` arm per line sidesteps it and is what's
// actually exercised by CI and local test runs.
import Foundation

extension ChiselError {
    public enum Category: Sendable, Equatable {
        case operational
        case fatal
    }

    /// Operational: your mistake, or a transient condition — the database on
    /// disk is intact and the handle stays usable. Fatal: an integrity
    /// invariant was violated or I/O is unreliable — close and reopen.
    ///
    /// NO `default:` arm, deliberately: this switch is a compile-time
    /// tripwire. If the generated `ChiselError` ever gains a case (a new
    /// engine variant mapped in chisel-ffi/src/error.rs), this file fails to
    /// build until the new case is classified here.
    public var category: Category {
        switch self {
        case .InvalidHandle: return .operational
        case .NoActiveTransaction: return .operational
        case .TransactionAlreadyActive: return .operational
        case .SavepointNotFound: return .operational
        case .DuplicateSavepoint: return .operational
        case .ReadOnlyMode: return .operational
        case .DatabaseFileNotFound: return .operational
        case .InvalidRootName: return .operational
        case .RootNameTableFull: return .operational
        case .InvalidSuperblockCount: return .operational
        case .InvalidArgon2Params: return .operational
        case .CacheFull: return .operational
        case .SpillwayFull: return .operational
        case .TransactionInProgress: return .operational
        case .TagMismatch: return .operational
        case .NoEncryptionKey: return .operational
        case .InvalidEncryptionKey: return .operational
        case .EncryptionNotSupported: return .operational
        case .NoFreeKeySlot: return .operational
        case .LastKeySlot: return .operational
        // No engine variant produces these — the NativeDatabase wrapper raises
        // `.Closed` directly (via closed_err in chisel-ffi/src/database.rs) on a
        // call after close(); `.AlreadyFinished` is reserved for a finished
        // handle. "You used a handle you already closed or finished" is a caller
        // mistake with the database untouched, the same shape as every other
        // operational case.
        case .Closed: return .operational
        case .AlreadyFinished: return .operational
        // FATAL. `Poisoned` means the manager already hit an integrity/I-O
        // failure and refuses further work; the only recovery is to drop the
        // handle and reopen. Fatal here even though the engine's `is_fatal()`
        // calls it non-fatal — see the file header and
        // `python/src/errors.rs:155-172`.
        case .Poisoned: return .fatal
        case .Io: return .fatal
        case .ChecksumMismatch: return .fatal
        case .CorruptSuperblock: return .fatal
        case .FileSizeMismatch: return .fatal
        case .LockFailed: return .fatal
        case .UnsupportedFormatVersion: return .fatal
        case .UnsupportedPageSize: return .fatal
        case .CorruptPage: return .fatal
        case .InvalidPageId: return .fatal
        case .DecryptionFailed: return .fatal
        case .InvalidFreemapDepth: return .fatal
        case .InvalidNextHandle: return .fatal
        case .TxnCounterExhausted: return .fatal
        case .HandleSpaceExhausted: return .fatal
        // The Rust side sets `fatal` from the engine's own is_fatal(), so an
        // engine variant added after this binding was last regenerated still
        // reaches the caller at the right tier instead of being guessed.
        case .Other(_, let fatal): return fatal ? .fatal : .operational
        }
    }

    public var isPoisoning: Bool { category == .fatal }

    /// The engine's human-readable explanation of what went wrong. Every
    /// case carries one as of the Rust-side `ChiselError` message field
    /// (chisel-ffi/src/error.rs); `.Io` carries `kind`/`errno` alongside it,
    /// so its arm binds only the third payload.
    ///
    /// No `default:` arm, for the same compile-time-tripwire reason as
    /// `category` above.
    public var message: String {
        switch self {
        case .InvalidHandle(let m): return m
        case .NoActiveTransaction(let m): return m
        case .TransactionAlreadyActive(let m): return m
        case .SavepointNotFound(let m): return m
        case .DuplicateSavepoint(let m): return m
        case .ReadOnlyMode(let m): return m
        case .DatabaseFileNotFound(let m): return m
        case .InvalidRootName(let m): return m
        case .RootNameTableFull(let m): return m
        case .InvalidSuperblockCount(let m): return m
        case .InvalidArgon2Params(let m): return m
        case .CacheFull(let m): return m
        case .SpillwayFull(let m): return m
        case .TransactionInProgress(let m): return m
        case .Closed(let m): return m
        case .AlreadyFinished(let m): return m
        case .TagMismatch(let m): return m
        case .NoEncryptionKey(let m): return m
        case .InvalidEncryptionKey(let m): return m
        case .EncryptionNotSupported(let m): return m
        case .NoFreeKeySlot(let m): return m
        case .LastKeySlot(let m): return m
        case .Io(_, _, let m): return m
        case .ChecksumMismatch(let m): return m
        case .CorruptSuperblock(let m): return m
        case .FileSizeMismatch(let m): return m
        case .LockFailed(let m): return m
        case .UnsupportedFormatVersion(let m): return m
        case .UnsupportedPageSize(let m): return m
        case .CorruptPage(let m): return m
        case .InvalidPageId(let m): return m
        case .Poisoned(let m): return m
        case .DecryptionFailed(let m): return m
        case .InvalidFreemapDepth(let m): return m
        case .InvalidNextHandle(let m): return m
        case .TxnCounterExhausted(let m): return m
        case .HandleSpaceExhausted(let m): return m
        case .Other(let m, _): return m
        }
    }
}

extension ChiselError: CustomStringConvertible {
    public var description: String { message }
}
