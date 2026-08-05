// transaction.rs — Transaction lifecycle, savepoints, commit protocol, and data operations.
// This is the orchestration layer (layer 6 in the module graph per ARCHITECTURE.md) that ties
// together the handle table, data pages, overflow pages, freemap, superblock, and page
// cache into a coherent transactional API.
//
// Durability model (shadow paging, no WAL):
//   - Writes never overwrite live pages. Mutations go to freshly allocated pages via
//     PageCache::new_page() and the new roots are threaded through a rebuilt handle
//     table spine (COW). The previously-committed pages remain intact on disk until
//     the new superblock supersedes them.
//   - A commit becomes visible atomically when a new superblock with a higher
//     txn_counter and a valid checksum is fsync'd to its (alternating) slot.
//   - Crash recovery = open_existing() runs Superblock::select() and picks the
//     highest-txn_counter superblock with a valid checksum. A torn/partially-written
//     new superblock fails its checksum, so the previous committed state wins —
//     no log replay, no undo.
//
// Concurrency model:
//   - A TransactionManager is single-writer. active_txn guards against nested begin().
//     Multi-process exclusion is enforced at the file layer by flock() in PageIo;
//     only one TransactionManager may hold the database open at a time.
//   - TransactionManager is NOT internally thread-safe — callers must serialize
//     access. Readers and writers share the same PageCache; there is no MVCC.
//
// In-memory vs on-disk state during an open transaction:
//   - All mutations live in the PageCache as dirty entries. Nothing mutated by the
//     transaction is durable (or even written to the file in general) until commit().
//   - The superblock on disk still points at committed_roots; current_roots lives
//     only in memory. A crash mid-transaction discards all dirty pages from cache
//     and the on-disk superblock still references the prior committed snapshot.
//   - NOTE: `new_page()` (file extension) extends the underlying file immediately;
//     data-page allocation (via `cow_alloc`) prefers reuse from the committed
//     freemap tree but also calls through to `new_page()` when the freemap is
//     empty. Either way, any pages
//     extended-but-uncommitted before a crash are harmless because nothing in the
//     committed superblock references them, and the rollback path
//     (`cache.truncate(committed_roots.total_pages)` — I3) actively shrinks the
//     file on a clean rollback so they don't accumulate at all.
//
// In-memory mode: `TransactionManager::create_new` and `open_existing` are
// backend-agnostic — whether the underlying PageIo is backed by a file (with
// flock) or by a Vec<u8> (no flock, no durability) is invisible here. The
// in-memory entry points live in `lib.rs` and just hand this module a
// memory-backed PageIo. Every transactional invariant in this file (commit
// ordering, poison on fatal error, watermark rollback) applies equally to the
// in-memory backend.

use std::cell::{Cell, RefCell};
// I127 (ISSUES.md, 2026-06-21): FxHashMap (not std SipHash) for the per-op
// slot-accounting maps (the SlotPacker live-slot maps in packing.rs,
// Savepoint.live_slots below, and the open-time scan map in recovery.rs).
// Keys are trusted local u64 page ids — no DoS surface — so SipHash is pure cost,
// exactly the I77 rationale; that pass converted the page cache/LRU but missed
// these. FxHashMap is a drop-in std HashMap with a faster non-DoS-resistant hasher.
use rustc_hash::{FxHashMap, FxHashSet};

use crate::data_page::DataPage;
use crate::error::{ChiselError, Result};
use crate::freemap_tree::FreeMapTree;
use crate::handle_table::{HandleEntry, HandleFlags, HandleTable};
use crate::membership_index::{MembershipIndex, RadixU64};
use crate::overflow::Overflow;
use crate::page::{self, PAGE_ID_NONE, PAGE_SIZE};
use crate::page_cache::PageCache;
use crate::stats::ChiselCounters;
use crate::superblock::{
    NamedRoot, Superblock, MAX_SUPERBLOCKS, NAMED_ROOT_COUNT, NAMED_ROOT_NAME_LEN,
};

// Largest value stored inline in a data-page slot. Larger values are written to an
// overflow chain and referenced by a single HandleEntry with HandleFlags::Overflow.
//
// I117 (ISSUES.md, 2026-06-21): COMPUTED from the page constants (was a
// hand-maintained `8162` literal with only a prose "keep in sync" note). A data
// page's usable body is `CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE`, minus one
// `SLOT_ENTRY_SIZE` slot-directory entry. Deriving it makes drift impossible —
// which is what makes the `.expect("value fits in empty page")` in
// `insert_into_data_page` safe by construction: a value `<= MAX_INLINE_VALUE`
// always fits an empty page, so that expect is structurally unreachable.
const MAX_INLINE_VALUE: usize =
    page::CHECKSUM_OFFSET - page::DATA_PAGE_HEADER_SIZE - crate::data_page::SLOT_ENTRY_SIZE;

