# Swift Binding — Design

Date: 2026-07-18
Status: Approved (design). Superseded by `2026-08-23-swift-binding-port-design.md`.
Topic: A reusable, published Swift package exposing the Chisel storage engine to iOS and macOS applications

---

## 1. Goal and scope

Ship a **reusable, published Swift package** — installable via Swift Package Manager (SPM)
with prebuilt binaries — that lets iOS and macOS application developers embed Chisel as their
storage engine through an idiomatic, modern-Swift API.

The Python binding (`python/`, a PyO3 crate) is the ergonomic template: an `open()` factory, a
database handle, transaction/savepoint scope objects, opaque integer handles/tags, and a
two-tier error tree. This design mirrors that surface for Swift, adapted to Swift's idioms
(`throws`, `enum Error`, closures, `actor`/`async`, `Data`).

**In scope (v1):**

- The full data/transaction surface: CRUD (`allocate`/`read`/`update`/`delete`/`delete_many`),
  transactions (`begin`/`commit`/`rollback`), savepoints (`savepoint`/`rollback_to`/`release`),
  tags (`allocate_tagged`/`tag`/`handles_with_tag`/`delete_tagged`/`delete_with_tag`),
  client byte (`client_byte`/`set_client_byte`), named roots
  (`set_root_name`/`get_root_name`/`clear_root_name`), enumeration (`handles`), `defrag`,
  `stats`/`counters`, `file_size_bytes`, `is_poisoned`, and the between-transaction config
  mutators (`set_cache_max_bytes`/`set_spillway_max_bytes`/`set_drain_insertion`).
- **On-disk encryption**: open-with-key (raw bytes or passphrase), `add_key`/`rotate_key`/
  `remove_key`, Argon2id parameters, and Keychain-aware key retrieval.
- Two concurrency surfaces: a low-level synchronous thread-confined type and a high-level
  `actor` with an `async` API.
- Distribution as an SPM package with a prebuilt `.xcframework`, plus a build-from-source path
  for development.

**Out of scope (documented boundaries, §9):**

- Kotlin/Android bindings. UniFFI could generate them from the same definition later; not built now.
- CocoaPods / Carthage distribution. SPM only for v1.
- Any change to the Chisel engine crate. The engine is consumed unmodified.

---

## 2. Decisions (and why)

| Decision | Choice | Rationale |
|---|---|---|
| Driving use case | Reusable published library | Raises the bar on API polish, ABI/version stability, and prebuilt-binary packaging. |
| Rust→Swift bridge | **UniFFI (Mozilla)** | Generates C scaffolding *and* idiomatic Swift (error enum, `Data` buffers, records, SPM/xcframework). Proven path for Rust engines on iOS (matrix-rust-sdk). Its `Send + Sync` object model turns the single-thread constraint into a clean `Sendable` story. |
| Concurrency surface | **Both** sync core + async actor | Idiomatic `async` facade for apps; synchronous thread-confined escape hatch for power users. Matches the completeness-over-tracer-bullets rule. |
| v1 API scope | **Full surface incl. encryption** | Publish the whole Python-equivalent surface at once rather than dribbling it out. |
| Engine ownership across FFI | `Arc<Mutex<chisel::Chisel>>` **in the FFI crate only** | `Chisel` is `Send + !Sync`; `Mutex<Chisel>` is `Send + Sync`, satisfying UniFFI. The mutex *is* Chisel's "one client at a time" invariant, relocated to the boundary. The engine is never touched. |
| FFI vs Swift responsibility | **FFI flat, ergonomics in Swift** | The FFI crate exports flat methods mirroring `chisel::Chisel`; closures/actor/scoping are hand-written in Swift. Keeps UniFFI free of foreign-callback machinery. Mirrors how the Python binding wraps flat Rust calls. |
| Byte values | Swift `Data` | UniFFI maps `Vec<u8>` → `Data`; idiomatic for a storage engine. |
| Error model | One `enum ChiselError: Error` + `.category` | Swift enums can't subclass, so Python's `Operational`/`Fatal` class tree flattens to one enum with a computed operational/fatal discriminator and an `.isPoisoning` convenience. |
| Distribution | SPM + prebuilt `.xcframework` | Dev builds from source; releases ship a checksummed remote binary target. |
| Minimum deployment target | **iOS 15 / macOS 12** (adjustable) | Async facade uses `withCheckedContinuation` + a dedicated `DispatchQueue`, avoiding the iOS 17 custom-executor floor. |

