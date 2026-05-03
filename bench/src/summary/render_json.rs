// JSON renderer for the summary post-processor. Produces a flat
// composite-key schema (`<row>/<mode>/<size>` keys) with explicit
// nulls for missing-on-one-side data — keeps the schema rectangular
// for PR 7's CI diff to consume without conditional-existence checks.

use crate::summary::discover::Cell;
use crate::summary::metadata::Metadata;
use serde_json::{json, Map, Value};

/// Render a Vec<Cell> + Metadata into the results.json document.
/// Output schema:
///
///   {
///     "metadata": { ... metadata fields ... },
///     "cells": {
///       "<row>/<mode>/<size>": {
///         "p50_ns": ..., "p95_ns": ..., "p99_ns": ...,
///         "file_size_delta_bytes": ..., "counters": ...
///       },
///       ...
///     }
///   }
///
/// Missing data is explicit `null`, not omitted — keeps the schema
/// rectangular for diff tooling.
pub fn render_json(cells: &[Cell], metadata: &Metadata) -> Value {
    let mut cells_map = Map::new();
    for cell in cells {
        let key = format!("{}/{}/{}", cell.row, cell.mode, cell.size);
        cells_map.insert(key, render_cell_json(cell));
    }
    json!({
        "metadata": metadata,
        "cells": cells_map,
    })
}

fn render_cell_json(cell: &Cell) -> Value {
    json!({
        "p50_ns": cell.timing.map(|t| t.p50_ns),
        "p95_ns": cell.timing.map(|t| t.p95_ns),
        "p99_ns": cell.timing.map(|t| t.p99_ns),
        "file_size_delta_bytes": cell.aux.map(|a| a.file_size_delta_bytes),
        "counters": cell.aux.and_then(|a| a.counters),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ChiselCountersDelta;
    use crate::summary::discover::{AuxMetrics, TimingStats};
    use crate::summary::metadata::MachineInfo;

    fn fixture_metadata() -> Metadata {
        Metadata {
            timestamp: "2026-05-03T13:22:15Z".to_string(),
            chisel_commit: "abc123".to_string(),
            machine: MachineInfo {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                hostname: "test-host".to_string(),
            },
            post_processor_version: "0.1.0",
            criterion_dir: "target/criterion".to_string(),
            aux_metrics_path: "bench/results/aux_metrics.jsonl".to_string(),
            cell_count: 2,
        }
    }

    #[test]
    fn render_json_schema_round_trips() {
        let cells = vec![
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "chisel-strict".to_string(),
                size: "32B".to_string(),
                timing: Some(TimingStats {
                    p50_ns: 1234.5,
                    p95_ns: 1567.8,
                    p99_ns: 1890.2,
                }),
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 8192,
                    counters: Some(ChiselCountersDelta {
                        cache_hits: 12,
                        cache_misses: 3,
                        fsync_calls: 2,
                        pages_allocated: 4,
                    }),
                }),
            },
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "redb-strict".to_string(),
                size: "32B".to_string(),
                timing: None,
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 4096,
                    counters: None,
                }),
            },
        ];

        let value = render_json(&cells, &fixture_metadata());

        // Top-level keys
        assert_eq!(value["metadata"]["cell_count"], 2);
        assert!(value["cells"].is_object());

        // First cell
        let c1 = &value["cells"]["allocate-1pertx/chisel-strict/32B"];
        assert_eq!(c1["p50_ns"], 1234.5);
        assert_eq!(c1["p95_ns"], 1567.8);
        assert_eq!(c1["p99_ns"], 1890.2);
        assert_eq!(c1["file_size_delta_bytes"], 8192);
        assert_eq!(c1["counters"]["cache_hits"], 12);

        // Second cell — timing: None should serialize as null
        let c2 = &value["cells"]["allocate-1pertx/redb-strict/32B"];
        assert!(c2["p50_ns"].is_null());
        assert!(c2["p95_ns"].is_null());
        assert!(c2["p99_ns"].is_null());
        assert_eq!(c2["file_size_delta_bytes"], 4096);
        assert!(c2["counters"].is_null());

        // Round-trip via to_string + parse
        let s = serde_json::to_string(&value).unwrap();
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            parsed["cells"]["allocate-1pertx/chisel-strict/32B"]["p50_ns"],
            1234.5
        );
    }
}
