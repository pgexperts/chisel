// ChiselDatabase.swift — the primary hand-written ergonomic layer over the
// UniFFI-generated `NativeDatabase` (Generated/chisel_ffi.swift). Three types:
//
//   - `ChiselDatabase`: sync, thread-confined wrapper. Flat forwarders for
//     the read/config/key-management surface of `chisel::Chisel`, plus
//     `transaction { }`. Mutating ops (allocate/update/delete/…/defrag) are
//     closure-only — the engine requires an active transaction and there is
//     no auto-begin, so they live on `Transaction`, not here.
//   - `Transaction`: non-escaping façade valid only inside a `transaction { }`
//     body, forwarding the data ops plus `savepoint { }`.
//   - `Savepoint`: marker for the enclosing `savepoint { }` scope; it carries
//     no operations of its own (use the enclosing `Transaction` — same
//     engine, same active transaction).
//
// `ChiselStore` (the async/actor layer, `ChiselStore.swift`) wraps `ChiselDatabase`
// and moves every call onto a private serial queue; this file assumes nothing
// about concurrency beyond "the caller does not call in from two threads at
// once" (see `ChiselDatabase`'s doc comment).
import Foundation

// The generated `Options` memberwise init defaults every field except
// `drainInsertion` (UniFFI cannot default an enum field across the FFI
// boundary), so plain `Options()` doesn't compile from the generated code
// alone. Supply the one field the generated init leaves mandatory; every
// other default here just repeats the generated default so this stays a
// one-line indirection, not a second source of truth for defaults.
extension Options {
    /// Default options: 8 MiB cache, `1024×` spillway cap, LRU-tail drain,
    /// create-if-missing, read-write, 2 superblocks, no encryption.
    public init() {
        self.init(drainInsertion: .lruTail)
    }
}

/// Synchronous, thread-confined handle to a Chisel database. NOT `Sendable` —
/// the caller owns threading (use your own serial queue, or prefer the async
/// `ChiselStore`). All methods block on the underlying engine call.
///
/// No `deinit`-triggered `close()`: `close()` is one-shot (a second call
/// throws `.Closed`), so an implicit close on drop would race a caller who
/// already closed explicitly, or silently swallow the close error on a path
/// the caller can't observe. Dropping the generated object still tears down
/// the engine; committed data is already durable (fsync'd at commit), so
/// there is nothing an implicit close would flush that isn't flushed already.
public final class ChiselDatabase {
    let native: NativeDatabase

    /// `path: nil` opens an in-memory database. A non-nil `path` must be a
    /// file URL — see `chiselFilePath`.
    public init(path: URL?, options: Options = Options()) throws {
        let resolved = try path.map { try chiselFilePath($0) }
        self.native = try NativeDatabase(path: resolved, options: options)
    }

    /// True if this handle can no longer do work — either a fatal error has
    /// poisoned the engine, OR the handle has already been closed.
    ///
    /// The second condition is easy to misread: a database closed cleanly
    /// reports `true` here. The flag answers "can this handle still work",
    /// not "is the file on disk damaged", so a diagnostics screen that checks
    /// it after shutdown — or the drop-and-reopen recovery flow in
    /// swift/README.md, which closes first — must not conclude the database
    /// is corrupt from a `true` alone.
    public var isPoisoned: Bool { native.isPoisoned() }
    /// Close the database (drops the flock, closes the file). One-shot —
    /// see the type doc comment above for why there is no deinit-close.
    public func close() throws { try native.close() }

    // MARK: - Flat mirror of chisel::Chisel (top-level reads/config; mutators live on `Transaction`)
    //
    // Values cross the FFI as `Data` directly in both directions — the
    // generated `read` already takes/returns `Data`, so there is no
    // `[UInt8]` round-trip to do here.
    //
    // Mutating operations (allocate/update/delete/…/defrag) are NOT mirrored
    // here: the engine requires an active transaction for every mutation and
    // there is no auto-begin, so a top-level `db.allocate(...)` could never
    // succeed — it would always throw `.NoActiveTransaction`. Reach them via
    // `transaction { txn in txn.allocate(...) }` instead; `Transaction` below
    // carries the full mutating surface.

    /// Retrieve a value by handle.
    public func read(_ handle: Handle) throws -> Data { try native.read(handle: handle) }
    /// Enumerate all live handles; repeatable within a session, order unspecified.
    public func handles() throws -> [Handle] { try native.handles() }
    /// Handle count, page count, and file size.
    public func stats() throws -> Stats { try native.stats() }
    /// Cumulative cache/fsync activity counters since the most recent open.
    public func counters() throws -> Counters { try native.counters() }
    /// Physical size of the database file in bytes.
    public func fileSizeBytes() throws -> UInt64 { try native.fileSizeBytes() }

