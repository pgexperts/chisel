# Python ↔ Swift API parity

Source of truth for the Python surface: `python/chisel/chisel.pyi` (61 methods/
properties across `Chisel`, `Transaction`, `Savepoint`, plus the module-level
`open()` — 62 callables total). Source of truth for the Swift surface: the
hand-written wrapper layer in `swift/Sources/Chisel/` — `ChiselDatabase.swift`
(sync `ChiselDatabase` + `Transaction` + `Savepoint`), `ChiselStore.swift`
(async actor), `Extensions.swift`, `Keychain.swift` — verified by reading each
file directly, not inferred from the plan.

Every Python callable below has a Swift equivalent, either as a direct
forwarder or via one of the two deliberate closure mappings described next.

## GAPS (to close before release)

No known gaps. (`Transaction.stats` / `.counters` / `.defrag` — previously the
only omission — were added to the Swift `Transaction` struct in commit closing
this parity table; see the table below.)

## Deliberate mapping: transaction/savepoint control flow

Python exposes raw `Chisel.begin()` / `.commit()` / `.rollback()`,
`Transaction.commit()` / `.rollback()`, and `Savepoint.release()` /
`.rollback_to()` as standalone methods, callable either directly or through
the `__enter__`/`__exit__` context-manager protocol. The Swift
`ChiselDatabase` / `ChiselStore` **intentionally do not** expose any of these
as standalone methods. Instead:

- `Chisel.begin` + `Chisel.commit` + `Chisel.rollback` (and
  `Transaction.commit` / `Transaction.rollback`, and the `with` / `__enter__`
  / `__exit__` protocol on both) all map onto a single Swift construct:
  `ChiselDatabase.transaction { txn in ... }` / `ChiselStore.transaction { txn
  in ... }` (`ChiselDatabase.swift:117-128`, `ChiselStore.swift:198-203`). The
  closure calls `native.begin()` before running the body, `native.commit()`
  on a normal return, and `native.rollback()` (best-effort, via `try?`) if
  the body throws.
- `Savepoint.release` + `Savepoint.rollback_to` (and its `__enter__` /
  `__exit__`) map onto `Transaction.savepoint(name) { sp in ... }`
  (`ChiselDatabase.swift:184-195`), which calls `native.release(name:)` on
  return and `native.rollbackTo(name:)` on throw.

This is a deliberate v1 choice, not a gap: it makes "one transaction, always
resolved one way or the other" a structural guarantee (you cannot forget to
commit or roll back — the closure's return path decides it for you) rather
than a documentation convention. Raw manual control is still reachable for
power users through the UniFFI-generated `NativeDatabase` object directly
(`swift/Sources/Chisel/Generated/chisel_ffi.swift`), which does have
`begin()`, `commit()`, `rollback()`, `savepoint(name:)`, `release(name:)`,
and `rollbackTo(name:)` as `open func` members (verified at lines 819, 854,
996, 1026, 974, 1005) — they are simply not re-exposed on the ergonomic
wrapper types.

## Deliberate mapping: mutations are closure-only

Python's `Chisel.allocate` / `.update` / `.delete` / `.delete_many` /
`.allocate_tagged` / `.delete_tagged` / `.delete_with_tag` /
`.set_client_byte` / `.set_root_name` / `.clear_root_name` / `.defrag` are
flat top-level methods on `Chisel`. The engine requires an active
transaction for every one of them and there is no auto-begin, so a Swift
top-level `ChiselDatabase.allocate(...)` (or `ChiselStore.allocate(...)`)
could never succeed — it would always throw `.NoActiveTransaction`. These
are therefore **not** mirrored on `ChiselDatabase` / `ChiselStore` at all;
they exist only on `Transaction`, reachable via `transaction { txn in
txn.allocate(...) }` / `store.transaction { txn in ... }`. The parity table
below reflects this: each such row's Swift equivalent is `Transaction.*`,
not `ChiselDatabase.*` / `ChiselStore.*`.

Separately, Python's `with chisel.open(...) as db:` (`Chisel.__enter__` /
`.__exit__`) closes the database on scope exit — `python/src/db.rs:311-314`
confirms `__exit__` calls `self.close()`. Swift has no context-manager
protocol; the equivalent idiom is `defer { try? db.close() }` around the
scope, or an explicit `try db.close()` — see `ChiselDatabase.swift:39-44` for
why there is no implicit close-on-deinit. Not a gap, just a language-level
difference in how "run this, then close" is spelled.

## Full parity table