---

## 3. Architecture

### 3.1 Layer cake

Each layer depends only on the one below it.

```
┌─────────────────────────────────────────────────────────┐
│  Swift app (iOS / macOS, async/await, Swift 6)           │
├─────────────────────────────────────────────────────────┤
│  Chisel (Swift package)                    hand-written  │
│   • ChiselStore    — actor, async facade   [high-level]  │
│   • ChiselDatabase — sync, thread-confined [low-level]   │
│   • Transaction / Savepoint scope closures               │
│   • ChiselError, Options, Stats, Tag, Handle, ChiselKey  │
├─────────────────────────────────────────────────────────┤
│  chisel (generated .swift)                    generated  │
│  chiselFFI (C header + modulemap, in xcframework)        │
├─────────────────────────────────────────────────────────┤
│  chisel-ffi  (new Rust crate, 4th workspace member) new  │
│   • ChiselDatabase { inner: Mutex<chisel::Chisel> }      │
│   • #[uniffi::export] flat methods + error conversion    │
├─────────────────────────────────────────────────────────┤
│  chisel  (the engine — UNCHANGED)                        │
└─────────────────────────────────────────────────────────┘
```

### 3.2 Repository changes

- **New workspace member `chisel-ffi/`** — crate-types `["staticlib", "cdylib", "lib"]`, a
  path-dep on `chisel`, plus `uniffi`. Added to `[workspace] members`; kept **out of
  `default-members`** so `cargo build`/`test` from the root are unaffected — the exact pattern
  `python` already follows.
- **New top-level `swift/` directory** — the Swift package (`Package.swift`,
  `build-xcframework.sh`, generated + hand-written sources, tests). The built
  `Chisel.xcframework` is **`.gitignore`'d**, not committed.

### 3.3 The concurrency spine

The FFI crate wraps the engine:

```rust
pub struct ChiselDatabase { inner: Mutex<chisel::Chisel> }
```

Because `chisel::Chisel` is `Send` (uses `RefCell`, no `Rc`/raw-pointer markers) and `!Sync`,
`Mutex<Chisel>` is `Send + Sync`. Every exported method locks `inner` for the duration of the
call, serializing access exactly as the borrow checker's `&mut self` did in Rust.

UniFFI hands foreign code an `Arc<ChiselDatabase>` for exported objects, so the effective type
crossing the boundary is `Arc<ChiselDatabase>` wrapping `Mutex<Chisel>` — i.e. the conceptual
`Arc<Mutex<Chisel>>` referenced elsewhere in this document. The `Arc` is UniFFI's; the `Mutex` is
ours.

**De-risking the `Send` assumption.** `Send` was inferred from the absence of `Rc`/raw pointers,
but a future mmap or OS handle could block it. The FFI crate carries a compile-time assertion:

```rust
const _: () = { fn assert_send<T: Send>() {} let _ = assert_send::<chisel::Chisel>; };
```

If the engine ever becomes `!Send`, `chisel-ffi` fails to compile with a clear message. The
documented fallback is a **dedicated-thread actor**: one OS thread owns the `Chisel`, and
exported methods marshal calls to it over a channel. This changes only the FFI crate's internals,
not the Swift API.

---

## 4. The C-ABI / UniFFI surface

`chisel-ffi` exports **flat methods** that mirror `chisel::Chisel` one-to-one via
`#[uniffi::export]`. No transaction/savepoint *scoping* lives here — that is a Swift-layer
concern (§5.3). The exported object is `ChiselDatabase`; constructors are `new(path, options)`
and `new_in_memory(options)`.

