// Runner machinery for the micro grid. Holds the non-Criterion-dependent
// machinery (engine construction, snapshot population, workload application,
// aux-metric capture, JSONL writer); the Criterion-shaped cell-runner
// helpers live in `bench/benches/micro_grid.rs` because Criterion is in
// [dev-dependencies] and library code in src/ cannot import dev-deps.
//
// PR 6 (scenarios) will reuse most of this module — engine construction,
// populate_snapshot, apply_op, AuxMetricsWriter — for its own iteration
// shape, which is "one big workload run" rather than the per-iteration
// micro-grid cells PR 4b registers.

use crate::engine::{DurabilityMode, Engine, EngineResult};
use crate::{ChiselEngine, RedbEngine, SqliteEngine};
use std::path::Path;

/// One of the five engine-mode columns of the micro grid.
///
/// Hides the per-engine constructor asymmetry behind `EngineMode::open`:
/// ChiselEngine takes `(path, cache_size_pages)` while RedbEngine and
/// SqliteEngine take `(path, cache_size_pages, DurabilityMode)`. The enum
/// is the single source of truth for the 5 modes the micro grid measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    ChiselStrict,
    RedbStrict,
    RedbUnsafe,
    SqliteStrict,
    SqliteUnsafe,
}

impl EngineMode {
    /// All five modes the micro grid measures. The order is the canonical
    /// column order in the markdown summary (PR 5).
    pub const ALL: [Self; 5] = [
        Self::ChiselStrict,
        Self::RedbStrict,
        Self::RedbUnsafe,
        Self::SqliteStrict,
        Self::SqliteUnsafe,
    ];

    /// Stable column label used in BenchmarkIds and JSONL output.
    pub fn label(self) -> &'static str {
        match self {
            Self::ChiselStrict => "chisel-strict",
            Self::RedbStrict => "redb-strict",
            Self::RedbUnsafe => "redb-unsafe",
            Self::SqliteStrict => "sqlite-strict",
            Self::SqliteUnsafe => "sqlite-unsafe",
        }
    }

    /// True iff `Engine::internal_counters()` returns `Some(_)` for this
    /// mode. Currently only `ChiselStrict` (the other engines are
    /// black-box). The post-processor uses this to decide whether to
    /// fill the Chisel-internals appendix table for a cell.
    pub fn supports_internal_counters(self) -> bool {
        matches!(self, Self::ChiselStrict)
    }

    /// Construct a fresh engine of this mode, file-backed at `path` with
    /// the given cache budget. Hides the per-engine constructor
    /// asymmetry: ChiselEngine has no DurabilityMode parameter (always
    /// strict by design), the others do.
    pub fn open(self, path: &Path, cache_size_pages: usize) -> EngineResult<Box<dyn Engine>> {
        match self {
            Self::ChiselStrict => Ok(Box::new(ChiselEngine::open_file(path, cache_size_pages)?)),
            Self::RedbStrict => Ok(Box::new(RedbEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Strict,
            )?)),
            Self::RedbUnsafe => Ok(Box::new(RedbEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Unsafe,
            )?)),
            Self::SqliteStrict => Ok(Box::new(SqliteEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Strict,
            )?)),
            Self::SqliteUnsafe => Ok(Box::new(SqliteEngine::open_file(
                path,
                cache_size_pages,
                DurabilityMode::Unsafe,
            )?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::NamedTempFile;

    #[test]
    fn engine_mode_label_uniqueness() {
        let labels: HashSet<&'static str> = EngineMode::ALL.iter().map(|m| m.label()).collect();
        assert_eq!(
            labels.len(),
            EngineMode::ALL.len(),
            "labels must be distinct"
        );
        for label in &labels {
            assert!(!label.is_empty(), "no label may be empty");
        }
    }

    #[test]
    fn engine_mode_supports_internal_counters() {
        for mode in EngineMode::ALL {
            let expected = matches!(mode, EngineMode::ChiselStrict);
            assert_eq!(
                mode.supports_internal_counters(),
                expected,
                "only ChiselStrict reports internal counters; got {mode:?}"
            );
        }
    }

    #[test]
    fn engine_mode_open_each_mode() {
        for mode in EngineMode::ALL {
            let tf = NamedTempFile::new().unwrap();
            let mut engine = mode.open(tf.path(), 256).unwrap();
            // basic sanity: open + begin/commit a no-op tx
            engine.begin().unwrap();
            engine.commit().unwrap();
        }
    }
}
