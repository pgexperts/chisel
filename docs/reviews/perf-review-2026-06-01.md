# Chisel Performance Review — 2026-06-01

Source: `chisel-performance` + `rust-performance` skills, full hot-path sweep. Read-only;
no code was changed and no benchmarks were run. Six parallel review agents, one per
hot-path cluster, each inoculated with a dedup digest built from `ISSUES.md` so already-
tracked items (F2/F3/I51/I52/I33/I34/…) were not re-litigated. Every finding is classified
**[STATIC FACT]** (provable by reading the code; cost mechanism stated) or
**[HYPOTHESIS — needs bench]** (a suspected hot spot requiring a measurement to confirm
before acting), per the skills' measure-don't-guess rule.

Clusters reviewed:

1. Read path & handle-table traversal (`lib.rs`, `transaction.rs` read path, `handle_table.rs`, `data_page.rs` read, `overflow.rs` read)
2. Commit protocol & freemap (`transaction.rs` commit/persist_freemap/savepoint/rollback, `freemap.rs`)
3. Page cache, LRU & spillway (`page_cache.rs`, `lru.rs`, `spillway.rs`)
4. Write/mutation path & data layout (`data_page.rs`, `overflow.rs` write, `handle_table.rs` insert, `transaction.rs` mutation paths)
5. Defrag, page_io, superblock, stats, page, error (`defrag.rs`, `page_io.rs`, `superblock.rs`, `stats.rs`, `page.rs`, `error.rs`)
6. Build profile, allocator & bench-harness validity (`Cargo.toml` ×3, `bench/`)

---

## Executive summary

**The single most important finding is not an engine bug — it is that the benchmark harness
cannot currently be trusted to measure one.** Every engine-side optimization below is a
hypothesis that depends on a before/after number, and the harness that would produce that
number (a) elides un-observed read/alloc results because nothing is `black_box`'d, (b)
compiles Chisel and redb with weaker codegen than the bundled-SQLite C competitor because no
`[profile.release]` exists, and (c) never exercises Chisel's in-memory backend, so it cannot
separate CPU cost from fsync cost. Until those three are fixed, a "no measurable change"
result on any engine optimization is uninterpretable.

There is also a hard sequencing constraint: enabling LTO (part of the profile fix) makes the
missing-`black_box` problem *worse*, because more aggressive inlining gives the optimizer more
freedom to delete the un-observed work. So `black_box` must be fixed first.

Beyond the harness, the review found **no correctness bugs and no Don't-Break-List
violations in existing code.** The engine's data-handling is already tight: slot reads and
the handle-table descent are zero-copy/zero-allocation, overflow reads pre-size their result
buffer, `compact()` (the copy-out-write-back antipattern the skill warns about) is dead on
the production path, the I51 `lseek`-elision is verified intact, and the I18 freemap ordering,
3-fsync floor, and handle-table COW discipline are all confirmed correct.

The genuinely new, high-leverage engine findings are two:

- **Default SipHash on the two hottest `HashMap`s** (`PageCache.entries`, `LruIndex.nodes`).
  Every cache-hit read pays roughly six SipHash probes per `cache.get`, three to four `get`s
  per read, on trusted local `u64` page IDs with no DoS surface. A `foldhash`/`fxhash` swap is
  the textbook fix and carries no invariant exposure. **(P1)**
- **Per-insert full-page XXH3 re-stamp on the slot-packing path.** A 1000-small-value
  transaction re-hashes its data pages once *per value* instead of once per page, turning an
  `O(pages × 8 KB)` checksum cost into `O(values × 8 KB)`. **(P1)**

The commit path has a cheap, low-risk win — **swap instead of clone** for the per-commit
`current_freemap` (8 KB `Box` copy) and `current_live_slots` (`O(live-pages)` HashMap rebuild)
promotions — and the I/O layer has a codebase-wide **seek+write → positioned pread/pwrite**
opportunity that halves syscalls per page.

### Recommended sequencing

