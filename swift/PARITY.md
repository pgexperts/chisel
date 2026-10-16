# Python ↔ Swift API parity

Source of truth for the Python surface: `python/chisel/__init__.pyi` (61
methods/properties across `Chisel`, `Transaction`, `Savepoint`, plus the two
module-level functions `open()` and `rekey()` — 63 callables total). Source
of truth for the Swift surface: the hand-written wrapper layer in
`swift/Sources/Chisel/` — `ChiselDatabase.swift` (sync `ChiselDatabase` +
`Transaction` + `Savepoint`), `ChiselStore.swift` (async actor),
`Extensions.swift`, `Keychain.swift` — verified by reading each file
directly, not inferred from the plan. Locations are SYMBOL names, not line
numbers: an earlier revision of this table cited lines, and inserting one
comment block into ChiselDatabase.swift silently invalidated eight of them
while the "verified" claim above stayed put.

Every Python callable below has a Swift equivalent, either as a direct
forwarder or via one of the two deliberate closure mappings described next.

## GAPS (to close before release)

No known gaps. (`Transaction.stats` / `.counters` / `.defrag` — previously the
only omission — were added to the Swift `Transaction` struct in commit closing
this parity table; see the table below.)

## Deliberate divergence: a zero tag

The calls that take a tag (`allocateTagged`, `handlesWithTag`, `deleteTagged`,
`deleteWithTag`) reject tag `0` in both bindings, with different error types.
Python raises the builtin `ValueError`. Swift throws `ChiselError.InvalidTag`,
because the binding's contract is that every throwing call raises
`ChiselError` (ADR 0032).

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
  in ... }` (`ChiselDatabase.transaction(_:)`, `ChiselStore.transaction(_:)`).
  The closure calls `native.begin()` before running the body,
  `native.commit()` on a normal return, and `native.rollback()`
  (best-effort, via `try?`) if the body throws.
- `Savepoint.release` + `Savepoint.rollback_to` (and its `__enter__` /
  `__exit__`) map onto `Transaction.savepoint(name) { sp in ... }`
  (`Transaction.savepoint(_:_:)`), which calls `native.release(name:)` on
  return, and on throw calls `native.rollbackTo(name:)` then
  `native.release(name:)` so a retried scope does not hit
  `DuplicateSavepoint` (ADR 0032; Python's scope does the same, ADR 0025).

This is a deliberate v1 choice, not a gap: it makes "one transaction, always
resolved one way or the other" a structural guarantee (you cannot forget to
commit or roll back — the closure's return path decides it for you) rather
than a documentation convention. Raw manual control is still reachable for
power users through the UniFFI-generated `NativeDatabase` object directly
(`swift/Sources/Chisel/Generated/chisel_ffi.swift`), which does have
`NativeDatabase.begin()`, `NativeDatabase.commit()`,
`NativeDatabase.rollback()`, `NativeDatabase.savepoint(name:)`,
`NativeDatabase.release(name:)`, and `NativeDatabase.rollbackTo(name:)` as
`open func` members — they are simply not re-exposed on the ergonomic
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
scope, or an explicit `try db.close()` — see the `ChiselDatabase` type doc
comment (just above `close()`) for why there is no implicit close-on-deinit.
Not a gap, just a language-level difference in how "run this, then close" is
spelled.

## Full parity table

| Python (`__init__.pyi`) | Swift equivalent | Location |
|---|---|---|
| `open(path, **opts)` | `ChiselDatabase.init(path:options:)` / `ChiselStore.init(path:options:) async` | `ChiselDatabase.init(path:options:)` / `ChiselStore.init(path:options:)` |
| `rekey(path, key)` | `rekey(at:key:argon2Params:)` | Rekey.swift — `rekey(at:key:argon2Params:)` |

### `Chisel` class

| Python (`__init__.pyi`) | Swift equivalent | Location |
|---|---|---|
| `Chisel.is_poisoned` (property) | `.isPoisoned` (property) / `.isPoisoned()` (async) | `ChiselDatabase.isPoisoned` / `ChiselStore.isPoisoned()` |
| `Chisel.__enter__` | N/A — construction via `init` already yields a ready instance | — |
| `Chisel.__exit__` | `defer { try? db.close() }` idiom (no context-manager protocol in Swift) | caller code |
| `Chisel.close` | `.close()` | `ChiselDatabase.close()` / `ChiselStore.close()` |
| `Chisel.transaction()` | `.transaction { }` closure (see deliberate mapping above) | `ChiselDatabase.transaction(_:)` / `ChiselStore.transaction(_:)` |
| `Chisel.begin` | folded into `.transaction { }` entry (see deliberate mapping above) | `ChiselDatabase.transaction(_:)` |
| `Chisel.commit` | folded into `.transaction { }` normal-return path | `ChiselDatabase.transaction(_:)` |
| `Chisel.rollback` | folded into `.transaction { }` throw path | `ChiselDatabase.transaction(_:)` |
| `Chisel.allocate` | `Transaction.allocate(_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.allocate(_:)` |
| `Chisel.read` | `.read(_:)` | `ChiselDatabase.read(_:)` / `ChiselStore.read(_:)` |
| `Chisel.update` | `Transaction.update(_:_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.update(_:_:)` |
| `Chisel.delete` | `Transaction.delete(_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.delete(_:)` |
| `Chisel.delete_many` | `Transaction.deleteMany(_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.deleteMany(_:)` |
| `Chisel.handles` | `.handles()` | `ChiselDatabase.handles()` / `ChiselStore.handles()` |
| `Chisel.allocate_tagged` | `Transaction.allocateTagged(_:tag:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.allocateTagged(_:tag:)` |
| `Chisel.tag` | `.tag(_:)` | `ChiselDatabase.tag(_:)` / `ChiselStore.tag(_:)` |
| `Chisel.handles_with_tag` | `.handlesWithTag(_:)` | `ChiselDatabase.handlesWithTag(_:)` / `ChiselStore.handlesWithTag(_:)` |
| `Chisel.delete_tagged` | `Transaction.deleteTagged(_:tag:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.deleteTagged(_:tag:)` |
| `Chisel.delete_with_tag` | `Transaction.deleteWithTag(_:max:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.deleteWithTag(_:max:)` |
| `Chisel.client_byte` | `.clientByte(_:)` | `ChiselDatabase.clientByte(_:)` / `ChiselStore.clientByte(_:)` |
| `Chisel.set_client_byte` | `Transaction.setClientByte(_:_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.setClientByte(_:_:)` |
| `Chisel.stats` | `.stats()` | `ChiselDatabase.stats()` / `ChiselStore.stats()` |
| `Chisel.counters` | `.counters()` | `ChiselDatabase.counters()` / `ChiselStore.counters()` |
| `Chisel.set_root_name` | `Transaction.setRootName(_:_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.setRootName(_:_:)` |
| `Chisel.get_root_name` | `.getRootName(_:)` | `ChiselDatabase.getRootName(_:)` / `ChiselStore.getRootName(_:)` |
| `Chisel.clear_root_name` | `Transaction.clearRootName(_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.clearRootName(_:)` |
| `Chisel.defrag` | `Transaction.defrag(_:)` — closure-only, no top-level equivalent (see mapping above) | `Transaction.defrag(_:)` |
| `Chisel.add_key` | `.addKey(existing:new:)` | `ChiselDatabase.addKey(existing:new:)` / `ChiselStore.addKey(existing:new:)` |
| `Chisel.rotate_key` | `.rotateKey(old:new:)` | `ChiselDatabase.rotateKey(old:new:)` / `ChiselStore.rotateKey(old:new:)` |
| `Chisel.remove_key` | `.removeKey(_:)` | `ChiselDatabase.removeKey(_:)` / `ChiselStore.removeKey(_:)` |
| `Chisel.set_cache_max_bytes` | `.setCacheMaxBytes(_:)` | `ChiselDatabase.setCacheMaxBytes(_:)` / `ChiselStore.setCacheMaxBytes(_:)` |
| `Chisel.set_spillway_max_bytes` | `.setSpillwayMaxBytes(_:)` | `ChiselDatabase.setSpillwayMaxBytes(_:)` / `ChiselStore.setSpillwayMaxBytes(_:)` |
| `Chisel.set_drain_insertion` | `.setDrainInsertion(_:)` | `ChiselDatabase.setDrainInsertion(_:)` / `ChiselStore.setDrainInsertion(_:)` |

### `Transaction` class (mid-transaction object)

| Python (`__init__.pyi`) | Swift equivalent | Location |
|---|---|---|
| `Transaction.__enter__` | folded into `.transaction { }` entry (see deliberate mapping above) | `ChiselDatabase.transaction(_:)` |
| `Transaction.__exit__` | folded into `.transaction { }` return/throw path | `ChiselDatabase.transaction(_:)` |
| `Transaction.commit` | folded into `.transaction { }` normal-return path — no standalone early-commit | `ChiselDatabase.transaction(_:)` |
| `Transaction.rollback` | folded into `.transaction { }` throw path — no standalone early-rollback | `ChiselDatabase.transaction(_:)` |
| `Transaction.allocate` | `Transaction.allocate(_:)` | `Transaction.allocate(_:)` |
| `Transaction.read` | `Transaction.read(_:)` | `Transaction.read(_:)` |
| `Transaction.update` | `Transaction.update(_:_:)` | `Transaction.update(_:_:)` |
| `Transaction.delete` | `Transaction.delete(_:)` | `Transaction.delete(_:)` |
| `Transaction.delete_many` | `Transaction.deleteMany(_:)` | `Transaction.deleteMany(_:)` |
| `Transaction.allocate_tagged` | `Transaction.allocateTagged(_:tag:)` | `Transaction.allocateTagged(_:tag:)` |
| `Transaction.tag` | `Transaction.tag(_:)` | `Transaction.tag(_:)` |
| `Transaction.handles_with_tag` | `Transaction.handlesWithTag(_:)` | `Transaction.handlesWithTag(_:)` |
| `Transaction.delete_tagged` | `Transaction.deleteTagged(_:tag:)` | `Transaction.deleteTagged(_:tag:)` |
| `Transaction.delete_with_tag` | `Transaction.deleteWithTag(_:max:)` | `Transaction.deleteWithTag(_:max:)` |
| `Transaction.client_byte` | `Transaction.clientByte(_:)` | `Transaction.clientByte(_:)` |
| `Transaction.set_client_byte` | `Transaction.setClientByte(_:_:)` | `Transaction.setClientByte(_:_:)` |
| `Transaction.set_root_name` | `Transaction.setRootName(_:_:)` | `Transaction.setRootName(_:_:)` |
| `Transaction.get_root_name` | `Transaction.getRootName(_:)` | `Transaction.getRootName(_:)` |
| `Transaction.clear_root_name` | `Transaction.clearRootName(_:)` | `Transaction.clearRootName(_:)` |
| `Transaction.handles` | `Transaction.handles()` | `Transaction.handles()` |
| `Transaction.stats` | `Transaction.stats()` | `Transaction.stats()` |
| `Transaction.counters` | `Transaction.counters()` | `Transaction.counters()` |
| `Transaction.defrag` | `Transaction.defrag(_:)` | `Transaction.defrag(_:)` |
| `Transaction.savepoint` | `Transaction.savepoint(_:_:)` closure (see deliberate mapping above) | `Transaction.savepoint(_:_:)` |

### `Savepoint` class

| Python (`__init__.pyi`) | Swift equivalent | Location |
|---|---|---|
| `Savepoint.name` (attribute) | `Savepoint.name` (`public let`) | `Savepoint.name` |
| `Savepoint.__enter__` | folded into `.savepoint { }` entry (see deliberate mapping above) | `Transaction.savepoint(_:_:)` |
| `Savepoint.__exit__` | folded into `.savepoint { }` return/throw path | `Transaction.savepoint(_:_:)` |
| `Savepoint.release` | folded into `.savepoint { }` normal-return path — no standalone early-release | `Transaction.savepoint(_:_:)` |
| `Savepoint.rollback_to` | folded into `.savepoint { }` throw path (rollback, then release) — no standalone early-rollback | `Transaction.savepoint(_:_:)` |

## Swift-only additions (not gaps, informational)

- `ChiselDatabase.fileSizeBytes()` / `ChiselStore.fileSizeBytes()` — forwards
  `NativeDatabase.fileSizeBytes()`, which has no direct Python-side method
  (Python surfaces the same number as `Stats.file_size_bytes` instead of a
  standalone call).
- `ChiselError.category` / `.isPoisoning` (`Extensions.swift`) — a Swift-only
  convenience for bucketing the 39 generated error cases into
  operational/fatal, explicitly sourced from the Python exception hierarchy
  (`python/src/errors.rs`) as its classification authority.
- `ChiselKey.keychain(service:account:)` (`Keychain.swift`) — a Swift-only
  Keychain-backed key source; Python has no equivalent since it has no
  Keychain to bind to.
- `rekey(at:key:argon2Params:)`'s `argon2Params` parameter — Python's `rekey`
  exposes no cost-parameter argument. `nil` reproduces Python's behaviour
  exactly (inherit the unlocking slot's parameters); a supplied value
  re-hardens during the rewrite, which is the natural moment to do it.
