// Filesystem discovery layer for the summary post-processor. Walks
// target/criterion/<row>/<mode>/<size>/new/sample.json files, parses
// aux_metrics.jsonl, joins them by (row, mode, size) key into Cell
// values. Cells with missing-on-one-side data carry None for the
// missing field — renderers handle the partial-state cases explicitly.
//
// The `new/` component is Criterion's own, not ours: it writes the
// just-finished run to `new/` and retains the previous one under `base/`.
// Only `new/` is read here; see the walk in `discover_cells`.

use crate::runner::ChiselCountersDelta;
use crate::summary::format::percentile_linear_interp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// One cell of the micro grid, joined across Criterion sample.json and
/// aux_metrics.jsonl. Cells where one source is missing carry None for
/// that field; both sources missing means the cell isn't in the Vec at all.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cell {
    pub row: String,
    pub mode: String,
    pub size: String,
    pub timing: Option<TimingStats>,
    pub aux: Option<AuxMetrics>,
}

/// Per-cell timing percentiles computed from Criterion's raw sample.json
/// per-iteration times. p50/p95/p99 share the same sample distribution
/// so they're mutually comparable for regression detection.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TimingStats {
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
}

/// Per-cell auxiliary metrics from aux_metrics.jsonl. counters is None
/// for non-Chisel engines (mirroring the JSONL schema from PR 4b).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AuxMetrics {
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