| Phase | Goal | Items |
|------|------|-------|
| **0 — Measurement integrity** (prerequisite) | Make the harness trustworthy before measuring anything | `black_box` fencing (first); in-memory `EngineMode`; `[profile.release]` + bench `mimalloc` + re-baseline |
| **1 — High-leverage engine wins** (now measurable) | The two P1s, validated on the now-trustworthy grid | SipHash→`foldhash`; per-insert XXH3 re-stamp batching |
| **2 — Within-envelope optimizations** | Real but smaller, each bench-gated | commit swap-not-clone; `maybe_evict` clean-victim index; cache buffer pool; positioned pread/pwrite; `read_many` |
| **3 — Micro / cleanup** | Low-stakes, batchable | freemap alloc cursor; spillway micro-allocs; `drain_batch` Vec; redundant unspill checksum; scenario timed-region hygiene; noise-gate N=1; warmup-discard; cost-model doc fix |

---

## Findings

Severity legend matches `ISSUES.md`: **P0** correctness/data-loss; **P1** real perf pain or
blocks future work; **P2** known-correct simplification / latent; **P3** nice-to-have / micro.

### P1

#### PR-A. Default SipHash on `PageCache.entries` and `LruIndex.nodes`
- **Class:** [HYPOTHESIS — needs bench] (mechanism is a static fact; magnitude needs the bench)
- **Confidence:** high
- **Location:** `src/page_cache.rs:167` (`entries: HashMap::new()`), `src/lru.rs:67` (`nodes: HashMap<u64, Node>`); hot path through `page_cache.rs:194-207` (`get`) and `lru.rs:93-115` (`push_front`)
- **Evidence:** both maps use `std::collections::HashMap` with the default `RandomState`
  (SipHash-1-3). A single warm `cache.get(id)` does `contains_key` + `get` (two probes) then
  `touch_lru` → `push_front` (≈ four more probes across `unlink`/`insert`), ≈ six SipHash
  computations of a `u64` key per `get`. A depth-2 read calls `get` four times → ≈ 24 SipHash
  hashings of an 8-byte key per cache-hit read.
- **Cost mechanism:** CPU. SipHash-1-3 on a `u64` is far costlier than a multiply-shift hash;
  multiplied by ≈ six probes per `get` and three-to-four `get`s per read. The DoS-resistance
  SipHash buys is irrelevant for a single-writer embedded engine reading trusted local page IDs.
- **Proposed direction:** swap both maps to a fast `u64` hasher (`foldhash`, `rustc_hash::FxHashMap`,
  or `ahash`). Validate on the existing `read-warm` row (64 warm random reads, all cache hits,
  zero I/O — isolates exactly hashing + LRU bookkeeping). Cold-read rows will not move (I/O + XXH3 bound).
- **Don't-Break check:** clear. In-memory hashing choice; no on-disk format, fsync ordering,
  poison model, or `&mut self` interaction. LRU order is maintained by the linked-list pointers,
  not by map iteration order, so determinism is not relied on.
- **Dedup:** new. (Distinct from F3, which is the `Cell` counter, and I51, which is the `lseek`.)

#### PR-B. Per-insert full-page XXH3 re-stamp on the slot-packing path
- **Class:** [HYPOTHESIS — needs bench] (mechanism static; magnitude needs the bench)
- **Confidence:** high
- **Location:** `src/transaction.rs:1856-1869` (cursor path) and `:1892-1893` (fresh-page path);
  cost is in `src/page.rs:195-206` (`compute_checksum` over the 8184-byte page body)
- **Evidence:** the packing branch calls `page::stamp_checksum(buf)` after **every**
  `DataPage::insert`, hashing all 8184 bytes regardless of how few bytes changed. A 1000-value
  transaction packing ≈ 39 values/page across ≈ 26 pages still calls `stamp_checksum` 1000 times.
- **Cost mechanism:** checksum / write amplification. Only the final pre-flush (or pre-eviction)
  state of a page must carry a correct checksum; intermediate stamps on a page that receives at
  least one more insert are wasted. Eager stamping exists to keep an evicted-mid-transaction page
  valid (documented at `transaction.rs:1839-1842`), so the cost is real but the invariant is load-bearing.
