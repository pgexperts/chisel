# Chisel Performance Review — 2026-04-26

Date: 2026-04-26 (UTC)
Reviewer: chisel-performance skill, fresh-eyes pass
Commit: 1842652 (main, immediately after PR-1+PR-2 merge and doc refresh)
Prior review: none

This review applies the `chisel-performance` skill's framework — the cost
model, the seven optimization levers, and especially the **Don't-Break List**
and the **Performance Review Checklist** — against the engine as currently
shipped on `main`. Focus areas: the recent PR-1 (counter instrumentation)
and PR-2 (bench foundation) work, plus a fresh pass over hot paths the
skill specifically calls out.

## Executive summary

1. **`delete_many` is a thin loop, not a batched primitive.** The skill's
   own description, the bench-suite spec, and prior commentary all imply
   that `delete_many` does something more efficient than `delete()` in a
   loop — it doesn't. The implementation is `for h in handles { delete(h) }`
   with shared transaction state. Real batching would visit shared
   handle-table subtrees once instead of N times. (DESIGN, important.)

2. **No zero-copy read API.** `Chisel::read(handle) -> Result<Vec<u8>>`
   allocates and memcpy's on every call. For workloads with hot reads on
   warm cache, this is the dominant per-op cost. Lever 7 territory; not a
   bug but a real ceiling on read throughput for in-process Rust callers.
   (SMELL.)

3. **Per-call cache instrumentation overhead, by design.** PR 1 added
   `Cell<u64>::set(get() + 1)` to every `PageCache::get`/`get_mut`/`new_page`.
   On in-memory workloads at 100M cache hits/sec, that's ~200 ms/sec of
   pure counter overhead. Documented and intentional; worth a feature flag
   if the cost shows up in real profiles. (SMELL.)

4. **CI lacks `cargo audit` / `cargo deny`.** Test/clippy/fmt are in
   `.github/workflows/ci.yml` but no supply-chain check runs. Outside the
   chisel-performance skill's scope but worth flagging while reviewing.
   (DESIGN.)

5. **PRs 1 and 2 respect all 10 don't-break-list items.** No new fsyncs,
   no commit-protocol reordering, no on-disk format change, no dropped
   poison signals on the engine side, no cross-layer dependencies. The
   bench-suite work landed cleanly. (Positive observation.)

## Findings

### F1 (DESIGN, important): `delete_many` is a thin loop over `delete_inner`

**Location:** `src/transaction.rs:1397–1405`

**What the code does:**
```rust
fn delete_many_inner(&mut self, handles: &[u64]) -> Result<()> {
    if !self.active_txn {
        return Err(ChiselError::NoActiveTransaction);
    }
    for &handle in handles {
        self.delete_inner(handle)?;
    }
    Ok(())
}
```

**Why it's a problem.** The chisel-performance skill describes `delete_many`
as having a real optimization: "batches the handle-table COW work into one
transaction-internal pass instead of N." The bench-suite spec
(§2.2 / §3.1 row 9) treats `delete_many(1000)` as meaningfully different
from `delete()` × 1000. The actual implementation is the latter spelled
differently.

The amortization argument — "consecutive deletes within one transaction
share already-COW'd interior nodes via `current_roots`" — is real but
limited. Each `delete_inner` walks from `current_roots.handle_table_page`
to the target leaf (`O(depth)` cache_get calls), modifies the leaf, and
COWs. Shared interior nodes that were COW'd by an earlier delete remain
in the cache as dirty entries, so the next delete's walk hits them as
cache hits — that's where the amortization comes from. But each delete
still:

- Calls `check_alive()` is amortized: `delete_many` calls it once.
- Calls `lookup` independently: walks the (current) root once per handle.
- Modifies the leaf independently: 1 leaf-page COW per *unique destination
  leaf*. If 1000 handles span 5 leaves, that's 5 leaf COWs. But each delete
  call still does its own `find_leaf` descent.

A real batch would: (1) sort handles by their target leaf, (2) descend the
tree once per leaf-bucket, (3) write all tombstones for that leaf in one
COW pass. For dense delete patterns (e.g., "drop this entire range"), this
could be 5–10× faster than the loop. For sparse deletes (one per leaf),
the wins evaporate.

**Direction of fix.** Two options.

