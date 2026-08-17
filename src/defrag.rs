// defrag.rs — Maintenance pass that rewrites live values to compact storage.
//
// Role in system: a top-layer utility built entirely on the public
// `TransactionManager` surface (`handles`, `read`, `update`). It owns no
// page-level logic; all compaction is expressed as reads and updates that
// the transaction layer then routes through shadow paging.
//
// Why this is safe — the stable-handle invariant:
//   The whole point of the handle-table indirection is that a handle's u64
//   identity is decoupled from the (page, slot) it currently lives in. An
//   `update(handle, value)` is guaranteed to preserve `handle` while moving
//   the value to a freshly allocated slot and rewriting the handle-table
//   entry via COW. Therefore, reading a value and immediately writing it
//   back under the same handle is an observable no-op for callers but
//   relocates the physical storage — exactly what a compactor needs.
//   This module relies on that invariant from `transaction.rs` /
//   `handle_table.rs`; if it were ever weakened, defrag would silently
//   corrupt references.
//
// Transactionality:
//   Defrag runs *inside* the caller's active transaction. It does not
//   begin, commit, or rollback. This means (a) a defrag pass is atomic
//   with any other work in the same transaction, (b) a crash or rollback
//   mid-defrag leaves the previous committed state intact — the shadow
//   pages written during defrag simply become unreachable, and (c) the
//   caller is responsible for calling `commit()` to actually shrink the
//   live working set.
//
// Selective defrag (ISSUES.md R3):
//   Since R1 lets data pages pack multiple values, an `update(handle, value)`
//   under R1 naturally relocates the value into the transaction's insert
//   cursor — compacting as a side effect. R3 uses that property by only
//   targeting handles that live in SPARSE data pages (pages whose live-slot
//   count is a small fraction of the densest page), leaving dense pages
//   alone. Non-selective defrag still works correctly but wastes I/O
//   moving values that are already well-packed.
//
// Stat accuracy (ISSUES.md I17):
//   `pages_examined` now counts UNIQUE sparse pages touched during the
//   sweep (via a HashSet) rather than per-value. `pages_freed` is the
//   count of pages that held a live slot at the START of the sweep but
//   none at the END — the set difference of the `data_page_ids_snapshot`
//   taken before and after, NOT a net count delta. Net count is the wrong
//   metric here: a relocation drains a sparse source page while filling a
//   dense destination, so the live-page count can barely move even though a
//   page was genuinely emptied and returned to the freemap by the R2 commit
//   path.

use std::collections::HashSet;

use crate::error::Result;
use crate::page::PAGE_ID_NONE;
use crate::transaction::TransactionManager;

/// Knobs for a defrag pass.
///
/// `sparse_threshold`: fill-density fraction (live_slots / stored_slots)
/// below which a page is treated as sparse and its values are relocated
/// (e.g. 0.25 = pages less than 25% dense are compacted). Consumed by
/// `TransactionManager::sparse_data_pages` during step 2 of the sweep.
/// Dense pages above the threshold are left alone — per R3, the cost of
/// re-packing a mostly-full page exceeds the benefit. Values <= 0
/// disable the sweep entirely (the sparse set comes back empty).
///
/// `max_values`: soft cap on work per call, so a very large database can
/// be defragged incrementally across several transactions. `0` means no
/// limit. The cap counts values relocated, not pages touched. Breaking
/// the loop early leaves the transaction in a valid state; the caller
/// chooses commit vs rollback.
///
/// I36 (ISSUES.md, 2026-05-22): `#[non_exhaustive]` so a future
/// tuning knob (e.g. a per-page time cap, a pages-vs-values-priority
/// toggle) is not a breaking change. External callers construct via
/// `DefragOptions { ..Default::default() }` rather than a full struct
/// literal.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DefragOptions {
    pub sparse_threshold: f64,
    pub max_values: usize,
}

impl Default for DefragOptions {
    fn default() -> DefragOptions {
        DefragOptions {
            sparse_threshold: 0.25,
            max_values: 0,
        }
    }
}