- **Proposed direction:** defer the stamp — stamp a data page lazily at flush time *and* on the
  eviction path (a "needs-stamp" sub-flag, or stamp on cursor retirement plus an eviction hook).
  Collapses N stamps/page to one for cache-resident transactions. Validate on `allocate-1000pertx`
  / `update-1000pertx` at small value sizes (effect grows with packing density).
- **Don't-Break check:** clear **only if** a stamped checksum is still guaranteed before any
  eviction (invariant 8) and before flush. A naive "stamp at flush only" that forgets the eviction
  path would ship unstamped pages to the spillway/disk — hazard. Must hook both paths.
- **Dedup:** new.

#### PR-C. Bench harness has no `black_box` — read/alloc results are dead-code-eligible
- **Class:** [STATIC FACT]
- **Confidence:** high
- **Location:** `bench/src/runner.rs:216-250` (`apply_op`), `bench/benches/micro_grid.rs:111-115,139-144`,
  `bench/benches/scenarios.rs` timed loop. `grep black_box bench/` → none.
- **Evidence:** the read path is `engine.read(resolve(*alloc_index)).unwrap();` — the returned
  `Vec<u8>` is dropped, never observed. The criterion closures return `()` (results are consumed
  inside the closure), so criterion's built-in `black_box` on the closure return value does not
  protect the per-op results.
- **Validity impact:** the optimizer may elide unused-result work — at minimum the `to_vec()`
  copy in the redb/sqlite adapters, and more once cross-crate inlining is enabled. Read-warm
  numbers can be optimistically low and unstable, and **the risk grows precisely when the
  `[profile.release]` LTO fix lands** — so this must be fixed first.
- **Proposed direction:** fence both sides in `apply_op`/the read loops — feed the resolved id
  through `black_box` on the way in and `black_box` the returned bytes. Bench-only; no library change.
- **Consumer-neutrality:** clean (changes confined to the `publish=false` `bench/` crate).
- **Dedup:** new.

#### PR-D. No `[profile.release]` — Chisel/redb benched under weaker codegen than bundled SQLite
- **Class:** [STATIC FACT] for the asymmetry; [HYPOTHESIS — needs bench] for the magnitude
- **Confidence:** high
- **Location:** workspace `Cargo.toml` (no `[profile.*]`); confirmed absent in `bench/` and `python/`
  Cargo.toml; no `.cargo/config.toml` exists.
- **Evidence:** with no profile override, `cargo bench` compiles `chisel` and `redb` at
  `opt-level=3` but `lto=false`, `codegen-units=16`, `panic=unwind`. `rusqlite` with `bundled`
  compiles SQLite from C at its own cc flags (typically `-O2`), independent of Cargo's Rust profile.
- **Validity impact:** in the headline cross-engine table the two Rust engines run with weaker
  codegen (no cross-crate inlining across the `chisel`→bench boundary, fragmented CGUs) than the
  C competitor. Any "SQLite is faster here" conclusion is partly a build-config artifact. (The
  harness authors already equalize the macOS `F_FULLFSYNC` path in `sqlite_engine.rs:50-62` —
  profile parity is the missing build-side half of that same fairness intent.)
- **Proposed direction:** add a tuned profile at the **workspace root** `Cargo.toml`
  (`lto = "thin"` or `"fat"`, `codegen-units = 1`); re-record the tracked baselines once (uniform
  shift, a re-baseline not a regression). Land **after** PR-C.
- **Consumer-neutrality:** clean — a workspace-root `[profile.release]` affects only builds done
  *in this repo* (benches/tests/examples). Cargo ignores a dependency's profile, so a downstream
  `chisel = "0.1"` consumer's own profile governs. Do **not** instead tune via the published lib crate.
- **Dedup:** new.

#### PR-E. In-memory Chisel backend exists but is never benched — only the file backend is tracked
- **Class:** [STATIC FACT]
- **Confidence:** high
- **Location:** `bench/src/runner.rs:30-98` (`EngineMode` has only `ChiselStrict` → `open_file`);
  `micro_grid.rs:168`, `scenarios.rs:27-31` (all file-backed). `ChiselEngine::open_in_memory`
  (`chisel_engine.rs:51`) is called only from `bench/tests/`.