(a) Document the gap honestly: rename or comment `delete_many` to clarify
it's a syntactic convenience whose `O(handles)` cost is no better than
`delete()` in a loop. Update the chisel-performance skill, the bench-suite
spec, and any other commentary that overstates its efficiency. Cheap.

(b) Implement actual batching: sort by leaf bucket, descend once per
unique leaf, write tombstones in bulk. Non-trivial work; gated on
demonstrated need (a workload where bulk-delete latency matters more
than the existing fsync floor).

I'd land (a) before PR 4 of the bench-suite series so its row-8-vs-row-9
comparison ("`delete` × 1000 vs `delete_many(1000)`") doesn't get
mis-interpreted. (b) is a real future optimization but YAGNI today.

### F2 (SMELL, minor): `read()` always allocates a `Vec<u8>` via `to_vec()`

**Location:** `src/transaction.rs:1215, 1218–1230` (inline path); same
pattern via `Overflow::read` at `src/overflow.rs:146` for chains.

**What the code does:** `read_inner` calls `cache.get(entry.page_id)`,
gets `&[u8; PAGE_SIZE]`, calls `DataPage::read(buf, slot_index)` to get a
`&[u8]` borrowed from the buffer, then calls `data.to_vec()` to satisfy
the `Result<Vec<u8>>` return contract. The cache borrow ends naturally as
the function returns.

**Why it's a problem.** Every read allocates and memcpy's the value. At
8 KB values and modern allocator/memcpy speeds, this is ~400–500 ns per
read regardless of cache state. For workloads with hot reads on warm
cache, this is the dominant per-op cost (the actual lookup is cache hits
through the handle table — sub-100 ns total).

The skill's Lever 7 calls this out explicitly: "Per-read `Vec<u8>` for
the value... is the API contract; most callers want ownership. For
zero-copy reads, a separate API returning `&[u8]` borrowed from the cache
could be added."

**Direction of fix.** Add a sibling API, e.g.:

```rust
pub fn read_borrow(&self, handle: u64) -> Result<Ref<'_, [u8]>>
```

Returning a `Ref` (or similar) borrowed against the cache's `RefCell`.
Constraints: the borrow ties up the cache's mutable access until dropped
— next mutating call would block. Lifetime ergonomics need careful
design; this is plausibly a 0.2 minor-release feature.

For Python callers, the `Vec<u8>` is unavoidable anyway (PyBytes wants
owned bytes). This optimization helps Rust-only callers.

Not a 1.0 ship-blocker.

### F3 (SMELL, low): `PageCache::get` adds two memory ops per call (PR 1 instrumentation)

**Location:** `src/page_cache.rs:138, 140` (cache_hits/misses); `:233`
(pages_allocated in `new_page`).

**What the code does:** Every `cache.get()` does `cache_hits.set(get() + 1)`
or `cache_misses.set(get() + 1) + load_page(...)`. The Cell load + store
is two memory ops on a single-threaded path; no atomic, no allocation.

**Why it's a problem.** Cache get is Chisel's hottest function — every
read, every HT walk, every commit-time scan calls it. Two extra
instructions per call costs ~2 ns on modern hardware; at 100M calls/sec
(achievable in pure-CPU in-memory benchmarks), that's ~200 ms/sec of
overhead. For workloads bound by fsync (any committing workload), the
overhead is invisible. For pure-CPU in-memory profiling, it's real but
small.

**Direction of fix.** Three options, in increasing cost:

(a) **Accept the cost.** Instrumentation IS the goal of PR 1; the
overhead is small relative to the actual cache-get work (a HashMap
lookup, plus an Option unwrap). The bench harness uses these counters;
turning them off would defeat the point.

(b) **Feature-flag the counters.** `chisel = { features = ["counters"] }`
default-on. Compiles out the increments when off. Adds build-config
complexity for a small win.

(c) **Move to atomics for sharing/lock-free reads.** `AtomicU64::Relaxed`
fetch-and-add is similar overhead on x86_64 (single instruction);
`Cell<u64>` is correct because Chisel is single-writer and same-thread
reads are guaranteed. Not worth changing.

Recommendation: (a). Document that this is the chosen tradeoff in
ARCHITECTURE.md's "Engine-activity counters" section if not already
clear, and revisit if a real profile shows it dominating.

### F4 (NIT): `ChiselEngine::internal_counters` drops poison signal silently

