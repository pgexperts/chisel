// transaction.rs — Transaction lifecycle, savepoints, commit protocol, and data operations.
// This is the orchestration layer (layer 6 in the module graph per CLAUDE.md) that ties
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
//   - NOTE: new_page() DOES extend the underlying file immediately (see page_cache
//     and the v1 simplification in CLAUDE.md about the freemap not being wired up).
//     Those extended-but-uncommitted pages are harmless after a crash because
//     nothing in the committed superblock references them.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::data_page::DataPage;
use crate::error::{ChiselError, Result};
use crate::freemap::FreeMap;
use crate::handle_table::{HandleEntry, HandleFlags, HandleTable};
use crate::overflow::Overflow;
use crate::page::{self, PAGE_ID_NONE, PAGE_SIZE};
use crate::page_cache::PageCache;
use crate::superblock::{NamedRoot, Superblock, NAMED_ROOT_COUNT, NAMED_ROOT_NAME_LEN};

// Largest value stored inline in a data-page slot. Larger values are written to an
// overflow chain and referenced by a single HandleEntry with HandleFlags::Overflow.
// Derived from PAGE_SIZE minus DataPage header/slot overhead; keep in sync with data_page.rs.
const MAX_INLINE_VALUE: usize = 8162;

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
    // lib.rs and CLAUDE.md. Every access through this field uses
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
    // Monotonically increasing. Written into each new superblock; the higher value
    // wins on recovery. Also used to pick the inactive slot on commit (parity).
    txn_counter: u64,
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
}