- **Validity impact:** without an in-memory row the harness cannot separate Chisel's CPU cost
  (slot packing, XXH3, handle-table walk, COW management) from its full durability cost (fsync,
  `F_FULLFSYNC`, pwrite) — the exact decomposition that makes a "commit is slow" finding
  actionable. A pure-CPU regression can be masked by fsync-dominated wall time, or vice versa.
- **Proposed direction:** add a `ChiselMemory` `EngineMode` via `open_in_memory(cache_size)` and
  include it in `EngineMode::ALL` / the scenarios mode list (cleanest through `run_scenario_cell`,
  which already pre-populates in-process). Track both backends so the ratio is visible.
- **Consumer-neutrality:** clean (bench-only).
- **Dedup:** new. (Relates to the `chisel-performance` skill's Lever 5.)

### P2

#### PR-F. Commit promotes `current_freemap` and `current_live_slots` by clone, not swap
- **Class:** [STATIC FACT] (cost); [HYPOTHESIS — needs bench] (magnitude)
- **Confidence:** high
- **Location:** `src/transaction.rs:917` (`committed_freemap = current_freemap.clone()`),
  `:920` (`committed_live_slots = current_live_slots.clone()`); paid again per `begin()` at `:717,:721`.
- **Evidence:** `current_freemap`/`committed_freemap` are `Box<[u8; PAGE_SIZE]>` (8 KB);
  `current_live_slots` is `HashMap<u64, u32>` with one entry per live data page. A single-row
  commit deep-copies the full 8 KB bitmap **and** rebuilds an `O(live-pages)` HashMap, even though
  one bit / one page changed. (`persist_freemap` already does its own 8 KB copy at `:684`, so the
  bitmap is moved ≈ 3× per small commit.)
- **Cost mechanism:** allocation + memcpy + hash-rebuild, per commit, independent of change size.
- **Proposed direction:** `std::mem::swap(&mut committed_*, &mut current_*)` at step 5, then
  reseed `current_*` from `committed_*` at the next `begin()` (which already clones). Removes one
  8 KB alloc+copy and one `O(live-pages)` HashMap rebuild per commit. A per-txn delta-apply is the
  more invasive `O(changed)` alternative; the swap is the low-risk first step.
- **Don't-Break check:** clear. The swap must occur **after** the step-4 fsync linearization point
  (it does, at step 5). Single-writer `&mut self` means no aliasing concern.
- **Dedup:** new.

#### PR-G. `maybe_evict` re-scans the dirty LRU prefix per eviction in the mixed clean/dirty regime
- **Class:** [HYPOTHESIS — needs bench]
- **Confidence:** medium
- **Location:** `src/page_cache.rs:912-930` (Phase A victim search)
- **Evidence:** the `dirty_count == entries.len()` early-out only fires when **every** entry is
  dirty. In a mixed regime (some clean read-through pages, some dirty, clean victims clustered
  toward the MRU end) `iter_lru_to_mru().find(|id| !dirty)` walks past every dirty (pinned) entry
  from the tail on each call; across an allocation-heavy transaction that evicts repeatedly this is
  `O(n²)` in the dirty-prefix length.
- **Cost mechanism:** repeated linear scan over the dirty LRU prefix per eviction.
- **Proposed direction:** a separate intrusive "clean LRU" sub-list (or a free-victim cursor) so
  eviction is `O(1)` amortized regardless of where dirty pages sit. Confirm the regime occurs first
  — pure-write transactions hit the all-dirty early-out, pure reads find the first tail item; the
  quadratic only bites mixed read+write transactions large enough to evict.
- **Don't-Break check:** clear (a clean-victim index changes no durability/format invariant; dirty
  pages still never evicted).
- **Dedup:** new.

#### PR-H. No cache buffer pool — malloc/free per page churn, plus 8 KB memset on `new_page`
- **Class:** [HYPOTHESIS — needs bench]
- **Confidence:** medium
- **Location:** `src/page_cache.rs:326,446,736,853,873` (`Box::new([0u8; PAGE_SIZE])` / `Box::new(buf)`);
  eviction drops the `CacheEntry` (frees the Box).
