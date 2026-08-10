---
id: 0025
title: Make finished Python transaction and savepoint objects inert
date: 2026-08-10
status: Accepted
summary: Every PyTransaction data method checks the one-shot guard, rollback_to is repeatable, and a savepoint scope releases on both the clean and the exception exit.
---

# 0025. Make finished Python transaction and savepoint objects inert

## Context

`PyTransaction` carries no transaction identity — only a handle on the database — so a
finished wrapper is indistinguishable from a live one and re-binds to whatever transaction
the engine currently has open. The one-shot `finished` guard was checked by `commit()` and
`rollback()` and by none of the 20 data methods, so a stale `tx` held past its block
silently injected writes into an unrelated unit of work.

Separately, `rollback_to` SET the guard, making the binding stricter than the engine, which
deliberately keeps the mark on the stack. Because `__exit__` short-circuits on the same
flag, a savepoint whose body rolled back was never released and its name stayed taken for
the rest of the transaction, with nothing able to free it.

## Decision

A finished transaction object is dead: a shared `check_live()` runs first in every data
method, reads included. `rollback_to` CHECKS the guard but does not SET it, so it is
repeatable, matching the engine. A savepoint scope releases on BOTH exits — clean, and
after rolling back on an exception — so the name always returns to the enclosing scope
when the block ends.

Exception classes carry the importable `chisel._chisel` module path so they pickle.

## Alternatives considered

- **Guard only the mutating methods** (what the issue proposed) — rejected: a post-finish
  read is the same defect, returning whatever the engine currently holds rather than the
  finished transaction's snapshot. A uniform rule is also the only one that is easy to
  state and to sweep in a test.
- **Keep the one-shot rollback_to and rewrite the README** — rejected: it leaves an engine
  capability unreachable from Python and the savepoint name burned with no way to free it.
- **Fix only the clean-exit release** — rejected: the exception arm is the path a retry
  loop actually takes.

## Consequences

Behaviour changes for callers who were relying on the old leniency: a finished `tx`
raises `AlreadyFinishedError` from every method, and a second `rollback_to` no longer
raises. One existing test asserted the old savepoint semantics and was replaced.

Exception `__module__` values changed from `_chisel` to `chisel._chisel`. Nothing matched
on the old value, and pickling now round-trips — which matters because an exception raised
in a worker process is pickled to be re-raised in the parent, so the two-tier
operational/fatal contract previously collapsed into a `PicklingError` exactly where it
mattered most.

The Swift binding has the mirror-image savepoint defect (SWIFT-3); the shape chosen here
is the one to mirror when that binding lands.