/// Errors that can arise during discovery. The post-processor's CLI
/// layer surfaces these as user-facing error messages with exit code 1
/// when fatal, or warnings to stderr when non-fatal (per the failure-
/// handling matrix in spec §7.3).
#[derive(Debug)]
pub enum DiscoverError {
    /// criterion_dir doesn't exist or isn't readable.
    CriterionDirNotFound(std::path::PathBuf),
    /// criterion_dir exists but contains no sample.json leaves.
    NoCellsFound(std::path::PathBuf),
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CriterionDirNotFound(p) => write!(
                f,
                "Criterion output directory '{}' does not exist; run cargo bench --bench micro_grid first",
                p.display()
            ),
            Self::NoCellsFound(p) => write!(
                f,
                "no cells found under '{}'; the directory exists but contains no sample.json",
                p.display()
            ),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// JSONL line schema (matches PR 4b's CellAuxMetrics serde_json output).
#[derive(Deserialize)]
struct AuxLine {
    row: String,
    mode: String,
    size: String,
    file_size_delta_bytes: i64,
    counters: Option<ChiselCountersDelta>,
}

/// Criterion 0.5 sample.json schema. We only read these two arrays;
/// any additional fields Criterion adds in future versions are tolerated
/// via `#[serde(default)]` on the struct (omitted here since both fields
/// are required for any valid Criterion sample).
#[derive(Deserialize)]
struct SampleJson {
    iters: Vec<f64>,
    times: Vec<f64>,
}

/// Walk `criterion_dir` and parse `aux_metrics_path`, joining by
/// (row, mode, size) into a sorted Vec<Cell>. See module-level docs
/// for the partial-state semantics (Option<TimingStats> / Option<AuxMetrics>).
pub fn discover_cells(
    criterion_dir: &Path,
    aux_metrics_path: &Path,
) -> Result<Vec<Cell>, DiscoverError> {
    if !criterion_dir.exists() {
        return Err(DiscoverError::CriterionDirNotFound(
            criterion_dir.to_path_buf(),
        ));
    }

    // Step 1: load aux_metrics.jsonl into a hashmap. Missing file =>
    // empty map (every cell will have aux: None).
    let mut aux_map: HashMap<(String, String, String), AuxMetrics> = HashMap::new();
    if aux_metrics_path.exists() {
        match std::fs::read_to_string(aux_metrics_path) {
            Ok(contents) => {
                for (lineno, line) in contents.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<AuxLine>(line) {
                        Ok(entry) => {
                            aux_map.insert(
                                (entry.row, entry.mode, entry.size),
                                AuxMetrics {
                                    file_size_delta_bytes: entry.file_size_delta_bytes,
                                    counters: entry.counters,
                                },
                            );
                        }
                        Err(e) => {
                            eprintln!("warning: skipping malformed aux line {}: {}", lineno + 1, e);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "warning: aux-metrics file '{}' could not be read ({}); cells will have no file-size or counter data",
                    aux_metrics_path.display(),
                    e
                );
            }
        }
    } else {
        eprintln!(
            "warning: aux-metrics file '{}' missing; cells will have no file-size or counter data",
            aux_metrics_path.display()
        );
    }

    // Step 2: walk criterion_dir to find sample.json leaves.
    //
    // Each leaf is at depth 5: criterion_dir/row/mode/size/{new,base}/sample.json.
    // Criterion 0.5 interposes that last directory — `new/` is the run that just
    // finished, `base/` is the previous run kept for its own change-detection.
    // We want `new/` only: `base/` is a stale duplicate under the same
    // (row, mode, size) key, and counting it would emit two Cells per cell.
    //
    // The `new` check is the load-bearing filter; the depth bounds only
    // restate the grid's shape (a row/mode/size triple is exactly what the
    // path walk-back below needs, and what a Cell is keyed by).
    let mut cells: Vec<Cell> = Vec::new();
    let mut sample_paths_seen = 0usize;

    for entry in walkdir::WalkDir::new(criterion_dir)
        .min_depth(5)
        .max_depth(5)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_name() != "sample.json" {
            continue;
        }

        // Walk the path back: file -> new dir -> size dir -> mode dir -> row dir.
        let run_dir = entry.path().parent().unwrap();
        if run_dir.file_name().unwrap_or_default() != "new" {
            continue;
        }
        sample_paths_seen += 1;

        let size_dir = run_dir.parent().unwrap();
        let mode_dir = size_dir.parent().unwrap();
        let row_dir = mode_dir.parent().unwrap();

        let size = size_dir.file_name().unwrap().to_string_lossy().into_owned();
        let mode = mode_dir.file_name().unwrap().to_string_lossy().into_owned();
        let row = row_dir.file_name().unwrap().to_string_lossy().into_owned();

        // Parse sample.json; if it fails, log and emit timing: None.
        let timing = match parse_sample_json(entry.path()) {
            Ok(Some(t)) => Some(t),
            Ok(None) => None,
            Err(e) => {
                eprintln!(
                    "warning: skipping malformed sample.json at {}: {}",
                    entry.path().display(),
                    e
                );
                None
            }
        };

        // `remove` (not `get`): consuming the entry here is what makes Step 3
        // below correct — whatever stays in aux_map after this loop is exactly
        // the set of aux entries that had no matching Criterion cell.
        let aux = aux_map.remove(&(row.clone(), mode.clone(), size.clone()));

        // Only emit the cell if at least one side has data.
        if timing.is_some() || aux.is_some() {
            cells.push(Cell {
                row,
                mode,
                size,
                timing,
                aux,
            });
        }
    }

    // Step 3: any aux entries we didn't pair up with a Criterion cell
    // (rare but possible) get emitted with timing: None.
    for ((row, mode, size), aux) in aux_map.into_iter() {
        cells.push(Cell {
            row,
            mode,
            size,
            timing: None,
            aux: Some(aux),
        });
    }

    if sample_paths_seen == 0 && cells.is_empty() {
        return Err(DiscoverError::NoCellsFound(criterion_dir.to_path_buf()));
    }

    // Step 4: sort by (row, mode, size) for deterministic output.
    cells.sort_by(|a, b| {
        a.row
            .cmp(&b.row)
            .then_with(|| a.mode.cmp(&b.mode))
            .then_with(|| a.size.cmp(&b.size))
    });

    Ok(cells)
}

fn parse_sample_json(path: &Path) -> Result<Option<TimingStats>, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(path)?;
    let parsed: SampleJson = serde_json::from_str(&contents)?;
    if parsed.iters.len() != parsed.times.len() || parsed.iters.is_empty() {
        return Ok(None);
    }
    let mut per_iter: Vec<f64> = parsed
        .times
        .iter()
        .zip(&parsed.iters)
        .map(|(t, i)| t / i)
        .collect();
    per_iter.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(Some(TimingStats {
        p50_ns: percentile_linear_interp(&per_iter, 0.50).unwrap(),
        p95_ns: percentile_linear_interp(&per_iter, 0.95).unwrap(),
        p99_ns: percentile_linear_interp(&per_iter, 0.99).unwrap(),
    }))
}