- **Evidence:** under sustained pressure (working set > cache) the cache does malloc-on-load /
  free-on-evict in lockstep — a fresh 8 KB allocation per cache miss and per `new_page`, with the
  just-freed buffer not reused; `new_page` additionally zeroes 8 KB.
- **Cost mechanism:** allocation — one malloc/free pair per page churned, plus an 8 KB zero-fill on
  `new_page`. (Alloc-heavy paths are the worst case for shared-CI bench noise — measure on dedicated
  hardware, report-only, per the project's CI policy.)
- **Proposed direction:** a small free-list capped at a few× `max_pages`: push freed `Box`es on
  eviction, pop on load/`new_page` (zero only when handing out a `new_page`). This is the **small
  pool** idea, explicitly **not** the deferred I34 mmap redesign.
- **Don't-Break check:** clear (private heap; pooling must zero-on-handout for `new_page` to keep
  the fresh-zeroed-page contract).
- **Dedup:** new; distinct from I34 (deferred).

#### PR-I. `page_io` and `spillway` use seek+write / seek+read (two syscalls per page) instead of positioned I/O
- **Class:** [HYPOTHESIS — needs bench] (the syscall count is a static fact; the latency impact needs the bench)
- **Confidence:** high (pattern) / medium (impact)
- **Location:** `src/page_io.rs:209-213` (`read_page`), `:236-240` (`write_page`); same pattern in
  `src/spillway.rs:280-284` (`write_slot`), `:307-311` (`read_slot`)
- **Evidence:** each page I/O issues `seek(SeekFrom::Start(off))` then `read_exact`/`write_all` —
  two syscalls where `std::os::unix::fs::FileExt::{read_exact_at, write_all_at}` (pread/pwrite)
  would do one. The `flush()` loop calls `write_page` once per dirty page per commit; the spillway
  drain adds more.
- **Cost mechanism:** one extra `lseek` per page I/O (plus, for the spillway, a second `write` for
  the split header/page).
- **Proposed direction:** switch the `File` arm to `read_exact_at`/`write_all_at`; for the spillway,
  assemble header+page into one `[u8; SLOT_SIZE]` and issue a single positioned write. Stays inside
  `page_io.rs`/`spillway.rs`. For fsync-dominated commits the win may be in the noise — bench with a
  large-dirty-set commit and a syscall count (`strace -c` / `dtruss`).
- **Don't-Break check:** clear. Syscalls stay in the I/O modules (invariant 6); no change to fsync
  ordering (1), pre-drain (2), or format (4). The single-writer flock makes the shared-offset removal safe.
- **Dedup:** new.

#### PR-J. No `read_many` — batched reads re-descend the handle table per key (read analogue of I33)
- **Class:** [HYPOTHESIS — needs bench]
- **Confidence:** high (the absence is a static fact; the batched-descent win needs the bench)
- **Location:** absent from `src/lib.rs` (only `read`) and `src/transaction.rs` (only `read`/`read_inner`)
- **Evidence:** a caller reading K handles pays K independent `find_leaf` descents from the root.
  Keys sharing interior/leaf pages (common for monotonic handles read in ranges) re-fetch and
  re-hash the same interior pages K times — the read-side shape of the deferred I33.
- **Cost mechanism:** redundant descents → redundant `cache.get` (and, today, redundant SipHash) on
  shared interior pages; pure CPU once warm.
- **Proposed direction:** land PR-A first (cuts per-descent cost for free). Whether a dedicated
  leaf-grouping `read_many` beats the simple loop is the hypothesis — settle with a clustered-read
  bench row before building the batched descent (same YAGNI posture that deferred I33).
- **Don't-Break check:** clear — read-only `&self` API; must preserve per-handle error semantics.
- **Dedup:** new (read analogue of I33).

#### PR-K. `update_inner` always relocates; the in-code cost-model text is inaccurate under R1
- **Class:** [STATIC FACT]
- **Confidence:** high
- **Location:** `src/transaction.rs:1251-1321` (`update_inner`); contrast the unused in-place
  `DataPage::update` at `src/data_page.rs:258-290`
