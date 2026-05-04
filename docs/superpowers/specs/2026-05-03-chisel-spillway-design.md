# Chisel Spillway — Design

**Date:** 2026-05-03 (open questions resolved 2026-05-04)
**Status:** Approved; implementation plan pending.

## Motivation

The page cache is a single shared pool: clean pages live within the
soft limit `max_pages` and get evicted under LRU; dirty pages are
pinned (eviction would lose the transaction's only copy of pending
writes) and accumulate on top, capped only by the hard ceiling
`max_pages * HARD_CEILING_MULTIPLIER` (currently 8×, see ISSUES.md
I19). Once the hard ceiling is hit, `ChiselError::CacheFull` fires
and the caller must commit or roll back.

This is fine for short transactions but poor for long write-heavy
ones: working sets that don't fit in 8× the cache cannot be applied
in a single transaction at all, even though the file would
accommodate the result. The constraint is purely a memory-residency
artifact; the engine itself has no upper bound on how many pages a
transaction may touch.

This spec introduces the **spillway** — a sidecar file that absorbs
overflow dirty pages so a transaction can grow its dirty set
arbitrarily large at the cost of write amplification, not memory.

## Goals

- Let a single transaction touch a working set larger than the page
  cache without aborting.
- Preserve current crash safety: at no point may a partially-spilled
  transaction's bytes become visible to a future open. Rollback must
  remain "discard the spillway, untruncate; main file untouched."
- Preserve current commit cost in the no-spill case: a transaction
  whose dirty set fits in the cache pays exactly today's two
  fsyncs (one for data pages, one for the superblock) and zero
  additional writes.
- Keep the failure mode operational, not fatal: filling the spillway
  trips `ChiselError::SpillwayFull { limit }` analogous to today's
  `CacheFull`; the engine state is consistent and the caller recovers
  by committing or rolling back smaller chunks.

## Non-goals

- Bounding total memory below the existing soft `max_pages`. The
  spillway exists *because* memory is bounded; the cache itself
  becomes a strict bound under this design (no more 8× elasticity)
  and the spillway is the relief valve.
- Concurrent transactions sharing a spillway. Chisel is
  single-writer today; the spillway inherits that.
- Persisting the spillway across process restart. It is throwaway by
  construction — truncated at defrag time, ignored at open time.
- A general "spill any cache to disk" abstraction. The spillway only
  holds pages that the current transaction has dirtied; clean pages
  continue to be evicted (or kept) by the LRU as today.

## Architectural shape

Spilling is a `PageCache` concern (layer 3). The spillway file is
opened and managed by `PageCache`; no module above it sees the
spill/rehydrate path. From the freemap, data-page, overflow,
handle-table, and transaction layers' perspective, `get` / `get_mut`
/ `new_page` continue to return in-cache `&[u8; PAGE_SIZE]` buffers;
the cache transparently rehydrates from the spillway on miss.

The spillway is a separate file alongside the main database, e.g.
`mydb.chisel` + `mydb.chisel.spillway`. It is keyed by `page_id`,
not append-only: re-spilling a page that was already in the spillway
overwrites its slot in place. This bounds the spillway by the
transaction's dirty *working set*, not by mutation count.

## Lifecycle

```
open                  spillway file is (created and) truncated to zero.
                      Any pre-existing spillway content is garbage from
                      a crashed prior process and is unconditionally
                      discarded — no superblock can possibly point at
                      spillway bytes, so this is always safe.

dirty cache fills     when `entries.len() == max_pages` and a new
                      dirty page would push us over, the LRU-tail
                      dirty entry is written to its spillway slot
                      (allocated on first spill of that page_id) and
                      removed from the cache.

read of spilled page  `get` / `get_mut` finds no entry, checks the
                      spillway-resident set, reads from spillway,
                      verifies a per-slot checksum, and inserts into
                      the cache as dirty. (A re-loaded spilled page
                      is by definition dirty: it was dirty when
                      spilled, and no clean disk version reflects
                      the in-flight change.)

commit                drain (see below).

rollback              clear the spilled-set index, truncate the
                      spillway to zero, and proceed with today's
                      `discard_all_dirty` + `truncate(committed_total)`
                      against the main file. No main-file bytes are
                      ever orphaned by spillway use.

defrag                truncates the spillway to zero (it has no live
                      content between transactions, so this is a
                      housekeeping shrink, not a correctness step).
```

