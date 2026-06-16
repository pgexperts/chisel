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
//     `allocate_data_page` prefers reuse from `current_freemap` but also calls
//     through to `new_page()` when the freemap is empty. Either way, any pages
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
use std::collections::HashMap;

use crate::data_page::DataPage;
use crate::error::{ChiselError, Result};
use crate::freemap::FreeMap;
use crate::handle_table::{HandleEntry, HandleFlags, HandleTable};
use crate::membership_index::{MembershipIndex, RadixU64, TagDropProgress};
use crate::overflow::Overflow;
use crate::page::{self, PAGE_ID_NONE, PAGE_SIZE};
use crate::page_cache::PageCache;
use crate::stats::ChiselCounters;
use crate::superblock::{
    NamedRoot, Superblock, MAX_SUPERBLOCKS, NAMED_ROOT_COUNT, NAMED_ROOT_NAME_LEN,
};

// Largest value stored inline in a data-page slot. Larger values are written to an
// overflow chain and referenced by a single HandleEntry with HandleFlags::Overflow.
// Derived from PAGE_SIZE minus DataPage header/slot overhead; keep in sync with data_page.rs.
const MAX_INLINE_VALUE: usize = 8162;

/// Freemap-aware page allocator shared by data-page allocation and the
/// handle-table / membership-index COW paths.
///
/// When `reuse_enabled`, it first tries to reuse a page freed by a *prior*
/// committed transaction (a free bit in `current_freemap`), falling back to
/// extending the file via `PageCache::new_page`. `reuse_enabled` is false while
/// savepoints are active (R2: savepoint scopes disable freemap reuse to keep
/// `rollback_to` semantics simple) — matching the historical `allocate_data_page`
/// behavior, which this now also routes through.
///
/// Pages freed during the CURRENT transaction live in `txn_freed_pages` and are
/// NOT in `current_freemap` until commit, so `allocate_first` can never hand
/// back a page still referenced by the live tree (the I18 invariant). Routing
/// handle-table and membership COW allocation through here — rather than the
/// monotonic `new_page` — is what lets those structures reach a bounded
/// steady-state page count instead of leaking one page per mutation.
fn cow_alloc(
    cache: &mut PageCache,
    freemap: &mut [u8; PAGE_SIZE],
    reuse_enabled: bool,
) -> Result<u64> {
    if reuse_enabled {
        if let Some(id) = FreeMap::allocate_first(freemap) {
            cache.claim_page(id)?;
            return Ok(id);
        }
    }
    cache.new_page()
}

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
    freemap_page: u64,
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
    live_slots: HashMap<u64, u32>,
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
    cache: RefCell<PageCache>,
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
    txn_counter: u64,
    // Number of superblock slots occupying pages 0..superblock_count
    // (ISSUES.md R4). Set at open time from the winning superblock's
    // own `superblock_count` field; cached here so commit doesn't have
    // to re-fetch it. Must equal every slot's self-reported value in a
    // healthy database; divergence would indicate mid-flight reconfig
    // or corruption.
    superblock_count: u32,
    active_txn: bool,
    savepoints: Vec<Savepoint>,
    // Pages whose contents are no longer reachable from the new roots.
    // Merged into `current_freemap` at commit time so subsequent
    // transactions can reuse the space (ISSUES.md I9 / I10 / I11 / R2).
    // During the transaction itself these pages are NOT reusable —
    // their old contents must stay readable via `committed_roots` until
    // commit promotes the new roots.
    txn_freed_pages: Vec<u64>,
    // Freemap state (ISSUES.md R2). Single-page bitmap (capacity ~65K
    // pages ≈ 512 MB in v1). `committed_freemap` mirrors the on-disk
    // freemap page pointed to by `committed_roots.freemap_page`;
    // `current_freemap` is a working copy cloned at begin() time. Page
    // allocations during a transaction pull from `current_freemap` (so
    // they don't touch pages that are only logically free after commit),
    // and `txn_freed_pages` is merged into `current_freemap` at commit
    // time before the new freemap page is written. On rollback,
    // `current_freemap` is reset from `committed_freemap`.
    committed_freemap: Box<[u8; PAGE_SIZE]>,
    current_freemap: Box<[u8; PAGE_SIZE]>,
    // Live-slot count per data page (ISSUES.md R1). Tracks how many
    // handle-table entries currently point at each data page — this
    // is the information needed to decide when a page is fully empty
    // and can be returned to the freemap. `committed_live_slots` is
    // the durable state (rebuilt at open time by scanning the handle
    // table); `current_live_slots` is the in-transaction working copy.
    //
    // Kept in memory rather than on disk because updating a slot count
    // on a committed data page would require COW, and COWing a data
    // page would require rewriting every handle_table entry that
    // points into it — an O(live-slots-in-page) amplification per
    // delete that shadow paging does not handle well.
    committed_live_slots: HashMap<u64, u32>,
    current_live_slots: HashMap<u64, u32>,
    // Per-transaction "insert cursor" (ISSUES.md R1). The id of a data
    // page allocated earlier in the current transaction that still has
    // free space. New values pack into it until it fills, at which
    // point a new page is allocated and becomes the new cursor.
    //
    // `None` at the start of each transaction. Only set for pages
    // allocated during THIS transaction (so they're dirty in the cache
    // and safe to modify). A committed data page is never the cursor —
    // that would require COW, which is prohibitively expensive for data
    // pages (every handle_table entry pointing at the page would need
    // to be rewritten). Disabled entirely when savepoints are active,
    // same as freemap reuse (R2): the savepoint-snapshot cost becomes
    // manageable when only one code path interacts with packing state.
    insert_cursor: Option<u64>,
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

    // Test-only fault injection (BUG#2 atomic-staging regression tests). When
    // armed, the NEXT reverse-map (membership-index) update in
    // `allocate_inner`/`delete_inner` returns a non-fatal `CacheFull` BEFORE
    // touching the index, simulating a mid-operation resource-exhaustion strike
    // at exactly the forward/reverse divergence window. Gated `#[cfg(test)]` so
    // it carries no production code (mirrors `force_poison_for_test`). Cell so
    // the in-crate tests can arm it through `&self`.
    #[cfg(test)]
    fail_next_membership_op: Cell<bool>,

    // Companion to `fail_next_membership_op` for the FORWARD step: when armed,
    // the next `allocate_inner` handle-table insert returns a non-fatal
    // `CacheFull`, so tests can exercise the prepare-abort/unwind path of the
    // step that carries the eager depth bump (HandleTable::grow). `#[cfg(test)]`.
    #[cfg(test)]
    fail_next_handle_table_op: Cell<bool>,

    // For `update_inner`: when armed, the next update returns a non-fatal
    // `CacheFull` at the NEW-value-write step. With the fix this is the first
    // fallible step (a clean no-op); pre-fix it lands AFTER the old location was
    // already freed, so the regression test can prove the old value is not
    // prematurely freed before the new entry installs. `#[cfg(test)]`.
    #[cfg(test)]
    fail_next_update_value_write: Cell<bool>,
}

impl TransactionManager {
    /// Create a new database with `superblock_count` superblock slots.
    ///
    /// All N slots are initialized as VALID superblocks at staggered
    /// counters 0..N-1 (slot i gets counter N-1-i). This matters for
    /// crash safety (ISSUES.md I2 + R4):
    ///
    /// * The I2 fix for N=2: if the first user commit (which writes
    ///   slot 0 at counter N) is torn, slot 1 at counter N-2 still
    ///   holds a valid "empty database" superblock so the file stays
    ///   openable.
    /// * The R4 generalization for N>=3: multiple staggered fallback
    ///   slots survive CONSECUTIVE torn writes. For N=3, slots 1 and 2
    ///   both hold valid empty states at lower counters after slot 0
    ///   is written; a torn retry of the same commit still has slot 2
    ///   to fall back to.
    ///
    /// An fsync is issued before returning so the whole bank of slots
    /// is durable before any user data is written. Slot counters with
    /// the value 0 are SAFE even though zero bits are "the natural
    /// value of an uninitialized disk region" because `select()`
    /// filters on XXH3 checksum validity BEFORE comparing counters —
    /// a legitimate counter-0 slot has a valid checksum; a zeroed
    /// region doesn't.
    pub fn create_new(mut cache: PageCache, superblock_count: u32) -> Result<TransactionManager> {
        // Caller is expected to have validated bounds via Options in
        // lib.rs, but defend against direct-call misuse too.
        assert!(
            (2..=MAX_SUPERBLOCKS).contains(&superblock_count),
            "superblock_count {superblock_count} out of supported range 2..=16"
        );

        // Write N staggered slots. Slot 0 gets the highest counter
        // (superblock_count - 1), slot N-1 gets 0. First user commit
        // bumps to N, which modulo N is 0, so slot 0 is the first to
        // be overwritten — the behavior the I2 fix established for
        // N=2 generalizes cleanly to larger N.
        //
        // Invariant after this loop: every slot is a valid superblock
        // referencing the same (empty) roots, at counters 0..N-1.
        // `select()` at open time will pick slot 0 (highest counter).
        // After the first user commit, slot 0 holds the newest data
        // and the rest remain as "rollback fallbacks".
        let mut sb = Superblock::new_empty(superblock_count);
        for i in 0..superblock_count {
            sb.txn_counter = (superblock_count - 1 - i) as u64;
            let buf = sb.serialize();
            cache.io_mut().write_page(i as u64, &buf)?;
        }
        cache.io_mut().fsync()?;
        cache.set_next_page_id(superblock_count as u64);

        let roots = Roots {
            handle_table_page: PAGE_ID_NONE,
            freemap_page: PAGE_ID_NONE,
            // Start at 1: handle 0 is reserved as the "no handle" sentinel and is
            // never minted (see Superblock::new_empty, which seeds the persisted
            // superblock the same way). Must match new_empty so the in-memory
            // roots and the on-disk superblock of a fresh store agree.
            next_handle: 1,
            total_pages: superblock_count as u64,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            membership_index_page: PAGE_ID_NONE,
        };

        // A brand-new database has no freemap page on disk yet. Both
        // in-memory freemaps start as "nothing free" (FreeMap::init_page
        // sets the type tag and zero-initializes the bitmap); the
        // freemap page will be materialized on the first commit that
        // has something to persist.
        let mut committed_freemap = Box::new([0u8; PAGE_SIZE]);
        FreeMap::init_page(&mut committed_freemap);
        let current_freemap = committed_freemap.clone();

        Ok(TransactionManager {
            cache: RefCell::new(cache),
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: HandleTable::new(),
            membership_index: MembershipIndex::new(),
            // Slot 0 was written last in the loop above, at counter
            // (superblock_count - 1 - 0) = superblock_count - 1. That's
            // the highest counter and therefore the winner on select().
            txn_counter: (superblock_count - 1) as u64,
            superblock_count,
            active_txn: false,
            savepoints: Vec::new(),
            txn_freed_pages: Vec::new(),
            committed_freemap,
            current_freemap,
            // A fresh database has no data pages and no live slots yet.
            committed_live_slots: HashMap::new(),
            current_live_slots: HashMap::new(),
            insert_cursor: None,
            poisoned: Cell::new(false),
            #[cfg(test)]
            fail_next_membership_op: Cell::new(false),
            #[cfg(test)]
            fail_next_handle_table_op: Cell::new(false),
            #[cfg(test)]
            fail_next_update_value_write: Cell::new(false),
        })
    }

    /// Open an existing database from file.
    ///
    /// This is the crash recovery path. All N superblock slots (where
    /// N is discovered from disk — see the probe below) are read,
    /// `Superblock::select()` picks the one with the highest
    /// txn_counter and a valid XXH3 checksum, and a torn write to the
    /// most-recently-targeted slot silently falls back to the next
    /// best survivor. No log replay required.
    ///
    /// R4 slot discovery:
    ///
    /// The number of superblock slots is NOT a compile-time constant.
    /// A database created with `superblock_count=4` has 4 slots at
    /// pages 0..3; a default database has 2 at pages 0..1. To find N
    /// without any external hint we:
    ///
    ///   1. Read the first MAX_SUPERBLOCKS pages of the file (bounded
    ///      by EOF — a fresh DB has exactly N pages and no more). We
    ///      deliberately do NOT short-circuit on "this page doesn't
    ///      look like a superblock": a torn write that hit the magic
    ///      bytes of an otherwise-valid slot would look like garbage,
    ///      and short-circuiting would skip past the legitimate
    ///      successor slots. Reading a few extra pages is cheap.
    ///   2. Pass all candidates to `Superblock::select`, which uses
    ///      `deserialize` to filter on XXH3 checksum + MAGIC bytes.
    ///      Data pages that happen to sit at positions < MAX_SUPERBLOCKS
    ///      (e.g., in a database where N=2 and there's a data page at
    ///      page 2) fail the magic check and are harmlessly ignored.
    ///   3. The winner's `superblock_count` field tells us N, which we
    ///      cache on the TransactionManager for commit-time slot
    ///      selection.
    ///
    /// If no valid superblock is found in the first MAX_SUPERBLOCKS
    /// pages, we return `CorruptSuperblock`. This bounds the probe
    /// cost in the pathological case where every candidate is torn.
    pub fn open_existing(mut cache: PageCache) -> Result<TransactionManager> {
        // Step 1: read up to MAX_SUPERBLOCKS pages as candidates.
        let mut candidates: Vec<[u8; PAGE_SIZE]> = Vec::new();
        for i in 0..MAX_SUPERBLOCKS as u64 {
            // If the file is shorter than MAX_SUPERBLOCKS (fresh DB
            // with small N), read_page returns InvalidPageId (I16).
            // Stop probing at EOF.
            match cache.io_mut().read_page(i) {
                Ok(buf) => candidates.push(buf),
                Err(ChiselError::InvalidPageId { .. }) => break,
                Err(e) => return Err(e),
            }
        }

        // Step 2 + 3: pick the winner via select(). select() uses
        // deserialize, which validates checksum and magic — data
        // pages in the candidate list (if any) are filtered out.
        let sb = Superblock::select(&candidates).ok_or(ChiselError::CorruptSuperblock)?;

        // Format-version gate (see ISSUES.md I15 for the original check,
        // I29 for the major/minor split). Compare MAJOR only: the packed
        // u32 layout (upper 16 = major, lower 16 = minor) lets same-major
        // files open regardless of minor drift, which is what makes the
        // README's "sacred within a major version" promise enforceable.
        // Minor-newer files are accepted here but may be unsafe to WRITE
        // — I29 phase 2 will add a "refuse writes if file minor > my
        // minor" arm when the first 1.1 release makes this observable.
        //
        // We validate AFTER select() rather than inside deserialize()
        // because the winning superblock's version is what determines
        // compatibility — silently falling back to an older-version
        // superblock would hand the user a stale snapshot with
        // mysteriously missing data.
        if page::format_major(sb.format_version) != page::FORMAT_MAJOR_VERSION {
            return Err(ChiselError::UnsupportedFormatVersion {
                found: sb.format_version,
                expected: page::FORMAT_VERSION,
            });
        }

        let page_count = cache.io_mut().page_count()?;
        if page_count < sb.total_pages {
            return Err(ChiselError::FileSizeMismatch {
                expected: sb.total_pages * PAGE_SIZE as u64,
                actual: page_count * PAGE_SIZE as u64,
            });
        }
        // Reset next_page_id from the authoritative superblock, NOT from
        // the on-disk file length (ISSUES.md I4). This matters because a
        // crash mid-rollback could leave the file extended past the
        // committed superblock's `total_pages` — those trailing pages are
        // unreferenced garbage, and letting `new_page()` allocate above
        // them would mean the next commit's new pages live at the very
        // end of the file while the garbage sits in the middle. Reseeding
        // from `sb.total_pages` causes the next allocations to overwrite
        // the garbage, which is exactly what we want. The rollback-path
        // truncation added by I3 also prevents this situation from
        // arising in the first place, but the reseed is a defense-in-
        // depth guarantee against any crash that happened before I3 or
        // against external truncation/corruption tools.
        cache.set_next_page_id(sb.total_pages);

        let roots = Roots {
            handle_table_page: sb.root_handle_table_page,
            freemap_page: sb.root_freemap_page,
            next_handle: sb.next_handle,
            total_pages: sb.total_pages,
            named_roots: sb.named_roots,
            // Normalize old files (pre-chunk-tags bytes were zeroed) so the
            // rest of the engine has a single "empty" sentinel: PAGE_ID_NONE.
            membership_index_page: if sb.root_membership_index_page == 0 {
                PAGE_ID_NONE
            } else {
                sb.root_membership_index_page
            },
        };

        // The HandleTable struct keeps only its depth in memory; physical pages
        // live in the cache, reached via the root page_id in the superblock.
        // Reconstruct depth by walking the left spine (see HandleTable::recover_depth).
        let mut ht = HandleTable::new();
        ht.set_depth(HandleTable::recover_depth(
            &mut cache,
            sb.root_handle_table_page,
        )?);

        // Mirror the handle-table depth recovery for the membership index: its
        // outer RadixU64 keeps only depth in memory, rebuilt by walking the
        // persisted spine from the root recorded in the superblock. Uses the
        // normalized roots.membership_index_page (PAGE_ID_NONE for legacy files).
        let mut membership_index = MembershipIndex::new();
        if roots.membership_index_page != PAGE_ID_NONE {
            let depth = RadixU64::recover_depth(&mut cache, roots.membership_index_page)?;
            membership_index.set_outer_depth(depth);
        }

        // Load the freemap, if this database has ever persisted one.
        // A DB created under v1 (pre-R2) will have root_freemap_page ==
        // PAGE_ID_NONE because the freemap was never wired into the
        // allocator — in that case we start with an empty freemap just
        // like a fresh database. Loading via cache.get validates the
        // XXH3 checksum so a torn or corrupt freemap surfaces as a
        // fatal error rather than silent reuse of the wrong pages.
        let mut committed_freemap = Box::new([0u8; PAGE_SIZE]);
        FreeMap::init_page(&mut committed_freemap);
        if sb.root_freemap_page != PAGE_ID_NONE {
            let loaded = cache.get(sb.root_freemap_page)?;
            *committed_freemap = *loaded;
        }
        let current_freemap = committed_freemap.clone();

        // Rebuild the live-slot count map (ISSUES.md R1) by scanning the
        // handle table. Every Live entry contributes one live slot to
        // its target data page; Overflow and Deleted entries don't
        // count. Cost is O(live handles), paid once at open. In-memory
        // only — the alternative (storing the count on the data page
        // itself) would require COWing pages on every delete, which
        // shadow paging cannot afford.
        let mut committed_live_slots: HashMap<u64, u32> = HashMap::new();
        if sb.root_handle_table_page != PAGE_ID_NONE {
            let entries = ht.iter_live(&mut cache, sb.root_handle_table_page)?;
            for (_, entry) in entries {
                if entry.flags == HandleFlags::Live {
                    *committed_live_slots.entry(entry.page_id).or_insert(0) += 1;
                }
            }
        }
        let current_live_slots = committed_live_slots.clone();

        Ok(TransactionManager {
            cache: RefCell::new(cache),
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: ht,
            membership_index,
            txn_counter: sb.txn_counter,
            // R4: discovered from the winning superblock's own
            // `superblock_count` field. Cached so commit doesn't have
            // to re-look it up for slot selection.
            superblock_count: sb.superblock_count,
            active_txn: false,
            savepoints: Vec::new(),
            txn_freed_pages: Vec::new(),
            committed_freemap,
            current_freemap,
            committed_live_slots,
            current_live_slots,
            insert_cursor: None,
            poisoned: Cell::new(false),
            #[cfg(test)]
            fail_next_membership_op: Cell::new(false),
            #[cfg(test)]
            fail_next_handle_table_op: Cell::new(false),
            #[cfg(test)]
            fail_next_update_value_write: Cell::new(false),
        })
    }

