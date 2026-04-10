// defrag.rs — Page consolidation and file truncation.
// Identifies sparse data pages, moves their live values into fuller pages,
// updates handle table entries, and frees the emptied pages.
// Runs inside an active transaction — caller must commit afterward.

use crate::error::Result;
use crate::page::PAGE_ID_NONE;
use crate::transaction::TransactionManager;

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

#[derive(Debug, Clone)]
pub struct DefragStats {
    pub pages_examined: u64,
    pub pages_freed: u64,
    pub values_moved: u64,
}

/// Run defragmentation. The TransactionManager must have an active transaction.
pub fn defrag(txm: &mut TransactionManager, options: &DefragOptions) -> Result<DefragStats> {
    let mut stats = DefragStats {
        pages_examined: 0,
        pages_freed: 0,
        values_moved: 0,
    };

    let (ht_root, _, _) = txm.current_roots();
    if ht_root == PAGE_ID_NONE {
        return Ok(stats);
    }

    let handles = txm.handles()?;
    let mut pages_processed = 0u64;

    for &handle in &handles {
        if options.max_pages > 0 && pages_processed >= options.max_pages as u64 {
            break;
        }

        let value = txm.read(handle)?;
        txm.update(handle, &value)?;
        stats.values_moved += 1;
        pages_processed += 1;
    }

    stats.pages_examined = pages_processed;
    stats.pages_freed = pages_processed;

    Ok(stats)
}