## Commit drain

```
1. Flush all currently-dirty in-cache pages to their shadow IDs in
   the main file. Buffered writes only — no fsync yet.
2. While the spillway is non-empty:
     a. Read up to `max_pages` page-ids out of the spillway.
     b. For each, load into the cache as a dirty entry (overwriting
        any prior cached entry for that id).
     c. Flush as in step 1 — buffered writes to main file.
     d. Drop the drained slots from the spillway-resident index.
        (The spillway file itself is not shrunk per-batch; it is
        truncated to zero in step 4.)
3. fsync the main file. This is the same fsync the current
   `flush()` issues — the only durability barrier in Phase 1 of
   commit. Intermediate batch writes do not need their own fsync;
   the kernel is free to coalesce, and a crash before this fsync
   is just a rolled-back transaction (no main-file bytes are
   committed without it).
4. Truncate the spillway to zero and clear the spilled-set index.
5. Proceed with today's Phase 2: write the new superblock to the
   inactive slot, fsync it. (Unchanged.)
```

Two-fsync commit cost is preserved: one fsync of the main file
covers all writes (in-cache flush + every drain batch); one fsync
of the superblock makes the transaction visible. The spillway
itself is never fsynced — its content does not need to survive a
crash.

### Drain insertion policy

The drain pulls a batch of pages from the spillway and inserts
them into the (now-empty-of-dirty) cache for their write. Where
in the LRU should they land?

- **MRU** (the default for every other insert): treats drained
  pages as "just touched"; they survive the next eviction
  pressure, possibly displacing pre-transaction warm pages that
  the next transaction would have benefited from.
- **LRU-tail**: makes drained pages the first eviction candidates,
  preserving the pre-transaction warm working set across the
  commit boundary. The drained pages are the ones that lost the
  spill lottery earliest — weakly, the coldest dirty pages — so
  this is plausibly the better default, but it's an empirical
  question.

Both strategies are correct. Choice exposed as
`Options::drain_insertion` (enum: `Mru` | `LruTail`). The default
is `LruTail` per the reasoning above (preserves the
pre-transaction warm working set across the commit boundary,
biases against displacing those pre-existing warm pages with what
were the coldest dirty entries). The cross-policy benchmark in the
Testing section will confirm or revise this default; until then
`LruTail` ships as the recommended choice.

## Configuration

The existing `Options::cache_size` (a page count) is replaced with
a byte-denominated field, and a matching byte-denominated spillway
limit is added. Bytes are user-friendly — callers think in MB/GB,
not in 8KB units — and the conversion to internal page counts is a
trivial `bytes / PAGE_SIZE` (rounded down, clamped to at least one
page) at construction time:

| Field                | Type    | Default                      | Meaning                                                   |
|----------------------|---------|------------------------------|-----------------------------------------------------------|
| `cache_max_bytes`    | `u64`   | 8 MiB (= 1024 pages, today's `cache_size` default) | Strict upper bound on in-memory cache size. Converted internally to a page count. |
| `spillway_max_bytes` | `u64`   | `1024 * cache_max_bytes` (8 GiB at the default cache size) | Strict upper bound on spillway file size (excluding per-slot headers). Exceeding trips `SpillwayFull`. Setting to 0 disables spillway entirely (preserves today's "fail fast at the cache ceiling" contract via `CacheFull`). |
| `drain_insertion`    | enum    | `LruTail`                    | `Mru` or `LruTail`. See "Drain insertion policy" above.   |

Internally the cache continues to count entries (every entry is
exactly `PAGE_SIZE`, so byte-counting would just be a multiply);
the byte denomination is purely an `Options`-surface concern.

The existing `HARD_CEILING_MULTIPLIER` and the elasticity it
authorizes go away. Under the spillway design, `cache_max_bytes`
is a hard cap — overflow always spills first. `ChiselError::CacheFull`
is removed (or kept as a never-fired alias, deprecated) when
`spillway_max_bytes > 0`; with the spillway disabled, `CacheFull`
fires on the first allocation past `cache_max_bytes` (no 8×
elasticity).

### Runtime mutability

Both `cache_max_bytes` and `spillway_max_bytes` may be changed on
the fly **between transactions**. The engine exposes:

```rust
impl Chisel {
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<(), ChiselError>;
    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<(), ChiselError>;
    pub fn set_drain_insertion(&mut self, policy: DrainInsertion) -> Result<(), ChiselError>;
}
```

Each returns `ChiselError::TransactionInProgress` if a transaction
is in flight; the `TransactionManager` already tracks this state.

Why between-transactions only:

- **Cache shrink is trivial between transactions.** `flush()` runs
  at commit, so every cache entry is clean; the implementation just
  calls `maybe_evict` with the new lower bound and clean LRU-tail
  entries are dropped until we fit. Mid-transaction the same shrink
  would have to spill dirty entries instead of evicting them — a
  surprising side effect we'd rather not trigger on a config call.
- **Spillway resize is trivial between transactions.** The spillway
  is truncated to zero at every commit and rollback (see
  Lifecycle), so between transactions it has no live content;
  shrinking and growing are both no-ops on state. Mid-transaction,
  shrinking below current usage would have to either reject, evict
  spilled pages (impossible — they're dirty), or trip `SpillwayFull`
  retroactively; none is a clean story.
- **Cache grow and spillway grow** are trivial in both states, but
  the API rejects them mid-transaction anyway for symmetry: one
  rule ("config changes only between transactions") is easier to
  document and reason about than per-direction asymmetries.

`drain_insertion` is also only changeable between transactions, for
consistency, even though it would technically be safe to change
mid-drain.

## On-disk format of the spillway

The spillway file is a sequence of fixed-size slots, each holding
one page plus a header:

```
slot layout (PAGE_SIZE + 16 bytes):
  u64  page_id        // the main-file page id this slot shadows
  u64  checksum       // XXH3 over (page_id || page_bytes)
  [u8; PAGE_SIZE]     // raw page bytes
```

Slots are indexed by spillway position (0, 1, 2, ...), independent
of `page_id`. An in-memory `HashMap<u64, u64>` (page_id → spillway
slot index) maintains the mapping; this map is rebuilt only on
the rare path of "transaction grew the spillway."

The per-slot checksum is **not** redundant with the main page's
own XXH3 checksum: a freshly-allocated page that was never written
to the main file may be spilled before its first main-file write,
so its bytes have no checksum stamped yet. The slot-level
checksum gives the rehydrate path an integrity check independent
of the page-type module's eventual stamping.

A torn write to the spillway is detected on rehydrate (checksum
mismatch) and treated as fatal — the transaction is poisoned just
as a torn main-file write would be. This matches the I1 poison
model: any commit-protocol I/O failure is unrecoverable.

## Failure surface

| Trigger                                  | Result                                                                      |
|------------------------------------------|-----------------------------------------------------------------------------|
| Cache full, spillway has room            | spill the LRU-tail dirty page, succeed.                                     |
| Cache full, spillway at `spillway_max_bytes` | `ChiselError::SpillwayFull { limit_bytes }` — operational, recoverable by commit/rollback. |
| Spillway write I/O error                 | poison the transaction (fatal, matches main-file write semantics).          |
| Spillway read I/O error during rehydrate | poison the transaction.                                                     |
| Spillway slot checksum mismatch          | poison the transaction (`ChecksumMismatch { page_id }`).                    |
| Crash mid-transaction                    | spillway file is orphaned; next open truncates it. No main-file effect.     |

## Risk review

- **Write amplification.** Every spilled page is written twice
  (once to the spillway during spill, once to the main file
  during drain) and read once (during drain rehydrate). Long
  transactions that spill heavily will be measurably slower than
  fitting-in-cache transactions of the same logical size.
  Mitigation: the alternative today is `CacheFull`, i.e. impossible.
  Spillway is opt-out (set `spillway_max_bytes = 0` to preserve
  today's behavior of "fail fast at the cache ceiling") for
  workloads that prefer that contract.
- **Cold-cache after large transactions.** A drain that touches
  more pages than the cache holds will, regardless of insertion
  policy, leave the cache containing only the last-drained batch.
  This is inherent — you cannot retain a working set larger than
  the buffer. The drain-insertion policy only affects the
  *boundary* case where the working set fits but the drain order
  matters.
- **Spillway disk-space exhaustion.** `spillway_max_bytes` caps
  the spillway file's logical size, but the per-slot 16-byte
  header means the on-disk footprint is slightly larger than the
  configured cap. A misconfigured limit could also exhaust the
  volume independently of the cap. Mitigation: documentation note;
  optionally, a defensive `statvfs` check at spillway open. Out
  of scope for v1.
- **Defrag interaction.** Defrag truncating the spillway is safe
  only when no transaction is in flight; the existing defrag
  contract already requires that. No new constraint.
- **Format-version bump?** The spillway is a separate file with
  its own internal format; it does not affect the main file's
  on-disk layout or `format_version`. No `format_version` bump
  required.

## Testing

- **Unit:** spill → rehydrate round trip preserves bytes for both
  freshly-allocated and previously-flushed pages.
- **Unit:** spillway slot checksum mismatch produces
  `ChecksumMismatch` on rehydrate.
- **Unit:** re-spill of an already-resident page id overwrites
  the existing slot (spillway size does not grow).
- **Integration:** a transaction whose dirty working set exceeds
  `cache_max_bytes * 4` commits successfully and produces the
  same final state as the same workload split into smaller
  transactions.
- **Integration:** rollback of a transaction that spilled leaves
  the main file byte-identical to its pre-transaction state and
  the spillway truncated to zero.
- **Integration:** crash injection mid-spill — kill the process
  between spillway writes — and verify that the next open
  recovers the last-committed state and discards the spillway.
- **Cross-policy:** a parameterized test runs a benchmark workload
  under both `drain_insertion` policies and measures cache hit
  rate on the immediately-following transaction. (Sets the
  default for the option.)
- **No-spill regression:** confirm that a workload sized to fit
  in `cache_max_bytes` issues exactly two fsyncs per commit
  (unchanged from today) and zero spillway writes.
- **Runtime mutability:** `set_cache_max_bytes` and
  `set_spillway_max_bytes` succeed between transactions (verify
  shrink actually evicts to fit, grow takes effect on next
  allocation) and return `TransactionInProgress` mid-transaction.

## Resolved decisions (2026-05-04)

The three open questions in the original draft have been settled:

- **`spillway_max_bytes` default = `1024 * cache_max_bytes`** (8 GiB at
  the default 8 MiB cache). Scales with the cache size so a user who
  bumps the cache to 1 GiB also gets a proportionally larger spillway
  ceiling without an extra knob to tune.
- **`drain_insertion` default = `LruTail`**. Reasoning is in the
  "Drain insertion policy" subsection above; the cross-policy
  benchmark in the Testing section will confirm or revise.
- **`spillway_max_bytes = 0` is the documented opt-out** for
  preserving today's `CacheFull`-on-cache-pressure behavior. No
  separate `disable_spillway` flag — one knob, one rule
  ("zero means no spillway").