    // Tags
    /// Read a handle's tag, or `nil` if untagged.
    public func tag(_ handle: Handle) throws -> Tag? { try native.tag(handle: handle) }
    /// Enumerate live handles carrying `tag`; repeatable within a session, order unspecified.
    public func handlesWithTag(_ tag: Tag) throws -> [Handle] { try native.handlesWithTag(tag: tag) }

    // Client byte
    /// Read the handle's opaque client byte, `0` if unset.
    public func clientByte(_ handle: Handle) throws -> UInt8 { try native.clientByte(handle: handle) }

    // Root names
    /// Look up a named root; `nil` if unbound.
    public func getRootName(_ name: String) throws -> Handle? { try native.getRootName(name: name) }

    // Key management
    /// Stage a second credential; both `existing` and `new` then open the database.
    public func addKey(existing: ChiselKey, new: ChiselKey) throws { try native.addKey(existing: existing, new: new) }
    /// Replace credential `old` with `new` in one atomic commit.
    public func rotateKey(old: ChiselKey, new: ChiselKey) throws { try native.rotateKey(old: old, new: new) }
    /// Retire a credential; throws `.LastKeySlot` if it is the only one remaining.
    public func removeKey(_ key: ChiselKey) throws { try native.removeKey(key: key) }

    // Runtime config (each throws `TransactionInProgress` if a transaction is active)
    /// Resize the in-memory cache cap.
    public func setCacheMaxBytes(_ bytes: UInt64) throws { try native.setCacheMaxBytes(bytes: bytes) }
    /// Resize the spillway cap; `0` disables the spillway.
    public func setSpillwayMaxBytes(_ bytes: UInt64) throws { try native.setSpillwayMaxBytes(bytes: bytes) }
    /// Change the drain-insertion policy applied at the next commit.
    public func setDrainInsertion(_ policy: DrainInsertion) throws { try native.setDrainInsertion(policy: policy) }

    /// Run `body` inside a transaction, committing on normal return.
    ///
    /// The error path covers a throw from `body` OR from the `commit()` that
    /// follows it, and rethrows whichever one threw — the commit is inside the
    /// guarded region deliberately, so a failed commit still attempts a
    /// rollback. Note that a commit failure has already poisoned the engine
    /// (`Chisel::commit`: a failure inside the fsync/superblock protocol is
    /// fatal), so that rollback is expected to fail too; it is best-effort via
    /// `try?` for that reason, and a cleanup failure never masks the error the
    /// caller needs.
    ///
    /// Anything added to the error path therefore runs for commit failures as
    /// well as caller failures — do not put "user code failed" handling there.
    public func transaction<R>(_ body: (Transaction) throws -> R) throws -> R {
        try native.begin()
        let state = TransactionState()
        let txn = Transaction(native: native, state: state)
        // Both exits, including the commit-failure path. After this returns,
        // any copy of `txn` that escaped the closure refuses work.
        defer { state.finished = true }
        do {
            let result = try body(txn)
            try native.commit()
            return result
        } catch {
            try? native.rollback()
            throw error
        }
    }
}

/// One-shot liveness flag shared by every copy of a `Transaction` value.
///
/// `Transaction` stays a struct so its value semantics and its
/// `@Sendable`-closure usage in `ChiselStore` are unchanged; only the flag is
/// a reference. Copying the struct out of its closure copies this reference,
/// so an escaped copy observes the same `finished` and refuses work exactly as
/// a class-based façade would.
///
/// Not synchronized, and does not need to be: the sync `ChiselDatabase` is
/// thread-confined by contract, and `ChiselStore` funnels every access through
/// one serial queue — the same confinement invariant `DatabaseBox` documents.
final class TransactionState {
    var finished = false
}

/// Operations valid only inside a `transaction { }` block. Forwards to the
/// same engine object as the enclosing `ChiselDatabase`; not escapable
/// (don't store this past the closure — the transaction it refers to may
/// already be committed or rolled back), not `Sendable`.
//
// IMPORTANT: do not make Sendable — a Transaction must not escape its
// closure; see ChiselStore's serial-queue confinement.
public struct Transaction {
    let native: NativeDatabase
    let state: TransactionState

    /// The engine object, or a throw if this transaction's scope has exited.
    ///
    /// Every forwarder below goes through here. The FFI has reserved
    /// `.AlreadyFinished` for exactly this guard since the binding was written
    /// and nothing ever raised it; this is the Python shape ADR 0025 chose.
    private func live() throws -> NativeDatabase {
        guard !state.finished else {
            throw ChiselError.AlreadyFinished(
                message: "transaction has already been committed or rolled back"
            )
        }
        return native
    }