- **Evidence:** `update_inner` unconditionally retires the old slot (`release_data_slot`) and
  packs the new value into the cursor page — even for a same-size update. There is no in-place data
  COW; an update is a delete+insert that leaves a tombstone in the old page. This is correct under
  R1 (a committed data page may pack ~39 unrelated values and cannot be rewritten in place without
  rewriting every co-resident handle), but it means the cost-model line "update(same size) = 1 data
  page COW" does not describe what happens.
- **Cost mechanism:** tombstone accretion → defrag pressure / page-count inflation on update-heavy
  workloads (not a per-op latency cost).
- **Proposed direction:** mostly a documentation fix (correct the cost-model text). A narrow real
  optimization exists: when the old value lives on a page **dirtied in the current transaction** and
  the new value fits the old slot, overwrite in place via `DataPage::update` and skip the relocate.
  Bench before pursuing.
- **Don't-Break check:** any in-place path MUST be restricted to current-transaction dirty pages —
  overwriting a committed data page in place violates shadow paging. Existing code is conservative and safe.
- **Dedup:** new framing; the behavior itself is intended (consistent-with R1/I9/I10).

#### PR-L. Noise gate has an N=1 false-green; diff/gate conflate "no signal" with PASS/FAIL
- **Class:** [STATIC FACT]
- **Confidence:** high
- **Location:** `bench/src/noise_gate/cov.rs:24-30,36`, `bench/src/bin/noise_gate.rs:147-148`,
  `bench/src/diff/compare.rs:200-208`
- **Evidence:** `compute_cov` returns `cov = 0.0` for a single sample, and the gate treats
  `cov <= threshold` as pass — so a `--runs 1` invocation (or all-but-one cell failing to write)
  reports PASS with zero observed variance, qualifying a noisy machine. Zero-mean cells produce
  `NaN`/`inf` cov and `delta_pct`, marked failing rather than recognized as a broken measurement.
  (The gate is **correctly report-only** — `diff.rs` returns success regardless of regression count,
  and `bench.yml` has no `needs:` into the gating jobs — so this cannot turn CI red on shared-runner
  noise, consistent with project policy. The gaps are about trustworthiness of the report, not gating.)
- **Proposed direction:** require `runs >= 2` and surface a distinct `INDETERMINATE`/`Undefined`
  state for N<2 and zero-mean cells so "no signal" renders separately from PASS/FAIL.
- **Consumer-neutrality:** clean (bench-only).
- **Dedup:** new.

#### PR-M. Scenario timed region includes `begin/commit` framing, per-op `Instant::now()`, and payload `Vec` allocation
- **Class:** [STATIC FACT] (region contents); [HYPOTHESIS — needs bench] (distortion size)
- **Confidence:** high
- **Location:** `bench/src/runner.rs:548-563` (timed loop), `:230-240` (`apply_op` builds
  `vec![0u8; size]` inside the timed op)
- **Evidence:** the CI-tracked scenario tier brackets the whole loop with `total_start`, takes a
  per-op `Instant::now()` pair inside the loop, and allocates the payload `vec![0u8; size]` inside
  the timed op. The micro-grid correctly hoists payload/TempDir/file-copy into `iter_batched` setup
  closures; the hand-rolled scenario timer does not.
- **Validity impact:** per-op `Instant::now()` pairs (200K clock reads on a 100K-op run) and the
  payload alloc/zeroing are folded into throughput, taxing the fast (read) ops more than the
  fsync-bound (write) ops and compressing cross-engine deltas.
- **Proposed direction:** pre-build payloads once per size outside the loop; compute throughput from
  a single outer timer over an inner loop with no per-op `Instant::now()` (run the latency-sampled
  pass separately); document that scenario throughput is per-transaction.
- **Consumer-neutrality:** clean (bench-only).
- **Dedup:** new.

