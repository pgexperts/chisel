// ChiselStore.swift — the async/actor layer over the synchronous, thread-
// confined `ChiselDatabase`. Every engine call is hopped onto ONE private
// serial `DispatchQueue`, so the blocking fsync I/O inside the engine never
// runs on Swift's cooperative thread pool (which must never block) and never
// runs concurrently with itself (the engine is single-client / !Sync).
//
// The actor provides mutual exclusion at the async layer; the serial queue
// provides the actual thread on which the blocking work runs. The actor's
// `async` methods `await` the queue's result via a checked continuation.
import Foundation

/// A Sendable box confining the non-Sendable `ChiselDatabase`.
///
/// SAFETY (`@unchecked Sendable`): the wrapped `ChiselDatabase` is only ever
/// touched inside `ChiselStore.run` closures, every one of which executes on
/// the store's single serial `queue`. Because that queue is serial and is the
/// ONLY place `db` is accessed, there is never concurrent access. The
/// `@unchecked` is justified by that queue-confinement invariant — it is not a
/// suppression of the checker, it is a promise the surrounding code keeps.
/// Do not read `db` anywhere except inside a `run { }` body.
private final class DatabaseBox: @unchecked Sendable {
    let db: ChiselDatabase
    init(_ db: ChiselDatabase) { self.db = db }
}