### 4.1 Type mapping

| Rust | UniFFI | Swift surface |
|---|---|---|
| `Handle(u64)` | custom type over `u64` | `struct Handle: Hashable, Sendable` |
| `Tag(NonZeroU32)` | custom type over `u32` | `struct Tag: Hashable, Sendable` (failable init rejects 0) |
| `&[u8]` / `Vec<u8>` | `bytes` | `Data` |
| `Options` | Record | `struct Options` with a defaulted memberwise init |
| `DrainInsertion` | Enum | `enum DrainInsertion` |
| `Stats` | Record | `struct Stats` |
| `ChiselCounters` | Record | `struct Counters` |
| `DefragOptions` / `DefragStats` | Records | value structs |
| `TagDropProgress` | Record | `struct TagDropProgress { deleted: [Handle]; complete: Bool }` |
| `Argon2Params` | Record | `struct Argon2Params` |
| `Key` | Enum | `enum ChiselKey { raw(Data) / passphrase(String) }` |
| `ChiselError` | Error enum | `enum ChiselError: Error` (§4.2) |

`Handle` and `Tag` are UniFFI custom types (newtype structs over the integer) so the two cannot
be accidentally interchanged at a call site.

### 4.2 Error mapping

Python's `ChiselError → OperationalError | FatalError → <specific>` class hierarchy flattens to a
single Swift enum, because Swift enums cannot subclass:

```swift
public enum ChiselError: Error, Sendable {
    // operational — database state intact; caller mistake or transient condition
    case invalidHandle
    case noActiveTransaction
    case transactionAlreadyActive
    case savepointNotFound(name: String)
    case duplicateSavepoint(name: String)
    case readOnlyMode
    case databaseFileNotFound
    case invalidRootName
    case rootNameTableFull
    case invalidSuperblockCount
    case cacheFull
    case spillwayFull
    case transactionInProgress
    case closed
    case alreadyFinished
    case tagMismatch
    case noEncryptionKey
    case invalidEncryptionKey
    case encryptionNotSupported
    case noFreeKeySlot
    case lastKeySlot

    // fatal — database poisoned or on-disk state suspect
    case io(kind: String, errno: Int32?)
    case checksumMismatch
    case corruptSuperblock
    case fileSizeMismatch
    case lockFailed
    case unsupportedFormatVersion
    case unsupportedPageSize
    case corruptPage
    case invalidPageId
    case poisoned
    case decryptionFailed

    public enum Category { case operational, fatal }
    public var category: Category { /* mapped per case */ }
    public var isPoisoning: Bool { category == .fatal }
}
```

`.io(kind:errno:)` preserves the Python `IoError.kind`/`.errno` data (I42): `kind` is the string
form of `std::io::ErrorKind`; `errno` is the platform `raw_os_error` when present.

A Rust-side conversion (`impl From<chisel::ChiselError> for ChiselError`, the UniFFI error enum)
maps every engine variant to exactly one case. §7.1 requires a test asserting the mapping is total.

---

## 5. The Swift public API

### 5.1 Low-level: `ChiselDatabase` (synchronous, thread-confined)

```swift
public final class ChiselDatabase {           // NOT Sendable; caller owns threading
    public init(path: URL?, options: Options = .init()) throws
    public func close() throws
    public var isPoisoned: Bool { get }

    // flat mirror of chisel::Chisel — allocate/read/update/delete/delete_many,
    // begin/commit/rollback, savepoint/rollback_to/release,
    // allocate_tagged/tag/handles_with_tag/delete_tagged/delete_with_tag,
    // client_byte/set_client_byte, set_root_name/get_root_name/clear_root_name,
    // handles, stats/counters/file_size_bytes, defrag,
    // add_key/rotate_key/remove_key, set_cache_max_bytes/…

    // ergonomic scoping (see §5.3)
    public func transaction<R>(_ body: (Transaction) throws -> R) throws -> R
}
```