#### PR-N. Bench binaries could pin `mimalloc` for realistic best-case and cross-engine allocator parity
- **Class:** [STATIC FACT] (absent); [HYPOTHESIS — needs bench] (win)
- **Confidence:** high
- **Location:** no `#[global_allocator]` anywhere; natural home `bench/benches/{scenarios,micro_grid}.rs`
- **Evidence:** Chisel is alloc-heavy by construction (`Box<[u8; 8192]>` per cached page, `Vec<u8>`
  per read); redb's `value().to_vec()` likewise; SQLite's C core does far less Rust-side heap
  traffic. All three run on the platform default allocator, so the tracked numbers reflect "Chisel
  on system malloc," not its achievable best, and the allocator tax falls unevenly.
- **Proposed direction:** set `#[global_allocator] = MiMalloc` in the two bench binaries (the
  `publish=false` crate) and re-record baselines; optionally keep one system-allocator run.
- **Consumer-neutrality:** clean and load-bearing — a `#[global_allocator]` is process-global and a
  library must never force one on consumers. The bench crate is the only correct home; do **not** add
  it to the `chisel` or `chisel-py` crates.
- **Dedup:** new.

### P3

| Ref | Finding | Location | Class |
|----|---------|----------|-------|
| PR-O | Freemap `allocate_first` linear-scans the bitmap from byte 0 with no cursor/hint (found independently by the commit and write-path agents) | `src/freemap.rs:135-146` | [HYPOTHESIS — needs bench] |
| PR-P | Spillway re-validates XXH3 on every unspill — redundant with the main-file checksum on the **drain** path only; the resident-read-back path must keep it (silent-corruption trap otherwise) | `src/spillway.rs:235-255`, `page_cache.rs:438,848` | [STATIC FACT] / hazard-flagged |
| PR-Q | Spillway rehydrate does an extra stack→heap `Box::new(buf)` 8 KB copy; `drain_batch` allocates a fresh `Vec<u64>` per batch (the I52 sibling case) | `page_cache.rs:436-448`, `spillway.rs:204-210` | [STATIC FACT] |
| PR-R | Redundant double map-probe (`contains_key` then `get`) in `PageCache::get`/`get_mut` and `LruIndex` — folds into PR-A's area | `page_cache.rs:195-206`, `lru.rs:94-95` | [HYPOTHESIS — needs bench] |
| PR-S | Savepoint error variants (`SavepointNotFound`/`DuplicateSavepoint`) allocate a `String` at construction — cold control path, error branch only; noted for completeness | `src/error.rs:30-31`, `transaction.rs:1005,1054,1093` | [STATIC FACT] |
| PR-T | Scenario tier has no warmup-discard, so cold-start ops inflate the tracked p95/p99 tails | `bench/src/runner.rs:570-574` | [STATIC FACT] |
| PR-U | `Spillway::logical_bytes` over-reports after `forget` (high-water, not live) — surfaces in the I74 stat; stats-accuracy quirk, not perf | `src/spillway.rs:128-130` | [STATIC FACT] (non-perf) |

---

## Confirmed clean (negative findings worth recording)

These were checked specifically and found correct — recorded so a future pass need not re-derive them:

- **Read path data-handling is already tight.** `DataPage::read` returns a zero-copy `&[u8]`
  (`data_page.rs:231-241`); the only inline-read allocation is the tracked F2 `to_vec()`. The
  handle-table descent (`find_leaf`) is allocation- and clone-free; `HandleEntry` is `Copy`.
  Overflow read pre-sizes its result `Vec` from `total_length` (one allocation, no per-chunk Vec)
  with an `O(1)` cycle-detection counter. No residual double handle-table walk after I32.
- **The slot-directory copy-out antipattern is absent from the hot path.** `DataPage::insert`/
  `update` mutate in place on the cached page bytes; `compact()` (the copy-out-write-back method)
  is dead on the production path (only `#[cfg(test)]` callers). Slot packing uses an `O(1)` cursor,
  not a free-space scan. `handle_table::grow()` reparents in `O(1)` (installs the old root at child 0;
  no tree clone). Overflow write pre-sizes and copies chunks straight into page buffers.
- **`release_data_slot` deliberately writes no on-page tombstone** — liveness is tracked in
  `current_live_slots` + the handle-table tombstone, so deleting one value from a packed page COWs
  zero data pages. Intentional and cheaper; do not "fix."