// I36: chained setters paired with #[non_exhaustive]. Same shape as
// `Options`'s builder; method names match field names. External callers
// build via `DefragOptions::default().sparse_threshold(0.1).max_values(100)`.
impl DefragOptions {
    pub fn sparse_threshold(mut self, threshold: f64) -> Self {
        self.sparse_threshold = threshold;
        self
    }
    pub fn max_values(mut self, cap: usize) -> Self {
        self.max_values = cap;
        self
    }
}

/// I36: `#[non_exhaustive]` for symmetry with `DefragOptions` and so a
/// future bench-friendly stat (e.g. wall-time elapsed inside the sweep)
/// is not a breaking change.
// I121 (ISSUES.md, 2026-06-21): #[must_use] so `db.defrag(opts)?;` cannot
// silently discard the report (pages_examined / pages_freed). Callers who
// genuinely don't care bind it with `let _ = ...`.
#[must_use = "DefragStats reports what the sweep did (pages_examined/pages_freed); inspect it or bind with `let _`"]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DefragStats {
    pub pages_examined: u64,
    pub pages_freed: u64,
    pub values_moved: u64,
    /// Freemap-typed pages reclaimed by the orphan-sweep — pages a prior crash
    /// stranded when it lost the in-memory structural recycle pool (see
    /// `TransactionManager::reclaim_freemap_orphans`). 0 on a clean database.
    /// Reported, not gating: a non-zero value means a past crash leaked freemap
    /// pages that this pass returned to the bitmap as reusable space.
    pub freemap_orphans_reclaimed: u64,
}