/// Copy `estimates.json` and `sample.json` files from `criterion_dir`
/// into `raw_out_dir`, preserving the directory structure. Skips
/// Criterion's HTML reports, plot images, change/ subdirectories, and
/// other supporting files — those stay in target/criterion/ for live
/// browsing; the archive's job is reproducibility (so the markdown
/// numbers can be regenerated from the archive if target/ is wiped).
///
/// 165 cells × 2 small JSON files ≈ 330 KB total archive size.
pub fn copy_raw_archive(criterion_dir: &Path, raw_out_dir: &Path) -> std::io::Result<()> {
    for entry in walkdir::WalkDir::new(criterion_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name();
        if name != "estimates.json" && name != "sample.json" {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(criterion_dir)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let dest = raw_out_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(entry.path(), &dest)?;
    }
    Ok(())
}

/// Per-scenario metrics, one per (scenario, mode) cell. Mirrors the
/// scenarios_metrics.jsonl schema produced by bench/benches/scenarios.rs.
/// Renderers (render_json, render_md) consume Vec<ScenarioMetrics>
/// alongside Vec<Cell> to produce the full PR 5 + PR 6 output.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScenarioMetrics {
    pub scenario: String,
    pub mode: String,
    pub total_wall_clock_ns: u64,
    pub op_count: usize,
    pub throughput_ops_per_sec: f64,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub final_file_size_bytes: u64,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

/// Load the scenarios_metrics.jsonl file produced by the scenario
/// bench binary. Returns Vec<ScenarioMetrics> sorted by (scenario,
/// mode) for deterministic output. Missing file → empty Vec (matches
/// PR 5's "warn-and-continue for missing aux" pattern). Malformed
/// lines logged to stderr but don't abort.
pub fn load_scenarios_jsonl(path: &Path) -> Vec<ScenarioMetrics> {
    if !path.exists() {
        eprintln!(
            "warning: scenarios-metrics file '{}' missing; markdown will omit the scenario section",
            path.display()
        );
        return Vec::new();
    }
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: could not read scenarios-metrics '{}': {}",
                path.display(),
                e
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (lineno, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ScenarioMetrics>(line) {
            Ok(m) => out.push(m),
            Err(e) => {
                eprintln!(
                    "warning: skipping malformed scenarios-metrics line {}: {}",
                    lineno + 1,
                    e
                );
            }
        }
    }
    out.sort_by(|a, b| {
        a.scenario
            .cmp(&b.scenario)
            .then_with(|| a.mode.cmp(&b.mode))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn discover_cells_joins_criterion_with_aux() {
        let criterion_dir = fixtures_root().join("criterion");
        let aux_path = fixtures_root().join("aux_metrics.jsonl");
        let cells = discover_cells(&criterion_dir, &aux_path).unwrap();

        // Expect at least 2 valid cells (chisel-strict 32B, redb-strict 32B).
        // The corrupt cell (corrupt/chisel-strict/32B) has timing: None
        // and is not in aux_metrics.jsonl, so it's omitted from the Vec
        // (both sides None means no Cell emitted).
        assert!(
            cells.len() >= 2,
            "expected at least 2 cells, got {}",
            cells.len()
        );

        // The chisel-strict 32B fixture has BOTH a new/ and a base/ sample.json,
        // mirroring a real Criterion tree after a second run. Exactly one cell
        // must come out of it, and its numbers must be new/'s (base/ carries
        // times 10x larger, so a wrong pick shows up as p50 30000).
        assert_eq!(
            cells
                .iter()
                .filter(|c| c.row == "allocate-1pertx"
                    && c.mode == "chisel-strict"
                    && c.size == "32B")
                .count(),
            1,
            "base/ must not produce a second cell under the same key"
        );

        let chisel_cell = cells
            .iter()
            .find(|c| c.row == "allocate-1pertx" && c.mode == "chisel-strict" && c.size == "32B")
            .expect("chisel-strict 32B cell missing");
        let timing = chisel_cell
            .timing
            .expect("chisel-strict 32B should have timing");
        assert!((timing.p50_ns - 3000.0).abs() < 1e-6);
        assert!((timing.p95_ns - 4800.0).abs() < 1e-6);
        assert!((timing.p99_ns - 4960.0).abs() < 1e-6);
        let aux = chisel_cell.aux.expect("chisel-strict 32B should have aux");
        assert_eq!(aux.file_size_delta_bytes, 8192);
        let counters = aux
            .counters
            .expect("chisel-strict 32B should have non-null counters");
        assert_eq!(counters.cache_hits, 12);
        assert_eq!(counters.cache_misses, 3);
        assert_eq!(counters.fsync_calls, 2);
        assert_eq!(counters.pages_allocated, 4);

        let redb_cell = cells
            .iter()
            .find(|c| c.row == "allocate-1pertx" && c.mode == "redb-strict" && c.size == "32B")
            .expect("redb-strict 32B cell missing");
        let timing = redb_cell
            .timing
            .expect("redb-strict 32B should have timing");
        assert!((timing.p50_ns - 2000.0).abs() < 1e-6);
        assert!((timing.p95_ns - 2900.0).abs() < 1e-6);
        assert!((timing.p99_ns - 2980.0).abs() < 1e-6);
        let aux = redb_cell.aux.expect("redb-strict 32B should have aux");
        assert_eq!(aux.file_size_delta_bytes, 4096);
        assert!(aux.counters.is_none(), "redb cells have null counters");
    }

    #[test]
    fn discover_cells_handles_missing_aux_gracefully() {
        let criterion_dir = fixtures_root().join("criterion");
        // Point at a nonexistent path
        let aux_path = fixtures_root().join("nonexistent_aux.jsonl");
        let cells = discover_cells(&criterion_dir, &aux_path).unwrap();

        assert!(cells.len() >= 2);
        for cell in &cells {
            // Every cell should have aux: None since aux file missing
            assert!(
                cell.aux.is_none(),
                "expected aux: None for {}/{}/{} when aux file missing",
                cell.row,
                cell.mode,
                cell.size
            );
        }
        // But timing should still be populated for valid cells
        let valid_cells: Vec<_> = cells.iter().filter(|c| c.timing.is_some()).collect();
        assert!(
            valid_cells.len() >= 2,
            "expected at least 2 cells with timing"
        );
    }

    #[test]
    fn discover_cells_handles_malformed_sample_json() {
        let criterion_dir = fixtures_root().join("criterion");
        let aux_path = fixtures_root().join("aux_metrics.jsonl");
        let cells = discover_cells(&criterion_dir, &aux_path).unwrap();

        // The corrupt fixture is at corrupt/chisel-strict/32B/sample.json
        // (parse fails). If discover_cells visits it, it should either:
        //   (a) record timing: None for that cell (and aux: None since
        //       it's not in aux_metrics), then OMIT the cell from the Vec
        //       (both sides None means no Cell emitted), OR
        //   (b) keep the cell with timing: None (if you choose to emit
        //       cells with at-least-Criterion-visited)
        // Both behaviors are acceptable per the spec. Assert NOT-PANICKED
        // by getting here, and assert the valid cells are unaffected.
        let corrupt_cell = cells
            .iter()
            .find(|c| c.row == "corrupt" && c.mode == "chisel-strict" && c.size == "32B");
        if let Some(cell) = corrupt_cell {
            assert!(
                cell.timing.is_none(),
                "corrupt cell should have timing: None"
            );
        }

        // Valid cells must still be present and correct:
        let chisel_valid = cells
            .iter()
            .find(|c| c.row == "allocate-1pertx" && c.mode == "chisel-strict" && c.size == "32B")
            .expect("valid chisel-strict 32B should still discover cleanly");
        assert!(chisel_valid.timing.is_some());
    }
}