- **LRU is genuinely `O(1)` per touch/evict** (`lru.rs`) — the structure is an index map over an
  intrusive doubly-linked list with cached head/tail; no scan on any op.
- **Commit discipline intact:** I18 allocate-before-merge ordering (`transaction.rs:662` before
  `:668-674`), the 3-fsync floor and data-before-superblock barrier, the I52 scratch-vec reuse, and
  the handle-table COW touching exactly `depth+1` pages — all verified. No page is re-checksummed for
  an unchanged page on commit.
- **I51 verified fixed:** `page_io::read_page` reads `cached_page_count: Cell<u64>` — the per-read
  `lseek` is genuinely gone, with the HWM maintained on extending writes and `set_page_count`.
- **`Chisel::stats()`'s `O(live-handles)` walk is user-on-demand only** — no engine-internal hot-path
  caller. `InvalidHandle` on the lookup-miss path carries a bare `u64` (lazy `Display`), not a
  formatted string — the hot-path error antipattern done correctly. Superblock (de)serialize/select
  allocate nothing transient per commit. The defrag rewrite loop is `max_pages`-bounded, linear in
  handles via `O(depth)` lookups (no `O(n²)`), and skips dense pages (no redundant re-checksum).
- **Bench `chisel_engine.rs` adapter region is clean** (1-line delegations; the only allocation is
  the intrinsic `read`-returns-`Vec`); `harness=false` benches **are** release-built. The noise gate
  is correctly report-only and does not gate CI on shared-runner numbers.

---

## Don't-Break-List compliance

No existing-code violation was found. Every proposed optimization was checked against the list:

| Invariant | At risk from any finding? |
|-----------|---------------------------|
| 1. Data fsync before superblock fsync | No proposal reorders the commit barrier. |
| 2. Pre-drain flush stays | No proposal removes it. |
| 3. Poison model on fatal error | Untouched. |
| 4. On-disk format stable within a major | PR-B/PR-F/PR-O are in-memory/CPU only; the spillway sidecar (PR-I/PR-P/PR-Q) is explicitly ephemeral, not durable format. |
| 5. PageType 0x00 reserved | Untouched. |
| 6. Strict layer deps | PR-I keeps syscalls inside `page_io.rs`/`spillway.rs`. |
| 7. Single-writer `&mut self` | PR-A is in-memory hashing; no interior mutability added for mutation. |
| 8. Every page checksummed, verified on load | **PR-B and PR-P carry explicit hazard caveats** — PR-B must still stamp before eviction/flush; PR-P must keep the resident-read-back checksum. Both flagged, not blindly recommended. |
| 9. Handles never reused | Untouched. |
| 10. persist_freemap allocate-before-merge | Verified intact; PR-O's cursor changes only which already-safe free id is returned, not the ordering. |

---

## Open questions for the maintainer

1. **Hasher choice (PR-A):** `foldhash` (fast, no deps beyond the crate), `rustc_hash::FxHashMap`
   (battle-tested, used by rustc), or `ahash` (fast, slightly larger)? All are DoS-irrelevant here.
2. **XXH3 re-stamp (PR-B):** is the added "needs-stamp" bookkeeping worth it before a bench confirms
   the magnitude on `allocate-1000pertx`? The mechanism is certain; the size is not.
3. **LTO grade (PR-D):** `thin` (fast iteration) or `fat` (best baseline) for the tracked grid? The
   skill suggests `fat` for the recorded baseline, `thin` while iterating.
4. **Filing:** should the new items (PR-A … PR-T) be filed into `ISSUES.md` as I77+ now, or staged
   behind the Phase 0 bench-integrity work so each engine item lands with a real before/after?

## What was not reviewed

- The PyO3 binding (`python/`) — the `chisel-performance` skill's Lever 6 (GIL release, `PyBytes`
  marshaling) is a separate pass; this sweep covered the Rust engine + bench harness only.
- Actual benchmark execution — this was a static review. Every `[HYPOTHESIS]` item needs the bench
  it names, run on dedicated hardware (not the shared CI pool), report-only per project CI policy.
- The recovery/crash paths (`recovery_tests.rs`) beyond confirming `stats()` is their only `.stats()` caller.