/// Run defragmentation.
///
/// Preconditions (caller's responsibility):
/// - `txm` has an active transaction. `read`/`update` will error otherwise,
///   but we rely on that check rather than duplicating it here.
/// - No other references into the page cache are held across this call;
///   `&mut TransactionManager` enforces that at the type level.
///
/// Algorithm (R3 selective):
///   1. Short-circuit if BOTH the handle-table root and the freemap root are
///      empty — nothing to compact and no orphans possible. The handle-table
///      root alone is not sufficient: it can revert to PAGE_ID_NONE on an
///      allocate prepare-abort while a freemap tree persists (FREEMAP-9).
///   2. Compute the set of SPARSE data pages up front via
///      `txm.sparse_data_pages(sparse_threshold)`. A page is sparse if
///      `live_slots / stored_slots < sparse_threshold` — a per-page density
///      against the page's OWN stored-slot count, not against the densest
///      page in the database. (The older relative-to-densest rule was
///      abandoned because a lone sparse page scores 1.0 against itself and
///      was never collected; see `sparse_data_pages`.) Dense pages are
///      left alone.
///   3. Snapshot the initial set of data-page IDs (not a count) so we can
///      report `pages_freed` at the end as the set difference against the
///      final set — a net count delta is the wrong metric here, see the
///      header (I17).
///   4. Snapshot the handle list up front so the iteration walks a
///      stable set rather than a handle table we are concurrently
///      rewriting via update().
///   5. For each handle, look up its current data page. If that page
///      is in the sparse set, relocate the value via `update()` — under
///      R1, `update` packs the value into the transaction's insert
///      cursor, which is a fresh, densely-packed page. Repeat until
///      the max-work cap (if any) is reached.
///   6. Track UNIQUE sparse pages touched via a HashSet (for accurate
///      `pages_examined`) and compute `pages_freed` as the set difference
///      between the initial and final data-page-ID sets (NOT a net count
///      delta — see header).
///   7. Reclaim freemap pages orphaned by a prior crash that lost the
///      in-memory structural recycle pool, via
///      `txm.reclaim_freemap_orphans` (reported as
///      `freemap_orphans_reclaimed`). Runs unconditionally — even when
///      steps 2-6 found no sparse data pages — since freemap orphans are
///      an independent reclamation channel. EXCEPTION: the orphan sweep is
///      skipped while a savepoint is active (it returns 0). Since GAP-1
///      (issue #107) `rollback_to` DOES rewind the structural recycle streams,
///      so this is now defence in depth rather than the sole guarantee; the
///      residual it covers is `structural_reuse` consumption. See
///      `reclaim_freemap_orphans`. Run defrag outside a savepoint scope to
///      reclaim crash orphans.
///
/// What this does NOT do (v1):
/// - No fancy ordering of handle visits (e.g., group by page). A
///   handle-order traversal is simple and correct.
/// - No merging of adjacent sparse pages into one target. Relocated
///   values just go into whatever the insert cursor currently points at.
/// - Handle-table and overflow-chain COW garbage is not reclaimed by
///   this pass — only data pages are compacted, and (step 7) freemap
///   pages orphaned by a crash are swept back into the bitmap. Handle-
///   table and overflow spine cleanup still requires a separate mechanism.
pub fn defrag(txm: &mut TransactionManager, options: &DefragOptions) -> Result<DefragStats> {
    // A poisoned manager is dead, and defrag must refuse like every other entry
    // point does. It cannot call `check_alive` (private to the transaction
    // module), so it asks the same question through the public flag.
    //
    // Why here and not one of the three other candidate doors. Steps 2-7 were
    // already covered incidentally, though by one check rather than two:
    // `sparse_data_pages` at step 2 refused a poisoned manager and every later
    // step propagates with `?`, so step 7 never ran. (`reclaim_freemap_orphans`
    // grew its own `check_alive` in this same change; before that it had none.)
    // The empty-database fast path below returns before step 2, so `defrag` on
    // a poisoned, empty database returned `Ok(stats)`. Guarding that fast path
    // would fix the one return and leave the next early return uncovered;
    // guarding `Chisel::defrag` would fix the public API and leave the next
    // in-crate caller uncovered. This function is where defrag's preconditions
    // live (the `is_active` check below is the sibling) and every route into a
    // pass goes through it, so it is the door that closes the class.
    //
    // `Poisoned` is operational by `is_fatal()`'s classification, so returning
    // it poisons nothing further and reclassifies nothing — it reports a
    // manager that is already dead.
    if txm.is_poisoned() {
        return Err(crate::error::ChiselError::Poisoned);
    }

    // Defrag mutates through `txm.update`, which requires an active
    // transaction. Without this check, a caller who forgot to `begin()`
    // would do some reads (those fall back to committed_roots), start
    // relocating values, and then hit `NoActiveTransaction` on the
    // first `update` — leaving the sweep in a half-done state with
    // confusing stats. Fail fast instead.
    //
    // Not redundant with the identical check inside
    // `reclaim_freemap_orphans` (TXN-COMMIT-8): that one guards step 7,
    // which runs only AFTER steps 2-6 have already relocated values. This
    // one fires before anything moves.
    if !txm.is_active() {
        return Err(crate::error::ChiselError::NoActiveTransaction);
    }

    let mut stats = DefragStats {
        pages_examined: 0,
        pages_freed: 0,
        values_moved: 0,
        freemap_orphans_reclaimed: 0,
    };

    // Step 1: empty-database fast path.
    //
    // FREEMAP-9 (issue #107): this requires BOTH roots to be unset. It used to
    // test only the handle-table root, justified by the claim that that root
    // "materialises permanently on the first `allocate` and never reverts to
    // PAGE_ID_NONE, so an empty handle table implies no allocation has ever
    // succeeded — therefore no freemap tree exists".
    //
    // That claim is false, and `abort_allocate_prepare` is what falsifies it:
    // on a non-fatal failure mid-allocate (the CacheFull the fault injector
    // models) it restores `current_roots.handle_table_page` to its pre-allocate
    // value — PAGE_ID_NONE on a fresh database — and on the same path releases
    // the inline value's data slot onto `txn_freed_pages`. If the caller then
    // COMMITS rather than rolling back (which the public contract explicitly
    // permits), `persist` lazily CREATES the freemap tree to record that freed
    // page. The database now has `root_handle_table_page == PAGE_ID_NONE` and a
    // real `root_freemap_page`, permanently: nothing ever clears a freemap root.
    //
    // Under the old single-root test, every future `defrag()` on that database
    // returned here and never ran `reclaim_freemap_orphans` (step 7) — so any
    // freemap page a later crash stranded was unreclaimable by the only
    // mechanism that reclaims them.
    //
    // Steps 2-6 (data-page compaction) are genuine no-ops when the handle-table
    // root is unset, since `current_live_slots` is necessarily empty; the only
    // thing the early return was ever really skipping is step 7.
    if txm.current_handle_table_root_page() == PAGE_ID_NONE
        && txm.current_freemap_root_page() == PAGE_ID_NONE
    {
        return Ok(stats);
    }

    // Step 2: identify sparse pages. If none qualify, there is no data-page
    // compaction to do and we skip the (potentially expensive) handle scan —
    // but we still fall through to the freemap orphan-sweep below, which is
    // independent of data-page density (a crash can leave freemap orphans even
    // in a database with no sparse pages). This step loads each candidate data
    // page once to read its stored-slot count from the header, so it can fail
    // with a fatal I/O or checksum error (which will poison the manager via the
    // normal path).
    let sparse_pages: HashSet<u64> = txm.sparse_data_pages(options.sparse_threshold)?;
    if !sparse_pages.is_empty() {
        // Step 3: snapshot the set of data page ids at the start so we can
        // report `pages_freed` accurately. Net change in page count is the
        // wrong metric: a relocation simultaneously drains a sparse page
        // and creates a dense destination, so net change is ~0 even when
        // a page genuinely got reclaimed. The right metric is "pages that
        // existed at the start and are gone at the end", which we compute
        // via set difference below.
        let initial_page_ids = txm.data_page_ids_snapshot();

        // Step 4: snapshot the handle set.
        let handles = txm.handles()?;

        // Step 5: relocate handles living on sparse pages.
        let mut examined_pages: HashSet<u64> = HashSet::new();
        for &handle in &handles {
            if options.max_values > 0 && stats.values_moved >= options.max_values as u64 {
                break;
            }
            let page_id = match txm.handle_live_page_id(handle)? {
                Some(id) => id,
                None => continue, // Overflow or Deleted — nothing to compact.
            };
            if !sparse_pages.contains(&page_id) {
                continue; // Dense page — leave it alone.
            }
            examined_pages.insert(page_id);

            // Read-then-write under the same handle. The stable-handle
            // invariant guarantees `handle` still refers to this value
            // after update; R1's insert cursor packs the re-insertion
            // into a dense destination.
            let value = txm.read(handle)?;
            txm.update(handle, &value)?;
            stats.values_moved += 1;
        }

        // Step 6: accurate stats (I17). `pages_freed` counts pages that
        // were tracked at the start and are gone now — i.e., pages the
        // sweep fully drained and returned to the freemap.
        let final_page_ids = txm.data_page_ids_snapshot();
        stats.pages_examined = examined_pages.len() as u64;
        stats.pages_freed = initial_page_ids.difference(&final_page_ids).count() as u64;
    }

    // Step 7: reclaim freemap pages orphaned by a prior crash that lost the
    // in-memory recycle pool. Off the hot path (defrag is scheduled
    // maintenance), so this is exactly the place for the O(total_pages) scan.
    // Runs even when there was no data-page compaction above — the two are
    // independent reclamation channels.
    stats.freemap_orphans_reclaimed = txm.reclaim_freemap_orphans()?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PAGE_SIZE;
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;

    // Build a committed-once manager, mirroring transaction.rs's `fresh_manager`
    // (private to that module, hence duplicated here).
    fn fresh_manager() -> TransactionManager {
        let file = tempfile::NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let cache = PageCache::new(
            io,
            1024 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        let mut tm = TransactionManager::create_new(cache, 2, None, None).unwrap();
        tm.begin().unwrap();
        tm.commit().unwrap();
        tm
    }

    // A poisoned manager must refuse a defrag pass even when there is provably
    // nothing to compact. The empty-database fast path returns before
    // `sparse_data_pages` and `reclaim_freemap_orphans` — the two calls that
    // check the poison flag for themselves — so pre-fix this returned
    // `Ok(DefragStats { .. })` from a manager every other entry point had
    // already declared dead.
    //
    // The transaction must be opened BEFORE the poison: `begin()` checks the
    // flag too, so the reachable shape is a transaction that was healthy when it
    // started and hit a fatal error part-way through.
    #[test]
    fn defrag_on_a_poisoned_empty_database_refuses() {
        let mut tm = fresh_manager();
        tm.begin().unwrap();

        // Precondition: both roots unset, so the run under test really does take
        // the fast path. Without this the test could pass on the strength of
        // `sparse_data_pages`'s own check and never exercise the gap.
        assert_eq!(
            tm.current_handle_table_root_page(),
            PAGE_ID_NONE,
            "precondition: the handle-table root must be unset for the fast path"
        );
        assert_eq!(
            tm.current_freemap_root_page(),
            PAGE_ID_NONE,
            "precondition: the freemap root must be unset for the fast path"
        );

        tm.force_poison_for_test();
        let err = defrag(&mut tm, &DefragOptions::default()).unwrap_err();
        assert!(
            matches!(err, crate::error::ChiselError::Poisoned),
            "expected Poisoned from a defrag pass on a dead manager, got {err:?}"
        );
    }

    // A defrag pass reports any freemap orphan it reclaimed. Forge an orphan (a
    // crash-stranded freemap page), run defrag inside a transaction, and assert
    // the new `freemap_orphans_reclaimed` stat counts it. This is the public,
    // stat-level contract of the crash-recovery sweep wired into step 7.
    #[test]
    fn defrag_reports_freemap_orphans_reclaimed() {
        let mut tm = fresh_manager();

        // Establish a real freemap tree (a value + its delete frees a whole page,
        // materializing a freemap leaf), so defrag's empty-DB fast path is not
        // taken and the sweep has a live root to walk.
        let big: Vec<u8> = vec![0xCD; 1 << 14];
        tm.begin().unwrap();
        let h = tm.allocate(&big).unwrap();
        tm.commit().unwrap();
        tm.begin().unwrap();
        tm.delete(h).unwrap();
        tm.commit().unwrap();

        // Forge the orphan, then run defrag in a fresh transaction.
        let orphan = tm.test_forge_freemap_orphan().unwrap();
        tm.begin().unwrap();
        let stats = defrag(&mut tm, &DefragOptions::default()).unwrap();
        tm.commit().unwrap();

        assert!(
            stats.freemap_orphans_reclaimed >= 1,
            "defrag must report the forged orphan as reclaimed, got {}",
            stats.freemap_orphans_reclaimed
        );

        // Idempotent: a second defrag finds nothing (the orphan was already
        // reclaimed into the bitmap on the first pass).
        tm.begin().unwrap();
        let again = defrag(&mut tm, &DefragOptions::default()).unwrap();
        tm.commit().unwrap();
        assert_eq!(
            again.freemap_orphans_reclaimed, 0,
            "second sweep must find no orphans (idempotent)"
        );
        let _ = orphan;
    }

    // Cover the `structural_superseded` exclusion in `reclaim_freemap_orphans`.
    //
    // When defrag step 5 (data relocation) and step 7 (orphan sweep) run in the
    // SAME transaction, step-5 `update()` calls allocate fresh data pages via
    // `cow_alloc` → `allocate_first`, which claims a free bit from the freemap
    // bitmap and COWs the containing leaf.  The OLD (pre-COW) leaf id lands in
    // `structural_superseded`.  That page is unreachable from the current tree
    // and is NOT marked free in the bitmap — exactly the shape of an orphan —
    // yet it is NOT an orphan: it is live recycling state that commit will hand
    // to the next transaction.  If `reclaim_freemap_orphans` didn't exclude
    // `structural_superseded`, step 7 would wrongly reclaim those pages, marking
    // their adjacent data pages reusable and corrupting any live value that still
    // references them.
    //
    // Counterfactual: temporarily removing the `structural_superseded` term from
    // the exclusion set causes this test to fail — the reclaimed count exceeds 1
    // (the COW'd leaf is swept as an orphan) and subsequent reads return wrong
    // data.  With the exclusion in place, exactly the one forged orphan is
    // reclaimed and all surviving handles read back their original values.
    #[test]
    fn defrag_compact_and_sweep_excludes_structural_superseded() {
        let mut tm = fresh_manager();

        // Use values large enough that each takes a significant fraction of a
        // data page (~400 bytes), so ~20 fit per page.  Allocate 60 handles
        // across ~3 data pages, then delete 45 of them (every third is kept),
        // making the source pages ~33% full — below the default 0.25 threshold
        // only when the kept fraction is low enough.  Use a harsher threshold
        // (0.5) so the pages reliably qualify as sparse.
        //
        // The deletes free ~3 data pages back into the freemap bitmap, giving
        // `allocate_first` free bits to claim during step-5 updates.  Each
        // claim COWs the freemap leaf, pushing the old leaf id into
        // `structural_superseded`.
        let value: Vec<u8> = vec![0xAB; 400];
        let mut all_handles: Vec<u64> = Vec::new();
        tm.begin().unwrap();
        for _ in 0..60 {
            all_handles.push(tm.allocate(&value).unwrap());
        }
        tm.commit().unwrap();

        // Delete 45 of 60 handles (keep every 4th) so 3 of the ~3 data pages
        // become heavily sparse.
        let kept: Vec<u64> = all_handles
            .iter()
            .copied()
            .enumerate()
            .filter(|(i, _)| i % 4 == 0)
            .map(|(_, h)| h)
            .collect();
        let deleted: Vec<u64> = all_handles
            .iter()
            .copied()
            .enumerate()
            .filter(|(i, _)| i % 4 != 0)
            .map(|(_, h)| h)
            .collect();

        tm.begin().unwrap();
        for h in &deleted {
            tm.delete(*h).unwrap();
        }
        tm.commit().unwrap();

        // Forge exactly one freemap orphan before the defrag transaction.
        let orphan = tm.test_forge_freemap_orphan().unwrap();

        // Run defrag with a generous sparse threshold (0.5) so the source pages
        // definitely qualify.  Steps 5 and 7 execute inside the SAME transaction.
        //
        // Step 5 triggers at least one data-page allocation → `cow_alloc` →
        // `allocate_first` call, which COWs a freemap leaf and pushes its old id into
        // `structural_superseded`.  Step 7 must then exclude that id so only the
        // forged orphan is reclaimed.
        tm.begin().unwrap();
        let opts = DefragOptions::default().sparse_threshold(0.5);
        let stats = defrag(&mut tm, &opts).unwrap();

        // Step 5 must have actually relocated values (otherwise structural_superseded
        // would be empty and this test is vacuous).
        assert!(
            stats.values_moved > 0,
            "precondition: step 5 must have relocated at least one value (got 0); \
             increase the value count or lower the kept fraction so pages are sparse"
        );

        // Exactly one orphan reclaimed — the forged one.  If a COW'd leaf were
        // wrongly swept, the count would exceed 1.
        assert_eq!(
            stats.freemap_orphans_reclaimed, 1,
            "exactly the forged orphan must be reclaimed (not a COW'd leaf from \
             step 5); got {}",
            stats.freemap_orphans_reclaimed
        );

        tm.commit().unwrap();

        // Every surviving handle must still read back its original value — a
        // live freemap page wrongly reclaimed would corrupt the bitmap and could
        // cause a data-page double-hand-out, breaking subsequent reads.
        tm.begin().unwrap();
        for h in &kept {
            let v = tm.read(*h).unwrap();
            assert_eq!(
                v, value,
                "handle {h} returned wrong data after compact+sweep defrag"
            );
        }
        tm.commit().unwrap();

        let _ = orphan;
    }
}