/// Async, `Sendable` handle to a Chisel database. Serializes all engine calls
/// on a dedicated serial queue so blocking fsync I/O never touches the
/// cooperative pool. This is the concurrency-safe counterpart to the
/// synchronous `ChiselDatabase`; use it from Swift Concurrency code.
public actor ChiselStore {
    private let box: DatabaseBox
    private let queue: DispatchQueue

    /// `path: nil` opens an in-memory database. The `ChiselDatabase` is created
    /// ON the serial queue, so even the opening I/O stays off the cooperative
    /// pool and the queue-confinement invariant holds from the very first call.
    public init(path: URL?, options: Options = Options()) async throws {
        let q = DispatchQueue(label: "org.chisel.store")
        self.queue = q
        self.box = try await Self.run(on: q) {
            DatabaseBox(try ChiselDatabase(path: path, options: options))
        }
    }

    // Hop a blocking closure onto the serial queue and await its result.
    // `static` so `init` can use it before `self` is fully initialized; the
    // instance `run` just forwards to it with the stored `queue`.
    private static func run<R: Sendable>(
        on queue: DispatchQueue, _ body: @escaping @Sendable () throws -> R
    ) async throws -> R {
        try await withCheckedThrowingContinuation { cont in
            queue.async { cont.resume(with: Result { try body() }) }
        }
    }

    private func run<R: Sendable>(_ body: @escaping @Sendable () throws -> R) async throws -> R {
        try await Self.run(on: queue, body)
    }

    // MARK: - Lifecycle / status

    /// Whether the engine has poisoned itself (a fatal integrity/I-O failure).
    /// Non-throwing, but still hopped onto the queue: the poison flag is engine
    /// state and reading it stays under the same single-queue confinement.
    public func isPoisoned() async -> Bool {
        let box = box
        return await withCheckedContinuation { cont in
            queue.async { cont.resume(returning: box.db.isPoisoned) }
        }
    }

    /// Close the database (drops the flock, closes the file). One-shot —
    /// see `ChiselDatabase.close()` for why there is no deinit-close.
    public func close() async throws {
        let box = box
        try await run { try box.db.close() }
    }

    // MARK: - Flat mirror of ChiselDatabase (top-level reads/config; mutators live on `Transaction`)
    //
    // Mutating operations (allocate/update/delete/…/defrag) are NOT mirrored
    // here, for the same reason as `ChiselDatabase`: the engine requires an
    // active transaction for every mutation and there is no auto-begin, so a
    // top-level `store.allocate(...)` could never succeed. Reach them via
    // `store.transaction { txn in txn.allocate(...) }` instead.

    /// Retrieve a value by handle.
    public func read(_ handle: Handle) async throws -> Data {
        let box = box
        return try await run { try box.db.read(handle) }
    }

    /// Enumerate all live handles; repeatable within a session, order unspecified.
    public func handles() async throws -> [Handle] {
        let box = box
        return try await run { try box.db.handles() }
    }

    /// Handle count, page count, and file size.
    public func stats() async throws -> Stats {
        let box = box
        return try await run { try box.db.stats() }
    }

    /// Cumulative cache/fsync activity counters since the most recent open.
    public func counters() async throws -> Counters {
        let box = box
        return try await run { try box.db.counters() }
    }

    /// Physical size of the database file in bytes.
    public func fileSizeBytes() async throws -> UInt64 {
        let box = box
        return try await run { try box.db.fileSizeBytes() }
    }

    // MARK: - Tags

    /// Read a handle's tag, or `nil` if untagged.
    public func tag(_ handle: Handle) async throws -> Tag? {
        let box = box
        return try await run { try box.db.tag(handle) }
    }

    /// Enumerate live handles carrying `tag`; repeatable within a session, order unspecified.
    public func handlesWithTag(_ tag: Tag) async throws -> [Handle] {
        let box = box
        return try await run { try box.db.handlesWithTag(tag) }
    }

    // MARK: - Client byte

    /// Read the handle's opaque client byte, `0` if unset.
    public func clientByte(_ handle: Handle) async throws -> UInt8 {
        let box = box
        return try await run { try box.db.clientByte(handle) }
    }

    // MARK: - Root names

    /// Look up a named root; `nil` if unbound.
    public func getRootName(_ name: String) async throws -> Handle? {
        let box = box
        return try await run { try box.db.getRootName(name) }
    }

    // MARK: - Key management

    /// Stage a second credential; both `existing` and `new` then open the database.
    public func addKey(existing: ChiselKey, new: ChiselKey) async throws {
        let box = box
        try await run { try box.db.addKey(existing: existing, new: new) }
    }

    /// Replace credential `old` with `new` in one atomic commit.
    public func rotateKey(old: ChiselKey, new: ChiselKey) async throws {
        let box = box
        try await run { try box.db.rotateKey(old: old, new: new) }
    }

    /// Retire a credential; throws `.LastKeySlot` if it is the only one remaining.
    public func removeKey(_ key: ChiselKey) async throws {
        let box = box
        try await run { try box.db.removeKey(key) }
    }

    // MARK: - Runtime config

    /// Resize the in-memory cache cap.
    public func setCacheMaxBytes(_ bytes: UInt64) async throws {
        let box = box
        try await run { try box.db.setCacheMaxBytes(bytes) }
    }

    /// Resize the spillway cap; `0` disables the spillway.
    public func setSpillwayMaxBytes(_ bytes: UInt64) async throws {
        let box = box
        try await run { try box.db.setSpillwayMaxBytes(bytes) }
    }

    /// Change the drain-insertion policy applied at the next commit.
    public func setDrainInsertion(_ policy: DrainInsertion) async throws {
        let box = box
        try await run { try box.db.setDrainInsertion(policy) }
    }

    // MARK: - Transactions

    /// Run `body` inside a transaction (commit on return, rollback on throw),
    /// on the serial queue.
    ///
    /// The body is SYNCHRONOUS by design: it is `@Sendable (Transaction) throws
    /// -> R`, NOT async. If it were async, an `await` inside could suspend the
    /// task while the transaction (and its engine lock) is held, letting a
    /// second logical transaction interleave on the !Sync engine. A synchronous
    /// body makes "one transaction, start to finish, on one queue hop" a
    /// structural guarantee rather than a convention. Keep it non-async.
    public func transaction<R: Sendable>(
        _ body: @escaping @Sendable (Transaction) throws -> R
    ) async throws -> R {
        let box = box
        return try await run { try box.db.transaction(body) }
    }
}