    // --- Poison machinery (ISSUES.md I1) ---
    //
    // Every public entry point below follows the same wrapper pattern:
    //
    //     pub fn foo(&mut self, ...) -> Result<T> {
    //         self.check_alive()?;          // fast path: refuse if already poisoned
    //         let result = self.foo_inner(...);
    //         self.poison_on_fatal(result)  // poison iff the inner call returned a fatal error
    //     }
    //
    // commit() is the one exception: ANY error from the commit protocol
    // poisons (not just fatal variants), because partial-commit state is
    // fragile enough that we do not trust the in-memory view after a
    // half-finished commit even if the variant would otherwise be
    // operational. See commit() for the full reasoning.

    /// Returns Err(Poisoned) if the manager has previously seen a fatal
    /// error. Called at the top of every public entry point. Cheap.
    ///
    /// Takes `&self` because the poison flag lives in a `Cell<bool>`
    /// (F3: `read()` takes `&self`, and read paths must also check/set
    /// the flag).
    fn check_alive(&self) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        Ok(())
    }

    /// Inspect a Result and set the poison flag if it contains a fatal
    /// error. Returns the Result unchanged so the caller can `?` or return
    /// it. Never fires on an Ok or on an operational error.
    ///
    /// Takes `&self` (not `&mut self`) because the flag is a `Cell` —
    /// essential for the `&self`-taking read paths under F3.
    fn poison_on_fatal<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(ref e) = result {
            if e.is_fatal() {
                self.poisoned.set(true);
            }
        }
        result
    }

    /// Force the manager into the poisoned state. Test-only hook used by
    /// the I1 regression test to avoid needing a real I/O failure injection.
    #[cfg(test)]
    pub fn force_poison_for_test(&self) {
        self.poisoned.set(true);
    }

    /// True if this manager has been poisoned by a previous fatal error.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.get()
    }

    // --- Watermark-based rollback (ISSUES.md I3 + I7) ---
    //
    // `PageCache::new_page()` hands out monotonically increasing ids, so
    // every page allocated during a transaction has an id strictly greater
    // than or equal to the `next_page_id` watermark captured at begin() /
    // savepoint() time. `PageCache::truncate(watermark)` drops every cache
    // entry AND truncates the file to `watermark` pages, cleanly discarding
    // every transaction-allocated page without a per-page tracking list.
    //
    // This supersedes an earlier per-page `txn_dirty_pages` vector — the
    // list was a weaker mechanism (I7 showed it missed intermediate COW
    // pages and overflow allocations) and a redundant one once the
    // watermark invariant was in place. See memory
    // project_chisel_i3_watermark_rollback for the reasoning.
    //
    // Savepoints capture `cache.next_page_id()` at creation time (see the
    // `watermark` field on Savepoint) so `rollback_to(name)` can truncate
    // to that specific watermark — discarding every page allocated after
    // the savepoint while preserving those allocated before it.

    /// Snapshot the current `next_page_id` watermark. Cheap — one read
    /// through the RefCell.
    fn cache_watermark(&self) -> u64 {
        self.cache.borrow().next_page_id()
    }

    // --- Freemap-aware page allocation (ISSUES.md R2) ---
    //
    // `allocate_data_page` is the single entry point for allocating a
    // fresh data page during a transaction. It first tries to reuse an
    // id from `current_freemap` and falls back to extending the file.
    //
    // Two important scoping rules:
    //
    //   1. Reuse is disabled when any savepoint is active. A rollback_to
    //      would need to per-savepoint distinguish dirty entries at
    //      reused ids from dirty entries at preserved ids, which would
    //      require an 8 KB freemap snapshot per savepoint and a
    //      per-savepoint dirty-page list. For v1, the simpler rule is
    //      "reuse only outside savepoint scopes". Workloads that want
    //      reuse (e.g. F1 delete_subtree / drop_table) typically don't
    //      use savepoints at all.
    //
    //   2. Pages freed during the CURRENT transaction (in
    //      `txn_freed_pages`) are NOT reusable within the same
    //      transaction — their old contents must stay readable via
    //      `committed_roots` until commit swaps the superblock. This
    //      is enforced by only merging `txn_freed_pages` into
    //      `current_freemap` during commit, after the new roots have
    //      been computed.
    //
    // Handle-table and membership-index COW pages now share this same
    // freemap-aware allocator via `cow_alloc` (each `insert`/`delete` takes an
    // `alloc` closure that calls it), so they reuse freed pages before
    // extending — that is what bounds their steady-state page count. Overflow
    // pages still call `cache.new_page()` directly and always extend, but their
    // frees feed the freemap, so a later data- or handle-table allocation can
    // reclaim them. Routing overflow through the freemap too would need the
    // same allocator-closure plumbing at the overflow module boundary; left as
    // a v1 simplification since overflow churn is far smaller than HT churn.
    fn allocate_data_page(&mut self) -> Result<u64> {
        let reuse = self.savepoints.is_empty();
        let mut cache = self.cache.borrow_mut();
        cow_alloc(&mut cache, &mut self.current_freemap, reuse)
    }

    /// COW `handle`'s handle-table entry to `entry`, installing the new root
    /// and queuing the superseded spine pages for freemap reclamation at
    /// commit. Shared by `allocate`, `update`, and `set_client_byte`.
    ///
    /// The superseded pages are appended to `txn_freed_pages` ONLY after the
    /// new root is installed in `current_roots`: if the COW fails partway
    /// (e.g. `CacheFull`), the local `freed` list is dropped and the still-
    /// current old tree keeps all its pages — never freeing a live page.
    fn ht_insert(&mut self, handle: u64, entry: &HandleEntry) -> Result<()> {
        let mut freed: Vec<u64> = Vec::new();
        let new_root = {
            let mut cache = self.cache.borrow_mut();
            let reuse = self.savepoints.is_empty();
            let fm = &mut *self.current_freemap;
            let mut alloc = |c: &mut PageCache| cow_alloc(c, fm, reuse);
            self.handle_table.insert(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                entry,
                &mut alloc,
                &mut freed,
            )?
        };
        self.current_roots.handle_table_page = new_root;
        self.txn_freed_pages.append(&mut freed);
        Ok(())
    }

    // Persist the freemap at commit time (ISSUES.md R2 / I11 / I18).
    //
    // Called once at the very start of `commit_inner`, BEFORE cache.flush().
    //
    // STEP ORDER IS LOAD-BEARING (ISSUES.md I18). The at-risk set during
    // commit is the union of `committed_roots.freemap_page` and every id
    // in `txn_freed_pages` — both are still referenced by the currently-
    // committed on-disk superblock. If any of those ids were visible to
    // `allocate_data_page` when it picks the page for the new freemap
    // snapshot, the subsequent `cache.flush()` would overwrite a page
    // whose bytes the last-durable superblock still depends on. A crash
    // in the window between that flush and the superblock fsync would
    // then leave recovery pointing at bytes that no longer match what
    // was committed — shadow-paging invariant broken.
    //
    // We close that window by ALLOCATING FIRST and MERGING LAST:
    //   1. Early-exit if nothing in the freemap has changed.
    //   2. Allocate a page for the new freemap snapshot. At this point
    //      `current_freemap` still excludes both `old_freemap_page` and
    //      `txn_freed_pages`, so `FreeMap::allocate_first` can only
    //      return (a) a page that was already free in `committed_freemap`
    //      (safe: not referenced by the committed superblock) or
    //      (b) a freshly-extended page beyond `total_pages` (also safe).
    //   3. NOW merge `txn_freed_pages` and `old_freemap_page` into
    //      `current_freemap`. These frees land on disk as part of the
    //      new freemap snapshot (so future transactions can reclaim
    //      them) but they are never visible to THIS commit's
    //      allocator — the window is already closed.
    //   4. Serialize the resulting freemap into the allocated page and
    //      point `current_roots.freemap_page` at it.
    fn persist_freemap(&mut self) -> Result<()> {
        // Step 1: early-exit if this transaction produced neither a
        // free nor an allocation-from-freemap. An all-extend
        // transaction leaves `current_freemap == committed_freemap`
        // and `txn_freed_pages` empty; the committed freemap snapshot
        // is still valid as-is.
        if self.txn_freed_pages.is_empty() && self.current_freemap == self.committed_freemap {
            return Ok(());
        }

        // Step 2: allocate the new freemap page BEFORE merging any of
        // the at-risk ids into `current_freemap`. See the I18 note in
        // this method's header for why ordering matters.
        let new_freemap_page = self.allocate_data_page()?;

        // Step 3: merge the transaction's frees and the old freemap
        // page id. After this, `current_freemap` reflects the full
        // post-commit view that goes onto disk, but the allocation
        // above has already chosen a safe id.
        for &id in &self.txn_freed_pages {
            FreeMap::mark_free(&mut self.current_freemap, id);
        }
        let old_freemap_page = self.committed_roots.freemap_page;
        if old_freemap_page != PAGE_ID_NONE {
            FreeMap::mark_free(&mut self.current_freemap, old_freemap_page);
        }

        // Step 4: serialize current_freemap into the new page and
        // update the roots so the new superblock picks it up. The
        // in-memory buffer already carries the FreeMap page-type
        // tag (byte 0 = 0x04) and the bitmap body; a direct copy is
        // the correct on-disk format.
        {
            let mut cache = self.cache.borrow_mut();
            let buf = cache.get_mut(new_freemap_page)?;
            *buf = *self.current_freemap;
            page::stamp_checksum(buf);
        }
        self.current_roots.freemap_page = new_freemap_page;
        Ok(())
    }

    /// Begin a new transaction.
    ///
    /// Single-writer: returns TransactionAlreadyActive if one is already in
    /// flight. current_roots is reseeded from committed_roots so that any prior
    /// (aborted) in-progress state is discarded. The dirty/freed bookkeeping is
    /// cleared — this is the only place (besides commit/rollback) those vectors
    /// are zeroed, so callers must not rely on them surviving a begin().
    pub fn begin(&mut self) -> Result<()> {
        self.check_alive()?;
        let result = self.begin_inner();
        self.poison_on_fatal(result)
    }

    fn begin_inner(&mut self) -> Result<()> {
        // Fail fast on read-only mounts so callers don't build up
        // transaction state only to hit a ReadOnlyMode at the first
        // write_page call during commit.
        if self.cache.borrow().io().is_read_only() {
            return Err(ChiselError::ReadOnlyMode);
        }
        if self.active_txn {
            return Err(ChiselError::TransactionAlreadyActive);
        }
        self.current_roots = self.committed_roots.clone();
        // Clone the freemap so allocations during this transaction mutate
        // a working copy; a rollback will snap it back to committed_freemap.
        self.current_freemap = self.committed_freemap.clone();
        // R1: clone the live-slot counts and reset the insert cursor.
        // The cursor is always None at begin — it only tracks pages
        // allocated during the current transaction.
        self.current_live_slots = self.committed_live_slots.clone();
        self.insert_cursor = None;
        self.active_txn = true;
        self.savepoints.clear();
        self.txn_freed_pages.clear();
        Ok(())
    }

    /// Durably commit the active transaction.
    ///
    /// Commit protocol — ORDERING IS LOAD-BEARING. Each numbered step encodes a
    /// specific crash-safety guarantee; reordering any of them can lose data or
    /// expose torn state on recovery.
    ///
    /// 1. Flush all dirty data pages to disk AND fsync.
    ///    PageCache::flush() writes every dirty page then calls fsync(). After
    ///    this returns, every page the new superblock will reference is durable
    ///    on the storage medium. WHY FIRST: the new superblock is the pointer
    ///    that makes these pages "live". If we wrote the superblock before the
    ///    data pages were durable and crashed, recovery would pick up a
    ///    superblock whose root_handle_table_page points into a page whose
    ///    contents were never persisted — corruption with a valid checksum on
    ///    the superblock but garbage at the referenced page.
    ///
    /// 2. Compute the new superblock in memory.
    ///    Bump txn_counter first so (a) the new superblock outranks the old one
    ///    via Superblock::select()'s max_by_key, and (b) `txn_counter %
    ///    superblock_count` selects which slot to overwrite (step 3). For N=2
    ///    this is the original parity alternation; for N>=3 (R4) it is true
    ///    round-robin across all N slots. total_pages is queried from the file
    ///    AFTER flush() so any new_page() allocations are reflected.
    ///
    /// 3. Write the new superblock to the INACTIVE slot.
    ///    The target is `txn_counter % superblock_count`, which always
    ///    points at the stalest slot. The N-1 other slots (including the
    ///    previously-active one, at counter txn_counter-1) are untouched
    ///    and still hold valid superblocks at strictly smaller counters.
    ///    WHY: if we crash during this write, the target slot may be torn
    ///    (bad checksum) but every other slot still holds the last
    ///    committed state (or earlier ones). Recovery picks the highest
    ///    surviving valid counter and the transaction is simply lost —
    ///    never half-applied. Overwriting an active slot in place would
    ///    be catastrophic: a torn write there could destroy a valid
    ///    superblock. Higher N buys survival of CONSECUTIVE torn writes
    ///    to the same target slot on retry (see `create_new` docstring).
    ///
    /// 4. fsync the superblock write.
    ///    This is the LINEARIZATION POINT of the commit. Before this fsync the
    ///    transaction is not durable, even if write_page returned; the kernel
    ///    may still be holding the superblock page in its buffer cache. After
    ///    this fsync returns successfully, a crash-and-recover will observe the
    ///    new state. A SINGLE fsync (combining data pages and superblock) would
    ///    be unsafe because the OS is free to reorder writes within an fsync
    ///    boundary — the superblock could reach the disk before the data pages
    ///    it references, creating a window where a crash leaves a valid-looking
    ///    superblock pointing at non-durable data.
    ///
    /// 5. Update in-memory committed_roots and clear txn state.
    ///    Only after the superblock fsync succeeds do we promote current_roots
    ///    to committed_roots. If ANY step in the protocol fails the manager is
    ///    poisoned (see the I1 block below) — active_txn / committed_roots are
    ///    left untouched but no public API will accept further calls; the only
    ///    legal recovery is close + reopen, which picks the last-durable
    ///    superblock via `Superblock::select`. Retry-in-place is forbidden
    ///    because a half-committed state (dirty flags already cleared in the
    ///    cache, txn_counter possibly bumped, target slot possibly torn on
    ///    disk) cannot be safely continued, and Linux fsyncgate semantics make
    ///    re-calling fsync() after a failed fsync unsafe regardless.
    pub fn commit(&mut self) -> Result<()> {
        self.check_alive()?;
        // Special poison policy for commit: we refuse BOTH operational and
        // fatal errors that arise after the commit protocol has started.
        // The operational NoActiveTransaction case is checked BEFORE any
        // protocol state is touched, so it stays operational and does not
        // poison. But once cache.flush() has run, any subsequent error —
        // even an otherwise operational one — leaves the manager in a
        // partial-commit state (dirty flags cleared in the cache, counter
        // possibly bumped, superblock possibly torn on disk) that cannot be
        // safely continued. Under Linux fsyncgate semantics a failed fsync
        // cannot be retried at all, so we poison and force the caller to
        // reopen.
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn commit_inner(&mut self) -> Result<()> {
        // I27: flatten every still-active savepoint's `freed_pages`
        // back into `txn_freed_pages` before persist_freemap consumes
        // it. savepoint_inner moves `txn_freed_pages` INTO the
        // savepoint record (via std::mem::take), so any frees that
        // happened before a still-unreleased savepoint otherwise get
        // dropped on the floor when step 5 calls `savepoints.clear()`
        // — a permanent freemap leak for the "commit with savepoint
        // active" pattern. Mirrors `release_inner`'s merge but applied
        // across the full stack. We take the lists out of the
        // savepoints (rather than iterating by reference) so the
        // savepoints hold no stale `freed_pages` if we ever change
        // step 5 to drain instead of clear; current code is equivalent
        // either way.
        for sp in self.savepoints.iter_mut() {
            self.txn_freed_pages.append(&mut sp.freed_pages);
        }

        // I28: drain the page cache BEFORE persist_freemap runs. Without
        // this, `persist_freemap`'s own `allocate_data_page` can trip
        // `maybe_evict`'s spill-or-CacheFull decision (every existing entry
        // dirty, nothing evictable, and either spillway disabled or full)
        // and return `ChiselError::CacheFull` or `ChiselError::SpillwayFull`.
        // The CacheFull variant is operational-by-design (I19 docs: "caller
        // recovers by
        // committing or rolling back"), but commit's poison wrapper fires
        // on any error once the protocol has started — demoting an
        // operational signal to fatal for a caller who has no legal
        // action left (commit is precisely what failed). Pre-draining
        // clears every dirty pin so the ceiling is reachable via normal
        // eviction when persist_freemap itself allocates. Cost: one
        // extra fsync on every commit. That is consistent with the
        // project's explicit "durability over performance" posture —
        // the alternative reclassifies CacheFull as fatal inside commit,
        // which is both more surprising and harder to document cleanly.
        //
        // Ordering note: this flush is safe to do before persist_freemap.
        // The shadow-paging invariant requires "new-freemap-page durable
        // before superblock" (step 1's flush does that). The pre-drain
        // only affects user-dirty pages, which are already part of the
        // transaction's durable write set — just flushed earlier. The
        // subsequent step 1 flush handles the one new freemap page
        // persist_freemap adds.
        self.cache.borrow_mut().flush()?;

        // Step 0 (ISSUES.md R2 / I11): persist the freemap. This merges
        // `txn_freed_pages` into `current_freemap`, reclaims the old
        // freemap page id, allocates a new page for the updated freemap,
        // serializes it into that page's cache buffer, and updates
        // `current_roots.freemap_page`. Runs BEFORE the main flush so
        // the new freemap page is part of the same durable write set as
        // all other dirty data pages.
        self.persist_freemap()?;

        // Hold one RefMut for the remaining steps. Dropping and
        // re-borrowing between steps would be semantically identical
        // but noisier.
        let mut cache = self.cache.borrow_mut();

        // Step 1: Flush all dirty pages (PageCache::flush internally fsyncs).
        // After this, every page the new superblock will reference is on disk.
        cache.flush()?;

        // Step 2: Build the new superblock. Bumping txn_counter here both makes
        // it outrank the current superblock on recovery AND (via parity) picks
        // the target slot in step 3.
        self.txn_counter += 1;
        let total_pages = cache.file_page_count()?;
        let sb = Superblock {
            magic: page::MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: self.txn_counter,
            root_handle_table_page: self.current_roots.handle_table_page,
            root_freemap_page: self.current_roots.freemap_page,
            total_pages,
            next_handle: self.current_roots.next_handle,
            page_size: PAGE_SIZE as u32,
            named_roots: self.current_roots.named_roots,
            // R4: every slot records the current N so open-time
            // recovery can discover it from the winning slot without
            // external hints.
            superblock_count: self.superblock_count,
            root_membership_index_page: self.current_roots.membership_index_page,
        };
        let buf = sb.serialize();
        // Step 3: Write to the INACTIVE slot. For N superblock slots,
        // the slot is `txn_counter % N` — a round-robin that always
        // targets the stalest slot. With N=2 this is the parity
        // alternation from the original layout; with N>=3 it extends
        // to true round-robin. The currently-active slot (and every
        // other non-target slot) is never touched, so a torn write
        // here can only damage the new superblock, never the N-1
        // last-known-good ones.
        let inactive = self.txn_counter % self.superblock_count as u64;
        cache.io_mut().write_page(inactive, &buf)?;
        // Step 4: Durability linearization point. Until this fsync returns the
        // transaction is not crash-safe; after it returns the new state is
        // observable on recovery.
        cache.io_mut().fsync()?;

        // Step 5: Promote in-memory state. Only now is the txn officially committed.
        self.committed_roots = self.current_roots.clone();
        self.committed_roots.total_pages = total_pages;
        // The in-memory freemap also advances: current_freemap reflects
        // every allocation and free from this transaction (merged in
        // persist_freemap above). This value is now durable.
        self.committed_freemap = self.current_freemap.clone();
        // R1: promote the live-slot counts. The cursor is per-transaction
        // and gets reset for the next begin().
        self.committed_live_slots = self.current_live_slots.clone();
        self.insert_cursor = None;
        self.active_txn = false;
        self.savepoints.clear();
        // txn_freed_pages were already merged into current_freemap by
        // persist_freemap; clear the vector now that it's done its job.
        self.txn_freed_pages.clear();

        Ok(())
    }

    /// Abort the active transaction and discard all in-memory changes.
    ///
    /// Uses watermark-based rollback (ISSUES.md I3): `cache.truncate` is
    /// called with `committed_roots.total_pages`, which both drops every
    /// cache entry for pages allocated during the transaction AND truncates
    /// the file back to its pre-transaction size. This fixes the earlier
    /// bug where rollback would leave zeroed trailing pages in the file
    /// because the cache-level discard did not propagate to `ftruncate`.
    ///
    /// Because `PageCache::new_page()` hands out monotonically increasing
    /// ids, the pre-transaction watermark cleanly separates "pages that
    /// existed at begin() time" (< watermark, preserved) from "pages
    /// allocated during this transaction" (>= watermark, discarded). No
    /// per-page tracking list is required.
    pub fn rollback(&mut self) -> Result<()> {
        self.check_alive()?;
        let result = self.rollback_inner();
        self.poison_on_fatal(result)
    }

    fn rollback_inner(&mut self) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // Rollback the cache in two steps:
        //   (a) Discard every dirty entry. This catches pages REUSED from
        //       the freemap whose id is less than the watermark — the
        //       watermark-based truncate below only catches extended
        //       pages. After discard, the next read for such a page id
        //       will re-load the last-committed content from disk, which
        //       is exactly the pre-transaction state. Safe because
        //       `flush()` (commit) always clears dirty flags, so any
        //       dirty entry was created in the current transaction.
        //   (b) Truncate to committed_roots.total_pages. This rewinds
        //       next_page_id AND shrinks the file, dropping every page
        //       allocated via extension (id >= watermark). Together with
        //       (a), this returns the cache and file to their exact
        //       pre-transaction state.
        {
            let mut cache = self.cache.borrow_mut();
            cache.discard_all_dirty();
            cache.truncate(self.committed_roots.total_pages)?;
        }

        self.current_roots = self.committed_roots.clone();
        // C1: MembershipIndex.outer_depth is in-memory state that index grows
        // mutate during the transaction, but it is NOT carried in Roots, so the
        // snapshot restore above does not rewind it. Re-derive it from the (now
        // committed) root — mirroring the open-time recovery — so the in-memory
        // descent depth matches the page it descends. Otherwise handles_with_tag
        // mis-descends a rolled-back-shallow root with a stale-deep depth.
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                RadixU64::recover_depth(&mut cache, self.current_roots.membership_index_page)?;
            self.membership_index.set_outer_depth(depth);
        }
        // I99: HandleTable.depth is the same kind of in-memory radix-depth cache
        // as outer_depth above -- mutated by grows, not carried in Roots -- so it
        // must also be re-derived from the restored root. Otherwise a rolled-back
        // handle-table grow leaves the descent depth too deep and lookups
        // mis-descend, returning InvalidHandle for committed handles.
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                HandleTable::recover_depth(&mut cache, self.current_roots.handle_table_page)?;
            self.handle_table.set_depth(depth);
        }
        // Revert the freemap working copy — any marks (free or allocate)
        // made during the transaction are discarded.
        self.current_freemap = self.committed_freemap.clone();
        // R1: revert the live-slot counts and drop the insert cursor.
        self.current_live_slots = self.committed_live_slots.clone();
        self.insert_cursor = None;
        self.active_txn = false;
        self.savepoints.clear();
        self.txn_freed_pages.clear();
        Ok(())
    }

    /// Push a named savepoint onto the stack. Captures the current
    /// `next_page_id` watermark so `rollback_to(name)` can truncate the
    /// cache back to this exact point. `freed_pages` is moved INTO the
    /// savepoint record so the enclosing transaction's `txn_freed_pages`
    /// accumulates only frees from the savepoint's own scope.
    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.savepoint_inner(name);
        self.poison_on_fatal(result)
    }

    fn savepoint_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if self.savepoints.iter().any(|sp| sp.name == name) {
            return Err(ChiselError::DuplicateSavepoint(name.to_string()));
        }
        let watermark = self.cache_watermark();
        // R1: snapshot the live-slot map and the cursor. Also drop the
        // cursor in the active scope — once a savepoint exists, the
        // insert path stops packing into the cursor (same posture as
        // freemap reuse: savepoints disable the optimization so the
        // rollback_to semantics stay simple).
        let live_slots = self.current_live_slots.clone();
        let insert_cursor = self.insert_cursor;
        self.insert_cursor = None;
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            roots: self.current_roots.clone(),
            watermark,
            freed_pages: std::mem::take(&mut self.txn_freed_pages),
            live_slots,
            insert_cursor,
        });
        Ok(())
    }

    /// Roll back to a named savepoint without ending the transaction.
    /// Truncates the cache to the savepoint's watermark (discarding every
    /// page allocated after the savepoint), restores the roots snapshot,
    /// and pops any savepoints layered on top. The named savepoint itself
    /// remains on the stack and can be rolled back to again or released.
    ///
    /// NOTE: `freed_pages` from savepoints layered on top (and from
    /// `self.txn_freed_pages`) are dropped here, which is correct —
    /// those frees never became durable, and the roots/page contents
    /// those frees described have been rewound along with the cache
    /// truncate. Post-R2, `commit()` DOES return freed pages to the
    /// freemap; this rollback path simply discards the unfinished
    /// accounting.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.rollback_to_inner(name);
        self.poison_on_fatal(result)
    }

    fn rollback_to_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let idx = self
            .savepoints
            .iter()
            .position(|sp| sp.name == name)
            .ok_or_else(|| ChiselError::SavepointNotFound(name.to_string()))?;

        let watermark = self.savepoints[idx].watermark;
        self.cache.borrow_mut().truncate(watermark)?;

        self.current_roots = self.savepoints[idx].roots.clone();
        // C1: re-derive outer_depth from the restored savepoint root (see rollback_inner).
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                RadixU64::recover_depth(&mut cache, self.current_roots.membership_index_page)?;
            self.membership_index.set_outer_depth(depth);
        }
        // I99: re-derive handle-table depth from the restored savepoint root
        // (same rationale as rollback_inner / outer_depth).
        {
            let mut cache = self.cache.borrow_mut();
            let depth =
                HandleTable::recover_depth(&mut cache, self.current_roots.handle_table_page)?;
            self.handle_table.set_depth(depth);
        }
        // R1: restore live-slot counts and cursor from the savepoint
        // snapshot. The cursor was force-cleared when the savepoint was
        // created, so this sets the cursor back to whatever value it
        // held BEFORE the savepoint was taken (typically also None,
        // since savepoint-bearing transactions disable packing).
        self.current_live_slots = self.savepoints[idx].live_slots.clone();
        self.insert_cursor = self.savepoints[idx].insert_cursor;
        self.savepoints.truncate(idx + 1);
        self.txn_freed_pages.clear();

        Ok(())
    }

    /// Release (flatten) a named savepoint and everything layered on top
    /// of it. Under watermark-based rollback, this is just `savepoints
    /// .truncate(idx)` plus a merge of freed-page lists — the released
    /// savepoints' allocated pages remain reachable via the outer
    /// watermark (i.e. `committed_roots.total_pages`), which is still the
    /// correct rollback destination for the enclosing transaction.
    pub fn release(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.release_inner(name);
        self.poison_on_fatal(result)
    }

    fn release_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let idx = self
            .savepoints
            .iter()
            .position(|sp| sp.name == name)
            .ok_or_else(|| ChiselError::SavepointNotFound(name.to_string()))?;

        // Merge freed_pages from all released savepoints back into the
        // current transaction's list. This preserves the invariant that
        // txn_freed_pages holds every "frees that would go to the freemap
        // on commit" across the entire enclosing transaction, so a later
        // rollback correctly drops them.
        let mut merged_freed = Vec::new();
        for sp in self.savepoints[idx..].iter() {
            merged_freed.extend_from_slice(&sp.freed_pages);
        }
        merged_freed.append(&mut self.txn_freed_pages);

        self.savepoints.truncate(idx);
        self.txn_freed_pages = merged_freed;

        Ok(())
    }

    /// Insert a value and return a stable handle.
    ///
    /// Handles are dense u64s drawn from current_roots.next_handle, starting at
    /// 1 — handle 0 is reserved as the "no handle" sentinel and is never
    /// returned. Large values
    /// (> MAX_INLINE_VALUE) go to an overflow chain and the HandleEntry records
    /// the first overflow page directly; small values get a slot in a freshly
    /// allocated data page. Either way the handle_table.insert() COWs the spine
    /// from leaf to root and returns the new root page_id, which becomes the
    /// new current_roots.handle_table_page. This is the fundamental shadow-
    /// paging step: the old root is still reachable via committed_roots and is
    /// untouched on disk until commit swaps the superblock.
    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value, 0);
        self.poison_on_fatal(result)
    }

    pub fn allocate_tagged(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value, tag);
        self.poison_on_fatal(result)
    }

    /// Compute the membership-index root produced by inserting `(tag, handle)`
    /// WITHOUT installing it into `current_roots`; superseded pages are appended
    /// to `freed`. Split out so `allocate_inner` can stage the forward- and
    /// reverse-map updates and install them atomically (BUG#2). On error,
    /// `MembershipIndex::insert` leaves `self.outer_depth` unchanged (it writes
    /// back only on success), so the caller need not restore any reverse-map
    /// in-memory depth.
    fn membership_insert_candidate(
        &mut self,
        tag: u32,
        handle: u64,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        let mut cache = self.cache.borrow_mut();
        let reuse = self.savepoints.is_empty();
        let fm = &mut *self.current_freemap;
        let mut alloc = |c: &mut PageCache| cow_alloc(c, fm, reuse);
        self.membership_index.insert(
            &mut cache,
            self.current_roots.membership_index_page,
            tag,
            handle,
            &mut alloc,
            freed,
        )
    }

    /// Compute the handle-table root produced by inserting `entry` for `handle`
    /// WITHOUT installing it into `current_roots`; superseded spine pages are
    /// appended to `freed`. The forward-map counterpart to
    /// `membership_insert_candidate` for `allocate_inner`'s atomic staging
    /// (BUG#2). NOTE: `HandleTable::insert` may `grow`, which bumps the
    /// in-memory descent depth eagerly — the caller captures and restores that
    /// depth on the prepare-abort path.
    fn handle_table_insert_candidate(
        &mut self,
        handle: u64,
        entry: &HandleEntry,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        // Test-only injection (see `fail_next_handle_table_op`): simulate a
        // non-fatal CacheFull at the forward / handle-table step — the one
        // carrying the eager depth bump for allocate. Lives here so BOTH
        // allocate_inner and update_inner exercise the real abort/unwind. No
        // production artifact under `#[cfg(not(test))]`.
        #[cfg(test)]
        if self.fail_next_handle_table_op.replace(false) {
            return Err(ChiselError::CacheFull { limit: 0 });
        }
        let mut cache = self.cache.borrow_mut();
        let reuse = self.savepoints.is_empty();
        let fm = &mut *self.current_freemap;
        let mut alloc = |c: &mut PageCache| cow_alloc(c, fm, reuse);
        self.handle_table.insert(
            &mut cache,
            self.current_roots.handle_table_page,
            handle,
            entry,
            &mut alloc,
            freed,
        )
    }

    /// Unwind the in-memory side effects of a partially-completed
    /// `allocate_inner` PREPARE phase so a non-fatal failure is a true no-op.
    /// Restores the handle-table root pointer (an empty DB reverts to
    /// `PAGE_ID_NONE`, undoing any lazy `ensure_handle_table` materialization)
    /// and the eagerly-bumped descent depth, and releases the inline value's
    /// data slot so `current_live_slots` / the insert cursor stay consistent
    /// with the un-installed root — a page allocated solely for this value goes
    /// to zero and is queued for reclamation, while a shared cursor page keeps a
    /// defrag-reclaimable dead slot (exactly like a normal delete). Overflow
    /// value storage has no live-slot/cursor side effects, so its pages are left
    /// as the bounded commit-after-error leak class. `next_handle` was never
    /// consumed (it is bumped only on success), so there is nothing to undo there.
    fn abort_allocate_prepare(
        &mut self,
        saved_root: u64,
        saved_depth: u32,
        inline_page: Option<u64>,
    ) {
        self.current_roots.handle_table_page = saved_root;
        self.handle_table.set_depth(saved_depth);
        if let Some(page_id) = inline_page {
            self.release_data_slot(page_id);
        }
    }

    /// Compute the membership-index root produced by removing `(tag, handle)`
    /// WITHOUT installing it; returns `(new_root, was_present)`. Counterpart to
    /// `membership_insert_candidate` for `delete_inner`'s atomic staging (BUG#2).
    fn membership_remove_candidate(
        &mut self,
        tag: u32,
        handle: u64,
        freed: &mut Vec<u64>,
    ) -> Result<(u64, bool)> {
        let mut cache = self.cache.borrow_mut();
        let reuse = self.savepoints.is_empty();
        let fm = &mut *self.current_freemap;
        let mut alloc = |c: &mut PageCache| cow_alloc(c, fm, reuse);
        self.membership_index.remove(
            &mut cache,
            self.current_roots.membership_index_page,
            tag,
            handle,
            &mut alloc,
            freed,
        )
    }

    fn allocate_inner(&mut self, value: &[u8], tag: u32) -> Result<u64> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // BUG#2 atomic staging: the FORWARD map (the chunk's HandleEntry.tag in
        // the handle table) and the REVERSE map (tag -> handles in the
        // membership index, powering handles_with_tag / delete-by-tag) must
        // become durable together. We compute both candidate roots in a fallible
        // PREPARE phase that never installs into `current_roots`, then install
        // them together in an infallible phase. A non-fatal CacheFull/
        // SpillwayFull mid-prepare is unwound by `abort_allocate_prepare` into a
        // COMPLETE no-op: neither map changes, the eagerly-bumped handle-table
        // depth and any lazily-created root are restored, the inline value's data
        // slot is released (keeping live-slot / cursor accounting consistent),
        // and the handle id is not consumed (next_handle is bumped only on
        // success). The lone residue is bounded allocated-but-unreferenced
        // overflow/COW pages on a commit-after-error — the same pre-existing leak
        // class as any post-allocation failure, fully reclaimed by the expected
        // rollback.
        let handle = self.current_roots.next_handle;

        // Value storage (PREPARE). For an inline value this also bumps
        // current_live_slots and may set the insert cursor; capture its page id
        // so a later prepare failure can release it via `abort_allocate_prepare`,
        // keeping that bookkeeping consistent with the un-installed root.
        // Overflow storage has no live-slot/cursor side effects.
        let mut inline_page: Option<u64> = None;
        let entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
                tag,
                client_byte: 0,
            }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            inline_page = Some(data_page_id);
            HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
                tag,
                client_byte: 0,
            }
        };

        // Capture the handle-table root/depth BEFORE `ensure_handle_table` may
        // lazily materialize an empty root, so a prepare failure restores
        // current_roots to its true pre-allocate state (an empty DB reverts to
        // PAGE_ID_NONE). The depth capture also covers `HandleTable::grow`, which
        // bumps the in-memory descent depth EAGERLY (before its fallible leaf
        // COW) while we defer the root install. The membership index needs no
        // such save: MembershipIndex::insert writes its outer_depth back only on
        // the success path, so a failed reverse-map op never advances it.
        let saved_ht_root = self.current_roots.handle_table_page;
        let saved_ht_depth = self.handle_table.depth();
        self.ensure_handle_table()?;

        // FORWARD map: compute the new handle-table root; do NOT install yet.
        // (handle_table_insert_candidate carries the #[cfg(test)] forward-step
        // fault injection, shared with update_inner's handle-table step.)
        let mut ht_freed: Vec<u64> = Vec::new();
        let ht_new_root = match self.handle_table_insert_candidate(handle, &entry, &mut ht_freed) {
            Ok(r) => r,
            Err(e) => {
                self.abort_allocate_prepare(saved_ht_root, saved_ht_depth, inline_page);
                return Err(e);
            }
        };

        // REVERSE map: compute the new membership-index root; do NOT install
        // yet. tag 0 = untagged (never indexed).
        let mut mi_new_root: Option<u64> = None;
        let mut mi_freed: Vec<u64> = Vec::new();
        if tag != 0 {
            // Test-only injection (see `fail_next_membership_op`): simulate a
            // non-fatal CacheFull at the reverse-map step so the regression test
            // exercises the REAL failure handling below. No production artifact:
            // the non-test `let res` is the only one compiled outside tests.
            #[cfg(test)]
            let res: Result<u64> = if self.fail_next_membership_op.replace(false) {
                Err(ChiselError::CacheFull { limit: 0 })
            } else {
                self.membership_insert_candidate(tag, handle, &mut mi_freed)
            };
            #[cfg(not(test))]
            let res: Result<u64> = self.membership_insert_candidate(tag, handle, &mut mi_freed);

            match res {
                Ok(r) => mi_new_root = Some(r),
                Err(e) => {
                    // The handle-table insert above already succeeded and may
                    // have grown the tree (bumping the in-memory depth) and
                    // produced a candidate root we are now discarding. Unwind the
                    // whole prepare so current_roots, the depth, and the inline
                    // value's slot all return to their pre-allocate state.
                    self.abort_allocate_prepare(saved_ht_root, saved_ht_depth, inline_page);
                    return Err(e);
                }
            }
        }

        // INSTALL phase (infallible): both maps move together, and only now is
        // the handle id consumed and the superseded spine pages queued for
        // reclamation at commit.
        self.current_roots.next_handle += 1;
        self.current_roots.handle_table_page = ht_new_root;
        self.txn_freed_pages.append(&mut ht_freed);
        if let Some(root) = mi_new_root {
            self.current_roots.membership_index_page = root;
            self.txn_freed_pages.append(&mut mi_freed);
        }

        Ok(handle)
    }

    pub fn tag(&self, handle: u64) -> Result<u32> {
        self.check_alive()?;
        let result = self.tag_inner(handle);
        self.poison_on_fatal(result)
    }

    fn tag_inner(&self, handle: u64) -> Result<u32> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };
        if root == PAGE_ID_NONE {
            return Err(ChiselError::InvalidHandle(handle));
        }
        let mut cache = self.cache.borrow_mut();
        let entry = self
            .handle_table
            .lookup(&mut cache, root, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;
        Ok(entry.tag)
    }

    /// Return the opaque client byte stored in `handle`'s entry. Returns 0 if
    /// never set (including every chunk created before this feature). Mirrors
    /// the read-path root selection. Rejects deleted handles with
    /// `InvalidHandle` (following `read()`; this is stricter than `tag()`'s
    /// unguarded read of a tombstone — a pre-existing `tag()` quirk tracked
    /// separately). Takes `&self`.
    pub fn client_byte(&self, handle: u64) -> Result<u8> {
        self.check_alive()?;
        let result = self.client_byte_inner(handle);
        self.poison_on_fatal(result)
    }

    fn client_byte_inner(&self, handle: u64) -> Result<u8> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };
        if root == PAGE_ID_NONE {
            return Err(ChiselError::InvalidHandle(handle));
        }
        let mut cache = self.cache.borrow_mut();
        let entry = self
            .handle_table
            .lookup(&mut cache, root, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;
        match entry.flags {
            HandleFlags::Deleted => Err(ChiselError::InvalidHandle(handle)),
            _ => Ok(entry.client_byte),
        }
    }

    /// Set the opaque client byte for `handle`. Requires an active
    /// transaction; durable on commit, reverted on rollback. Any `u8` is
    /// valid. COWs only the handle-table leaf — no data-page, overflow, or
    /// membership-index work. Takes `&mut self`.
    pub fn set_client_byte(&mut self, handle: u64, byte: u8) -> Result<()> {
        self.check_alive()?;
        let result = self.set_client_byte_inner(handle, byte);
        self.poison_on_fatal(result)
    }

    fn set_client_byte_inner(&mut self, handle: u64, byte: u8) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let mut entry = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .lookup(&mut cache, self.current_roots.handle_table_page, handle)?
                .ok_or(ChiselError::InvalidHandle(handle))?
        };
        if matches!(entry.flags, HandleFlags::Deleted) {
            return Err(ChiselError::InvalidHandle(handle));
        }
        entry.client_byte = byte;
        self.ht_insert(handle, &entry)?;
        Ok(())
    }

    pub fn handles_with_tag(&self, tag: u32) -> Result<Vec<u64>> {
        self.check_alive()?;
        let result = self.handles_with_tag_inner(tag);
        self.poison_on_fatal(result)
    }

    fn handles_with_tag_inner(&self, tag: u32) -> Result<Vec<u64>> {
        let root = if self.active_txn {
            self.current_roots.membership_index_page
        } else {
            self.committed_roots.membership_index_page
        };
        let mut cache = self.cache.borrow_mut();
        // No PAGE_ID_NONE guard (unlike tag_inner): an empty/absent index is a
        // legitimate "no handles with this tag" -> handles_for_tag returns an
        // empty Vec for a PAGE_ID_NONE root, whereas a missing handle table is an error.
        self.membership_index.handles_for_tag(&mut cache, root, tag)
    }

    /// Read a value by handle.
    ///
    /// If a transaction is active, reads see the in-progress (uncommitted) state
    /// through current_roots — i.e. "read your own writes". Otherwise reads go
    /// through committed_roots, the last durably-committed snapshot. There is no
    /// MVCC / snapshot isolation for concurrent readers because the writer is
    /// single-threaded; this branch is purely about making the active writer
    /// see its own pending mutations.
    ///
    /// F3: takes `&self`. Internally, the page cache is wrapped in a
    /// RefCell so that the mutation required by LRU bookkeeping / page
    /// loading can happen behind a shared reference. See the field-level
    /// comment on `cache` for the full rationale and why RefCell was
    /// chosen over Mutex.
    pub fn read(&self, handle: u64) -> Result<Vec<u8>> {
        self.check_alive()?;
        let result = self.read_inner(handle);
        self.poison_on_fatal(result)
    }

    fn read_inner(&self, handle: u64) -> Result<Vec<u8>> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };

        if root == PAGE_ID_NONE {
            return Err(ChiselError::InvalidHandle(handle));
        }

        let mut cache = self.cache.borrow_mut();
        let entry = self
            .handle_table
            .lookup(&mut cache, root, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))?;

        match entry.flags {
            HandleFlags::Live => {
                let buf = cache.get(entry.page_id)?;
                // The handle-table entry insists this slot is live. If
                // `DataPage::read` returns None anyway, the data page's
                // structural state disagrees with the handle table — the
                // page header, slot directory, or slot entry is damaged.
                // That's CorruptPage (fatal / poisons the manager), not
                // InvalidHandle (operational).
                match DataPage::read(buf, entry.slot_index) {
                    Some(data) => Ok(data.to_vec()),
                    None => Err(ChiselError::CorruptPage {
                        page_id: entry.page_id,
                    }),
                }
            }
            HandleFlags::Overflow => Overflow::read(&mut cache, entry.page_id),
            HandleFlags::Deleted => Err(ChiselError::InvalidHandle(handle)),
        }
    }

    /// Update an existing handle to point at a new value.
    ///
    /// Allocates a new slot/overflow chain for the new value and rewrites
    /// the HandleEntry via COW. The OLD location is retired differently
    /// depending on its kind:
    ///
    ///   * Inline (Live): goes through `release_data_slot`, which
    ///     decrements the per-page live-slot count (R1). Only when the
    ///     count reaches zero does the entire page land in
    ///     `txn_freed_pages`. Otherwise the slot becomes a tombstone,
    ///     reclaimable only via defrag (R3).
    ///   * Overflow: the whole chain is deleted and every page in the
    ///     chain is pushed onto `txn_freed_pages`.
    ///
    /// The earlier "assumes one live slot per page / must change when R1
    /// lands" caveat is OBSOLETE — R1 has landed and the slot-level
    /// accounting below implements exactly the post-R1 contract.
    pub fn update(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        self.check_alive()?;
        let result = self.update_inner(handle, value);
        self.poison_on_fatal(result)
    }

    fn update_inner(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        let entry = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .lookup(&mut cache, self.current_roots.handle_table_page, handle)?
                .ok_or(ChiselError::InvalidHandle(handle))?
        };

        // Atomic staging (same discipline as delete_inner): do NOT retire the
        // OLD value's storage until the NEW entry is durably installed. The
        // previous "free old first" ordering meant a non-fatal CacheFull during
        // the new-value write or the handle-table install left the committed
        // handle still pointing at pages already queued for reclamation — a
        // reachable-but-free page that commit then frees, corrupting the live
        // value on the next freemap reuse. We compute the new value, the new
        // handle-table root, and the old-location free set in a fallible PREPARE
        // phase that touches no installed state, then install the new entry and
        // retire the old location together in an infallible INSTALL phase. A
        // mid-prepare failure is a complete no-op.
        //
        // `update` replaces an EXISTING handle, so handle_table.insert never
        // grows (handle < capacity) — no in-memory depth save is needed (unlike
        // allocate_inner).

        // Test-only injection (see `fail_next_update_value_write`): now the FIRST
        // fallible step, so a simulated failure here retires nothing.
        #[cfg(test)]
        if self.fail_next_update_value_write.replace(false) {
            return Err(ChiselError::CacheFull { limit: 0 });
        }

        // PREPARE: write the new value storage. Tags and the client byte are
        // entry-resident and carried forward unchanged (the handle is unchanged,
        // so the membership index needs no edit — only the value's storage
        // moves). Capture the inline data page so a later prepare failure can
        // release it, keeping live-slot / cursor bookkeeping consistent.
        let mut new_inline_page: Option<u64> = None;
        let new_entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
                tag: entry.tag,
                client_byte: entry.client_byte,
            }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            new_inline_page = Some(data_page_id);
            HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
                tag: entry.tag,
                client_byte: entry.client_byte,
            }
        };

        // PREPARE: compute the new handle-table root (no install). On failure,
        // release the just-reserved inline slot so live-slot accounting stays
        // consistent with the un-installed root (overflow new-value pages are the
        // bounded commit-after-error leak class); the OLD location is untouched.
        let mut ht_freed: Vec<u64> = Vec::new();
        let ht_new_root =
            match self.handle_table_insert_candidate(handle, &new_entry, &mut ht_freed) {
                Ok(r) => r,
                Err(e) => {
                    if let Some(page_id) = new_inline_page {
                        self.release_data_slot(page_id);
                    }
                    return Err(e);
                }
            };

        // PREPARE: compute the OLD location's free set without applying it. For
        // Overflow this is a read-only walk of the old chain (a fallible
        // cold-page load); for Live the page id is released in the install phase;
        // Deleted carries no storage. On the overflow-walk failure, unwind the
        // new inline slot — the old chain is untouched (the walk frees nothing).
        enum OldRelease {
            Inline(u64),
            Overflow(Vec<u64>),
            Nothing,
        }
        let old_release = match entry.flags {
            HandleFlags::Live => OldRelease::Inline(entry.page_id),
            HandleFlags::Overflow => {
                let walked = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::delete(&mut cache, entry.page_id)
                };
                match walked {
                    Ok(freed) => OldRelease::Overflow(freed),
                    Err(e) => {
                        if let Some(page_id) = new_inline_page {
                            self.release_data_slot(page_id);
                        }
                        return Err(e);
                    }
                }
            }
            HandleFlags::Deleted => OldRelease::Nothing,
        };

        // INSTALL phase (infallible): install the NEW entry first so the handle
        // points at the new storage, THEN retire the OLD location (now genuinely
        // unreferenced). For Live, release_data_slot does R1 slot accounting
        // (freeing the page only when its last live slot goes); for Overflow, the
        // walked chain pages are queued for reclamation.
        self.current_roots.handle_table_page = ht_new_root;
        self.txn_freed_pages.append(&mut ht_freed);
        match old_release {
            OldRelease::Inline(page_id) => self.release_data_slot(page_id),
            OldRelease::Overflow(freed) => self.txn_freed_pages.extend_from_slice(&freed),
            OldRelease::Nothing => {}
        }

        Ok(())
    }

    /// Delete a handle.
    ///
    /// Retires the old location (inline slot via `release_data_slot`
    /// with its R1 slot-level accounting; overflow chain by deleting
    /// every page in the chain into `txn_freed_pages`) and then asks
    /// the handle table to remove the mapping via COW. The earlier
    /// "whole-page free assumes one value per page" caveat referenced
    /// by `update`'s docstring is obsolete post-R1.
    pub fn delete(&mut self, handle: u64) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_inner(handle);
        self.poison_on_fatal(result)
    }

    fn delete_inner(&mut self, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // BUG#2 atomic staging (see allocate_inner): the FORWARD map (the
        // handle-table tombstone) and the REVERSE map (membership-index removal)
        // must become durable together. We compute both candidate roots — plus
        // the fallible part of value-storage release — in a PREPARE phase that
        // never touches `current_roots`, then install everything in an
        // infallible phase. A non-fatal CacheFull/SpillwayFull mid-prepare
        // leaves the delete a complete no-op, so the reverse index can never
        // retain a member for a tombstoned handle — the stale entry that would
        // otherwise later escalate to a fatal CorruptPage.
        //
        // No in-memory depth save is needed here (unlike allocate): handle-table
        // DELETE never grows the tree, and MembershipIndex::remove writes its
        // outer_depth back only on the success path.

        // FORWARD map: compute the tombstoned root; do NOT install yet. A single
        // tree walk (I32) COWs the leaf with a tombstone and returns the
        // previous entry. Returns (root, None) — unchanged root, no COW — if the
        // handle was absent or already a tombstone; we escalate None to
        // InvalidHandle to preserve the public-API behavior.
        let mut ht_freed: Vec<u64> = Vec::new();
        let (ht_new_root, prev_entry) = {
            let mut cache = self.cache.borrow_mut();
            let reuse = self.savepoints.is_empty();
            let fm = &mut *self.current_freemap;
            let mut alloc = |c: &mut PageCache| cow_alloc(c, fm, reuse);
            self.handle_table.delete(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                &mut alloc,
                &mut ht_freed,
            )?
        };
        let entry = prev_entry.ok_or(ChiselError::InvalidHandle(handle))?;

        // Stage the value-storage release. The only FALLIBLE part — walking an
        // overflow chain to collect its page ids — runs here in prepare;
        // `Overflow::delete` is a read-only walk that mutates no freemap state,
        // so discarding its result on a later failure is safe (the still-current
        // old entry keeps referencing the chain). The actual free-queueing /
        // slot release is deferred to the install phase below. Order vs. the
        // tombstone write does not matter for correctness — both become durable
        // (or roll back) atomically at commit.
        enum PendingRelease {
            Inline(u64),
            Overflow(Vec<u64>),
        }
        let release = match entry.flags {
            HandleFlags::Live => PendingRelease::Inline(entry.page_id),
            HandleFlags::Overflow => {
                let freed = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::delete(&mut cache, entry.page_id)?
                };
                PendingRelease::Overflow(freed)
            }
            HandleFlags::Deleted => {
                // I45 (ISSUES.md, 2026-05-22): a Deleted entry that `ok_or`
                // didn't catch means the in-memory state contradicts itself —
                // handle_table::delete returns None for already-tombstoned
                // handles and the ok_or above converts None into the typed
                // error, so reaching this arm signals a broken cross-module
                // contract (most likely a future refactor of delete). Surface
                // it typed rather than aborting the caller's process. Returned
                // BEFORE any install, so current_roots stays untouched.
                return Err(ChiselError::CorruptPage {
                    page_id: entry.page_id,
                });
            }
        };

        // REVERSE map: compute the membership-removed root; do NOT install yet.
        // The tag comes from the tombstoned entry. Tag 0 (untagged) is never in
        // the index, so there is nothing to remove.
        let mut mi_new_root: Option<u64> = None;
        let mut idx_freed: Vec<u64> = Vec::new();
        if entry.tag != 0 {
            // Test-only injection (see `fail_next_membership_op`): simulate a
            // non-fatal CacheFull at the reverse-map step so the regression test
            // exercises the REAL failure handling below. No production artifact:
            // the non-test `let res` is the only one compiled outside tests.
            #[cfg(test)]
            let res: Result<(u64, bool)> = if self.fail_next_membership_op.replace(false) {
                Err(ChiselError::CacheFull { limit: 0 })
            } else {
                self.membership_remove_candidate(entry.tag, handle, &mut idx_freed)
            };
            #[cfg(not(test))]
            let res: Result<(u64, bool)> =
                self.membership_remove_candidate(entry.tag, handle, &mut idx_freed);

            let (new_index_root, _removed) = res?;
            mi_new_root = Some(new_index_root);
        }

        // INSTALL phase (infallible): tombstone + value release + reverse-map
        // removal all become visible together. The superseded handle-table
        // spine pages are queued only now, post-install, so an early prepare
        // failure leaves them referenced by the still-current old tree.
        self.current_roots.handle_table_page = ht_new_root;
        self.txn_freed_pages.append(&mut ht_freed);
        match release {
            PendingRelease::Inline(page_id) => self.release_data_slot(page_id),
            PendingRelease::Overflow(freed) => self.txn_freed_pages.extend_from_slice(&freed),
        }
        if let Some(root) = mi_new_root {
            self.current_roots.membership_index_page = root;
            self.txn_freed_pages.append(&mut idx_freed);
        }

        Ok(())
    }

    pub fn delete_tagged(&mut self, handle: u64, tag: u32) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_tagged_inner(handle, tag);
        self.poison_on_fatal(result)
    }

    fn delete_tagged_inner(&mut self, handle: u64, tag: u32) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        // Lookup-then-delete: verify the tag before mutating anything, so a wrong
        // tag leaves both the chunk and the membership index untouched. The extra
        // lookup walk (delete_inner walks again) is the price of verify-before-mutate.
        let actual = {
            let root = self.current_roots.handle_table_page;
            if root == PAGE_ID_NONE {
                return Err(ChiselError::InvalidHandle(handle));
            }
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .lookup(&mut cache, root, handle)?
                .ok_or(ChiselError::InvalidHandle(handle))?
                .tag
        };
        if actual != tag {
            return Err(ChiselError::TagMismatch {
                handle,
                expected: tag,
                actual,
            });
        }
        self.delete_inner(handle)
    }

    pub fn delete_with_tag(&mut self, tag: u32, max: usize) -> Result<TagDropProgress> {
        self.check_alive()?;
        let result = self.delete_with_tag_inner(tag, max);
        self.poison_on_fatal(result)
    }

    fn delete_with_tag_inner(&mut self, tag: u32, max: usize) -> Result<TagDropProgress> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if max == 0 {
            return Ok(TagDropProgress {
                deleted: Vec::new(),
                complete: false,
            });
        }
        // Bounded enumeration: ask for max+1 so the count tells us whether more
        // remain (len > max => not complete). saturating_add keeps the absurd
        // max == usize::MAX case meaning "enumerate everything" instead of
        // wrapping to 0 — which would enumerate nothing and falsely report the
        // tag complete. The members snapshot is taken BEFORE the deletions, then
        // each is deleted via delete_inner (which removes it from the index and
        // frees its chunk).
        let members = {
            let root = self.current_roots.membership_index_page;
            let mut cache = self.cache.borrow_mut();
            self.membership_index.handles_for_tag_bounded(
                &mut cache,
                root,
                tag,
                max.saturating_add(1),
            )?
        };
        let complete = members.len() <= max;
        let take: Vec<u64> = members.into_iter().take(max).collect();
        for &h in &take {
            self.delete_inner(h)?;
        }
        Ok(TagDropProgress {
            deleted: take,
            complete,
        })
    }

    /// Delete many handles in a single transaction.
    ///
    /// Today: this is a loop over `delete_inner`. After PR-A's fusion
    /// (I32), each delete walks the handle table once per handle. For
    /// dense delete patterns (many handles in the same leaf), a
    /// per-leaf batched implementation would walk once per leaf
    /// instead — that's tracked as I33 in ISSUES.md, deferred until
    /// a workload demonstrates the win is worth the complexity.
    ///
    /// Error semantics: on the first error the loop stops and returns
    /// the error. Handles deleted before the failure remain marked
    /// for deletion in `current_roots`, so the caller can choose
    /// between `rollback()` (abandon the whole batch) or `commit()`
    /// (keep the partial work).
    pub fn delete_many(&mut self, handles: &[u64]) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_many_inner(handles);
        self.poison_on_fatal(result)
    }

    fn delete_many_inner(&mut self, handles: &[u64]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        // See I33 in ISSUES.md for the deferred per-leaf batching work.
        for &handle in handles {
            self.delete_inner(handle)?;
        }
        Ok(())
    }

    /// Iterate over all live handles.
    ///
    /// F3: takes `&self` (same rationale as `read`).
    pub fn handles(&self) -> Result<Vec<u64>> {
        self.check_alive()?;
        let result = self.handles_inner();
        self.poison_on_fatal(result)
    }

    fn handles_inner(&self) -> Result<Vec<u64>> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut cache = self.cache.borrow_mut();
        let entries = self.handle_table.iter_live(&mut cache, root)?;
        Ok(entries.into_iter().map(|(h, _)| h).collect())
    }

    /// Snapshot the four engine-activity counters (cache hits/misses,
    /// pages allocated, fsync calls). Counters are cumulative from the
    /// most recent open; the bench harness reads-subtract-reads for
    /// per-cell deltas. Takes `&self` (F3); poison-aware via
    /// `check_alive`.
    pub fn counters(&self) -> Result<ChiselCounters> {
        self.check_alive()?;
        Ok(self.cache.borrow().counters())
    }

    /// I74 (ISSUES.md, 2026-05-22): peek the spillway's current
    /// (logical_bytes, max_bytes) for `Chisel::stats`. `None` if the
    /// spillway has never been opened. Routes through the same
    /// poison check as `counters()` so a fatal-error state surfaces
    /// here too — operators reading `stats()` get a `Poisoned`
    /// error rather than stale-looking Some(0,0).
    pub fn spillway_capacity(&self) -> Result<Option<(u64, u64)>> {
        self.check_alive()?;
        Ok(self.cache.borrow().spillway_capacity())
    }

    // --- Named roots (ISSUES.md F2) ---
    //
    // The named-root table lives inside the superblock (see
    // `superblock::NamedRoot`). Modifications update
    // `current_roots.named_roots` in memory; on commit that array is
    // copied into the new Superblock and fsync'd along with the rest.
    // On rollback or `rollback_to`, the usual snapshot restore reverts
    // named roots alongside the handle-table root — no extra plumbing.
    //
    // Name validation is intentionally strict: names must be non-empty,
    // must fit in NAMED_ROOT_NAME_LEN bytes, must not contain NUL
    // (because NUL is the "empty slot" sentinel), and must be valid
    // UTF-8 at the API boundary. Names are compared byte-for-byte after
    // validation; the fixed 24-byte buffer is NUL-padded.

    /// Validate a root name and return its byte form, padded to
    /// NAMED_ROOT_NAME_LEN with trailing NULs. Returns `InvalidRootName`
    /// on any violation.
    fn encode_root_name(name: &str) -> Result<[u8; NAMED_ROOT_NAME_LEN]> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > NAMED_ROOT_NAME_LEN {
            return Err(ChiselError::InvalidRootName);
        }
        if bytes.contains(&0) {
            return Err(ChiselError::InvalidRootName);
        }
        let mut encoded = [0u8; NAMED_ROOT_NAME_LEN];
        encoded[..bytes.len()].copy_from_slice(bytes);
        Ok(encoded)
    }

    /// Bind `name` to `handle` in the named-root table. If `name` already
    /// exists, its handle is overwritten. If it doesn't exist and the
    /// table has no empty slots, returns `RootNameTableFull`. Requires an
    /// active transaction and becomes durable on commit; reverts on
    /// rollback/rollback_to.
    pub fn set_root_name(&mut self, name: &str, handle: u64) -> Result<()> {
        self.check_alive()?;
        let result = self.set_root_name_inner(name, handle);
        self.poison_on_fatal(result)
    }

    fn set_root_name_inner(&mut self, name: &str, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let encoded = Self::encode_root_name(name)?;

        // First pass: update in place if the name already exists.
        for entry in self.current_roots.named_roots.iter_mut() {
            if !entry.is_empty() && entry.name == encoded {
                entry.handle = handle;
                return Ok(());
            }
        }
        // Second pass: install in the first empty slot.
        for entry in self.current_roots.named_roots.iter_mut() {
            if entry.is_empty() {
                entry.name = encoded;
                entry.handle = handle;
                return Ok(());
            }
        }
        Err(ChiselError::RootNameTableFull)
    }

    /// Look up a named root. Returns `Ok(None)` if the name is not bound.
    /// Reads see the transactional view: inside an active transaction,
    /// pending `set_root_name` / `clear_root_name` changes are visible;
    /// outside a transaction, reads the last durably committed table.
    ///
    /// Takes `&self` — named-root reads are semantically read-only.
    pub fn get_root_name(&self, name: &str) -> Result<Option<u64>> {
        self.check_alive()?;
        let result = self.get_root_name_inner(name);
        self.poison_on_fatal(result)
    }

    fn get_root_name_inner(&self, name: &str) -> Result<Option<u64>> {
        let encoded = Self::encode_root_name(name)?;
        let table = if self.active_txn {
            &self.current_roots.named_roots
        } else {
            &self.committed_roots.named_roots
        };
        for entry in table.iter() {
            if !entry.is_empty() && entry.name == encoded {
                return Ok(Some(entry.handle));
            }
        }
        Ok(None)
    }

    /// Remove a named root. No-op if the name is not bound (returns Ok).
    /// Requires an active transaction. Becomes durable on commit;
    /// reverts on rollback/rollback_to.
    pub fn clear_root_name(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.clear_root_name_inner(name);
        self.poison_on_fatal(result)
    }

    fn clear_root_name_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let encoded = Self::encode_root_name(name)?;
        for entry in self.current_roots.named_roots.iter_mut() {
            if !entry.is_empty() && entry.name == encoded {
                *entry = NamedRoot::EMPTY;
                return Ok(());
            }
        }
        Ok(())
    }

    /// Poisoning-aware wrapper around `PageCache::file_page_count`. Called
    /// by `Chisel::stats()` so that a fatal I/O error while measuring the
    /// file size also poisons the manager.
    ///
    /// F3: takes `&self`.
    pub fn file_page_count(&self) -> Result<u64> {
        self.check_alive()?;
        let result = self.cache.borrow_mut().file_page_count();
        self.poison_on_fatal(result)
    }

    // --- Selective defragmentation support (ISSUES.md R3 + I17) ---
    //
    // These methods expose just enough of the R1 live-slot tracking
    // for `defrag::defrag` to do selective page compaction. The
    // defrag module is in-crate and could in principle access the
    // fields directly, but going through named methods keeps the
    // intent obvious at each call site.

    /// Page ids of data pages whose effective density (live slots /
    /// stored slots) is strictly less than `threshold_ratio`. A
    /// freshly-packed page with every slot still live has density
    /// 1.0; a page that originally packed 39 values but now has only
    /// 5 live (34 dead-weight tombstones) has density 0.128 and is a
    /// strong defrag candidate.
    ///
    /// The metric uses the page's OWN stored-slot count (read from
    /// the on-disk header via `DataPage::slot_count`) as the
    /// denominator — not the max-observed count in the database —
    /// because dead-weight slots are what defrag is trying to reclaim.
    /// The older "relative to densest" metric failed for the case of a
    /// single remaining sparse page (density 1.0 against itself).
    ///
    /// Returns an empty set when `threshold_ratio <= 0`. Fallible
    /// because the per-page stored count is read through the cache.
    pub fn sparse_data_pages(
        &self,
        threshold_ratio: f64,
    ) -> Result<std::collections::HashSet<u64>> {
        self.check_alive()?;
        let result = self.sparse_data_pages_inner(threshold_ratio);
        self.poison_on_fatal(result)
    }

    fn sparse_data_pages_inner(
        &self,
        threshold_ratio: f64,
    ) -> Result<std::collections::HashSet<u64>> {
        let mut sparse = std::collections::HashSet::new();
        if threshold_ratio <= 0.0 {
            return Ok(sparse);
        }
        let page_ids: Vec<u64> = self.current_live_slots.keys().copied().collect();
        for page_id in page_ids {
            let live = match self.current_live_slots.get(&page_id) {
                Some(&n) if n > 0 => n,
                _ => continue,
            };
            let stored = {
                let mut cache = self.cache.borrow_mut();
                DataPage::slot_count(cache.get(page_id)?) as u32
            };
            if stored == 0 {
                continue;
            }
            let density = live as f64 / stored as f64;
            if density < threshold_ratio {
                sparse.insert(page_id);
            }
        }
        Ok(sparse)
    }

    /// Number of data pages currently tracked in the in-transaction
    /// view.
    ///
    /// `#[allow(dead_code)]`: referenced in `defrag.rs` and this
    /// module's own doc comments as the wrong-metric example (the I17
    /// fix uses `data_page_ids_snapshot` instead). Kept as the named
    /// counter to make the contrast explicit; any future caller
    /// wanting a coarse "how many data pages are in play" number has
    /// the obvious accessor.
    #[allow(dead_code)]
    pub fn data_page_count(&self) -> usize {
        self.current_live_slots.len()
    }

    /// Snapshot of the page ids currently tracked as holding at least
    /// one live slot. Used by `defrag::defrag` for the I17 stat: after
    /// the sweep, `pages_freed` is the count of ids that were in this
    /// snapshot and are no longer in `current_live_slots` — i.e.,
    /// pages that the sweep fully drained and returned to the freemap.
    /// Net change in `data_page_count` is the wrong metric here
    /// because a relocation simultaneously drains a sparse page and
    /// creates a dense one; the former should count as "reclaimed"
    /// even when the latter offsets the net count.
    pub fn data_page_ids_snapshot(&self) -> std::collections::HashSet<u64> {
        self.current_live_slots.keys().copied().collect()
    }

    /// Look up the data page id that currently holds `handle`. Returns
    /// `Ok(None)` if the handle doesn't exist, is deleted, or points
    /// at an overflow chain (for which the notion of "data page" does
    /// not apply).
    ///
    /// Takes `&self`; uses the RefCell around the cache to perform the
    /// handle-table lookup. Poisons the manager on fatal I/O or
    /// checksum errors.
    pub fn handle_live_page_id(&self, handle: u64) -> Result<Option<u64>> {
        self.check_alive()?;
        let result = self.handle_live_page_id_inner(handle);
        self.poison_on_fatal(result)
    }

    fn handle_live_page_id_inner(&self, handle: u64) -> Result<Option<u64>> {
        let root = if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        };
        if root == PAGE_ID_NONE {
            return Ok(None);
        }
        let mut cache = self.cache.borrow_mut();
        let entry = match self.handle_table.lookup(&mut cache, root, handle)? {
            Some(e) => e,
            None => return Ok(None),
        };
        if entry.flags == HandleFlags::Live {
            Ok(Some(entry.page_id))
        } else {
            Ok(None)
        }
    }

    /// The handle-table root page id of the active transaction's
    /// in-progress roots. Used by `defrag::defrag` to short-circuit
    /// the empty-database fast path.
    ///
    /// I39 (ISSUES.md, 2026-05-22): replaces a `pub fn current_roots()
    /// -> (u64, u64, u64)` tuple return that exposed three fields when
    /// only one was ever read. YAGNI: if a future caller wants
    /// `freemap_page` or `next_handle`, add a sibling accessor at that
    /// time rather than guessing the API shape now. `pub(crate)`
    /// because `defrag` is the sole intended caller (transaction
    /// module became `pub(crate)` in I35).
    pub(crate) fn current_handle_table_root_page(&self) -> u64 {
        self.current_roots.handle_table_page
    }

    pub fn is_active(&self) -> bool {
        self.active_txn
    }

    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.borrow_mut().set_cache_max_bytes(bytes)
    }

    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.borrow_mut().set_spillway_max_bytes(bytes)
    }

    pub fn set_drain_insertion(&mut self, policy: crate::DrainInsertion) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        // I40: PageCache::set_drain_insertion is now infallible (returns
        // `()`). The poison + active-txn checks above are the real
        // failure modes; we promote PageCache's `()` to `Ok(())` here.
        self.cache.borrow_mut().set_drain_insertion(policy);
        Ok(())
    }

    // --- Private helpers ---

    /// Release one slot from a data page (ISSUES.md R1). Decrements
    /// `current_live_slots[page_id]`; if the count reaches zero, the
    /// whole page becomes unreferenced and is pushed to
    /// `txn_freed_pages` so commit can return it to the freemap.
    /// Otherwise the slot becomes a tombstone: dead weight inside a
    /// still-live page, reclaimable only via defrag.
    ///
    /// If the page is somehow not tracked in `current_live_slots` (a
    /// bug; open-time scan should catch every live data page), this is
    /// a no-op — we prefer leaking to a spurious free.
    ///
    /// NOTE: a stray orphaned line "Lazily create a handle table root
    /// on first insert. A fresh database has" previously sat at the
    /// top of this doc block (an interleaved remnant of
    /// `ensure_handle_table`'s docstring); removed 2026-04-17 during
    /// the commenting pass. The counterpart ("root_handle_table_page
    /// == PAGE_ID_NONE; we don't materialize...") still sits above
    /// `ensure_handle_table` below — both belong together.
    fn release_data_slot(&mut self, page_id: u64) {
        let Some(count) = self.current_live_slots.get_mut(&page_id) else {
            return;
        };
        if *count > 0 {
            *count -= 1;
        }
        if *count == 0 {
            self.current_live_slots.remove(&page_id);
            // If this page is the active insert cursor, clear the
            // cursor — it's about to become free space, and we don't
            // want future inserts to pack into it and then find it
            // disappearing at commit time.
            if self.insert_cursor == Some(page_id) {
                self.insert_cursor = None;
            }
            self.txn_freed_pages.push(page_id);
        }
    }

    /// Lazily create a handle table root on first insert. A fresh
    /// database has `root_handle_table_page == PAGE_ID_NONE`; we don't
    /// materialize the root until there is a handle to put in it, so
    /// empty databases never pay for a handle-table page. No per-page
    /// rollback bookkeeping — the watermark rollback mechanism (I3)
    /// handles any page allocated here automatically.
    fn ensure_handle_table(&mut self) -> Result<()> {
        if self.current_roots.handle_table_page == PAGE_ID_NONE {
            let root = {
                let mut cache = self.cache.borrow_mut();
                self.handle_table.create_root(&mut cache)?
            };
            self.current_roots.handle_table_page = root;
        }
        Ok(())
    }

    /// Place a value in a data page and return (page_id, slot_index).
    ///
    /// Post-R1 packing model: the transaction maintains an "insert
    /// cursor" — a data page allocated earlier in THIS transaction
    /// that still has space — and packs successive small-value inserts
    /// into it until it fills. When the cursor is absent/full, a new
    /// page is allocated (via `allocate_data_page`, which prefers
    /// freemap reuse over file extension — R2) and becomes the new
    /// cursor. Packing is disabled while savepoints are active: the
    /// cursor is force-cleared by `savepoint()` and is NOT set when a
    /// new page is allocated inside a savepoint scope, so each insert
    /// under a savepoint gets its own page (the pre-R1 behavior). This
    /// keeps the per-savepoint snapshot cheap to restore.
    ///
    /// Checksum is stamped eagerly after every mutation so the page carries a
    /// valid internal checksum before any path could write it to the main
    /// file — either the `flush` `write_page` at commit, or a spill-then-drain
    /// write (an LRU-pressured dirty page is spilled to the spillway and later
    /// drained back out to the main file). The next cold-load
    /// (`page_cache::load_page`) verifies that checksum.
    ///
    /// Note: the spillway *transfer* does NOT rely on this. `rehydrate`
    /// verifies the spillway's own per-slot checksum (`spillway::slot_checksum`),
    /// never the page's internal bytes 8184..8192 — so a spilled page round-trips
    /// safely whether or not its internal checksum is current. The internal
    /// checksum only matters on the way to the main file.
    ///
    /// I78 proposes deferring this re-stamp to flush/drain time so a packed page
    /// is hashed once, not once per value (a large bulk-insert win on fast
    /// storage). It is deferred pending a benchmark; the difficulty is exactly
    /// the spill-then-drain path, which would then have to re-stamp before the
    /// main-file write. See ISSUES.md.
    ///
    /// Live-slot bookkeeping: every successful insert increments
    /// `current_live_slots[page_id]`. `delete`/`update` consult this
    /// map (via `release_data_slot`) to decide when a page is fully
    /// empty and can be freed back to the freemap on commit. The map
    /// is kept purely in memory — storing a slot count ON the data
    /// page would force a COW (and a handle-table rewrite for every
    /// entry pointing into it) on every delete.
    fn insert_into_data_page(&mut self, value: &[u8]) -> Result<(u64, u16)> {
        // Packing path: try to reuse the current cursor page if it
        // has room. The cursor only exists when savepoints are empty
        // (see savepoint_inner) so this branch implicitly respects
        // the "no packing under savepoints" rule.
        if let Some(cursor_page_id) = self.insert_cursor {
            let slot_option = {
                let mut cache = self.cache.borrow_mut();
                let buf = cache.get_mut(cursor_page_id)?;
                let result = DataPage::insert(buf, value);
                if result.is_some() {
                    page::stamp_checksum(buf);
                }
                result
            };
            if let Some(slot) = slot_option {
                *self.current_live_slots.entry(cursor_page_id).or_insert(0) += 1;
                return Ok((cursor_page_id, slot));
            }
            // Cursor page is full. Fall through to allocate a new one;
            // the new page becomes the new cursor.
        }

        // Allocate a fresh data page. Under active savepoints, the
        // cursor stays None (set below, then cleared by the savepoint
        // check in subsequent calls) so each insert gets its own page —
        // matching the pre-R1 "one value per page" behavior within
        // savepoint scopes, which is the price of keeping rollback_to
        // semantics simple.
        let page_id = self.allocate_data_page()?;
        let slot = {
            let mut cache = self.cache.borrow_mut();
            let buf = cache.get_mut(page_id)?;
            DataPage::init_page(buf);
            // I46 INVARIANT: DataPage::insert can only return None for
            // "no room"; the page was just init'd via DataPage::init_page
            // (empty), and the value's length was already checked against
            // MAX_INLINE_VALUE upstream (the overflow path catches anything
            // larger before we get here). If DataPage::insert ever grows
            // other failure modes, this expect needs to translate them to
            // typed errors instead of panicking.
            let slot = DataPage::insert(buf, value).expect("value fits in empty page");
            page::stamp_checksum(buf);
            slot
        };

        // Only install the new page as the cursor if we're outside any
        // savepoint scope. During a savepoint scope the cursor stays
        // None so packing is effectively disabled.
        if self.savepoints.is_empty() {
            self.insert_cursor = Some(page_id);
        }
        *self.current_live_slots.entry(page_id).or_insert(0) += 1;
        Ok((page_id, slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_io::PageIo;
    use tempfile::{NamedTempFile, TempDir};

    fn fresh_manager() -> TransactionManager {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        // Match Options::default()'s cache_max_bytes of 8 MiB (1024 pages)
        // so tests that intentionally allocate many pages in a single
        // transaction (e.g. the I3+I7 handle-table-growth test allocates
        // 510+) stay well under the strict cache cap. spillway_max_bytes=0
        // preserves the legacy CacheFull-at-cap behavior in tests.
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm = TransactionManager::create_new(cache, 2).unwrap();
        // Commit once so there's a real baseline to read/write against.
        tm.begin().unwrap();
        tm.commit().unwrap();
        tm
    }

    /// C1 invariant: after a commit, NO page reachable from `committed_roots`
    /// may be marked free in `committed_freemap`. A correct COW frees only
    /// superseded pages; freeing a still-referenced page (the textbook C1
    /// violation — e.g. `grow` freeing the reparented old root, or `update`
    /// freeing the OLD value before the new entry is installed) shows up here as
    /// a page that is both reachable and free. This is deterministic regardless
    /// of the freemap's lowest-id-first selection order, which makes black-box
    /// reopen tests unreliable for catching C1.
    ///
    /// Reachability covers BOTH the index spines (handle-table + membership
    /// outer/inner) AND the value storage every live handle points at (its
    /// inline data page or its full overflow chain). The value-storage half is
    /// essential: a spine-only walk cannot catch a value-page premature-free.
    ///
    /// PRECONDITION: call only between transactions (right after a commit), where
    /// `committed_roots == current_roots` and the in-memory `handle_table.depth`
    /// matches the committed root. `iter_live` descends with that live depth, so
    /// calling this mid-transaction after a grow would mis-descend.
    fn assert_no_reachable_page_is_free(tm: &TransactionManager) {
        let mut reachable = Vec::new();
        {
            let mut cache = tm.cache.borrow_mut();
            tm.handle_table
                .collect_page_ids(
                    &mut cache,
                    tm.committed_roots.handle_table_page,
                    &mut reachable,
                )
                .unwrap();
            tm.membership_index
                .collect_page_ids(
                    &mut cache,
                    tm.committed_roots.membership_index_page,
                    &mut reachable,
                )
                .unwrap();

            // Value storage reachable through each live HandleEntry.
            if tm.committed_roots.handle_table_page != PAGE_ID_NONE {
                let live = tm
                    .handle_table
                    .iter_live(&mut cache, tm.committed_roots.handle_table_page)
                    .unwrap();
                for (_handle, entry) in live {
                    match entry.flags {
                        HandleFlags::Live => reachable.push(entry.page_id),
                        HandleFlags::Overflow => {
                            // `Overflow::delete` is a read-only chain walk that
                            // returns the chain's page ids (it frees nothing
                            // itself); reuse it to enumerate the whole chain.
                            let chain = Overflow::delete(&mut cache, entry.page_id).unwrap();
                            reachable.extend(chain);
                        }
                        HandleFlags::Deleted => {}
                    }
                }
            }
        }
        for id in reachable {
            assert!(
                !FreeMap::is_free(&tm.committed_freemap, id),
                "page {id} is reachable from committed_roots but marked FREE in \
                 committed_freemap — a still-referenced page was freed (C1 violation)"
            );
        }
    }

    // COW page reclamation must never free a page still referenced by the
    // committed tree, even after the trees GROW (the reparenting paths). Forces
    // a handle-table grow (>510 handles) and a membership inner-tree grow
    // (>1021 members under one tag), then churns with reclamation, asserting the
    // C1 invariant after every commit.
    #[test]
    fn reclamation_never_frees_a_reachable_page_after_grow() {
        let mut tm = fresh_manager();
        let tag = 9u32;
        let mut handles = Vec::new();
        let mut v: u32 = 0;

        // Build >1021 tagged members in small batches (stay under the 1024-page
        // cache cap), forcing both trees to grow to depth >= 1.
        for _ in 0..12 {
            tm.begin().unwrap();
            for _ in 0..100 {
                let h = tm.allocate_tagged(&v.to_le_bytes(), tag).unwrap();
                handles.push(h);
                v += 1;
            }
            tm.commit().unwrap();
            assert_no_reachable_page_is_free(&tm);
        }
        assert!(
            handles.len() > 1021,
            "workload must exceed one membership leaf to force an inner grow"
        );

        // Churn with reclamation across committed transactions: update relocates
        // the value and COWs the handle-table spine (freeing the old spine);
        // set_client_byte COWs only the leaf. Batched per ~100 handles so a
        // single transaction's dirty COW pages stay under the cache cap (within
        // a txn, this-txn frees are not yet reusable). Re-check after each commit.
        for round in 0..12u32 {
            for (chunk_idx, chunk) in handles.chunks(100).enumerate() {
                tm.begin().unwrap();
                for (j, h) in chunk.iter().enumerate() {
                    if (round as usize + chunk_idx + j) % 2 == 0 {
                        tm.set_client_byte(*h, round as u8).unwrap();
                    } else {
                        tm.update(*h, &round.to_le_bytes()).unwrap();
                    }
                }
                tm.commit().unwrap();
                assert_no_reachable_page_is_free(&tm);
            }
        }

        // Every handle still carries its tag and is enumerable after the churn.
        for h in &handles {
            assert_eq!(tm.tag(*h).unwrap(), tag);
        }
        assert_eq!(tm.handles_with_tag(tag).unwrap().len(), handles.len());
    }

    #[test]
    fn tagged_membership_survives_rolled_back_outer_grow() {
        let mut tm = fresh_manager();
        // Commit a small tag; the outer (tag-keyed) tree stays depth 0.
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"keep", 3).unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(3).unwrap(), vec![h]);
        // New txn: a tag >= 1021 forces the outer tree to grow (depth 0 -> 1).
        // Roll back. The grown root is discarded and current_roots snaps back to
        // the depth-0 committed root; outer_depth must be restored to match.
        tm.begin().unwrap();
        let _ = tm.allocate_tagged(b"discard", 5000).unwrap();
        tm.rollback().unwrap();
        // The committed small tag must still be readable (was silently lost before the fix).
        assert_eq!(
            tm.handles_with_tag(3).unwrap(),
            vec![h],
            "rolled-back outer grow corrupted committed membership"
        );
        assert_eq!(tm.tag(h).unwrap(), 3);
        // The discarded tag is gone.
        assert_eq!(tm.handles_with_tag(5000).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn tagged_membership_survives_rollback_to_savepoint() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"keep", 7).unwrap();
        tm.savepoint("sp").unwrap();
        // Grow the outer tree past depth 0 inside the savepoint, then roll back to it.
        let _ = tm.allocate_tagged(b"discard", 6000).unwrap();
        tm.rollback_to("sp").unwrap();
        // Still inside the active txn: the pre-savepoint tag must remain readable.
        assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
        assert_eq!(tm.handles_with_tag(6000).unwrap(), Vec::<u64>::new());
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
    }

    // Regression test for ISSUES.md I1. Once the manager is poisoned,
    // every public entry point must return ChiselError::Poisoned rather
    // than attempting the operation. This is the core invariant of the
    // poison model — the test asserts it for each method independently
    // so a future refactor that forgets to wrap a new entry point will
    // fail loudly.
    #[test]
    fn poisoned_manager_rejects_every_public_entry_point() {
        let mut tm = fresh_manager();
        tm.force_poison_for_test();
        assert!(tm.is_poisoned());

        assert!(matches!(tm.begin(), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.commit(), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.rollback(), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.savepoint("x"), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.rollback_to("x"), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.release("x"), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.allocate(b"v"), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.read(0), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.update(0, b"v"), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.delete(0), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.handles(), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.file_page_count(), Err(ChiselError::Poisoned)));
        assert!(matches!(
            tm.allocate_tagged(b"v", 1),
            Err(ChiselError::Poisoned)
        ));
        assert!(matches!(tm.tag(0), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.handles_with_tag(1), Err(ChiselError::Poisoned)));
        assert!(matches!(tm.delete_tagged(0, 1), Err(ChiselError::Poisoned)));
        assert!(matches!(
            tm.delete_with_tag(1, 10),
            Err(ChiselError::Poisoned)
        ));
        assert!(matches!(tm.client_byte(0), Err(ChiselError::Poisoned)));
        assert!(matches!(
            tm.set_client_byte(0, 1),
            Err(ChiselError::Poisoned)
        ));
    }

    // A fatal error observed during a normal operation must also poison
    // the manager, not just commit errors. This exercises the
    // `poison_on_fatal` branch of the wrapper pattern.
    #[test]
    fn fatal_error_outside_commit_also_poisons() {
        let tm = fresh_manager();
        // Inject a fatal error via the helper's test hook: force the
        // flag, then confirm a subsequent operation sees it through a
        // shared reference. (A genuine fault-injection harness is out
        // of scope; we exercise the machinery deterministically.)
        tm.force_poison_for_test();
        let err = tm.read(0).unwrap_err();
        assert!(matches!(err, ChiselError::Poisoned));
    }

    // Regression test for ISSUES.md I3 + I7. A transaction that forces
    // handle-table growth allocates many pages (the data pages for each
    // value, the handle-table leaves, the COW spine clones, and the
    // new interior root from grow()). After rollback, every one of those
    // pages must be gone — both from the in-memory cache AND from the
    // file itself.
    //
    // Pre-I7, the old per-page dirty list missed intermediate COW pages.
    // Pre-I3, rollback only discarded cache entries without truncating
    // the file, so the extended pages leaked permanently. This test
    // exercises both conditions in one shot by asserting the
    // `next_page_id` watermark and the cache page-count return to their
    // pre-transaction values after rollback.
    #[test]
    fn rollback_truncates_cache_and_file_to_pre_txn_watermark() {
        let mut tm = fresh_manager();
        let pre_watermark = tm.cache.borrow().next_page_id();
        let pre_file_pages = tm.cache.borrow_mut().file_page_count().unwrap();

        tm.begin().unwrap();
        tm.allocate(b"seed").unwrap();
        // Force handle-table growth by crossing the 510-entry leaf boundary.
        for _ in 0..510 {
            tm.allocate(b"f").unwrap();
        }
        // Sanity: the transaction must have extended the cache past the
        // pre-transaction watermark. Otherwise the test below is vacuous.
        let mid_watermark = tm.cache.borrow().next_page_id();
        assert!(
            mid_watermark > pre_watermark + 510,
            "expected the transaction to allocate many pages beyond {pre_watermark}, got {mid_watermark}"
        );

        tm.rollback().unwrap();

        let post_watermark = tm.cache.borrow().next_page_id();
        let post_file_pages = tm.cache.borrow_mut().file_page_count().unwrap();
        assert_eq!(
            post_watermark, pre_watermark,
            "rollback must rewind next_page_id to the pre-transaction watermark"
        );
        assert_eq!(
            post_file_pages, pre_file_pages,
            "rollback must truncate the file back to its pre-transaction page count"
        );
    }

    // rollback_to(name) must truncate cache+file to the savepoint's
    // watermark, discarding every page allocated after the savepoint
    // while preserving those allocated before it. This is the per-
    // savepoint analogue of the full-rollback test above.
    #[test]
    fn rollback_to_savepoint_truncates_to_savepoint_watermark() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h1 = tm.allocate(b"before").unwrap();
        tm.savepoint("sp").unwrap();
        let savepoint_watermark = tm.cache.borrow().next_page_id();
        let _h2 = tm.allocate(b"after").unwrap();
        let _h3 = tm.allocate(b"after-2").unwrap();
        assert!(tm.cache.borrow().next_page_id() > savepoint_watermark);

        tm.rollback_to("sp").unwrap();
        assert_eq!(
            tm.cache.borrow().next_page_id(),
            savepoint_watermark,
            "rollback_to must rewind to the savepoint's watermark"
        );
        // The pre-savepoint handle must still be readable.
        assert_eq!(tm.read(h1).unwrap(), b"before");
        tm.commit().unwrap();
    }

    // An operational error (NoActiveTransaction, DuplicateSavepoint,
    // InvalidHandle, etc.) must NOT poison. These are caller mistakes,
    // not integrity failures — the manager stays usable.
    #[test]
    fn operational_error_does_not_poison() {
        let mut tm = fresh_manager();

        // NoActiveTransaction from commit — operational.
        assert!(matches!(tm.commit(), Err(ChiselError::NoActiveTransaction)));
        assert!(!tm.is_poisoned());

        // NoActiveTransaction from allocate — operational.
        assert!(matches!(
            tm.allocate(b"v"),
            Err(ChiselError::NoActiveTransaction)
        ));
        assert!(!tm.is_poisoned());

        // DuplicateSavepoint — operational.
        tm.begin().unwrap();
        tm.savepoint("a").unwrap();
        assert!(matches!(
            tm.savepoint("a"),
            Err(ChiselError::DuplicateSavepoint(_))
        ));
        assert!(!tm.is_poisoned());

        // InvalidHandle from read — operational.
        assert!(matches!(tm.read(999), Err(ChiselError::InvalidHandle(_))));
        assert!(!tm.is_poisoned());
    }

    // Regression test for ISSUES.md I18. Inside commit_inner's
    // persist_freemap step, the new-freemap-page allocation must never
    // return an id that is still referenced by the currently-committed
    // on-disk superblock. The two sources of such at-risk ids are:
    //
    //   (1) `committed_roots.freemap_page` itself — the current
    //       on-disk freemap page; overwriting it mid-commit destroys
    //       the committed freemap snapshot.
    //   (2) Any id in `txn_freed_pages` — pages that held handle
    //       values reachable through the committed handle table;
    //       overwriting any of them mid-commit destroys a
    //       committed value.
    //
    // A crash in the window between `cache.flush()` and the superblock
    // fsync would then leave the last-durable superblock pointing at
    // a page whose bytes no longer match what it committed to —
    // breaking the core shadow-paging invariant. The fix defers the
    // merge of both at-risk sets into `current_freemap` until AFTER
    // the new-freemap-page allocate has run, so `FreeMap::allocate_first`
    // cannot return any of them during the vulnerable window.
    #[test]
    fn persist_freemap_does_not_reuse_committed_live_pages() {
        let mut tm = fresh_manager();

        // Commit 1: seed a non-trivial committed state. We need
        // persist_freemap to actually materialize a freemap page,
        // not take the early-exit path. That requires `txn_freed_pages`
        // to be non-empty at commit, which means freeing at least one
        // WHOLE data page — R1 slot packing keeps multi-slot data
        // pages live even after individual deletes. The simplest way
        // to guarantee whole-page frees is to use overflow-sized
        // values (> MAX_INLINE_VALUE): each gets its own overflow
        // chain, and delete releases every page in the chain into
        // txn_freed_pages via Overflow::delete.
        let big: Vec<u8> = vec![0xAB; MAX_INLINE_VALUE + 32];
        tm.begin().unwrap();
        let h_throwaway = tm.allocate(&big).unwrap();
        let h_live_a = tm.allocate(&big).unwrap();
        let h_live_b = tm.allocate(&big).unwrap();
        let h_live_c = tm.allocate(&big).unwrap();
        tm.delete(h_throwaway).unwrap();
        tm.commit().unwrap();

        let committed_freemap_page = tm.committed_roots.freemap_page;
        assert_ne!(
            committed_freemap_page, PAGE_ID_NONE,
            "test precondition: commit 1 should have established a freemap page"
        );

        // Commit 2: delete two more handles. release_data_slot pushes
        // their data pages into `txn_freed_pages`; those pages are
        // still referenced by commit 1's (currently-on-disk)
        // superblock at the moment commit_inner runs persist_freemap.
        tm.begin().unwrap();
        tm.delete(h_live_a).unwrap();
        tm.delete(h_live_b).unwrap();

        let frozen_txn_freed: Vec<u64> = tm.txn_freed_pages.clone();
        assert!(
            !frozen_txn_freed.is_empty(),
            "test precondition: deletes should have populated txn_freed_pages"
        );

        tm.commit().unwrap();

        // The at-risk set: anything that was still live under the
        // prior committed superblock at the moment persist_freemap
        // started allocating.
        let mut still_live_pre_commit = frozen_txn_freed.clone();
        still_live_pre_commit.push(committed_freemap_page);

        let new_freemap_page = tm.committed_roots.freemap_page;
        assert!(
            !still_live_pre_commit.contains(&new_freemap_page),
            "I18: persist_freemap allocated the new freemap page at an id \
             that was still referenced by the last-durable superblock. \
             new_freemap_page={new_freemap_page}, \
             committed_freemap_page was {committed_freemap_page}, \
             txn_freed_pages at commit time = {frozen_txn_freed:?}"
        );

        // Sanity: the un-deleted handle still reads back correctly
        // (rules out a subtler corruption that survived the invariant
        // check but poisoned the data plane).
        assert_eq!(tm.read(h_live_c).unwrap(), big);
    }

    // Regression test for ISSUES.md I27. `savepoint_inner` moves
    // `txn_freed_pages` into the savepoint record via `std::mem::take`.
    // If commit runs with savepoints still on the stack, the pre-fix
    // `commit_inner` just called `self.savepoints.clear()` at step 5
    // and those `freed_pages` lists were dropped — never reaching the
    // freemap. `persist_freemap` iterates only `self.txn_freed_pages`.
    // The post-fix merge in commit_inner flattens every active
    // savepoint's `freed_pages` back into `txn_freed_pages` before
    // `persist_freemap` runs, so every page freed anywhere in the
    // transaction reaches the committed freemap.
    //
    // Observable via `FreeMap::is_free(&committed_freemap, id)` — the
    // freemap's public predicate avoids any reliance on subsequent
    // allocator reuse (which depends on `savepoints.is_empty()` too and
    // would muddy the test).
    #[test]
    fn commit_with_active_savepoint_returns_freed_pages_to_freemap() {
        let mut tm = fresh_manager();

        // Seed: enough overflow-sized handles that deleting them
        // produces genuine page frees (R1 slot-packing would otherwise
        // keep multi-slot data pages live).
        let big: Vec<u8> = vec![0xCD; MAX_INLINE_VALUE + 32];
        tm.begin().unwrap();
        let h_a = tm.allocate(&big).unwrap();
        let h_b = tm.allocate(&big).unwrap();
        let h_keepalive = tm.allocate(&big).unwrap();
        tm.commit().unwrap();

        // The leak pattern: delete first, THEN open a savepoint. The
        // savepoint captures the accumulated `txn_freed_pages`, leaving
        // the outer `txn_freed_pages` empty for the rest of the txn.
        tm.begin().unwrap();
        tm.delete(h_a).unwrap();
        tm.delete(h_b).unwrap();
        let frozen_txn_freed: Vec<u64> = tm.txn_freed_pages.clone();
        assert!(
            !frozen_txn_freed.is_empty(),
            "test precondition: overflow-sized deletes should free at least one page"
        );

        tm.savepoint("s").unwrap();
        assert!(
            tm.txn_freed_pages.is_empty(),
            "savepoint_inner should have moved txn_freed_pages into the savepoint"
        );

        // Commit WITHOUT releasing the savepoint. Pre-fix this silently
        // drops savepoint.freed_pages on `savepoints.clear()`; post-fix
        // commit_inner merges them into txn_freed_pages first.
        tm.commit().unwrap();

        for id in &frozen_txn_freed {
            assert!(
                FreeMap::is_free(&tm.committed_freemap, *id),
                "I27: freed page {id} should be marked free in committed_freemap \
                 after commit-with-active-savepoint; frozen_txn_freed={frozen_txn_freed:?}"
            );
        }

        // Sanity: the surviving handle still reads back (rules out a
        // wider corruption that happens to also trip the is_free check).
        assert_eq!(tm.read(h_keepalive).unwrap(), big);
    }

    // Regression test for ISSUES.md I28. I19 introduced `CacheFull` as
    // an **operational** error (documented as "commit or rollback to
    // recover"), but `commit_inner` runs `persist_freemap` BEFORE
    // `cache.flush()` — and `persist_freemap` itself calls
    // `allocate_data_page`, which may trip `maybe_evict`'s ceiling
    // check when every existing cache entry is dirty. Pre-fix the
    // resulting `CacheFull` propagated out of commit_inner and
    // commit()'s poison wrapper poisoned the manager unconditionally.
    // The recovery advice ("commit to flush") became impossible to
    // follow because commit itself failed.
    //
    // Post-fix: commit drains the cache BEFORE persist_freemap, so the
    // cap is always reachable via eviction when persist_freemap
    // itself allocates. CacheFull cannot arise on the commit path.
    //
    // Setup note: we deliberately use a small `max_pages` so the
    // strict cap is cheap to saturate with a few allocations.
    // spillway_max_bytes=0 keeps CacheFull reachable (no spillway
    // escape hatch), matching the pre-spillway path this test exercises.
    #[test]
    fn commit_does_not_poison_when_cache_is_at_strict_cap() {
        // I66 (ISSUES.md, 2026-05-22): TempDir for RAII cleanup —
        // replaces the pre-I66 NamedTempFile + std::mem::forget(file)
        // pattern that leaked the temp path on every test run.
        let _dir = TempDir::new().unwrap();
        let db_path = _dir.path().join("test.chisel");
        let io = PageIo::open(&db_path, false).unwrap();
        // max_pages=16 — big enough for baseline operations (handle-table
        // root + superblocks + freemap) to coexist, small enough that a
        // handful of big allocations saturate the strict cap quickly.
        // spillway_max_bytes=0 means CacheFull fires at max_pages itself.
        let cache = PageCache::new(
            io,
            16 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm = TransactionManager::create_new(cache, 2).unwrap();
        tm.begin().unwrap();
        tm.commit().unwrap();

        // Seed one handle so the victim transaction can produce a
        // non-empty `txn_freed_pages` via delete. Without any frees AND
        // with `current_freemap == committed_freemap`, `persist_freemap`
        // takes its early-exit path and never allocates — which would
        // mean it also cannot trip CacheFull, and the test would fail
        // to reproduce the bug.
        let big: Vec<u8> = vec![0x99; MAX_INLINE_VALUE + 32];
        tm.begin().unwrap();
        let victim = tm.allocate(&big).unwrap();
        tm.commit().unwrap();

        // Victim transaction: delete to populate txn_freed_pages, then
        // allocate until the cache saturates at the strict cap.
        // CacheFull from an allocate() is operational — we catch it and
        // proceed to commit, which is what we actually want to stress.
        tm.begin().unwrap();
        tm.delete(victim).unwrap();
        let mut saturated = false;
        for _ in 0..200 {
            match tm.allocate(&big) {
                Ok(_) => continue,
                Err(ChiselError::CacheFull { .. }) => {
                    saturated = true;
                    break;
                }
                Err(e) => panic!("unexpected error during cache-fill setup: {e:?}"),
            }
        }
        assert!(
            saturated,
            "test precondition: cache did not reach CacheFull in 200 allocations"
        );
        assert!(
            !tm.is_poisoned(),
            "precondition: CacheFull from allocate() is operational and must not poison"
        );

        // The actual I28 check. Pre-fix, `persist_freemap`'s internal
        // `allocate_data_page` trips the ceiling and propagates
        // CacheFull out of commit_inner; commit()'s poison wrapper
        // then sets the poison flag. Post-fix commit drains first.
        let result = tm.commit();
        assert!(
            result.is_ok(),
            "I28: commit over a saturated cache should succeed; got {result:?}"
        );
        assert!(
            !tm.is_poisoned(),
            "I28: CacheFull during commit must not poison — it's operational by design"
        );
    }

    // ===================================================================
    // BUG#2 (2026-06-16 deepdive): forward/reverse tag-map atomic staging.
    //
    // `allocate_tagged` maintains two maps that must stay in lockstep: the
    // FORWARD map (each chunk's `HandleEntry.tag`, in the handle table) and
    // the REVERSE map (tag -> handles, in the membership index, powering
    // `handles_with_tag`/delete-by-tag). Before the fix the two roots were
    // installed sequentially, so a NON-FATAL `CacheFull`/`SpillwayFull`
    // striking between them committed a half-update:
    //   * allocate: the forward map gained the tag but the reverse did not
    //     (a tagged chunk invisible to `handles_with_tag`);
    //   * delete:   the tombstone landed but the reverse entry stayed (a
    //     stale member that later escalates to a FATAL CorruptPage when
    //     surfaced and acted upon).
    // Because those errors are non-fatal they do NOT poison the manager, so
    // the half-update survives to `commit()` and onto disk.
    //
    // Atomic staging computes BOTH candidate roots in a fallible prepare
    // phase and installs them together in an infallible phase, so a mid-op
    // failure is a complete no-op. The `fail_next_membership_op` hook fires
    // a simulated CacheFull at exactly the reverse-map step — the precise
    // divergence window — so these are deterministic, not timing-dependent.

    #[test]
    fn allocate_membership_failure_leaves_maps_consistent() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        // The id the about-to-fail allocate will (try to) use.
        let ghost = tm.current_roots.next_handle;

        tm.fail_next_membership_op.set(true);
        let err = tm.allocate_tagged(b"payload", 7).unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { .. }),
            "expected the injected CacheFull, got {err:?}"
        );
        assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

        // The forward (tag) and reverse (membership) maps MUST agree. Pre-fix
        // the forward map carried tag 7 for `ghost` while the reverse index
        // did not — a committed-out-of-sync divergence.
        let in_reverse = tm.handles_with_tag(7).unwrap().contains(&ghost);
        let in_forward = matches!(tm.tag(ghost), Ok(7));
        assert_eq!(
            in_forward, in_reverse,
            "forward/reverse tag maps diverged after a failed tagged allocate"
        );

        // Atomic staging makes the failed allocate a COMPLETE no-op: neither
        // map changed and the handle id was not even burned.
        assert!(
            !in_forward,
            "failed allocate must not install the forward entry"
        );
        assert_eq!(
            tm.current_roots.next_handle, ghost,
            "failed allocate must not burn the handle id"
        );

        // ...and the no-op extends to the R1 packing bookkeeping: the inline
        // value's data slot was released, so no phantom live-slot count or ghost
        // insert cursor survives to skew later packing / defrag density / page
        // reclamation. (Pre-fix this leaked `{page: 1}` and `Some(page)`.)
        assert!(
            tm.current_live_slots.is_empty(),
            "failed allocate left a phantom live-slot count: {:?}",
            tm.current_live_slots
        );
        assert_eq!(
            tm.insert_cursor, None,
            "failed allocate left a ghost insert cursor"
        );

        // The manager is still fully usable: a disarmed retry reuses the same
        // id and is consistent across BOTH maps, in-session and after commit.
        let h = tm.allocate_tagged(b"payload", 7).unwrap();
        assert_eq!(h, ghost, "retry should reuse the un-burned handle id");
        assert_eq!(tm.tag(h).unwrap(), 7);
        assert!(tm.handles_with_tag(7).unwrap().contains(&h));
        tm.commit().unwrap();
        assert!(tm.handles_with_tag(7).unwrap().contains(&h));
        // C1: no page reachable from committed_roots may be free in the
        // committed freemap — pins that the dropped prepare freed-lists never
        // queued a still-referenced page.
        assert_no_reachable_page_is_free(&tm);
    }

    // The FORWARD-step counterpart: a non-fatal CacheFull during the
    // handle-table insert (the step that eagerly bumps the descent depth via
    // HandleTable::grow, and on a fresh DB lazily materializes the root). The
    // prepare-abort must restore BOTH the depth and the handle-table root
    // pointer and release the inline slot — a true no-op.
    #[test]
    fn allocate_handle_table_failure_leaves_maps_consistent() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        // Fresh DB: the handle table has never been materialized.
        let saved_root = tm.current_roots.handle_table_page;
        assert_eq!(saved_root, PAGE_ID_NONE, "precondition: empty handle table");
        let ghost = tm.current_roots.next_handle;

        tm.fail_next_handle_table_op.set(true);
        let err = tm.allocate_tagged(b"payload", 7).unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { .. }),
            "expected the injected CacheFull, got {err:?}"
        );
        assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

        // Complete no-op: the lazily-created root is reverted (back to
        // PAGE_ID_NONE), the handle id is not burned, neither map has content,
        // and the R1 packing state is clean.
        assert_eq!(
            tm.current_roots.handle_table_page, saved_root,
            "failed forward step left a lazily-materialized handle-table root installed"
        );
        assert_eq!(tm.current_roots.next_handle, ghost);
        assert!(tm.tag(ghost).is_err(), "no forward entry should exist");
        assert!(tm.handles_with_tag(7).unwrap().is_empty());
        assert!(
            tm.current_live_slots.is_empty(),
            "failed forward step left a phantom live-slot count: {:?}",
            tm.current_live_slots
        );
        assert_eq!(tm.insert_cursor, None);

        // Disarmed retry succeeds and is consistent across both maps.
        let h = tm.allocate_tagged(b"payload", 7).unwrap();
        assert_eq!(h, ghost, "retry should reuse the un-burned handle id");
        assert_eq!(tm.tag(h).unwrap(), 7);
        assert!(tm.handles_with_tag(7).unwrap().contains(&h));
        tm.commit().unwrap();
        assert_no_reachable_page_is_free(&tm);
    }

    #[test]
    fn delete_membership_failure_leaves_maps_consistent() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"payload", 7).unwrap();
        tm.commit().unwrap();

        tm.begin().unwrap();
        tm.fail_next_membership_op.set(true);
        let err = tm.delete(h).unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { .. }),
            "expected the injected CacheFull, got {err:?}"
        );
        assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

        // Pre-fix the tombstone (forward) was installed but the reverse entry
        // stayed, so `read(h)` failed while `handles_with_tag` still listed
        // `h` — exactly the divergence that later escalates to CorruptPage.
        let in_reverse = tm.handles_with_tag(7).unwrap().contains(&h);
        let still_live = tm.read(h).is_ok();
        assert_eq!(
            still_live, in_reverse,
            "forward/reverse tag maps diverged after a failed tagged delete"
        );
        // Atomic staging makes the failed delete a COMPLETE no-op.
        assert!(still_live, "failed delete must not install the tombstone");
        assert!(in_reverse, "failed delete must not drop the reverse entry");

        // A disarmed retry succeeds and removes `h` from BOTH maps.
        tm.delete(h).unwrap();
        assert!(tm.read(h).is_err());
        assert!(!tm.handles_with_tag(7).unwrap().contains(&h));
        tm.commit().unwrap();
        assert!(!tm.handles_with_tag(7).unwrap().contains(&h));
        assert_no_reachable_page_is_free(&tm);
    }

    // Delete's durability guard (delete is the more dangerous direction: a
    // stale reverse member for a tombstoned handle later escalates to a FATAL
    // CorruptPage). Fail the reverse-map step, COMMIT, then REOPEN: the failed
    // delete must have committed NOTHING — `h` is still live AND still a tagged
    // member on disk. Pre-fix the tombstone committed while the reverse member
    // stayed, so the reopened DB would read `h` as deleted yet still list it.
    #[test]
    fn delete_membership_failure_survives_reopen_consistently() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bug2-del.chisel");

        let open = |create: bool| -> TransactionManager {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            if create {
                TransactionManager::create_new(cache, 2).unwrap()
            } else {
                TransactionManager::open_existing(cache).unwrap()
            }
        };

        let h = {
            let mut tm = open(true);
            tm.begin().unwrap();
            let h = tm.allocate_tagged(b"payload", 9).unwrap();
            tm.commit().unwrap();

            tm.begin().unwrap();
            tm.fail_next_membership_op.set(true);
            assert!(matches!(
                tm.delete(h).unwrap_err(),
                ChiselError::CacheFull { .. }
            ));
            // Commit the post-failure state: pre-fix this durably tombstones the
            // forward map while the reverse keeps `h`; post-fix it is a no-op.
            tm.commit().unwrap();
            h
        };

        // Reopen: the forward (read) and reverse (handles_with_tag) views must
        // agree, and since the delete was a no-op both must still see `h`.
        let tm = open(false);
        let still_live = tm.read(h).is_ok();
        let in_reverse = tm.handles_with_tag(9).unwrap().contains(&h);
        assert_eq!(
            still_live, in_reverse,
            "reopened forward/reverse views diverged for the failed delete's handle"
        );
        assert!(
            still_live && in_reverse,
            "a failed tagged delete must commit nothing: h should survive in both maps \
             (read ok = {still_live}, in reverse index = {in_reverse})"
        );
    }

    // CRITICAL durable-corruption regression (surfaced by the BUG#2 adversarial
    // review): `update_inner` must not free the OLD value's pages before the new
    // entry is durably installed. A non-fatal CacheFull at the new-value-write
    // step — which pre-fix runs AFTER the old free — left the committed handle
    // still pointing at pages already queued for reclamation, so commit freed a
    // reachable page and a later reuse silently corrupted the live value.
    #[test]
    fn update_value_write_failure_does_not_free_old_value_pages() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        // Overflow-sized old value, so the whole chain is at stake.
        let big = vec![0xABu8; MAX_INLINE_VALUE * 3];
        let h = tm.allocate(&big).unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.read(h).unwrap(), big, "precondition: old value readable");

        tm.begin().unwrap();
        tm.fail_next_update_value_write.set(true);
        let err = tm.update(h, b"replacement").unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { .. }),
            "expected the injected CacheFull, got {err:?}"
        );
        assert!(!tm.is_poisoned(), "a non-fatal CacheFull must not poison");

        // The failed update is a no-op in-session: the old value is intact.
        assert_eq!(
            tm.read(h).unwrap(),
            big,
            "failed update lost the old value in-session"
        );

        // Commit the post-failure state, then assert C1: no page the committed
        // handle still references may be free. Pre-fix the old overflow chain was
        // queued into txn_freed_pages and freed here while `h` still points at it.
        tm.commit().unwrap();
        assert_no_reachable_page_is_free(&tm);
        assert_eq!(
            tm.read(h).unwrap(),
            big,
            "failed update lost the old value after commit"
        );

        // End-to-end: churn allocations to force the freemap to hand out any
        // wrongly-freed pages, then confirm the old value survived. Pre-fix the
        // reachable-but-free chain pages would be reused and overwritten, turning
        // read(h) into a CorruptPage / wrong bytes.
        tm.begin().unwrap();
        for i in 0..40u8 {
            tm.allocate(&[i; 64]).unwrap();
        }
        tm.commit().unwrap();
        assert_eq!(
            tm.read(h).unwrap(),
            big,
            "old value corrupted after its prematurely-freed pages were reused"
        );
    }

    // Same guarantee for an INLINE old value (the Live old-release path). When
    // the old page held only this value, the pre-fix code released it to the
    // freemap before the new write, so a failed update committed a
    // reachable-but-free data page just like the overflow case.
    #[test]
    fn update_value_write_failure_does_not_free_old_inline_page() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let old = b"inline-original-value";
        let h = tm.allocate(old).unwrap();
        tm.commit().unwrap();

        tm.begin().unwrap();
        tm.fail_next_update_value_write.set(true);
        assert!(matches!(
            tm.update(h, b"replacement").unwrap_err(),
            ChiselError::CacheFull { .. }
        ));
        assert!(!tm.is_poisoned());
        assert_eq!(
            tm.read(h).unwrap(),
            old,
            "failed update lost the inline value"
        );

        tm.commit().unwrap();
        assert_no_reachable_page_is_free(&tm);
        assert_eq!(tm.read(h).unwrap(), old);

        // Force reuse, then confirm the old inline value survived.
        tm.begin().unwrap();
        for i in 0..40u8 {
            tm.allocate(&[i; 64]).unwrap();
        }
        tm.commit().unwrap();
        assert_eq!(
            tm.read(h).unwrap(),
            old,
            "old inline value corrupted after a prematurely-freed page was reused"
        );
    }

    // Highest-value coverage for the post-write prepare-unwind contract: the new
    // value is ALREADY written when the handle-table install fails. The fix must
    // (a) leave the OLD location referenced (never freed) and (b) release the
    // just-written NEW inline slot so no phantom live-slot / ghost cursor
    // survives. Driven via the shared fail_next_handle_table_op hook, which
    // handle_table_insert_candidate honors for both allocate and update. (The
    // old-overflow-walk failure exit runs the identical new-inline-slot release,
    // so this test covers that contract too.)
    #[test]
    fn update_handle_table_failure_preserves_old_value_and_releases_new_slot() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let old = b"inline-original-value";
        let h = tm.allocate(old).unwrap();
        tm.commit().unwrap();

        tm.begin().unwrap();
        tm.fail_next_handle_table_op.set(true);
        let err = tm.update(h, b"small-new").unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { .. }),
            "expected the injected CacheFull, got {err:?}"
        );
        assert!(!tm.is_poisoned());

        // (a) The old value is untouched — the old location was never freed.
        assert_eq!(tm.read(h).unwrap(), old, "failed update lost the old value");
        // (b) The new value's inline slot was released: the committed baseline
        // had exactly one live slot (for `old`), and the failed update must
        // leave precisely that — no phantom count for the abandoned new value.
        assert_eq!(
            tm.current_live_slots.values().sum::<u32>(),
            1,
            "failed update left a phantom live-slot: {:?}",
            tm.current_live_slots
        );

        // Durability: commit, assert C1, force reuse, re-read the old value.
        tm.commit().unwrap();
        assert_no_reachable_page_is_free(&tm);
        tm.begin().unwrap();
        for i in 0..40u8 {
            tm.allocate(&[i; 64]).unwrap();
        }
        tm.commit().unwrap();
        assert_eq!(
            tm.read(h).unwrap(),
            old,
            "old value corrupted after reuse following a failed update"
        );
    }

    // The headline durability guard: a failed tagged allocate, then COMMIT,
    // then REOPEN from the last durable superblock. Pre-fix this persisted a
    // forward-map ghost (a committed `HandleEntry.tag` with no reverse-index
    // member); post-fix the failed allocate committed nothing.
    #[test]
    fn allocate_membership_failure_survives_reopen_consistently() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bug2.chisel");

        let open = |create: bool| -> TransactionManager {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            if create {
                TransactionManager::create_new(cache, 2).unwrap()
            } else {
                TransactionManager::open_existing(cache).unwrap()
            }
        };

        let ghost = {
            let mut tm = open(true);
            tm.begin().unwrap();
            tm.commit().unwrap();

            tm.begin().unwrap();
            let ghost = tm.current_roots.next_handle;
            tm.fail_next_membership_op.set(true);
            assert!(matches!(
                tm.allocate_tagged(b"payload", 9).unwrap_err(),
                ChiselError::CacheFull { .. }
            ));
            // Commit the post-failure state: pre-fix this makes the forward-map
            // ghost durable; post-fix it commits a clean no-op.
            tm.commit().unwrap();
            ghost
        };

        // Reopen and verify the FORWARD map carries no ghost tag-9 entry that
        // the REVERSE map is missing. `handles_with_tag(9)` reads the reverse
        // map (empty under both code paths); the discriminating check is the
        // forward map via `tag(ghost)`.
        let tm = open(false);
        let forward = tm.tag(ghost);
        assert!(
            forward.is_err() || forward.as_ref().unwrap() != &9,
            "reopened forward map has a ghost tag-9 entry (tag({ghost}) = {forward:?}) \
             with no matching reverse-index member — the maps committed out of sync"
        );
        assert!(
            tm.handles_with_tag(9).unwrap().is_empty(),
            "tag 9 should have no committed members"
        );
    }

    // I29: the open-time format-version gate compares MAJOR only, not
    // the full u32. Same-major files (regardless of minor) open cleanly;
    // different-major files fail fast with UnsupportedFormatVersion.
    // This encodes the "sacred within a major version" promise from the
    // README: any file written by an N.x binary is readable by every
    // other N.x binary, because minor bumps can only add fields in the
    // superblock's reserved region (they never break backward reads).
    //
    // We exercise both halves in one test because they share setup
    // (fresh-DB file → patch slots → reopen). Patching both superblock
    // slots in lockstep matters because Superblock::select picks the
    // highest-counter valid slot; if only slot 0 were patched, slot 1
    // (with the unmodified version) would win on some commits.
    #[test]
    fn format_version_gate_is_major_only() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        // Step 0: create a fresh database so there's something on disk
        // to patch. The default superblock_count of 2 means we need to
        // patch pages 0 AND 1.
        {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let _ = TransactionManager::create_new(cache, 2).unwrap();
            // drop() releases the flock so the test can read+write the
            // file directly below.
        }

        // Helper: patch every superblock slot to the given packed
        // format_version and re-stamp the trailing checksum so the
        // deserialize path still accepts it as a valid slot.
        let patch_all_slots = |version: u32, slot_count: usize| {
            let mut bytes = std::fs::read(&path).unwrap();
            for slot in 0..slot_count {
                let offset = slot * PAGE_SIZE;
                bytes[offset + 4..offset + 8].copy_from_slice(&version.to_le_bytes());
                let page_arr: &mut [u8; PAGE_SIZE] =
                    (&mut bytes[offset..offset + PAGE_SIZE]).try_into().unwrap();
                page::stamp_checksum(page_arr);
            }
            std::fs::write(&path, &bytes).unwrap();
        };

        // Case 1: patch both slots to (FORMAT_MAJOR_VERSION, +42). A minor
        // bump within the same major must open cleanly — this is the
        // whole point of the packed scheme. Pre-fix (exact-match gate)
        // this case rejected with UnsupportedFormatVersion.
        let minor_bump = page::pack_format_version(
            page::FORMAT_MAJOR_VERSION,
            page::FORMAT_MINOR_VERSION.wrapping_add(42),
        );
        patch_all_slots(minor_bump, 2);
        {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let tm = TransactionManager::open_existing(cache);
            assert!(
                tm.is_ok(),
                "same-major / different-minor file should open cleanly; got {:?}",
                tm.err()
            );
        }

        // Case 2: patch to (FORMAT_MAJOR_VERSION + 1, 0). A major bump
        // is a real format break and must be refused regardless of minor.
        let major_bump = page::pack_format_version(page::FORMAT_MAJOR_VERSION.wrapping_add(1), 0);
        patch_all_slots(major_bump, 2);
        {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            match TransactionManager::open_existing(cache) {
                Err(ChiselError::UnsupportedFormatVersion { .. }) => {}
                Err(e) => panic!("expected UnsupportedFormatVersion, got {e:?}"),
                Ok(_) => panic!("expected UnsupportedFormatVersion, got Ok"),
            }
        }
    }

    #[test]
    fn allocate_tagged_then_tag_and_handles_with_tag() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"row", 42).unwrap();
        let u = tm.allocate(b"untagged").unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.tag(h).unwrap(), 42);
        assert_eq!(tm.tag(u).unwrap(), 0);
        assert_eq!(tm.handles_with_tag(42).unwrap(), vec![h]);
        assert_eq!(tm.handles_with_tag(99).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn handles_with_tag_accumulates_multiple_handles() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let a = tm.allocate_tagged(b"a", 42).unwrap();
        let b = tm.allocate_tagged(b"b", 42).unwrap();
        let c = tm.allocate_tagged(b"c", 42).unwrap();
        tm.commit().unwrap();
        // The reverse index accumulates members; it must not overwrite.
        let mut got = tm.handles_with_tag(42).unwrap();
        got.sort();
        let mut want = vec![a, b, c];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(tm.tag(a).unwrap(), 42);
        assert_eq!(tm.tag(c).unwrap(), 42);
    }

    #[test]
    fn allocate_tagged_overflow_value_preserves_tag() {
        let mut tm = fresh_manager();
        // A value larger than MAX_INLINE_VALUE takes the overflow path in
        // allocate_inner; the tag must be stored on the Overflow HandleEntry,
        // readable via tag(), and indexed for handles_with_tag().
        let big = vec![0xABu8; MAX_INLINE_VALUE + 100];
        tm.begin().unwrap();
        let h = tm.allocate_tagged(&big, 77).unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.tag(h).unwrap(), 77);
        assert_eq!(tm.handles_with_tag(77).unwrap(), vec![h]);
        assert_eq!(tm.read(h).unwrap(), big);
    }

    #[test]
    fn update_preserves_immutable_tag() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"v1", 42).unwrap();
        tm.commit().unwrap();
        tm.begin().unwrap();
        tm.update(h, b"v2").unwrap();
        tm.commit().unwrap();
        // Tags are immutable: changing the value must NOT change the tag, and the
        // membership index must still list the handle under its original tag.
        assert_eq!(tm.tag(h).unwrap(), 42);
        assert_eq!(tm.handles_with_tag(42).unwrap(), vec![h]);
    }

    #[test]
    fn delete_removes_tagged_chunk_from_index() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"row", 7).unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(7).unwrap(), vec![h]);
        tm.begin().unwrap();
        tm.delete(h).unwrap();
        tm.commit().unwrap();
        // The tag's last member is gone -> handles_with_tag is empty.
        assert_eq!(tm.handles_with_tag(7).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn delete_tagged_rejects_wrong_tag() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let h = tm.allocate_tagged(b"row", 5).unwrap();
        // Wrong tag: error, nothing deleted, index intact.
        let err = tm.delete_tagged(h, 6).unwrap_err();
        assert!(
            matches!(err, ChiselError::TagMismatch { handle, expected: 6, actual: 5 } if handle == h)
        );
        assert_eq!(tm.handles_with_tag(5).unwrap(), vec![h]);
        // TagMismatch is operational, NOT fatal: a wrong tag must leave the
        // manager usable (guards against a future edit moving it into is_fatal).
        assert!(!tm.is_poisoned());
        // Right tag: deletes (and self-maintains the index via delete_inner).
        tm.delete_tagged(h, 5).unwrap();
        assert_eq!(tm.handles_with_tag(5).unwrap(), Vec::<u64>::new());
        tm.commit().unwrap();
    }

    #[test]
    fn delete_one_of_two_tagged_keeps_the_other() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let a = tm.allocate_tagged(b"a", 7).unwrap();
        let b = tm.allocate_tagged(b"b", 7).unwrap();
        tm.commit().unwrap();
        tm.begin().unwrap();
        tm.delete(a).unwrap();
        tm.commit().unwrap();
        // Only `a` is removed from the reverse index; `b` survives under tag 7.
        assert_eq!(tm.handles_with_tag(7).unwrap(), vec![b]);
        assert_eq!(tm.tag(b).unwrap(), 7);
    }

    // ── Migrated 2026-05-22 from tests/transactions.rs (I35 reshape) ──
    //
    // Exercises TransactionManager::open_existing end-to-end: a value
    // written through one TransactionManager survives a drop + reopen
    // on the same path. The other tests in tests/transactions.rs use
    // only the public Chisel API and stay in tests/.
    #[test]
    fn reopen_preserves_committed_data() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let handle;
        {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                64 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let mut txm = TransactionManager::create_new(cache, 2).unwrap();
            txm.begin().unwrap();
            handle = txm.allocate(b"persistent").unwrap();
            txm.commit().unwrap();
        }
        {
            let io = PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                64 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let txm = TransactionManager::open_existing(cache).unwrap();
            let data = txm.read(handle).unwrap();
            assert_eq!(data, b"persistent");
        }
    }

    #[test]
    fn delete_with_tag_drops_in_bounded_batches() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        let mut hs = Vec::new();
        for i in 0..10u64 {
            hs.push(tm.allocate_tagged(format!("row{i}").as_bytes(), 3).unwrap());
        }
        tm.commit().unwrap();
        tm.begin().unwrap();
        let p1 = tm.delete_with_tag(3, 4).unwrap();
        assert_eq!(p1.deleted.len(), 4);
        assert!(!p1.complete);
        let p2 = tm.delete_with_tag(3, 100).unwrap();
        assert_eq!(p2.deleted.len(), 6);
        assert!(p2.complete);
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(3).unwrap(), Vec::<u64>::new());
        // The chunks themselves are gone too.
        for h in hs {
            assert!(tm.read(h).is_err());
        }
    }

    #[test]
    fn delete_with_tag_exact_max_reports_complete() {
        // Boundary: exactly `max` members remain, so the max+1 enumeration
        // returns exactly `max` and `complete = max <= max` must be TRUE. This
        // is the <= vs < edge: a `<` here would falsely report incomplete and
        // cost the caller an extra empty pass. (The other delete_with_tag test
        // only exercises len > max and len < max, never len == max.)
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        for i in 0..5u64 {
            tm.allocate_tagged(format!("r{i}").as_bytes(), 8).unwrap();
        }
        tm.commit().unwrap();
        tm.begin().unwrap();
        let p = tm.delete_with_tag(8, 5).unwrap();
        assert_eq!(p.deleted.len(), 5);
        assert!(
            p.complete,
            "deleting exactly all members in one max-sized pass must report complete"
        );
        tm.commit().unwrap();
        assert_eq!(tm.handles_with_tag(8).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn handle_table_depth_restored_after_rolled_back_grow() {
        // I99 regression: a rolled-back handle-table grow must not leave the
        // in-memory depth too deep. A leaf holds ENTRIES_PER_LEAF (510) ids
        // 0..=509; handle 0 is the reserved "no handle" sentinel, so allocation
        // starts at id 1. Allocating ids 1..=509 (509 handles) stays at depth 0,
        // and the next handle (id 510, where `id >= cap` triggers the grow) goes
        // to depth 1. Roll that back, then a committed handle must still read (it
        // returned InvalidHandle before the fix).
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        for i in 0..509u64 {
            tm.allocate(format!("v{i}").as_bytes()).unwrap();
        }
        tm.commit().unwrap();
        let baseline = tm.read(5).unwrap();
        tm.begin().unwrap();
        tm.allocate(b"grow").unwrap();
        tm.rollback().unwrap();
        assert_eq!(
            tm.read(5).unwrap(),
            baseline,
            "committed handle lost after rolled-back grow"
        );
        // New inserts still work (depth consistent for the next transaction).
        tm.begin().unwrap();
        let h = tm.allocate(b"after").unwrap();
        tm.commit().unwrap();
        assert_eq!(tm.read(h).unwrap(), b"after");
    }

    #[test]
    fn handle_table_depth_restored_after_rollback_to_savepoint() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();
        // Handle 0 is the reserved "no handle" sentinel, so allocation starts at
        // id 1: ids 1..=509 (509 handles) stay at depth 0, and the "grow" below
        // (id 510, where `id >= ENTRIES_PER_LEAF` triggers the grow) is the one
        // that crosses to depth 1, past the savepoint. Handle h holds "v{h-1}".
        for i in 0..509u64 {
            tm.allocate(format!("v{i}").as_bytes()).unwrap();
        }
        tm.savepoint("sp").unwrap();
        tm.allocate(b"grow").unwrap(); // grows depth 0 -> 1 past the savepoint
        tm.rollback_to("sp").unwrap();
        // A handle present at the savepoint must still read within the active txn.
        // Handle 5 was the i=4 allocation, so it holds "v4".
        assert_eq!(tm.read(5).unwrap(), b"v4");
        tm.commit().unwrap();
        assert_eq!(tm.read(5).unwrap(), b"v4");
    }
}