| Python (`chisel.pyi`) | Swift equivalent | Location |
|---|---|---|
| `open(path, **opts)` | `ChiselDatabase.init(path:options:)` / `ChiselStore.init(path:options:) async` | ChiselDatabase.swift:49 / ChiselStore.swift:37 |

### `Chisel` class

| Python (`chisel.pyi`) | Swift equivalent | Location |
|---|---|---|
| `Chisel.is_poisoned` (property) | `.isPoisoned` (property) / `.isPoisoned()` (async) | ChiselDatabase.swift:54 / ChiselStore.swift:65 |
| `Chisel.__enter__` | N/A — construction via `init` already yields a ready instance | — |
| `Chisel.__exit__` | `defer { try? db.close() }` idiom (no context-manager protocol in Swift) | caller code |
| `Chisel.close` | `.close()` | ChiselDatabase.swift:57 / ChiselStore.swift:74 |
| `Chisel.transaction()` | `.transaction { }` closure (see deliberate mapping above) | ChiselDatabase.swift:117 / ChiselStore.swift:198 |
| `Chisel.begin` | folded into `.transaction { }` entry (see deliberate mapping above) | ChiselDatabase.swift:118 |
| `Chisel.commit` | folded into `.transaction { }` normal-return path | ChiselDatabase.swift:122 |
| `Chisel.rollback` | folded into `.transaction { }` throw path | ChiselDatabase.swift:125 |
| `Chisel.allocate` | `Transaction.allocate(_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:142 |
| `Chisel.read` | `.read(_:)` | ChiselDatabase.swift:58 / ChiselStore.swift:88 |
| `Chisel.update` | `Transaction.update(_:_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:146 |
| `Chisel.delete` | `Transaction.delete(_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:148 |
| `Chisel.delete_many` | `Transaction.deleteMany(_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:150 |
| `Chisel.handles` | `.handles()` | ChiselDatabase.swift:65 / ChiselStore.swift:94 |
| `Chisel.allocate_tagged` | `Transaction.allocateTagged(_:tag:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:152 |
| `Chisel.tag` | `.tag(_:)` | ChiselDatabase.swift:73 / ChiselStore.swift:120 |
| `Chisel.handles_with_tag` | `.handlesWithTag(_:)` | ChiselDatabase.swift:75 / ChiselStore.swift:126 |
| `Chisel.delete_tagged` | `Transaction.deleteTagged(_:tag:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:158 |
| `Chisel.delete_with_tag` | `Transaction.deleteWithTag(_:max:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:160 |
| `Chisel.client_byte` | `.clientByte(_:)` | ChiselDatabase.swift:79 / ChiselStore.swift:134 |
| `Chisel.set_client_byte` | `Transaction.setClientByte(_:_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:164 |
| `Chisel.stats` | `.stats()` | ChiselDatabase.swift:67 / ChiselStore.swift:100 |
| `Chisel.counters` | `.counters()` | ChiselDatabase.swift:69 / ChiselStore.swift:106 |
| `Chisel.set_root_name` | `Transaction.setRootName(_:_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:166 |
| `Chisel.get_root_name` | `.getRootName(_:)` | ChiselDatabase.swift:83 / ChiselStore.swift:142 |
| `Chisel.clear_root_name` | `Transaction.clearRootName(_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:170 |
| `Chisel.defrag` | `Transaction.defrag(_:)` — closure-only, no top-level equivalent (see mapping above) | ChiselDatabase.swift:178 |
| `Chisel.add_key` | `.addKey(existing:new:)` | ChiselDatabase.swift:99 / ChiselStore.swift:150 |
| `Chisel.rotate_key` | `.rotateKey(old:new:)` | ChiselDatabase.swift:101 / ChiselStore.swift:156 |
| `Chisel.remove_key` | `.removeKey(_:)` | ChiselDatabase.swift:103 / ChiselStore.swift:162 |
| `Chisel.set_cache_max_bytes` | `.setCacheMaxBytes(_:)` | ChiselDatabase.swift:107 / ChiselStore.swift:170 |
| `Chisel.set_spillway_max_bytes` | `.setSpillwayMaxBytes(_:)` | ChiselDatabase.swift:109 / ChiselStore.swift:176 |
| `Chisel.set_drain_insertion` | `.setDrainInsertion(_:)` | ChiselDatabase.swift:111 / ChiselStore.swift:182 |

### `Transaction` class (mid-transaction object)

| Python (`chisel.pyi`) | Swift equivalent | Location |
|---|---|---|
| `Transaction.__enter__` | folded into `.transaction { }` entry (see deliberate mapping above) | ChiselDatabase.swift:117 |
| `Transaction.__exit__` | folded into `.transaction { }` return/throw path | ChiselDatabase.swift:120-127 |
| `Transaction.commit` | folded into `.transaction { }` normal-return path — no standalone early-commit | ChiselDatabase.swift:122 |
| `Transaction.rollback` | folded into `.transaction { }` throw path — no standalone early-rollback | ChiselDatabase.swift:125 |
| `Transaction.allocate` | `Transaction.allocate(_:)` | ChiselDatabase.swift:142 |
| `Transaction.read` | `Transaction.read(_:)` | ChiselDatabase.swift:144 |
| `Transaction.update` | `Transaction.update(_:_:)` | ChiselDatabase.swift:146 |
| `Transaction.delete` | `Transaction.delete(_:)` | ChiselDatabase.swift:148 |
| `Transaction.delete_many` | `Transaction.deleteMany(_:)` | ChiselDatabase.swift:150 |
| `Transaction.allocate_tagged` | `Transaction.allocateTagged(_:tag:)` | ChiselDatabase.swift:152 |
| `Transaction.tag` | `Transaction.tag(_:)` | ChiselDatabase.swift:154 |
| `Transaction.handles_with_tag` | `Transaction.handlesWithTag(_:)` | ChiselDatabase.swift:156 |
| `Transaction.delete_tagged` | `Transaction.deleteTagged(_:tag:)` | ChiselDatabase.swift:158 |
| `Transaction.delete_with_tag` | `Transaction.deleteWithTag(_:max:)` | ChiselDatabase.swift:160 |
| `Transaction.client_byte` | `Transaction.clientByte(_:)` | ChiselDatabase.swift:162 |
| `Transaction.set_client_byte` | `Transaction.setClientByte(_:_:)` | ChiselDatabase.swift:164 |
| `Transaction.set_root_name` | `Transaction.setRootName(_:_:)` | ChiselDatabase.swift:166 |
| `Transaction.get_root_name` | `Transaction.getRootName(_:)` | ChiselDatabase.swift:168 |
| `Transaction.clear_root_name` | `Transaction.clearRootName(_:)` | ChiselDatabase.swift:170 |
| `Transaction.handles` | `Transaction.handles()` | ChiselDatabase.swift:172 |
| `Transaction.stats` | `Transaction.stats()` | ChiselDatabase.swift:174 |
| `Transaction.counters` | `Transaction.counters()` | ChiselDatabase.swift:176 |
| `Transaction.defrag` | `Transaction.defrag(_:)` | ChiselDatabase.swift:178 |
| `Transaction.savepoint` | `Transaction.savepoint(_:_:)` closure (see deliberate mapping above) | ChiselDatabase.swift:184 |

### `Savepoint` class

| Python (`chisel.pyi`) | Swift equivalent | Location |
|---|---|---|
| `Savepoint.name` (attribute) | `Savepoint.name` (`public let`) | ChiselDatabase.swift:209 |
| `Savepoint.__enter__` | folded into `.savepoint { }` entry (see deliberate mapping above) | ChiselDatabase.swift:184 |
| `Savepoint.__exit__` | folded into `.savepoint { }` return/throw path | ChiselDatabase.swift:187-194 |
| `Savepoint.release` | folded into `.savepoint { }` normal-return path — no standalone early-release | ChiselDatabase.swift:189 |
| `Savepoint.rollback_to` | folded into `.savepoint { }` throw path — no standalone early-rollback | ChiselDatabase.swift:192 |

## Swift-only additions (not gaps, informational)

- `ChiselDatabase.fileSizeBytes()` / `ChiselStore.fileSizeBytes()`
  (`ChiselDatabase.swift:81`, `ChiselStore.swift:112`) — forwards
  `NativeDatabase.fileSizeBytes()`, which has no direct Python-side method
  (Python surfaces the same number as `Stats.file_size_bytes` instead of a
  standalone call).
- `ChiselError.category` / `.isPoisoning` (`Extensions.swift`) — a Swift-only
  convenience for bucketing the 32 generated error cases into
  operational/fatal, explicitly sourced from the Python tiering
  (`python/src/errors.rs:101-172`) as its classification authority.
- `ChiselKey.keychain(service:account:)` (`Keychain.swift`) — a Swift-only
  Keychain-backed key source; Python has no equivalent since it has no
  Keychain to bind to.
