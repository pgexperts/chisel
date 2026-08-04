# Chisel — Theory of Operation

This document is for a human engineer who is about to *change* Chisel. It is the "why" companion to two siblings:

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the cold-start map: the layer model, the exact on-disk byte format, the enumerated load-bearing invariants, the commit-protocol step list. When this document needs to point at a layout or an invariant, it cross-references ARCHITECTURE rather than restating it.
- [`README.md`](README.md) — what Chisel does and how to use it.

Read this one **once, slowly**. It is narrative, not reference. It exists to build a durable mental model — the shape of the decisions and the tradeoffs behind them — so that when you later open the code you already understand why it is the way it is. After that first read, come back to it only when you need to remember *why* something is the way it is; use ARCHITECTURE for *what* it is.

A note on the division of labor: wherever you feel the urge to check "what exactly is at byte 324" or "what are all the fatal-error variants," that belongs in ARCHITECTURE and you will find it there. This document deliberately does not repeat those tables, because duplicated content drifts and a cross-reference cannot.

---

## What Chisel is

Chisel is an **embedded, single-writer, copy-on-write (shadow-paging) transactional slot store** written in Rust. You hand it a byte value, it gives you back a stable `u64` handle; you hand it the handle later, it gives you back the value. Values can be updated, deleted, tagged into groups, and defragmented, all inside ACID transactions that survive a crash. There is no server, no network, no query language — it is a library that owns one file.

One through-line explains most of the design: **the recovery path *is* the normal path.** Chisel never writes a log that must be replayed on crash. Instead, every mutation writes fresh pages and leaves the last-good state fully intact on disk; a commit atomically swaps a small pointer (the superblock) to the new state. Opening a database — any database, crashed or clean — runs exactly the same "pick the winning superblock" procedure. There is no separate crash-recovery subsystem to get wrong, because there is nothing to recover: the file is always in a consistent state, either the new one (if the commit's final fsync landed) or the previous one (if it didn't).

