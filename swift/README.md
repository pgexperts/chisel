# Chisel (Swift binding)

Swift Package Manager binding for [Chisel](..), a transactional slot-based storage engine written in Rust with shadow-paging durability. The package wraps the engine via [UniFFI](https://mozilla.github.io/uniffi-rs/) and offers two Swift surfaces: a synchronous, thread-confined `ChiselDatabase`, and an async/await `ChiselStore` actor for Swift Concurrency code.

## Status

Pre-1.0, tracking the engine's own `0.1.0`. The Swift API is stable-by-intent but subject to revision until the engine reaches 1.0. See the root [`README.md`](../README.md#status) and [`ARCHITECTURE.md`](../ARCHITECTURE.md) for the engine's durability model and on-disk format guarantees, which this binding does not change.

## Platform support

iOS 15+ / macOS 12+, Swift tools version 5.9. No Linux or Windows support — the underlying engine uses `flock`, and the package ships as an Apple `.xcframework` (arm64 + x86_64 device/simulator/macOS slices).

## Install

### Development (local build)

The `Package.swift` manifest lives at the **repository root**, not here — SwiftPM requires that for the remote-URL form below to work. Its `ChiselFFI` binary target has two modes, selected by the `CHISEL_LOCAL_XCFRAMEWORK` environment variable: unset (the default) resolves a prebuilt `Chisel.xcframework.zip` from a GitHub release asset by URL and checksum; set to any value, it points at a local `.binaryTarget(path: "swift/Chisel.xcframework")` instead. `Chisel.xcframework` is `.gitignore`'d — build it yourself, then opt into the local path:

```bash
swift/build-xcframework.sh              # cargo builds + uniffi-bindgen + xcodebuild -create-xcframework
CHISEL_LOCAL_XCFRAMEWORK=1 swift test   # from the repository root — the manifest is not in swift/
```

The script builds `chisel-ffi` for all five Apple target triples, regenerates `Sources/Chisel/Generated/chisel_ffi.swift` from the engine's UniFFI metadata, and assembles `Chisel.xcframework`. Add the package to another project as a local Swift package (`.package(path: "path/to/chisel")` — the repository root, not `swift/` — or Xcode's "Add Local Package") once the xcframework exists, and set `CHISEL_LOCAL_XCFRAMEWORK=1` in that project's environment too, since no release exists yet for the default mode to fall back to.

### Release (tagged versions)

Not yet available — no tagged release has been cut. The manifest's default `ChiselFFI` target is already the checksummed remote binary target below (currently carrying a placeholder checksum, rewritten when a release is cut):

```swift
.binaryTarget(
    name: "ChiselFFI",
    url: "https://github.com/pgexperts/chisel/releases/download/vX.Y.Z/Chisel.xcframework.zip",
    checksum: "..."   // from `swift package compute-checksum Chisel.xcframework.zip`
)
```