/// Snapshot of the mutable "pointers" that define a consistent database state.
/// A commit succeeds by writing a superblock that references exactly these roots;
/// a rollback succeeds by reverting current_roots back to committed_roots.
///
/// The `named_roots` array is part of this snapshot (ISSUES.md F2) so that
/// set_root_name / clear_root_name participate in the transactional commit
/// point for free — a rollback or `rollback_to` restores named roots at
/// the same time it restores the handle-table root, with no extra plumbing.
#[derive(Debug, Clone)]
struct Roots {
    handle_table_page: u64,
    // Root page of the freemap tree (PageType::FreeMap leaf at depth 0, or a
    // FreeMapInterior at depth > 0). PAGE_ID_NONE until the first free
    // materializes the tree (see persist_freemap). Paired with `freemap_depth`,
    // these two words ARE the committed freemap; cloning Roots at
    // begin/commit/rollback/savepoint carries them with no extra plumbing.
    freemap_page: u64,
    // Depth of the freemap tree rooted at `freemap_page`. Depth 0 = today's
    // single-leaf format (a lone FreeMap page reached directly), so existing
    // databases load unchanged. Grows logarithmically with database size.
    freemap_depth: u32,
    next_handle: u64,
    total_pages: u64,
    named_roots: [NamedRoot; NAMED_ROOT_COUNT],
    // Root page of the membership index (chunk-tags). PAGE_ID_NONE until the
    // first tagged chunk is written. Cloned automatically with the rest of
    // Roots at begin/commit/rollback/savepoint — no extra plumbing needed.
    membership_index_page: u64,
}

/// A nested rollback point within an active transaction.
///
/// Captures the roots and the `next_page_id` watermark at savepoint
/// creation time. `rollback_to(name)` restores the roots and calls
/// `cache.truncate(watermark)`, which drops every cache entry and
/// truncates the file back to the watermark — cleanly discarding every
/// page the transaction allocated after the savepoint (ISSUES.md I3).
///
/// `freed_pages` is still tracked per-savepoint so a future freemap
/// reclamation pass (R2) can restore freed-but-not-yet-reclaimed pages
/// if a savepoint is rolled back to. It is a distinct concern from the
/// cache-level rollback that the watermark handles.
///
/// `live_slots` and `insert_cursor` snapshot the R1 packing state
/// (live slot counts per data page + the current in-progress insert
/// cursor). `rollback_to` restores these so a savepoint rewind leaves
/// the packer in a consistent state. Cloning the HashMap is O(map
/// size) per savepoint but savepoints are rare in the target workloads
/// (drop_table / delete_many don't use them).
#[derive(Debug)]
struct Savepoint {
    name: String,
    roots: Roots,
    watermark: u64,
    freed_pages: Vec<u64>,
    live_slots: FxHashMap<u64, u32>,
    insert_cursor: Option<u64>,
}