**Location:** `bench/src/chisel_engine.rs` — the `.ok()` mapping in
`fn internal_counters`.

**What the code does:**
```rust
fn internal_counters(&self) -> Option<ChiselCounters> {
    self.db.counters().ok()
}
```

`Chisel::counters()` returns `Result<ChiselCounters>`. On a poisoned
engine, this is `Err(ChiselError::Poisoned)`. The `.ok()` discards the
error, mapping `Err(_)` to `None`.

**Why it's a problem.** The `Engine` trait's `internal_counters` method
returns `Option<ChiselCounters>` — `None` is the documented signal for
"this engine doesn't expose counters." A poisoned ChiselEngine returns
`None` for the same reason it would for a hypothetical no-counters
engine. The bench runner can't distinguish "Chisel doesn't have counters"
(which it always does — Some by contract) from "Chisel is poisoned and
the next op will fail."

In practice, the runner's next happy-path call (`engine.allocate`,
`engine.commit`) will surface the poison. But if the runner ONLY reads
counters between measurements without doing operations — possible during
a teardown/reporting phase — poison stays silent until cleanup.

**Direction of fix.** Two options:

(a) Bench harness code that reads counters should also call
`Chisel::is_poisoned()` (which exists at `lib.rs:367`) and propagate the
result alongside the counters snapshot. Simple defensive pattern; lives in
the runner, not the engine.

(b) Change the `Engine` trait method shape in PR 3 to
`internal_counters(&self) -> EngineResult<Option<ChiselCounters>>`. This
is a trait change touching every impl (Chisel, redb, sqlite). Worth
doing while the trait is still young.

Either is fine. (b) is more idiomatic Rust; (a) is faster to land.

### F5 (NIT): `Identifier(pub u64)` lacks `#[repr(transparent)]`; per-call Vec allocation in `delete_many`

**Location:** `bench/src/engine.rs` (Identifier definition);
`bench/src/chisel_engine.rs` (delete_many impl).

**What the code does:**
```rust
// engine.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identifier(pub u64);

// chisel_engine.rs delete_many
let handles: Vec<u64> = ids.iter().map(|i| i.0).collect();
Ok(self.db.delete_many(&handles)?)
```

The collect() allocates a `Vec<u64>` of length N every call. Without
`#[repr(transparent)]`, Rust does not guarantee that
`Identifier`'s memory layout matches `u64`, so `&[Identifier] →
&[u64]` would be UB even with `transmute`.

**Why it's a problem.** PR 4 of the bench-suite series will call
`delete_many` at every micro-grid measurement (row 9 of the 9-row × 6-size
grid). Each call allocates and copies. For 1000-handle deletes the
allocation is 8 KB and the copy is one cache-line-aligned memcpy — small
but measurable in a tight benchmark loop.

**Direction of fix.** Add `#[repr(transparent)]` to `Identifier`, then
use a documented `unsafe` slice transmute in `delete_many`:

```rust
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identifier(pub u64);

fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
    // SAFETY: Identifier is #[repr(transparent)] over u64, so a slice
    // of Identifier and a slice of u64 have identical layout. The
    // borrow ends with this call; no aliasing concern.
    let handles: &[u64] = unsafe {
        std::slice::from_raw_parts(ids.as_ptr() as *const u64, ids.len())
    };
    Ok(self.db.delete_many(handles)?)
}
```

This is small, well-understood `unsafe` appropriate for a hot internal
helper. The chisel-performance skill's checklist item "Introduce `unsafe`
at any layer ≤ transaction.rs" is the gate — `bench/` is layer 9 (above
transaction.rs / lib.rs), so the gate doesn't apply. PR 3 should fold
this in alongside the per-method trait doc comments and the Identifier
construction-guidance comment.

### F6 (DESIGN, low): CI runs no supply-chain checks

**Location:** `.github/workflows/ci.yml`

**What the code does:** Three jobs — `test`, `clippy`, `fmt` — all
running their respective cargo subcommands. No `cargo audit`, no
`cargo deny`, no MSRV pinning.

**Why it's a problem.** The chisel-performance skill doesn't explicitly
require this (it focuses on engine performance), but the deepdive-rust
review framework treats absence of supply-chain CI as `DESIGN`-tier
finding. Chisel has only two production deps (`xxhash-rust`, `libc`) and
they're well-maintained, so the practical risk is low — but a vulnerable
transitive dep would land silently.