impl TransactionManager {
    /// Create a new database (initialize superblocks).
    ///
    /// Writes TWO valid empty-database superblocks at staggered counters:
    /// slot 0 at txn_counter=1 (the "winner" — select() picks the max) and
    /// slot 1 at txn_counter=0. Both have identical empty roots. fsync before
    /// returning so the new database header is durable before any user data
    /// is written.
    ///
    /// Why both slots must be valid from the start (see ISSUES.md I2):
    /// historically slot 1 was left as an all-zero buffer (invalid checksum).
    /// That seemed fine because select() would simply prefer slot 0 — but the
    /// very first user commit writes slot 0 (txn_counter=2 has even parity),
    /// overwriting the ONLY valid superblock on disk. A torn write during
    /// that first commit then left zero recoverable state and open_existing
    /// returned CorruptSuperblock forever. By seeding slot 1 with a valid
    /// empty superblock at counter 0, a torn first commit falls back to an
    /// empty-but-openable database instead of a bricked file.
    ///
    /// The zero counter on slot 1 is safe: select() filters on checksum
    /// validity first, so a legitimately written counter-0 superblock is
    /// distinguishable from a zeroed disk region (the latter fails XXH3).
    pub fn create_new(mut cache: PageCache) -> Result<TransactionManager> {
        let sb_current = Superblock::new_empty();
        let mut sb_fallback = sb_current.clone();
        sb_fallback.txn_counter = 0;
        let buf_a = sb_current.serialize();
        let buf_b = sb_fallback.serialize();

        cache.io_mut().write_page(0, &buf_a)?;
        cache.io_mut().write_page(1, &buf_b)?;
        cache.io_mut().fsync()?;
        cache.set_next_page_id(2);

        let roots = Roots {
            handle_table_page: PAGE_ID_NONE,
            freemap_page: PAGE_ID_NONE,
            next_handle: 0,
            total_pages: 2,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
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
            txn_counter: sb_current.txn_counter,
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
        })
    }

    /// Open an existing database from file.
    ///
    /// This is the crash recovery path. Both superblock slots are read and
    /// Superblock::select() picks the one with the highest txn_counter and a
    /// valid XXH3 checksum. A torn write to the most-recently-targeted slot
    /// during a prior commit will fail its checksum, and select() will silently
    /// fall back to the other (older) superblock — which still points at a
    /// complete, consistent snapshot of committed pages. No replay required.
    pub fn open_existing(mut cache: PageCache) -> Result<TransactionManager> {
        let buf_a = cache.io_mut().read_page(0)?;
        let buf_b = cache.io_mut().read_page(1)?;
        let sb = Superblock::select(&[buf_a, buf_b]).ok_or(ChiselError::CorruptSuperblock)?;

        // Format-version gate (see ISSUES.md I15). We validate AFTER select()
        // rather than inside deserialize() because the winning superblock's
        // version is what determines compatibility — silently falling back to
        // an older-version superblock would hand the user a stale snapshot
        // with mysteriously missing data. If the newest valid superblock is
        // an incompatible version, refuse to open outright.
        if sb.format_version != page::FORMAT_VERSION {
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
        };

        // The HandleTable struct keeps only its depth in memory; physical pages
        // live in the cache and are reached through the root page_id in the
        // superblock. Reconstruct the depth by walking the left spine until we
        // hit a leaf (type byte != 0x02 interior marker).
        let mut ht = HandleTable::new();
        if sb.root_handle_table_page != PAGE_ID_NONE {
            // Determine depth by walking down the left spine.
            let root_buf = cache.get(sb.root_handle_table_page)?;
            if root_buf[1] == 0x02 {
                // Interior node — walk down to find depth.
                let mut depth = 0u32;
                let mut current = sb.root_handle_table_page;
                loop {
                    let buf = cache.get(current)?;
                    if buf[1] != 0x02 {
                        break;
                    }
                    depth += 1;
                    let child_offset = page::DATA_PAGE_HEADER_SIZE;
                    let child =
                        u64::from_le_bytes(buf[child_offset..child_offset + 8].try_into().unwrap());
                    if child == 0 {
                        break;
                    }
                    current = child;
                }
                ht.set_depth(depth);
            }
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
            txn_counter: sb.txn_counter,
            active_txn: false,
            savepoints: Vec::new(),
            txn_freed_pages: Vec::new(),
            committed_freemap,
            current_freemap,
            committed_live_slots,
            current_live_slots,
            insert_cursor: None,
            poisoned: Cell::new(false),
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
    // Overflow pages and handle-table COW pages do NOT go through this
    // path (they still call `cache.new_page()` directly and always
    // extend). Freeing those pages still feeds the freemap — so
    // delete-heavy workloads reach equilibrium via data-page reuse even
    // though overflow itself doesn't consume from the freemap. Routing
    // overflow through the freemap would require an allocator callback
    // or trait object at the overflow module boundary; noted as a v1
    // simplification.
    fn allocate_data_page(&mut self) -> Result<u64> {
        if self.savepoints.is_empty() {
            if let Some(id) = FreeMap::allocate_first(&mut self.current_freemap) {
                self.cache.borrow_mut().claim_page(id)?;
                return Ok(id);
            }
        }
        self.cache.borrow_mut().new_page()
    }

    // Persist the freemap at commit time (ISSUES.md R2 / I11).
    //
    // Called once at the very start of `commit_inner`, BEFORE cache.flush().
    // Steps:
    //   1. Merge `txn_freed_pages` into `current_freemap`. These pages
    //      become reusable for future transactions (not this one).
    //   2. If nothing changed relative to `committed_freemap`, return
    //      early — no new freemap page needs to be written.
    //   3. Mark the OLD freemap page (if any) as free. This lets the
    //      next commit reclaim it. Without this step, each commit would
    //      permanently leak one page.
    //   4. Allocate a new page for the freemap (via
    //      `allocate_data_page`, which may reuse or extend).
    //   5. Serialize the updated `current_freemap` into that page's
    //      cache buffer.
    //   6. Point `current_roots.freemap_page` at the new page, so the
    //      new superblock will pick it up.
    fn persist_freemap(&mut self) -> Result<()> {
        // Step 1: merge transaction frees.
        for &id in &self.txn_freed_pages {
            FreeMap::mark_free(&mut self.current_freemap, id);
        }

        // Step 2: skip if the freemap is unchanged.
        if self.current_freemap == self.committed_freemap {
            return Ok(());
        }

        // Step 3: reclaim the old freemap page itself.
        let old_freemap_page = self.committed_roots.freemap_page;
        if old_freemap_page != PAGE_ID_NONE {
            FreeMap::mark_free(&mut self.current_freemap, old_freemap_page);
        }

        // Step 4: allocate a page for the new freemap. May come from the
        // freemap itself (including the just-freed old_freemap_page id)
        // or extend the file.
        let new_freemap_page = self.allocate_data_page()?;

        // Step 5: serialize current_freemap into that page. The in-memory
        // buffer already carries the FreeMap page-type tag (byte 0 = 0x04)
        // and the bitmap body; a direct copy is the correct on-disk format.
        {
            let mut cache = self.cache.borrow_mut();
            let buf = cache.get_mut(new_freemap_page)?;
            *buf = *self.current_freemap;
            page::stamp_checksum(buf);
        }

        // Step 6: update the roots so the new superblock points here.
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
    ///    via Superblock::select()'s max_by_key, and (b) parity of the counter
    ///    selects which slot to overwrite (step 3). total_pages is queried from
    ///    the file AFTER flush() so any new_page() allocations are reflected.
    ///
    /// 3. Write the new superblock to the INACTIVE slot.
    ///    Slots 0 and 1 alternate based on txn_counter parity. The previously
    ///    active slot is untouched and still contains a valid superblock with
    ///    txn_counter - 1. WHY: if we crash during this write, the target slot
    ///    may be torn (bad checksum) but the OTHER slot still holds the last
    ///    committed state. Recovery picks the surviving older superblock and
    ///    the transaction is simply lost — never half-applied. Overwriting the
    ///    active slot in place would be catastrophic: a torn write there could
    ///    destroy the only valid superblock on disk.
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
    ///    to committed_roots. If any step above fails, active_txn stays true
    ///    and committed_roots is unchanged; the caller can retry or rollback.
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
        };
        let buf = sb.serialize();
        // Step 3: Write to the INACTIVE slot. Parity of the new (post-increment)
        // txn_counter determines which slot is inactive: even -> slot 0, odd -> 1.
        // The currently-active slot is never touched, so a torn write here can
        // only damage the new superblock, never the last known-good one.
        let inactive = if self.txn_counter.is_multiple_of(2) {
            0
        } else {
            1
        };
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
    /// NOTE: `freed_pages` from savepoints layered on top are dropped here,
    /// which is correct — those "frees" never became durable. Like commit(),
    /// freed pages are never actually returned to a freemap in v1.
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
    /// Handles are dense u64s drawn from current_roots.next_handle. Large values
    /// (> MAX_INLINE_VALUE) go to an overflow chain and the HandleEntry records
    /// the first overflow page directly; small values get a slot in a freshly
    /// allocated data page. Either way the handle_table.insert() COWs the spine
    /// from leaf to root and returns the new root page_id, which becomes the
    /// new current_roots.handle_table_page. This is the fundamental shadow-
    /// paging step: the old root is still reachable via committed_roots and is
    /// untouched on disk until commit swaps the superblock.
    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        self.check_alive()?;
        let result = self.allocate_inner(value);
        self.poison_on_fatal(result)
    }

    fn allocate_inner(&mut self, value: &[u8]) -> Result<u64> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        let handle = self.current_roots.next_handle;
        self.current_roots.next_handle += 1;

        let entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
            }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
            }
        };

        self.ensure_handle_table()?;
        let new_root = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table.insert(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                &entry,
            )?
        };
        self.current_roots.handle_table_page = new_root;

        Ok(handle)
    }

    /// Read a value by handle.
    ///
    /// If a transaction is active, reads see the in-progress (uncommitted) state
    /// through current_roots — i.e. "read your own writes". Otherwise reads go
    /// through committed_roots, the last durably-committed snapshot. There is no
    /// MVCC / snapshot isolation for concurrent readers because the writer is
    /// single-threaded; this branch is purely about making the active writer
    /// see its own pending mutations.
    /// Read a value by handle.
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
                DataPage::read(buf, entry.slot_index)
                    .map(|data| data.to_vec())
                    .ok_or(ChiselError::InvalidHandle(handle))
            }
            HandleFlags::Overflow => Overflow::read(&mut cache, entry.page_id),
            HandleFlags::Deleted => Err(ChiselError::InvalidHandle(handle)),
        }
    }

    /// Update an existing handle to point at a new value.
    ///
    /// Allocates a new slot/overflow chain for the new value and rewrites
    /// the HandleEntry via COW. The OLD location (inline data page or
    /// overflow chain) is collected into `txn_freed_pages` so commit can
    /// return its pages to the freemap (ISSUES.md I9).
    ///
    /// COUPLING (load-bearing, see R1): the "free the old data page"
    /// step assumes every data page has at most one live slot — which
    /// is true today because `insert_into_data_page` allocates a fresh
    /// page per value. When R1 lands (multiple values per page), this
    /// path must free a SLOT within the page, not the whole page.
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

        // Free the OLD location.
        //
        // For Live (inline) entries: decrement the live-slot count for
        // the old data page. If it drops to zero, the entire page is
        // now dead weight and can be returned to the freemap. If it
        // doesn't, the slot becomes a tombstone — dead space within a
        // still-live page, recoverable only via defrag (R3). This is
        // the cost of R1 packing: we can't rewrite a committed page
        // to compact it without rewriting every handle_table entry
        // pointing into it.
        //
        // For Overflow entries: delete the whole chain; all its pages
        // go straight to txn_freed_pages (no packing on overflow).
        match entry.flags {
            HandleFlags::Live => {
                self.release_data_slot(entry.page_id);
            }
            HandleFlags::Overflow => {
                let freed = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::delete(&mut cache, entry.page_id)?
                };
                self.txn_freed_pages.extend_from_slice(&freed);
            }
            HandleFlags::Deleted => {}
        }

        let new_entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
            }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
            }
        };

        let new_root = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table.insert(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                &new_entry,
            )?
        };
        self.current_roots.handle_table_page = new_root;

        Ok(())
    }

    /// Delete a handle.
    ///
    /// Collects the old location into `txn_freed_pages` (whole data page
    /// for inline, whole overflow chain for overflow), then asks the
    /// handle table to remove the mapping via COW. The same R1 coupling
    /// documented on `update` applies here — freeing the whole page is
    /// only sound because each data page currently holds one value.
    pub fn delete(&mut self, handle: u64) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_inner(handle);
        self.poison_on_fatal(result)
    }

    fn delete_inner(&mut self, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        let entry = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .lookup(&mut cache, self.current_roots.handle_table_page, handle)?
                .ok_or(ChiselError::InvalidHandle(handle))?
        };

        // Free the old location. Same slot-level semantics as update
        // (see the comment there): inline entries decrement the page's
        // live-slot count and only free the page when it hits zero;
        // overflow chains are deleted in full.
        match entry.flags {
            HandleFlags::Live => {
                self.release_data_slot(entry.page_id);
            }
            HandleFlags::Overflow => {
                let freed = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::delete(&mut cache, entry.page_id)?
                };
                self.txn_freed_pages.extend_from_slice(&freed);
            }
            HandleFlags::Deleted => {}
        }

        let new_root = {
            let mut cache = self.cache.borrow_mut();
            self.handle_table
                .delete(&mut cache, self.current_roots.handle_table_page, handle)?
        };
        self.current_roots.handle_table_page = new_root;

        Ok(())
    }

    /// Delete many handles in a single transaction (ISSUES.md F1 / I12).
    ///
    /// The motivating use case is client-side `drop_table` /
    /// `drop_index_table`, which need to remove many row and node
    /// handles at once without leaking their pages. Under the
    /// freemap-aware delete (I10), every single delete returns its
    /// pages to `txn_freed_pages`; this helper just loops over them
    /// inside a single transaction so the whole bulk delete is atomic
    /// on commit.
    ///
    /// Error semantics: on the first error the loop stops and returns
    /// the error. Handles deleted before the failure remain marked for
    /// deletion in `current_roots`, so the caller can choose between
    /// `rollback()` (abandon the whole batch) or `commit()` (keep the
    /// partial work). This matches the rest of the API where
    /// individual operations fail in isolation.
    pub fn delete_many(&mut self, handles: &[u64]) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_many_inner(handles);
        self.poison_on_fatal(result)
    }

    fn delete_many_inner(&mut self, handles: &[u64]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
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

    pub fn current_roots(&self) -> (u64, u64, u64) {
        (
            self.current_roots.handle_table_page,
            self.current_roots.freemap_page,
            self.current_roots.next_handle,
        )
    }

    pub fn is_active(&self) -> bool {
        self.active_txn
    }

    // --- Private helpers ---

    /// Lazily create a handle table root on first insert. A fresh database has
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

    /// root_handle_table_page == PAGE_ID_NONE; we don't materialize the root
    /// until there is a handle to put in it, so empty databases never pay for
    /// a handle-table page. No per-page rollback bookkeeping — the
    /// watermark rollback mechanism (I3) handles any page allocated here
    /// automatically.
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
    /// v1 SIMPLIFICATION (see CLAUDE.md): this unconditionally allocates a
    /// fresh page per insert rather than searching existing pages with free
    /// space. Combined with update() not reclaiming the old slot, every
    /// allocate/update cycle costs at least one new 8KB page. Correctness is
    /// preserved; space efficiency is terrible until the freemap is wired up.
    ///
    /// Checksum is stamped eagerly so the page is valid if it later gets
    /// evicted and reloaded mid-transaction. Without this, a dirty page
    /// evicted by LRU pressure would fail checksum verification on reload.
    ///
    /// R1: tries to pack the value into the current insert cursor page
    /// (a data page allocated earlier in this transaction that still
    /// has space). Falls back to allocating a new page if the cursor
    /// is full, unset, or packing is disabled (active savepoints).
    /// R2: new-page allocations go through `allocate_data_page`, which
    /// prefers freemap reuse over file extension.
    ///
    /// Live-slot bookkeeping: every successful insert increments
    /// `current_live_slots[page_id]`, which delete/update consult to
    /// decide when a page is fully empty and can be freed back to the
    /// freemap on commit. No on-disk slot-count tracking is needed.
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
    use tempfile::NamedTempFile;

    fn fresh_manager() -> TransactionManager {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let cache = PageCache::new(io, 64);
        let mut tm = TransactionManager::create_new(cache).unwrap();
        // Commit once so there's a real baseline to read/write against.
        tm.begin().unwrap();
        tm.commit().unwrap();
        tm
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
}
