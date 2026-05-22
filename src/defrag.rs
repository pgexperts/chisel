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
//   net drop in `data_page_count()` from the start to the end of the
//   sweep — capturing pages that were fully emptied by relocation and
//   returned to the freemap by the R2 commit path.

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
/// `max_pages`: soft cap on work per call, so a very large database can
/// be defragged incrementally across several transactions. `0` means no
/// limit. DESPITE THE NAME, the cap counts values relocated, not pages
/// touched — the field name is a carry-over from pre-R3 defrag and is
/// preserved for API stability. Breaking the loop early leaves the
/// transaction in a valid state; the caller chooses commit vs rollback.
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
    pub max_pages: usize,
}

impl Default for DefragOptions {
    fn default() -> DefragOptions {
        DefragOptions {
            sparse_threshold: 0.25,
            max_pages: 0,
        }
    }
}

// I36: chained setters paired with #[non_exhaustive]. Same shape as
// `Options`'s builder; method names match field names. External callers
// build via `DefragOptions::default().sparse_threshold(0.1).max_pages(100)`.
impl DefragOptions {
    pub fn sparse_threshold(mut self, threshold: f64) -> Self {
        self.sparse_threshold = threshold;
        self
    }
    pub fn max_pages(mut self, cap: usize) -> Self {
        self.max_pages = cap;
        self
    }
}

/// I36: `#[non_exhaustive]` for symmetry with `DefragOptions` and so a
/// future bench-friendly stat (e.g. wall-time elapsed inside the sweep)
/// is not a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DefragStats {
    pub pages_examined: u64,
    pub pages_freed: u64,
    pub values_moved: u64,
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
///   1. Short-circuit if the handle-table root is empty — nothing to do.
///   2. Compute the set of SPARSE data pages up front via
///      `txm.sparse_data_pages(sparse_threshold)`. A page is sparse if
///      its live-slot count is at or below `threshold × max_observed`.
///      Dense pages are left alone.
///   3. Record the initial data-page count so we can report
///      `pages_freed` at the end as the net drop (I17).
///   4. Snapshot the handle list up front so the iteration walks a
///      stable set rather than a handle table we are concurrently
///      rewriting via update().
///   5. For each handle, look up its current data page. If that page
///      is in the sparse set, relocate the value via `update()` — under
///      R1, `update` packs the value into the transaction's insert
///      cursor, which is a fresh, densely-packed page. Repeat until
///      the max-work cap (if any) is reached.
///   6. Track UNIQUE sparse pages touched via a HashSet (for accurate
///      `pages_examined`) and compute `pages_freed` as the net drop
///      in data-page count after the sweep.
///
/// What this does NOT do (v1):
/// - No fancy ordering of handle visits (e.g., group by page). A
///   handle-order traversal is simple and correct.
/// - No merging of adjacent sparse pages into one target. Relocated
///   values just go into whatever the insert cursor currently points at.
/// - Handle-table and overflow-chain COW garbage is not reclaimed by
///   this pass — only data pages are compacted. Handle-table spine
///   cleanup would require a separate mechanism.
pub fn defrag(txm: &mut TransactionManager, options: &DefragOptions) -> Result<DefragStats> {
    // Defrag mutates through `txm.update`, which requires an active
    // transaction. Without this check, a caller who forgot to `begin()`
    // would do some reads (those fall back to committed_roots), start
    // relocating values, and then hit `NoActiveTransaction` on the
    // first `update` — leaving the sweep in a half-done state with
    // confusing stats. Fail fast instead.
    if !txm.is_active() {
        return Err(crate::error::ChiselError::NoActiveTransaction);
    }

    let mut stats = DefragStats {
        pages_examined: 0,
        pages_freed: 0,
        values_moved: 0,
    };

    // Step 1: empty-database fast path.
    // I39: single-field accessor replaces the pre-I39 positional
    // `(u64, u64, u64)` tuple return; only the handle-table root is
    // consulted here.
    if txm.current_handle_table_root_page() == PAGE_ID_NONE {
        return Ok(stats);
    }

    // Step 2: identify sparse pages. If none qualify, there's nothing
    // to do and we skip the (potentially expensive) handle scan. This
    // step loads each candidate data page once to read its stored-slot
    // count from the header, so it can fail with a fatal I/O or
    // checksum error (which will poison the manager via the normal
    // path).
    let sparse_pages: HashSet<u64> = txm.sparse_data_pages(options.sparse_threshold)?;
    if sparse_pages.is_empty() {
        return Ok(stats);
    }

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
        if options.max_pages > 0 && stats.values_moved >= options.max_pages as u64 {
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

    Ok(stats)
}