`path: nil` opens an in-memory database.

### 5.2 High-level: `ChiselStore` (actor, async)

```swift
public actor ChiselStore {
    public init(path: URL?, options: Options = .init()) async throws
    public func allocate(_ value: Data) async throws -> Handle
    public func read(_ handle: Handle) async throws -> Data
    // …full async mirror of the surface…
    public func transaction<R: Sendable>(
        _ body: @Sendable (Transaction) throws -> R
    ) async throws -> R
}
```

`ChiselStore` owns a `ChiselDatabase` plus a **dedicated serial `DispatchQueue`**. Every method
bridges to that queue via `withCheckedThrowingContinuation`, so the blocking engine calls run on
the store's private thread and **never block Swift's cooperative thread pool**. The queue's
serialization is redundant with the FFI mutex but makes the "off the cooperative pool" property
explicit and independent of the lock.

### 5.3 Transactions and savepoints as closures

The Python `Transaction`/`Savepoint` context managers become Swift closures. The block runs to
completion; a normal return commits (or releases), a thrown error rolls back and rethrows:

```swift
let handle = try await store.transaction { txn in
    let h = try txn.allocate(Data("hello".utf8))
    try txn.setRootName("greeting", h)
    return h
}   // commit on return; throw → rollback → rethrow
```

`Transaction` (and `Savepoint`) are lightweight façades forwarding to the owning database; they
are valid only inside the closure (non-escaping, not `Sendable`).

**The transaction body is synchronous by design.** `body` is a non-`async`, `@Sendable`
closure. If it were `async`, an `await` inside could suspend the task while the db lock is held,
letting the actor re-enter and interleave two logical transactions on one `!Sync` engine. A
synchronous body makes "one transaction, start to finish" a type-level guarantee. `await` occurs
only on entering/leaving the actor, never mid-transaction.

Top-level flat mutators (`store.allocate(_:)` outside any `transaction {}`) mirror the engine's
exact transaction semantics; the closure is the recommended path.

### 5.4 Encryption and key handling

```swift
public enum ChiselKey: Sendable {
    case raw(Data)               // high-entropy key bytes → HKDF
    case passphrase(String)      // human secret → Argon2id
    public static func keychain(service: String, account: String) throws -> ChiselKey
}
// Options.encryptionKey: ChiselKey?
// Options.argon2Params: Argon2Params?      // create-time cost params for passphrases
// store.addKey(existing:new:) / rotateKey(old:new:) / removeKey(_:)
```

Key bytes are handed straight across the FFI, where Rust wraps them in `Zeroizing`. The
`keychain(service:account:)` helper retrieves bytes via `SecItemCopyMatching` and passes them
through with minimal lifetime. **Honest, documented limitation:** Swift `Data`/`String` do not
guarantee zeroing on release, so the Swift-side copy is wiped best-effort; Rust-side zeroization
is guaranteed.

---

## 6. Packaging and distribution

### 6.1 Build pipeline → `Chisel.xcframework`

`swift/build-xcframework.sh` performs the standard UniFFI flow:

1. `cargo build --release -p chisel-ffi` for five targets:
   - `aarch64-apple-ios` (device)
   - `aarch64-apple-ios-sim` + `x86_64-apple-ios` → `lipo` into one **simulator** fat lib
   - `aarch64-apple-darwin` + `x86_64-apple-darwin` → `lipo` into one **macOS** fat lib
   - *(Mac Catalyst `*-macabi` optional; default off.)*
2. `uniffi-bindgen generate --library <lib>` → generated `chisel.swift`, `chiselFFI.h`, modulemap.
3. `xcodebuild -create-xcframework` bundling the three static-lib slices + headers →
   `Chisel.xcframework`.

### 6.2 SPM layout

```
swift/
├── Package.swift
├── build-xcframework.sh
├── Sources/Chisel/           # generated chisel.swift + hand-written ergonomic layer
├── Tests/ChiselTests/
└── Chisel.xcframework         # .gitignore'd — built locally / downloaded for release
```