    /// Store a value; returns its handle.
    public func allocate(_ value: Data) throws -> Handle { try live().allocate(value: value) }
    /// Retrieve a value by handle.
    public func read(_ handle: Handle) throws -> Data { try live().read(handle: handle) }
    /// Replace a value in place; the handle is preserved.
    public func update(_ handle: Handle, _ value: Data) throws { try live().update(handle: handle, value: value) }
    /// Remove a handle.
    public func delete(_ handle: Handle) throws { try live().delete(handle: handle) }
    /// Batch-delete several handles in this transaction.
    public func deleteMany(_ handles: [Handle]) throws { try live().deleteMany(handles: handles) }
    /// Store a value with an immutable tag; returns its handle.
    public func allocateTagged(_ value: Data, tag: Tag) throws -> Handle { try live().allocateTagged(value: value, tag: tag) }
    /// Read a handle's tag, or `nil` if untagged.
    public func tag(_ handle: Handle) throws -> Tag? { try live().tag(handle: handle) }
    /// Enumerate live handles carrying `tag`; repeatable within a session, order unspecified.
    public func handlesWithTag(_ tag: Tag) throws -> [Handle] { try live().handlesWithTag(tag: tag) }
    /// Delete only if the handle's stored tag matches; else throws `.TagMismatch`.
    public func deleteTagged(_ handle: Handle, tag: Tag) throws { try live().deleteTagged(handle: handle, tag: tag) }
    /// Bounded relation-drop: delete up to `max` chunks carrying `tag`.
    public func deleteWithTag(_ tag: Tag, max: UInt64) throws -> TagDropProgress { try live().deleteWithTag(tag: tag, max: max) }
    /// Read the handle's opaque client byte, `0` if unset.
    public func clientByte(_ handle: Handle) throws -> UInt8 { try live().clientByte(handle: handle) }
    /// Set the opaque client byte; `update()` preserves it.
    public func setClientByte(_ handle: Handle, _ byte: UInt8) throws { try live().setClientByte(handle: handle, byte: byte) }
    /// Bind a name to a handle in the named-root table.
    public func setRootName(_ name: String, _ handle: Handle) throws { try live().setRootName(name: name, handle: handle) }
    /// Look up a named root; `nil` if unbound.
    public func getRootName(_ name: String) throws -> Handle? { try live().getRootName(name: name) }
    /// Remove a named root.
    public func clearRootName(_ name: String) throws { try live().clearRootName(name: name) }
    /// Enumerate all live handles; repeatable within a session, order unspecified.
    public func handles() throws -> [Handle] { try live().handles() }
    /// Handle count, page count, and file size.
    public func stats() throws -> Stats { try live().stats() }
    /// Cumulative cache/fsync activity counters since the most recent open.
    public func counters() throws -> Counters { try live().counters() }
    /// Consolidate sparse data pages.
    public func defrag(_ options: DefragOptions = DefragOptions()) throws -> DefragStats { try live().defrag(options: options) }

    /// Nested rollback point within the active transaction. Releases on normal
    /// return, keeping the work and dropping the rollback target.
    ///
    /// The error path covers a throw from `body` OR from the `release()` that
    /// follows it, and rethrows whichever one threw. It rolls back to `name` —
    /// undoing only what happened since this savepoint, not the whole
    /// transaction — and then releases the name, so the scope is symmetric and
    /// the same name can be used again in this transaction.
    public func savepoint<R>(_ name: String, _ body: (Savepoint) throws -> R) throws -> R {
        try live().savepoint(name: name)
        let sp = Savepoint(native: native, name: name)
        do {
            let result = try body(sp)
            try native.release(name: name)
            return result
        } catch {
            // Both calls, in this order. The engine keeps the mark after a
            // rollback_to — deliberately, so a caller can roll back to the
            // same point twice — which means rolling back alone leaves the
            // name occupied for the rest of the transaction and the next
            // `savepoint(name)` throws DuplicateSavepoint. Releasing after
            // the rollback makes the scope symmetric on both exit paths,
            // which is the whole point of the closure form.
            //
            // Both are best-effort: a cleanup failure must not mask the error
            // the caller actually needs to see.
            try? native.rollbackTo(name: name)
            try? native.release(name: name)
            throw error
        }
    }
}

/// A named savepoint scope. It carries no operations of its own — use the
/// enclosing `Transaction` for reads and writes, same engine, same active
/// transaction. This type exists only so `savepoint { }` has something
/// distinct from `Transaction` to hand the body.
///
/// Deliberately has NO one-shot `finished` guard, unlike `Transaction`: it has
/// no forwarders for such a guard to protect. Python's `Savepoint` does need
/// one (ADR 0025) because it exposes `release` / `rollback_to` as callable
/// methods; here those are reachable only through the closure's own exit
/// paths, so an escaped `Savepoint` can do nothing at all with it. If this
/// type ever gains an operation, it needs the `Transaction` treatment first.
//
// IMPORTANT: do not make Sendable — a Savepoint must not escape its
// closure; see ChiselStore's serial-queue confinement.
public struct Savepoint {
    let native: NativeDatabase
    /// The name this savepoint was created with.
    public let name: String
}