`Chisel.xcframework.zip` is uploaded to the GitHub Release rather than committed to the repository; SPM downloads and verifies it against the checksum on first resolve. `swift/release/prepare.sh vX.Y.Z` rewrites the url/checksum above and drives the rest of the cut — see the root [`README.md`](../README.md#swift-binding) for the full release flow.

## Quick start

The async `ChiselStore` is the recommended entry point for app code:

```swift
import Chisel
import Foundation

let store = try await ChiselStore(path: nil)          // in-memory; pass a file URL to persist
let handle = try await store.transaction { txn in
    try txn.allocate(Data("hello world".utf8))
}
let value = try await store.read(handle)
assert(value == Data("hello world".utf8))
try await store.close()
```

The synchronous `ChiselDatabase` is the same API without `async`/`await`, for code that already owns its own thread or serial queue:

```swift
import Chisel
import Foundation

let db = try ChiselDatabase(path: nil)
let handle = try db.transaction { txn in
    try txn.allocate(Data("hello world".utf8))
}
let value = try db.read(handle)
try db.close()
```

Both open a file-backed database by passing a `URL` instead of `nil`: `ChiselDatabase(path: someURL)` / `try await ChiselStore(path: someURL)`.

## Concurrency model

Two types, matched to two calling contexts:

- **`ChiselDatabase`** — synchronous, thread-confined, NOT `Sendable`. Every method blocks the calling thread on the underlying engine call. The caller owns threading: use it from your own dedicated thread or serial queue, never from more than one thread at a time.
- **`ChiselStore`** — an `actor`, `Sendable`. Every method hops onto one private serial `DispatchQueue` before touching the engine, so the blocking fsync I/O inside the engine never runs on Swift's cooperative thread pool (which must never block) and never runs concurrently with itself. Use this from Swift Concurrency code.

Both wrap the same engine invariant: Chisel is single-client (`!Sync`) — one logical transaction at a time, full stop.

**The transaction closure body is synchronous, even on `ChiselStore`.** `store.transaction { txn in ... }` takes a closure typed `@Sendable (Transaction) throws -> R`, not `async throws -> R` — there is no `await` available inside it, and this is enforced by the type signature, not a convention to remember. If the body could suspend mid-transaction, a second logical transaction could interleave on the single-client engine while the first is still holding its lock. Do all `await`ing before or after the `transaction { }` call, never inside it:

```swift
let precomputed = try await someAsyncWork()
try await store.transaction { txn in
    try txn.allocate(precomputed)   // synchronous work only in here
}
```

## Transactions and savepoints

Both `ChiselDatabase` and `ChiselStore` expose transactions as a closure — commit on normal return, rollback on thrown error — rather than standalone `begin()`/`commit()`/`rollback()` calls. This makes "every transaction resolved exactly one way" a structural guarantee instead of a documentation convention:

```swift
try db.transaction { txn in
    let keep = try txn.allocate(Data("keep".utf8))
    try txn.savepoint("experiment") { _ in
        _ = try txn.allocate(Data("maybe discard".utf8))
        if somethingWentWrong { throw MyError.abort }   // rolls back to "experiment"; keep stays
    }
}
```

`savepoint(name:_:)` releases on normal return and rolls back to `name` on throw, the same shape as `transaction`. See [`PARITY.md`](PARITY.md) for exactly how this closure form maps onto the Python binding's `begin`/`commit`/`rollback` and `with`-statement API — there is no Swift gap, only a different, more structural spelling of the same operations.

## Encryption and Keychain

Pass an `encryptionKey` when opening to create or reopen an encrypted database. Because the generated `Options` memberwise initializer leaves `drainInsertion` mandatory (UniFFI cannot default an enum field across the FFI boundary), specify it alongside any other option you set — only the bare `Options()` gets it for free:

```swift
import Chisel

// Passphrase, stretched via Argon2id.
let options = Options(drainInsertion: .lruTail, encryptionKey: .passphrase(phrase: "correct horse battery staple"))
let db = try ChiselDatabase(path: someURL, options: options)

// Raw 32-byte key, stretched via HKDF-SHA256 — sourced from the Keychain
// rather than embedded in source or UserDefaults.
let key = try ChiselKey.keychain(service: "org.myapp.chisel", account: "db-key")
let db2 = try ChiselDatabase(path: someURL, options: Options(drainInsertion: .lruTail, encryptionKey: key))
```

`ChiselKey.keychain(service:account:)` (`Keychain.swift`) is a Swift-only convenience with no Python equivalent: it looks up a generic-password Keychain item and wraps its bytes as `.raw(bytes:)`. It throws `KeychainError.itemNotFound` on a lookup miss and `.unhandled(OSStatus)` for any other `SecItemCopyMatching` failure.

Rotate or add credentials with `addKey(existing:new:)`, `rotateKey(old:new:)`, and `removeKey(_:)` on either `ChiselDatabase` or `ChiselStore` — see [`ARCHITECTURE.md#on-disk-encryption`](../ARCHITECTURE.md#on-disk-encryption) for the on-disk envelope layout these operate on.

## Error handling

Every throwing call raises `ChiselError`, generated by UniFFI as a Swift enum with one case per engine error variant. `Extensions.swift` adds a `category` classification on top, sourced from the same operational/fatal split as the Python binding:

```swift
do {
    _ = try await store.transaction { txn in try txn.read(someHandle) }
} catch let error as ChiselError {
    switch error.category {
    case .operational:
        // Database is healthy; the caller made a mistake (bad handle, stale
        // savepoint name, etc.). Log error.message and continue.
        print("operational error: \(error.message)")
    case .fatal:
        // error.isPoisoning is also true here. Drop and reopen:
        print("fatal error, reopening: \(error.message)")
        try? await store.close()
        store = try await ChiselStore(path: someURL)
    }
}
```

`error.isPoisoning` is `category == .fatal` spelled as a single check, for the common case of "should I drop and reopen?" without needing the full category switch. `error.message` (and `ChiselError`'s `CustomStringConvertible` conformance, so `"\(error)"` works too) returns the engine's diagnostic text carried by every case.

## Full API parity

Every method on the Python binding's `Chisel`, `Transaction`, and `Savepoint` classes has a Swift equivalent — see [`PARITY.md`](PARITY.md) for the complete method-by-method mapping, including the two deliberate closure-based mappings (transaction control flow, savepoint control flow) and the small set of Swift-only additions (`fileSizeBytes()`, `ChiselError.category`, `ChiselError.message`, `ChiselKey.keychain`).

## Building and testing

Run these from the **repository root** — `Package.swift` lives there, not in `swift/` — with `CHISEL_LOCAL_XCFRAMEWORK=1` set so the manifest resolves the xcframework you just built instead of trying to fetch a release asset that does not exist yet:

```bash
swift/build-xcframework.sh                               # required once before the package will build at all
CHISEL_LOCAL_XCFRAMEWORK=1 swift build                    # debug
CHISEL_LOCAL_XCFRAMEWORK=1 swift build -c release         # release
CHISEL_LOCAL_XCFRAMEWORK=1 swift test                     # XCTest suite
CHISEL_LOCAL_XCFRAMEWORK=1 swift test --sanitize=thread   # best-effort; some macOS hosts block sanitizer injection into xctest
```

CI (`.github/workflows/swift.yml`) runs the `chisel-ffi` Rust test suite, a cross-compile check for the iOS device/simulator targets, the full xcframework build, and the Swift test suite on `macos-14`.
