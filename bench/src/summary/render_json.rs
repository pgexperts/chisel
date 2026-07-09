// JSON renderer for the summary post-processor. Produces a flat
// composite-key schema (`<row>/<mode>/<size>` keys) with explicit
// nulls for missing-on-one-side data — keeps the schema rectangular
// for PR 7's CI diff to consume without conditional-existence checks.

use crate::summary::discover::{Cell, ScenarioMetrics};
use crate::summary::metadata::Metadata;
use serde_json::{json, Map, Value};

/// Render a Vec<Cell> + Vec<ScenarioMetrics> + Metadata into the
/// results.json document. Output schema:
///
///   {
///     "metadata": { ... metadata fields ... },
///     "cells": {
///       "<row>/<mode>/<size>": { ... timing + aux fields ... },
///       ...
///     },
///     "scenarios": {
///       "<scenario>/<mode>": { ... scenario fields ... },
///       ...
///     }
///   }
///
/// Missing data is explicit `null`, not omitted — keeps the schema
/// rectangular for diff tooling. Empty `scenarios` slice produces
/// `"scenarios": {}` (still present, just empty).
pub fn render_json(cells: &[Cell], scenarios: &[ScenarioMetrics], metadata: &Metadata) -> Value {
    let mut cells_map = Map::new();
    for cell in cells {
        let key = format!("{}/{}/{}", cell.row, cell.mode, cell.size);
        cells_map.insert(key, render_cell_json(cell));
    }
    let mut scenarios_map = Map::new();
    for s in scenarios {
        let key = format!("{}/{}", s.scenario, s.mode);
        scenarios_map.insert(key, render_scenario_json(s));
    }
    json!({
        "metadata": metadata,
        "cells": cells_map,
        "scenarios": scenarios_map,
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

fn render_scenario_json(s: &ScenarioMetrics) -> Value {
    json!({
        "total_wall_clock_ns": s.total_wall_clock_ns,
        "op_count": s.op_count,
        "throughput_ops_per_sec": s.throughput_ops_per_sec,
        "p50_ns": s.p50_ns,
        "p95_ns": s.p95_ns,
        "p99_ns": s.p99_ns,
        "final_file_size_bytes": s.final_file_size_bytes,
        "file_size_delta_bytes": s.file_size_delta_bytes,
        "counters": s.counters,
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

        let value = render_json(&cells, &[], &fixture_metadata());

        assert_eq!(value["metadata"]["cell_count"], 2);
        assert!(value["cells"].is_object());
        assert!(value["scenarios"].is_object());
        assert_eq!(value["scenarios"].as_object().unwrap().len(), 0);

        let c1 = &value["cells"]["allocate-1pertx/chisel-strict/32B"];
        assert_eq!(c1["p50_ns"], 1234.5);
        assert_eq!(c1["counters"]["cache_hits"], 12);

        let c2 = &value["cells"]["allocate-1pertx/redb-strict/32B"];
        assert!(c2["p50_ns"].is_null());
        assert_eq!(c2["file_size_delta_bytes"], 4096);
        assert!(c2["counters"].is_null());
    }

    #[test]
    fn render_json_includes_scenarios_top_level() {
        use crate::summary::discover::ScenarioMetrics;

        let scenarios = vec![
            ScenarioMetrics {
                scenario: "ycsb-a".to_string(),
                mode: "chisel-strict".to_string(),
                total_wall_clock_ns: 15_000_000_000,
                op_count: 100_000,
                throughput_ops_per_sec: 6666.7,
                p50_ns: 120_000.0,
                p95_ns: 180_000.0,
                p99_ns: 250_000.0,
                final_file_size_bytes: 104_857_600,
                file_size_delta_bytes: 4_194_304,
                counters: Some(ChiselCountersDelta {
                    cache_hits: 99_000,
                    cache_misses: 1_000,
                    fsync_calls: 100_000,
                    pages_allocated: 12_500,
                }),
            },
            ScenarioMetrics {
                scenario: "ycsb-a".to_string(),
                mode: "redb-strict".to_string(),
                total_wall_clock_ns: 19_000_000_000,
                op_count: 100_000,
                throughput_ops_per_sec: 5263.2,
                p50_ns: 145_000.0,
                p95_ns: 200_000.0,
                p99_ns: 320_000.0,
                final_file_size_bytes: 110_000_000,
                file_size_delta_bytes: 5_000_000,
                counters: None,
            },
        ];

        let value = render_json(&[], &scenarios, &fixture_metadata());

        assert!(value["scenarios"].is_object());
        let scenarios_obj = value["scenarios"].as_object().unwrap();
        assert_eq!(scenarios_obj.len(), 2);

        let s1 = &value["scenarios"]["ycsb-a/chisel-strict"];
        assert_eq!(s1["op_count"], 100_000);
        assert_eq!(s1["p50_ns"], 120_000.0);
        assert_eq!(s1["counters"]["cache_hits"], 99_000);

        let s2 = &value["scenarios"]["ycsb-a/redb-strict"];
        assert!(s2["counters"].is_null());
        assert_eq!(s2["file_size_delta_bytes"], 5_000_000);
    }
}