Three enforced commitments follow from that and shape everything downstream. They are stated as facts in [ARCHITECTURE.md#design-commitments](ARCHITECTURE.md#design-commitments); the rest of *this* document is their justification.

- **Single-writer, embedded** — one process, `&mut self` mutators, no internal locking.
- **Shadow paging, not WAL** — fresh pages plus a superblock swap; recovery is superblock selection.
- **Durability over performance** — fsync on every commit, a checksum on every page, and a poison model that treats any fsync failure as terminal.

If you internalize only one thing: Chisel spends disk space and write amplification to buy *provability*. You can convince yourself of its crash-safety by inspection, without simulating a log replay in your head. Almost every "why not the faster thing?" answer below reduces to "the faster thing costs us that provability."

---

## The load-bearing decisions

What follows is the spine of the document: the eighteen design decisions that, if reversed, would make Chisel a different system. Each one names what was **chosen**, what was **rejected** (usually the more instructive half), and **why**. Sources are cited as `ADR-N`, spec dates, or `I<n>` issue numbers so you can go deeper.

### Durability model: shadow paging over WAL (ADR-1)

**Chosen.** Every mutation allocates a fresh page. Commit writes the new pages, fsyncs, writes a new superblock into a *different* slot, and fsyncs again. Recovery is `Superblock::select` scanning N candidate slots and picking the highest valid `txn_counter`.

**Rejected.** A write-ahead log with in-place updates — the PostgreSQL/SQLite standard. WAL recovery is a substantial subsystem in its own right: a replay state machine, checkpoint handling, log truncation. That machinery adds a risk surface comparable to *the entire rest of Chisel*. A hybrid (WAL for small changes, shadow for large) was also rejected, as combining the worst of both — the recovery paths multiply and the mode boundary becomes another correctness obligation nobody asked for.

**Why.** Shadow paging trades disk space (you hold the live and previous version of every mutated page until commit) for code simplicity. There is no log to replay; the "is this database openable?" check *is* the crash-recovery path; crash safety is provable by inspection. The known cost is accepted with eyes open: write amplification (a one-byte change costs a full 8 KB page) and the eventual need for `defrag.rs` to reclaim sparse pages over time. See [ARCHITECTURE.md#commit-protocol](ARCHITECTURE.md#commit-protocol) for the mechanism.

### Page size: 8 KB, aligned to the storage block

**Chosen.** `PAGE_SIZE = 8192`, fixed for the life of a database and pervasive in every geometry calculation (slot directories, radix fan-out, freemap coverage).

**Why.** 8 KB was chosen to align with the underlying storage block size, so that a page maps cleanly onto the device's own block granularity rather than straddling it. *(Recorded directly from the engine author; there is no written ADR for the page-size choice.)*

### Single-writer, enforced by the type system (ADR-2)

**Chosen.** One process, one writer. The invariant is enforced at three levels at once: an OS exclusive `flock` in `page_io.rs`, `&mut self` on every mutating API, and an explicit statement that this is philosophical, not a v1 shortcut. Reads take `&self` because the cache lives behind a `RefCell`, not a `Mutex`.

**Rejected.** Multi-writer with an internal `RwLock`/`Mutex` plus conflict detection and deadlock handling — roughly doubles engine complexity. MVCC — adds version chains, garbage collection, and snapshot-isolation semantics, all out of scope for an embedded store. And crucially, "single-writer now, multi-writer later" was rejected: relaxing `&mut self` after the fact is a 2.0 breaking change for *every* consumer, not a minor bump, so deferring the decision doesn't actually defer the cost.

**Why.** With `&mut self`, "two concurrent transactions" is impossible to even *express* — so there is no test for it, because there is no API for it. The type system encodes the invariant in a way internal locking never could. Choosing `RefCell` over `Mutex` is a direct consequence: with no concurrency to guard against, a `Mutex` would be pure overhead and a lie about the model. (See the memory note `project_chisel_single_client_design` — this really is a design stance, not a limitation to be lifted.)

### Per-module copy-on-write, no central abstraction (ADR-3)

**Chosen.** Each module implements its own COW. `handle_table.rs` clones the root-to-leaf path; the freemap tree allocates fresh structural pages during `persist_freemap`; `data_page.rs` mutates in place via `claim_page` and reuses the same page across commits. There is no `trait Cow`, no `enum CowStrategy`.

**Rejected.** A centralized COW trait spanning all page-type modules. It was rejected because the modules' actual COW shapes differ enough that a uniform interface would be either too generic (losing the useful per-module detail) or too specific (sprouting variants that serve exactly one module). A generic page-mutation wrapper around a strategy object hit the same wall, plus it would leak generics into the public API.

**Why.** Co-locating COW with each page-type's logic lets each evolve independently — the handle table grew its `grow()` and short-circuit optimizations without touching the freemap or the data pages. The accepted cost is real: some repeated boilerplate across three or four modules, and an onboarding surprise. New readers look for "the COW abstraction" and there isn't one — deliberately. If you are that reader right now: this is the answer. See [ARCHITECTURE.md#per-module-copy-on-write](ARCHITECTURE.md#per-module-copy-on-write).

### N rotating superblocks for atomic commit (ADR-4)

**Chosen.** N superblock slots (`Options::superblock_count`, range `2..=16`, default 2) occupy file offsets `0..N`. Commit writes to slot `txn_counter % N`; recovery validates all N (magic, checksum, count-in-range) and picks the highest valid `txn_counter`.

**Rejected.** A single superblock overwritten each commit, protected by a PostgreSQL-style double-write buffer. Rejected because shadow paging *already* double-protects the data pages, so only the superblock itself needs torn-write protection, and N rotating slots are simpler than standing up a separate buffer area. A write-side mini-WAL just for the superblock fell to the same complexity argument as ADR-1.

**Why.** A single superblock overwritten in place is vulnerable to a torn write leaving the file unrecoverable. Rotating slots give trivial torn-write recovery — a torn slot fails its checksum and is ignored — need no separate journal (the slots *are* the journal), and offer a configurable space/durability tradeoff: higher N survives more *consecutive* torn writes. The cost is N pages of overhead at the file head and the fact that changing N for an existing file needs migration. The commit always writes the *stalest* slot, so a torn write can only damage a slot you were about to discard anyway; see the round-robin reasoning in [ARCHITECTURE.md#commit-protocol](ARCHITECTURE.md#commit-protocol).

**Why the default is 2.** The default `superblock_count` of 2 is a deliberate integrity/performance balance: two slots already give clean torn-write recovery, and 3 is defensible, but larger N carries considerable performance cost for little additional safety — each extra slot only buys survival of one more *consecutive* torn write, already a vanishingly rare event. The `2..=16` range leaves headroom for a deployment that wants more redundancy without imposing it by default. *(The specific default was recorded from the engine author; ADR-4 covered the rotating-slot mechanism but not the chosen value.)*

### Poison model on fatal errors (ADR-6, I1)

**Chosen.** Any fatal error — a commit-path `IoError`, a `ChecksumMismatch`, a `CorruptSuperblock`, a `DecryptionFailed`, anything raised after the commit protocol has begun (see `ChiselError::is_fatal()`) — sets a poison flag on the `TransactionManager`. Every subsequent call, *including reads*, returns `Poisoned`. The only legal recovery is to drop the handle and `Chisel::open` again.

**Rejected.** Retrying the failed fsync — rejected outright per fsyncgate (below). Poisoning writes but letting reads continue — rejected because reads share the page cache with writes, so a corrupt page may already have been served, and there is no way to know *which* reads are tainted. Auto-reopen on poison — rejected because reopen is a substantial state transition (file descriptors, locks, cache) that the caller must orchestrate; forcing it silently would hide a boundary the caller needs to see.

**Why.** This is the fsyncgate decision, and it is worth understanding rather than memorizing. On Linux since 2018, a failed `fsync()` cannot be safely retried: the kernel reports the error on the *next* fsync and then *clears* its error state, so a later "successful" fsync does **not** mean the earlier data is durable — those dirty pages may already have been dropped from the page cache. PostgreSQL responds by PANICking on fsync failure for exactly this reason. Chisel takes the same stance, and it costs almost nothing here because shadow paging plus embedded single-writer makes reopen cheap — and reopen exercises the *same* recovery code path a real crash would, which is a testing win. The design mirrors `std::sync::Mutex` poisoning: once the invariant might be broken, refuse to pretend otherwise. See the memory note `project_chisel_i1_poison_decision` and [ARCHITECTURE.md#poison-model](ARCHITECTURE.md#poison-model).

### Spillway sidecar over a hard cache ceiling (ADR-5)

**Chosen.** A sidecar file `<db_path>.spillway` absorbs LRU-tail dirty pages when the cache is full of dirty (un-evictable) pages, turning the cache into a strict bound rather than an elastic one. It is bounded by `Options::spillway_max_bytes` (default `1024 × cache_max_bytes`). Each slot carries its own XXH3 over `page_id ‖ page_bytes`; it is *never* fsynced and is truncated at open and at every commit/rollback. Setting `spillway_max_bytes = 0` disables it and restores `CacheFull`-at-cap.

**Rejected.** The pre-2026-05-04 design had a `HARD_CEILING_MULTIPLIER = 8` — the cache could balloon to 8× its nominal size before erroring. That was rejected because real workloads legitimately need to dirty far more than 8× the cache (a document-store benchmark with log-normal value sizes can dirty 100×). Simply *raising* the multiplier only delays the same failure. Unbounded growth was rejected as an OOM path that violates the memory budget. Spilling into a reserved area of the *main* file was rejected because that becomes a permanent on-disk artifact needing its own checksum and durability management.

**Why.** The key insight — the reason the spillway is *simple* — is that its contents are, by definition, uncommitted. So it never needs to be fsynced: a crash just discards it, and the prior committed superblock stays active. That single fact ("never fsync the spillway") is what makes the whole feature correct and cheap. The accepted cost: a no-spill commit is now three fsyncs (the I28 pre-drain, the main-pages flush, and the superblock), not two, plus a new `SpillwayFull` error. The public-API break was `Options::cache_size` (a page count) becoming `cache_max_bytes` (a byte budget). See [ARCHITECTURE.md#spillway](ARCHITECTURE.md#spillway) and spec `2026-05-03-chisel-spillway-design`.

### In-memory mode via a `Vec<u8>`-backed `PageIo` (ADR-8)

**Chosen.** `Chisel::open_in_memory` runs the *full* engine against a `Vec<u8>`-backed `PageIo` — no filesystem, no `flock` — exposed as `chisel.open(None)` in Python. Same code path, same guarantees, except durability.

**Rejected.** A tmpfs or OS-level mock filesystem — still incurs kernel-call overhead and real FS semantics, whereas a `Vec<u8>` removes the OS from the loop entirely and is faster. A separate `MemChisel` type — rejected because it would duplicate every method and drift from the real one. A public `Backend` trait — rejected because the internal `Backing` enum (File / Memory) already *is* that, and exposing it through a constructor keeps the public API smaller.

**Why.** Tests, benchmarks, and ephemeral workloads shouldn't pay for disk I/O or touch the filesystem — but running them down the *same* code path means an in-memory test catches bugs that would also bite on disk. The one cost worth remembering: counters reset on close+reopen, because a `Vec` doesn't persist.

### Counter instrumentation via `Chisel::counters()` (ADR-9)

**Chosen.** Four cumulative-from-open counters — `cache_hits`, `cache_misses`, `pages_allocated`, `fsync_calls` — each a `Cell<u64>` at its increment site, aggregated into a `#[non_exhaustive] ChiselCounters` read via `&self`. Misses, allocations, and hits count *attempts*; `fsync_calls` counts only *successes*.

**Rejected.** Internal logging/tracing — rejected because it forces log-parsing on the consumer, whereas counters give a structured, allocation-free read. Latency histograms — out of scope for v1 (a histogram-crate dependency and per-op overhead); the bench harness computes distributions externally instead. A configurable counter set — rejected as complexity for marginal benefit, with `#[non_exhaustive]` keeping future additions cheap anyway.

**Why.** Wall-clock time is too noisy for *component-level* analysis — you cannot tell a cache-hit-rate regression from an fsync-rate regression from a timer. The bench harness reads counters before and after each scenario cell and reports the delta, giving per-cell attribution. `fsync_calls` counts only successes for a principled reason: a failed fsync poisons the engine, and a counter on a poisoned engine has no defined further meaning. See [ARCHITECTURE.md#engine-activity-counters](ARCHITECTURE.md#engine-activity-counters).

### Chunk tags and the reverse membership index (ADR-12)

**Chosen.** An optional immutable `u32` tag per chunk, fixed at allocation (`tag == 0` is the untagged sentinel). The *forward* map (handle → tag) lives in four of the `HandleEntry`'s five reserved bytes, so `tag(handle)` is O(1) with no extra page read. The *reverse* map (tag → {handles}) is a two-level COW radix (`MembershipIndex` over `RadixU64`): an outer tree keyed by tag whose leaf bit-packs `(inner_depth | inner_root)`, and per-tag inner trees keyed by handle. `delete_with_tag(tag, max)` is the bounded relation-drop primitive.

**Rejected.** Storing the group id inside the value and scanning all chunks — O(all chunks) per scan or drop, the exact cost the feature exists to avoid. A single flat radix over a packed `(tag:handle)` key — rejected for the two-level form, which reuses the u64-radix shape *and* makes "enumerate the distinct tags" O(T). A `u64` tag — rejected because it wouldn't fit the reserved entry bytes, and fitting there is what makes forward storage *free*. Mutable tags — deferred, because immutability removes the retag path entirely (retagging becomes allocate-new-plus-delete-old). Bitmap inner sets — deferred, since per-tag sets are expected sparse, making a radix the right default.

**Why.** The relational client needs three operations without an O(all chunks) pass: scan a relation, drop a relation, and delete-a-chunk-and-remove-it-from-its-set. Forward storage in the reserved entry bytes is *free* — no side table, no extra page reads — which is precisely why `u32` (which fits) beat `u64` (which doesn't). The reverse map reuses the existing radix machinery, COW discipline, superblock-anchored root, freemap reclaim, and poison model, adding *no* new commit-protocol surface. Untagged chunks cost nothing and are invisible to non-users. See [ARCHITECTURE.md#chunk-tags-the-membership-index-in-use](ARCHITECTURE.md#chunk-tags-the-membership-index-in-use) and spec `2026-06-02-chunk-tags-design`.

### Within-session iteration-stability contract (ADR-13)

**Chosen.** Promote an already-true property to a *documented, tested* guarantee: within a single open instance, repeated `handles()` / `handles_with_tag(tag)` calls return an identical `Vec` as long as the live set is unchanged and no `defrag` ran. The order itself stays **unspecified** — this is a repeatability guarantee, not an ordering one. No production code changed to add it.

**Rejected.** Guaranteeing a *specific* order (ascending handle, which the radix walk already happens to produce) — rejected because it would commit the public contract to a particular order and constrain future index internals. Snapshot/MVCC isolation across mutation — out of scope (single-writer, and the requirement explicitly excludes mutation between scans). A wider scope surviving reopen or defrag — rejected in favor of single-session, which constrains internals the least. An internal `debug_assert` sortedness canary — rejected because it would couple internal code to the very ascending behavior the contract deliberately declines to promise.

**Why.** The relational client wants to scan a relation, do work, and scan again expecting identical results — to re-drive a query, resume a pass, cross-check — without defensively sorting or snapshotting. "Repeatable but opaque" serves that exactly while leaving the radix order, the reopen layout, and defrag reordering all free to change. The guarantee rests on the radix-depth re-derivation invariant below; a rolled-back `grow()` that failed to restore depth would make a later scan mis-enumerate. See [ARCHITECTURE.md#handle-stability](ARCHITECTURE.md#handle-stability) and spec `2026-06-04-stable-chunk-iteration-design`.

### Client byte — spending the last reserved entry byte (ADR-14)

**Chosen.** A mutable per-chunk `u8` "client byte" in `HandleEntry` byte `[15]`: `set_client_byte` / `client_byte`, default 0, fully opaque (no search, filter, or index), carried forward across value `update()` and reverted on rollback. No on-disk format change; `FORMAT_MINOR_VERSION` stays 1.

**Rejected.** Storing the byte with the value on a data page — rejected because it forces a full value rewrite per change and re-couples metadata to value bytes. Making it immutable/set-at-allocation like the tag — rejected because the client needs in-place change. A richer `Handle { id, tag, client_byte }` return type — rejected as a breaking change to every handle-returning signature. A MINOR bump 1→2 for record-keeping — rejected because the layout is byte-identical and there is nothing for a reader to gate on.

**Why.** Byte `[15]` has *always* been part of the 16-byte entry and always written as 0, so activating a reserved byte is not a versioned change — there is genuinely nothing for a reader to distinguish. Entry-resident storage makes a flip cost one handle-table leaf COW, independent of the value's size. This decision refines ADR-7's versioning rule: reserved bytes are part of the format from creation, and only *new structures or semantics a reader must gate on* warrant a version bump. The one accepted caveat: a pre-feature binary hardcodes `[15] = 0`, so rewriting an entry under an old binary silently clears the byte — acceptable pre-1.0. See [ARCHITECTURE.md#client-byte](ARCHITECTURE.md#client-byte).

### Two-tier format versioning: file MAJOR/MINOR plus per-page byte (ADR-7, I29/I31)

**Chosen.** The superblock carries a packed `format_version` u32 (upper 16 bits MAJOR, lower 16 MINOR); the open-time gate compares MAJOR only. Every non-superblock page carries a one-byte `page_format_version`. Version dispatch is per-module and decode-only — the reader branches on the byte; writes always stamp `page::current_version`. A file whose MINOR exceeds the binary's is forced read-only rather than rejected.

**Rejected.** File-level versioning only — rejected because then every per-page tweak forces a file-wide migration. Page-level only — rejected because it loses the clean "this binary simply cannot read this file" failure. A full schema-migration system (migration scripts, version-jump testing) — out of scope for embedded.

**Why.** Compatibility genuinely has two granularities — the file's overall layout and any single page-type's layout — and conflating them forces a file-wide bump for a change that touches one page type. Chisel has four-plus page types likely to evolve at different rates. The MAJOR check gives a crisp incompatibility failure; the per-page byte lets one page-type's layout move within a major without migrating the others, with lazy COW-on-write upgrade as the default path. A refinement worth noting (and the seed of ADR-14): a zero-default *additive* field needs no version bump at all — the per-page byte exists only to disambiguate absent-versus-zero where zero is a legitimate value. The read-side dispatch is **dormant today** (nothing reads the byte yet); see [ARCHITECTURE.md#format-versioning-two-tier](ARCHITECTURE.md#format-versioning-two-tier) and spec `2026-06-21-per-page-format-versioning-design`.

### Multi-page freemap: a COW radix of bitmap leaves (spec 2026-06-22)

**Chosen.** Generalize the single-page freemap into a COW radix tree of bitmap leaves — a *third* radix alongside the handle table and the membership index. The leaf is today's FreeMap page (`0x04`), unchanged; the interior is a new `FreeMapInterior` (`0x07`); **depth 0 is exactly the current single-page format**. Depth is stored explicitly in the superblock (byte 320), the freemap moved into the page cache, and an in-memory "lowest free" hint accelerates find-first-free.

**Rejected.** A linked chain of freemap pages — O(n) to scan for a free page in a high range. A fixed two-level directory — re-introduces a (merely higher) ceiling. A `FreeMapFull` guard *instead of* the real feature — rejected: guarding the symptom isn't fixing it. Spine-walk depth recovery (the trick the other two radixes use) — rejected *here specifically* because a sparse freemap makes it ambiguous: a zero pointer near the root looks like a shallow tree, so depth must be stored, not derived. Per-interior "subtree-has-free" summary bits — deferred (YAGNI for v1) in favor of the in-memory hint.

**Why.** This one has a *bug* behind it. Past 65,344 pages (~512 MB at 8 KB pages) the single-page `FreeMap::mark_free` silently no-ops — reclamation just *stops*, and every freed page leaks forever with **no error surfaced**. A radix-of-bitmaps removes the ceiling entirely (coverage `65,344 × 1021^depth`, growing as `log_1021`) and is the lowest-surprise fix available, because the engine already has two proven COW radix trees to imitate. Moving the freemap into the cache makes per-transaction cost independent of database size.

There is one **critical, revised** sub-decision that is easy to get wrong and worth carrying in your head: the freemap's *own* structural COW pages must recycle **out-of-band** — from an in-memory one-commit-deferred pool plus file extension — and **never** from the bitmap itself. An earlier draft wrongly assumed bitmap reclamation. The reason it cannot work: sourcing a structural page from a free bit *clears* that bit, which COWs a leaf, which needs another structural page, which clears another bit — an unbounded recursion. Crash-orphaned recycle entries (the pool is in-memory, so a crash loses it) are swept back by a defrag orphan-scan, chosen over persisting the recycle list or accepting a permanent per-crash leak. See [ARCHITECTURE.md#freemap-reclamation](ARCHITECTURE.md#freemap-reclamation) and the memory note `project_chisel_multipage_freemap`.

### Encryption cipher: XChaCha20-Poly1305 with a random 192-bit nonce (ADR-15)

**Chosen.** XChaCha20-Poly1305 AEAD, a fresh random 192-bit nonce per page write stored alongside the ciphertext, with AAD = `page_id` (anti-relocation).

**Rejected.** AES-256-XTS — the FDE standard, length-preserving, zero overhead, no format change — rejected because it provides confidentiality *only*, with no cryptographic tamper-detection; the client wanted authenticated encryption and the one-time format change is free (there are no production databases yet). A deterministic nonce from `(page_id, counter)` — rejected as **unsafe under shadow paging** (see below). AES-256-GCM — rejected because its 96-bit nonce forces either the deterministic-nonce hazard or a stored counter, and it leans on AES-NI for constant time.

**Why.** Two forces decide this. First, AEAD upgrades integrity from the *forgeable* non-cryptographic XXH3 to an *unforgeable* Poly1305 tag — you get tamper-detection, not just secrecy. Second, and decisively, the 192-bit extended nonce lets random nonces be used *safely*, which matters enormously under Chisel's specific machinery: a crashed transaction discards its writes and returns page_ids to the freemap **while the durable counter does not advance**. So a deterministic `(page_id, counter)` nonce would write *different plaintext* under the *same* `(key, nonce)` after a crash-and-retry — catastrophic keystream reuse. Random nonces have nothing to persist and cannot hit that crash-reuse class at all. ChaCha is also constant-time in portable software with no AES-NI dependence — the right default for an embedded library shipping to unknown hardware — and since crypto throughput sits far below fsync latency, AES-GCM's hardware edge is irrelevant here. This nonce-reuse-under-shadow-paging reasoning is the single most important thing to understand about the crypto layer. See [ARCHITECTURE.md#on-disk-encryption](ARCHITECTURE.md#on-disk-encryption) and spec `2026-06-29-on-disk-encryption-design` §2/§2.1.

### Encryption keys: envelope DEK/KEK with an 8-slot table, O(1) rotation (ADR-15)

**Chosen.** A random per-database 256-bit DEK (from `OsRng` at create) seals every page and the sensitive superblock body. The DEK is never stored bare — it is wrapped under a KEK derived from the client key (HKDF-SHA256 for raw keys, Argon2id for passphrases) and held in an 8-slot key-slot table in the superblock's plaintext reserved region. `add_key` / `rotate_key` / `remove_key` re-wrap the *stable* DEK — O(1), no data re-encryption. `rotate_key` stages the new slot before revoking the old (no zero-key window); `remove_key` refuses to clear the last active slot (brick prevention). A successful unwrap *is* proof the client key is correct — there is no separate password verifier.

**Rejected.** Encrypting only the data pages and leaving the superblock plaintext — rejected because `named_roots` holds user-chosen names, which are real user data a plaintext body would leak. Full DEK rotation (re-encrypting every page under a fresh DEK) — deferred (I142) as a heavy O(total_pages) whole-file operation reserved for "the DEK itself is compromised"; credential rotation is the far more common need and is O(1).

**Why.** Envelope encryption makes credential rotation O(1) — you re-wrap the DEK — instead of O(database size). The per-slot KDF choice matches input entropy: HKDF is fast and correct for high-entropy keys, while Argon2id is memory-hard to resist brute-forcing low-entropy passphrases (its params are recorded per slot). And every rotation op is an ordinary superblock A/B + fsync commit, so it reuses the existing crash-safe protocol wholesale: a metadata-only `rewrite_crypto_header` commit persists a rotated slot table atomically (write the inactive slot, fsync, promote), so a crash mid-rotation leaves the old table intact.

Two threat-model boundaries are documented rather than solved, and you should know them before you rely on this: there is **no rollback/replay resistance** (an attacker who substitutes a wholly older, validly-signed image is undetectable without an external trust anchor like a TPM), and the DEK sits in plaintext in process memory during a session (mitigated by zeroize-on-drop, not by encryption). See spec `2026-06-29` §3/§5/§9 and [issue #140](https://github.com/pgexperts/chisel/issues/140) (the deferred bulk DEK rotation, formerly I142).

### Encryption page format: 8232-byte stride, logical page stays 8192, MAJOR 1→2 (ADR-15)

**Chosen.** Encrypted databases use an 8232-byte on-disk stride (8192 ciphertext + 16 tag + 24 nonce) uniformly from birth, including superblock slots (zero-padded). The *logical* page stays 8192, so freemap, data-page, and handle-table geometry are untouched. Encryption is a transform at the page-I/O seam: `PageCache` owns a `PageCipher` and seals once (on flush *or* evict); `page_io` is stride-aware but crypto-agnostic; the spillway holds sealed blobs. Encrypted DBs stamp MAJOR = 2; plaintext stays MAJOR = 1 and byte-identical.

**Rejected.** Shrinking the logical content to fit a 40-byte trailer *inside* 8192 — rejected because it makes every geometry constant encryption-dependent, an invasive and bug-prone refactor of the engine's core page math. Whole-page-sealing the superblock — rejected because page 0 must stay plaintext-bootstrappable (magic, format version, txn counter, page size, superblock count, and the crypto-header/key-slot table) so the engine can learn the stride and find the key material before anything can be decrypted. A per-page (I31) format change — not required, because the logical 8192 image is unchanged; encryption lives entirely *below* it.

**Why.** A larger stride is the smallest possible blast radius. The page cache, the freemap, the data pages, the handle table, and the entire transaction layer keep producing and consuming byte-identical 8192-byte pages; only the I/O stride and the seal/open transform change, at a cost of 0.49% larger files. Sealing once (drain is a verbatim byte copy of the already-sealed blob — both spill and main-file writes use `AAD = page_id`, so no re-seal is needed) avoids a crypto round-trip on drain and keeps exactly one ciphertext per write. MAJOR = 2 is the *first real exercise* of ADR-7's MAJOR tier: an encryption-unaware old binary, gating on `MAJOR == 1`, refuses a MAJOR = 2 file rather than misreading ciphertext as plaintext. The inner XXH3 checksum, now redundant with the AEAD tag, is deliberately *kept* so the upper layers stay untouched — a cheap inner sanity check after open. See spec `2026-06-29` §4/§5/§8.

### MSRV pinned at 1.82 with tilde-pins on edition2024 deps (Cargo.toml, I55/I61/I110)

**Chosen.** `rust-version = 1.82` in all three subcrates; a CI `msrv` job builds `-p chisel` library-only against `dtolnay/rust-toolchain@1.82`. Dependencies are tilde-pinned to hold that floor: `zeroize ~1.8` (with the derive feature dropped entirely — only `Zeroizing<T>` is used) and the transitive `base64ct ~1.6` (via `argon2 0.5`). The RustCrypto deps are unconditional (seal/open is always compiled) but meaningfully reach the dependency tree only when a database is opened with a key.

**Rejected.** A floating MSRV on stable — rejected because an unannounced 1.x bump can land silently (README's "Rust stable" is not a pinned promise). Running the msrv job with `--tests` — tried and reverted, because proptest's transitive `getrandom` needs edition2024 (1.85+); the MSRV promise is about the *published library* (no getrandom/proptest in its tree), so the job stays lib-only. Adopting a real Cargo workspace — deferred (I61): members would share edition/rust-version/feature resolution, too restrictive for the PyO3 abi3 binding, and the bench floats its floor faster than the engine wants.

**Why.** 1.82 is a conservative pin driven by actual language usage (`is_none_or` is 1.82+; the true stdlib floor is nearer 1.74). Pinning gives a stable, published MSRV promise you bump only with a release-notes call-out. The tilde pins are what *hold* that floor: `zeroize 1.9` and `base64ct 1.7+` both adopted edition2024 (Rust 1.85+), which would drag the library MSRV above 1.82; the pinned lines are the last edition-2021 releases, and the un-exercised code paths (no `#[derive(Zeroize)]`, never emitting PHC hash strings) make the pins safe.

---

## Implementation history

This section is history, framed to explain *why the code looks the way it does* when you open it. It is not a changelog; it is the story that leaves fingerprints on the structure.

### The benchmark suite, PRs 1–8

The benchmark infrastructure was built incrementally across eight PRs, and its shape is deliberate. It lives in a `bench/` subcrate that is a *sibling* to `python/`, not a plain workspace member drawn into the engine's own build in a way that would auto-run its 10–25 minutes of tests on every `cargo test`. (This is also the concrete reason plain `cargo test` versus `cargo test --lib` matters in this repo — see the memory note `feedback_cargo_test_full`.) The subcrate depends on `redb` and `rusqlite` for cross-engine comparison; keeping it a sibling is what keeps the *engine's* dependency graph minimal — the storage engine itself needs neither.

The suite has three layers, and they exist because no single tool fits all three jobs:

1. **Cross-engine equivalence tests** — the same operations run against Chisel, redb, and SQLite, asserting identical *results*, so a performance comparison is never comparing engines that secretly disagree on behavior.
2. **A Criterion micro-grid** — 165 cells of small, isolated operations. Criterion's many-samples statistical model is exactly right for micro-measurement.
3. **A YCSB-style scenario tier** — realistic mixed workloads, timed with `Instant::now()` rather than Criterion, because Criterion's sampling model *exceeds* the 1–6 minute scenario budget. This is why you'll see a hand-rolled timing hybrid there instead of "just use Criterion everywhere."

That three-layer split (ADR-10) and the sibling-crate decision together are why the bench directory looks structurally unlike the rest of the repo.

### Cross-engine fairness and the macOS fsync problem (ADR-11)

A subtle fairness bug drove ADR-11. SQLite's default `fsync` on macOS does *not* actually flush to stable storage the way Chisel's does — Chisel uses `F_FULLFSYNC`. If you benchmark them naively, macOS numbers flatter SQLite by comparing Apple's default fsync semantics against Chisel's stricter durability. The fix: `SqliteEngine` issues `PRAGMA fullfsync=ON` for Strict durability, with *no* `cfg(target_os)` gate — Linux simply ignores the pragma, and macOS matches Chisel's `F_FULLFSYNC`. The absence of the platform gate is intentional and is the whole point: it makes the macOS numbers reflect *engine behavior*, not Apple's default fsync.

### The counter-driven measurement idea

The instrumentation counters (ADR-9, above) exist *because* the benchmark suite needed them. Wall-clock time can tell you a scenario got slower but not *which component* regressed. The counters — hits, misses, allocations, fsyncs — let the harness read a before/after delta per scenario cell and attribute the change: a rise in `fsync_calls` is a durability-cost story, a fall in the hit rate is a cache story. The counters and the bench harness are two halves of one measurement design.

### The spillway feature

The spillway (ADR-5, above) is the clearest case of a benchmark *finding a real ceiling*. The document-store scenario with log-normal value sizes could legitimately dirty ~100× the cache, and the old `HARD_CEILING_MULTIPLIER = 8` turned that into a `CacheFull` error on a workload that was doing nothing wrong. The spillway replaced the elastic ceiling with a strict cap plus a discardable sidecar. When you read the commit protocol and wonder why there are three fsyncs instead of the two the durability story implies, the extra one (the I28 pre-drain) traces directly back to this feature's interaction with `persist_freemap`.

### Lessons captured

A few hard-won lessons are worth carrying forward because they recur:

- **The radix-depth silent-corruption bug.** Both the handle table and the membership index keep their tree depth as an *in-memory* field that is never serialized — it's re-derived by walking the left spine from the root, since each `grow()` installs the old root at child 0. The bug: any path that *restores* a root (open, and especially rollback) must re-derive that depth, or the in-memory descent depth disagrees with the page it descends. A rolled-back `grow()` shrinks the tree by a level; a stale-deep depth then mis-descends and returns `InvalidHandle` for *committed* handles, or mis-enumerates a tag — silently. It surfaced first in the membership index during chunk-tags work and was then recognized as the *same* root cause in the handle table. The two fixes (I99 for the handle table, C1 for the membership half) extract the open-time spine walk into a reusable `recover_depth` called from both rollback paths. This is why iteration stability (ADR-13) rests on that invariant, and why [ARCHITECTURE.md#in-memory-radix-depth-is-re-derived-from-the-root-never-stored](ARCHITECTURE.md#in-memory-radix-depth-is-re-derived-from-the-root-never-stored) flags it as load-bearing.
- **The commit-protocol ordering is not incidental.** Every step in `TransactionManager::commit` is placed where it is for a crash-window reason. The most non-obvious is the I28 pre-drain: `persist_freemap` can trip the cache's spill-or-error path, and a `CacheFull` raised *mid-commit* would be silently promoted from operational to fatal by the commit wrapper's poison-on-any-error rule. Pre-draining every dirty pin makes the strict cap reachable via normal eviction, at the cost of one extra fsync. Reordering these steps changes what a recovering reader can observe; treat the order as a contract.
- **Structural freemap pages must never come from the bitmap.** Restated here because it cost a design draft: an earlier multi-page-freemap plan assumed the freemap's own COW pages could be reclaimed from free bits. They can't — it recurses without termination. Out-of-band recycling is the only correct source.

---

## Benchmark methodology

The measurement design is worth a section of its own because its choices are principled, not arbitrary, and because someone will eventually need to add a benchmark and should follow the same rules.

**Measurement layers.** As above, there are three tiers, and the split is about matching the tool to the timescale: Criterion's many-samples model for the micro-grid (where each op is sub-millisecond and statistical rigor is achievable and cheap), and a hand-rolled `Instant::now()` hybrid for the YCSB-style scenarios (where a single run is 1–6 minutes and Criterion's sampling would blow the budget). Cross-engine equivalence tests underpin both, so no comparison is ever between engines that secretly disagree on behavior.

**macOS `F_FULLFSYNC` fairness.** The most important methodological rule is that durability must be compared like-for-like. Chisel always uses `F_FULLFSYNC` on macOS; a naive SQLite comparison would pit Chisel's true flush against Apple's weaker default and make Chisel look slow for being *more* correct. The `PRAGMA fullfsync=ON` fix (ADR-11), ungated by platform, is what makes the numbers honest. If you add a new comparison engine, this is the trap to check first.

**Counter-driven measurement.** Prefer the engine's own counters over wall-clock whenever you want to know *why* something changed. Time tells you a scenario regressed; the delta in `cache_hits` / `cache_misses` / `pages_allocated` / `fsync_calls` tells you which subsystem did it. The harness reads counters before and after each scenario cell and reports deltas; this is the intended primary consumer of `Chisel::counters()`.

**A CI caution (from the project's standing guidance).** Benchmark numbers from shared CI runners are too noisy to gate a pipeline on — a throttled or noisy-neighbor VM can spike an allocation-heavy bench 2×+ on byte-identical code. Keep perf/benchmark CI steps report-only (non-blocking); let only correctness (build, test, lint, clippy) gate the pipeline. Benchmark baselines exist for *trend-tracking*, not pass/fail, and should be re-recorded when a trend shows a persistent shift. Gate on perf only on consistent, dedicated hardware.

---

## Rationale not recovered

The following choices are real and load-bearing, but the project's ADRs, specs, and issue log do **not** record *why* the specific value or option was picked over its neighbors. They are listed here honestly rather than back-filled with a plausible-sounding invention — a fabricated rationale in a "why" document is worse than an acknowledged gap, because a future reader would trust it. As gaps are answered by the people who made the calls, they move up into the decision sections above: the **8 KB page size** (aligned to the storage block) and the **`superblock_count` default of 2** (an integrity/performance balance) were recorded from the engine author and now live with their decisions.

- **Why XXH3 specifically** (over CRC32C, BLAKE3, or another non-cryptographic checksum) for the page and spillway checksum. XXH3 is used everywhere, but no source records the comparison; only the I77 FxHash-over-SipHash swap for *internal maps* has recorded rationale.

  > Rationale not recovered from project sources.

- **Why 8 key-slots** (over 4 or 16). Spec §3.2 shows that 8 × 128 bytes fits comfortably in the reserved region, but nothing explains why 8 is the right *operational* number of credentials.

  > Rationale not recovered from project sources.

- **The chosen `Argon2Params` defaults** (m_cost / t_cost / p_cost) for create-time passphrase slots, and the security/latency tradeoff behind them. The slot codec stores them and the API exposes `Argon2Params`, but the default cost values are unexplained.

  > Rationale not recovered from project sources.

- **Why `spillway_max_bytes` default = 1024× `cache_max_bytes`** (8 GiB at the 8 MiB cache default). ADR-5 states the multiplier but does not justify 1024 specifically as the ceiling.

  > Rationale not recovered from project sources.

- **The membership-tree depth-6 bound** beyond the fan-out arithmetic. The freemap spec derives its fan-out math, but the membership tree's depth-6 `MAX_DEPTH` is only cross-referenced, not independently justified in the harvested sources.

  > Rationale not recovered from project sources.