/// The single writer for a Chisel database. Not thread-safe; file-level mutual
/// exclusion across processes is provided by flock() in PageIo. Holds both the
/// last durably-committed roots (for reads outside a txn and for rollback) and
/// the in-progress current_roots (only valid while active_txn is true).
pub struct TransactionManager {
    // Interior mutability (ISSUES.md F3): the page cache is mutated on read
    // (LRU bookkeeping, page loads, checksum validation), but from Chisel's
    // public API perspective a read() is semantically a read. Wrapping in
    // RefCell lets `read()` / `handles()` / `stats()` take `&self` so
    // callers don't need an external RefCell<Chisel> wrapper. RefCell (not
    // Mutex) because Chisel is deliberately single-threaded — see
    // lib.rs and ARCHITECTURE.md. Every access through this field uses
    // `borrow_mut()`; reborrowing for downstream `&mut PageCache` parameters
    // (e.g., handle_table methods) is done via `&mut *cache` on a single
    // RefMut held for the duration of the operation.
    pub(crate) cache: RefCell<PageCache>,
    // Roots that match the superblock currently on disk. Safe to read at any time.
    committed_roots: Roots,
    // Roots under construction. Equals committed_roots when no txn is active;
    // diverges from it as mutations create new COW pages during a txn.
    current_roots: Roots,
    handle_table: HandleTable,
    /// In-memory state for the membership index (chunk tags). Holds only the
    /// outer tree's depth; the root lives in current/committed `Roots`.
    membership_index: MembershipIndex,
    // Monotonically increasing. Written into each new superblock; the higher value
    // wins on recovery. Also used to pick the inactive slot on commit via
    // `txn_counter % superblock_count`.
    pub(crate) txn_counter: u64,
    // Number of superblock slots occupying pages 0..superblock_count
    // (ISSUES.md R4). Set at open time from the winning superblock's
    // own `superblock_count` field; cached here so commit doesn't have
    // to re-fetch it. Must equal every slot's self-reported value in a
    // healthy database; divergence would indicate mid-flight reconfig
    // or corruption.
    pub(crate) superblock_count: u32,
    active_txn: bool,
    savepoints: Vec<Savepoint>,
    // Pages whose contents are no longer reachable from the new roots.
    // Merged into `current_freemap` at commit time so subsequent
    // transactions can reuse the space (ISSUES.md I9 / I10 / I11 / R2).
    // During the transaction itself these pages are NOT reusable —
    // their old contents must stay readable via `committed_roots` until
    // commit promotes the new roots.
    txn_freed_pages: Vec<u64>,
    // The structural-page recycle cluster and freemap commit/alloc/persist
    // machinery — the crash-durability backbone — extracted into its own owned
    // unit. No code outside `freemap.rs` touches the inner fields; the rest of
    // `TransactionManager` reaches the freemap through `FreemapRecycle`'s narrow
    // surface (the transient-tree trio, the commit-path persist/reclaim wrappers
    // on this type, and the begin/commit/rollback lifecycle hooks). See the
    // `FreemapRecycle` doc in `freemap.rs` for the recycle/rotation model.
    freemap: freemap::FreemapRecycle,
    // R1 slot-packing state (live-slot counts per data page + the insert
    // cursor), extracted into its own owned unit. No code outside `packing.rs`
    // touches the inner fields; `TransactionManager` reaches packing through
    // the narrow `SlotPacker` interface (the `insert_into_data_page` /
    // `release_data_slot` wrappers plus the lifecycle/savepoint/stats hooks).
    // See the `packing` module header for the live-slot and cursor models.
    packer: packing::SlotPacker,
    // Poison flag (ISSUES.md I1). Once set, every public entry point returns
    // ChiselError::Poisoned until the manager is dropped. Set by commit() on
    // any error in the commit protocol, and by `poison_on_fatal()` for any
    // fatal error observed during other operations. Modeled on
    // std::sync::Mutex poisoning: the only legal recovery is to drop the
    // Chisel handle and reopen; the shadow-paging crash-recovery logic then
    // returns the database to the last durable state. Linux fsync semantics
    // (fsyncgate, 2018) make this the ONLY safe response to a mid-commit
    // I/O error — a failed fsync cannot be retried without first closing
    // and reopening the file.
    //
    // Stored as `Cell<bool>` (not plain `bool`) so it can be set from the
    // `&self`-taking read paths introduced by F3. Cell rather than
    // AtomicBool because TransactionManager is !Sync by design (see
    // lib.rs); there is no cross-thread access to synchronize against.
    poisoned: Cell<bool>,
    /// Per-session page cipher for an encrypted database. `None` for plaintext.
    /// Holds the unwrapped DEK (zeroizing) for the life of the manager; the
    /// PageCache gets its own clone at open time (recovery.rs) for per-page
    /// seal/open. The manager's copy is also threaded through CommitCtx for the
    /// superblock body seal on every commit. The DEK inside PageCipher is
    /// zeroizing and is cleared on drop.
    pub(crate) cipher: Option<crate::crypto::PageCipher>,
    /// The crypto-header (algorithm id + key-slot table) for an encrypted database.
    /// Written verbatim into every committed superblock. `None` for plaintext DBs.
    /// The key-slot contents never change after create or open: the slots hold the
    /// DEK wrapped under KEKs from each user key and are opaque to the commit path.
    pub(crate) crypto_header: Option<crate::superblock::CryptoHeader>,

    // Test-only fault injection consolidated off the production type (review
    // 2026-06-22 SMELL #4): the four BUG#2 atomic-staging arming flags live in
    // their own `#[cfg(test)]` struct so they carry no production scaffolding
    // fields here. See `fault.rs` for each Cell's precise divergence window.
    #[cfg(test)]
    fault: fault::FaultInjector,
}

mod commit;
mod config;
#[cfg(test)]
mod create_tests;
#[cfg(test)]
mod fault;
mod freemap;
mod keys;
mod lifecycle;
mod mutate;
mod named_roots;
mod packing;
mod read;
mod recovery;
mod savepoints;
mod staging;
mod stats;
#[cfg(test)]
mod tests;
