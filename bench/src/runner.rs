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

use chisel::stats::ChiselCounters;

/// Per-cell deltas of the four Chisel internal counters (master spec
/// §6.1). Reported in the JSONL aux-metrics file for cells where the
/// engine is Chisel; `None` for redb / SQLite.
///
/// Values are subtracted (after - before) and are guaranteed
/// non-negative because counters are cumulative-from-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ChiselCountersDelta {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub fsync_calls: u64,
    pub pages_allocated: u64,
}

/// Identifier of one micro-grid cell — the 3-tuple (row, mode, size).
/// Serialized via `#[serde(flatten)]` into `CellAuxMetrics` so the
/// JSONL line has top-level `row`, `mode`, `size` fields rather than
/// nested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CellId {
    pub row: &'static str,
    pub mode: &'static str,
    pub size: &'static str,
}

/// One JSONL line in `bench/results/aux_metrics.jsonl`. The full payload
/// for one cell of the micro grid: identifier + file-size delta + (for
/// Chisel) counter deltas. PR 5's post-processor reads this alongside
/// Criterion's `estimates.json` to produce the markdown summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CellAuxMetrics {
    #[serde(flatten)]
    pub cell_id: CellId,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>, // serialized as `counters: null` for non-Chisel
}

/// Compute the per-field delta between two ChiselCounters snapshots.
/// Returns `None` if either snapshot is `None` (i.e., for non-Chisel
/// engines whose `internal_counters()` returns `Ok(None)`).
///
/// Subtraction uses `saturating_sub` defensively even though the
/// invariant is that counters are cumulative-from-open (so after >=
/// before always). This guards against any future re-architecture that
/// would otherwise produce a debug-build panic.
pub fn counter_delta(
    before: Option<ChiselCounters>,
    after: Option<ChiselCounters>,
) -> Option<ChiselCountersDelta> {
    let (b, a) = before.zip(after)?;
    Some(ChiselCountersDelta {
        cache_hits: a.cache_hits.saturating_sub(b.cache_hits),
        cache_misses: a.cache_misses.saturating_sub(b.cache_misses),
        fsync_calls: a.fsync_calls.saturating_sub(b.fsync_calls),
        pages_allocated: a.pages_allocated.saturating_sub(b.pages_allocated),
    })
}

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};

/// Append-only JSONL writer for the per-cell aux-metrics output.
/// One line per cell. Truncates the file on `create()` so re-runs
/// don't accumulate stale entries; appends thereafter via `append()`.
///
/// The file is opened in buffered mode; every `append()` call ends
/// with `flush()` so a Ctrl-C mid-grid leaves the partial output
/// parseable (each completed cell is one full line).
pub struct AuxMetricsWriter {
    writer: BufWriter<File>,
}

impl AuxMetricsWriter {
    /// Open `path` for write, truncating any prior contents. Creates
    /// parent directories if missing (so the bench can write to
    /// `bench/results/aux_metrics.jsonl` without the runner having to
    /// pre-create `bench/results/`).
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    /// Append one cell's metrics as a single JSON line. Flushes after
    /// the write so partial output is parseable on interrupt.
    pub fn append(&mut self, metrics: &CellAuxMetrics) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, metrics)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
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

    #[test]
    fn aux_metrics_writer_jsonl_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/aux_metrics.jsonl");
        let mut writer = AuxMetricsWriter::create(&path).unwrap();

        writer
            .append(&CellAuxMetrics {
                cell_id: CellId {
                    row: "allocate-1pertx",
                    mode: "chisel-strict",
                    size: "32B",
                },
                file_size_delta_bytes: 262_144,
                counters: Some(ChiselCountersDelta {
                    cache_hits: 12,
                    cache_misses: 35,
                    fsync_calls: 2,
                    pages_allocated: 18,
                }),
            })
            .unwrap();

        writer
            .append(&CellAuxMetrics {
                cell_id: CellId {
                    row: "allocate-1pertx",
                    mode: "redb-strict",
                    size: "32B",
                },
                file_size_delta_bytes: 196_608,
                counters: None,
            })
            .unwrap();

        writer
            .append(&CellAuxMetrics {
                cell_id: CellId {
                    row: "delete-1pertx",
                    mode: "chisel-strict",
                    size: "1MB",
                },
                file_size_delta_bytes: -1_048_576, // delete shrinks
                counters: Some(ChiselCountersDelta {
                    cache_hits: 0,
                    cache_misses: 1,
                    fsync_calls: 1,
                    pages_allocated: 0,
                }),
            })
            .unwrap();

        // Read the file back and verify each line is parseable JSON
        // with the expected schema.
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "exactly 3 lines for 3 appends");

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            // top-level row/mode/size from the flatten attr
            assert!(v.get("row").is_some());
            assert!(v.get("mode").is_some());
            assert!(v.get("size").is_some());
            assert!(v.get("file_size_delta_bytes").is_some());
            // counters key must be present (null or object)
            assert!(v.get("counters").is_some());
        }

        // Spot-check one line in detail.
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["row"], "allocate-1pertx");
        assert_eq!(v["mode"], "chisel-strict");
        assert_eq!(v["size"], "32B");
        assert_eq!(v["file_size_delta_bytes"], 262_144);
        assert_eq!(v["counters"]["cache_hits"], 12);

        // The non-Chisel line should have counters: null.
        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(v2["counters"].is_null());

        // The negative-delta line should serialize as a negative number.
        let v3: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(v3["file_size_delta_bytes"], -1_048_576);
    }
}