- **Dev:** `.binaryTarget(name: "ChiselFFI", path: "Chisel.xcframework")` — built from source.
- **Release:** on a tag, `.binaryTarget(url: "…/releases/…/Chisel.xcframework.zip",
  checksum: "<sha256>")` — a prebuilt binary attached to a GitHub Release.
- Product: `.library(name: "Chisel", targets: ["Chisel"])`. The `Chisel` target imports the
  generated C module `chiselFFI` and exposes the idiomatic surface; test target `ChiselTests`.

The xcframework is never committed (large, multi-arch): `.gitignore` it, build in dev, ship via
Releases.

---

## 7. Testing

### 7.1 Rust (`chisel-ffi`)

- The `Mutex` wrapper and the `Send` compile-assertion.
- A round-trip test asserting **every** `chisel::ChiselError` variant maps to a distinct
  `ChiselError` UniFFI case (catches silently-dropped errors; the mapping must be total).

### 7.2 Swift (XCTest)

- Behavioral suite: open/close, CRUD, `transaction` commit/rollback, savepoints, tags, client
  byte, named roots, `handles`, `defrag`, `stats`/`counters`.
- Encryption: create encrypted → reopen with key → wrong key yields `.invalidEncryptionKey`;
  supplying a key to a plaintext DB yields `.encryptionNotSupported`; `add_key`/`rotate_key`/
  `remove_key` round-trips; `remove_key` on the last slot yields `.lastKeySlot`.
- Poison behavior: a poisoning failure surfaces as `.isPoisoning == true`.
- **Concurrency:** fan many `Task`s at one `ChiselStore`, assert serialized results and no data
  race, run under **Thread Sanitizer**.

### 7.3 Parity table

A checked-in map (`swift/PARITY.md` or a test fixture) of every Python-binding method → Swift
method, so the Swift surface cannot silently drift below the Python one. Reviewed as a diff.

---

## 8. CI and documentation

- **CI:** a new **macOS** GitHub Actions job (separate from the Linux engine job): build
  `chisel-ffi` + run its Rust tests → build the xcframework (macOS + simulator slices for
  speed) → `swift test` under TSan → `swift build`. Correctness gates the pipeline; any Swift
  benchmark is report-only.
- **Docs:** `swift/README.md` (SPM install, quick start, concurrency + Keychain guide) + DocC
  comments on the public API. Update root `README.md` and `ARCHITECTURE.md` to list Swift as a
  third consumer alongside `python`/`bench`.
- **ADR:** after the first implementation push, record "Swift binding via UniFFI" as a new
  decision via the codebase-memory skill (post-implementation, per the project's ADR discipline).

---

## 9. Known boundaries and risks

- **`Send` assumption** — mitigated by the compile-time assertion (§3.3) and the dedicated-thread
  fallback. Confirm at implementation time.
- **Top-level-mutator transaction semantics** — the Swift flat mutators mirror the engine's
  exact auto-transaction-or-error behavior; the precise semantic is confirmed against
  `chisel::Chisel` when writing the plan. Does not affect the API *shape* (both `db.allocate`
  and `txn.allocate` exist regardless).
- **Swift key-material zeroization** — best-effort only on the Swift side (Foundation limitation);
  guaranteed on the Rust side.
- **No Kotlin/Android, no CocoaPods/Carthage** in v1 (§1). UniFFI leaves the Kotlin door open.
- **xcframework size** — static libs across five architectures are large; shipped via Releases,
  not committed.

---

## 10. Open questions for the implementation plan

1. Confirm `chisel::Chisel: Send` compiles (the assertion); if not, switch `chisel-ffi` to the
   dedicated-thread actor internally.
2. Confirm the exact transaction requirement of each flat mutator against the engine.
3. Decide whether Mac Catalyst slices are included in v1 (default: no).
4. Confirm the minimum deployment target (default: iOS 15 / macOS 12).