**Direction of fix.** Add a `audit` job to `.github/workflows/ci.yml`
running `cargo install cargo-audit && cargo audit`. Or use the
`rustsec/audit-check` action. Costs one CI minute per build. A 1.0 ship
should have this.

## Don't-break-list compliance — PRs 1 and 2

Walking the 10 commitments against the recently-merged work:

| # | Commitment | PR 1 | PR 2 |
|---|------------|------|------|
| 1 | Two-fsync ordering preserved | ✅ no change | ✅ no engine change |
| 2 | Pre-drain flush preserved | ✅ no change | ✅ no engine change |
| 3 | Poison model: fatal poisons | ✅ counters() poisons via check_alive | ✅ ChiselEngine wraps but doesn't suppress (see F4 note) |
| 4 | On-disk format stability | ✅ counters are in-memory only | ✅ no on-disk change |
| 5 | `PageType = 0x00` reservation | ✅ no PageType added | ✅ no PageType added |
| 6 | Strict layer dependency | ✅ counters internal to layer 2/3 | ✅ bench/ is layer 9 (above lib.rs); no upward refs |
| 7 | Single-writer `&mut self` contract | ✅ Cell<u64> is for counters, not mutating state | ✅ no contract change |
| 8 | Checksum coverage | ✅ no checksum bypass | ✅ no checksum bypass |
| 9 | Handle stability | ✅ no handle reuse | ✅ Identifier wraps but doesn't reuse |
| 10 | Freemap allocates-before-merging | ✅ no commit-protocol change | ✅ no commit-protocol change |

All 10 ✅. Both PRs landed cleanly under the architectural commitments.

The closest call is **#7 (single-writer)**: PR 1's counters use `Cell<u64>`,
which is a form of interior mutability. The commitment says "no internal
`Mutex`, no interior mutability for *mutating paths*, no concurrent
transactions." The counter increments are *not* mutating paths in the
storage sense — they're observability instrumentation that happens to live
inside `&mut self` methods today (though they could move to `&self` if
the counter sites ever did). The Cell is a deliberate, documented choice;
it does not violate the commitment in spirit.

## Open questions

1. **Is there a real workload where `read_borrow` would matter?** F2's
   fix has API-design cost (Ref-returning methods are awkward in Rust).
   Worth doing if there's a measured regression vs. an alternative store
   on a read-heavy benchmark; not worth doing speculatively.

2. **Should `delete_many` actually batch (F1, option (b))?** Depends on
   whether bulk-delete latency matters for any real Chisel client. The
   committed client uses `delete_many` for `drop_table` patterns where
   the user is already waiting on the operation; the latency floor is
   the fsync, not the per-handle work. Probably no — but the bench-suite
   scenario tier (PR 6's "mutation log" scenario) will tell us.

3. **The cost-model table in the skill — is row 9 (delete_many) correct
   given F1?** The skill says `delete_many(1000)` should be measured
   against itself in row 9, "the diff between rows 8 and 9 tells you
   whether `delete_many` actually amortizes better than the loop." If F1
   is correct (it does NOT amortize), rows 8 and 9 should be near-identical
   in PR 4's results. That's actually a useful post-hoc validation of the
   F1 finding.

## What was not reviewed

- **redb / sqlite engines** — not yet implemented (PR 3).
- **Workload generators / Runner / Reporter** — not yet implemented (PRs
  4–5).
- **Python binding's PyBytes ↔ Vec<u8> conversion overhead** — Lever 6
  territory; the binding is shipped (PR 1's Python work) but not
  benchmarked. PR 4's micro grid will surface anything material.
- **`overflow.rs` and `data_page.rs` allocation patterns beyond what the
  read path exposes** — these are layer 4 modules with their own internal
  patterns; a full audit is appropriate when overflow workload sizing
  shows up in PR 4's results.
- **`defrag.rs`** — runs explicitly during low-activity windows per the
  skill's Lever 3; not a hot path, not reviewed.

---

**Summary recommendation:** None of the findings block any current work.
F1 (the `delete_many` documentation gap) is the highest-leverage one to
fix because it affects how PR 4's micro grid will be interpreted. F4
(poison-signal-via-Option) is worth folding into the deferred PR-3 trait
docs work that's already on the TODO. F2 / F5 / F6 are deferable.
