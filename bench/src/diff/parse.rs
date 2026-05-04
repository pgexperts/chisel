// JSON parser for the diff's input. Reads PR 5's results.json
// schema (top-level "scenarios" map keyed by "<scenario>/<mode>")
// and produces a typed view containing only the four metrics the
// diff cares about (throughput + p50/p95/p99). All other fields
// (cells, metadata, counters, file size) are ignored.
//
// BTreeMap rather than HashMap: we want deterministic key
// iteration order in tests and renderer output.

use std::collections::BTreeMap;
use std::path::Path;

/// The four scenario metrics the diff binary compares. Mirrors a
/// subset of PR 5's `ScenarioMetrics` but flat — no Option wrapping,
/// no counters, no file-size fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScenarioMetrics {
    pub throughput_ops_per_sec: f64,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
}

/// Typed view of a results.json file, restricted to the scenarios
/// data the diff cares about. Cells, metadata, counters all dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedResults {
    /// Map from "<scenario>/<mode>" key to metrics.
    pub scenarios: BTreeMap<String, ScenarioMetrics>,
}

#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    Json(serde_json::Error),
    MissingScenariosKey,
    MalformedScenarioEntry(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Io(e) => write!(f, "I/O error reading results.json: {e}"),
            ParseError::Json(e) => write!(f, "JSON parse error in results.json: {e}"),
            ParseError::MissingScenariosKey => {
                write!(
                    f,
                    "results.json `scenarios` key is missing or not an object"
                )
            }
            ParseError::MalformedScenarioEntry(key) => {
                write!(f, "results.json scenario entry `{key}` is malformed")
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<std::io::Error> for ParseError {
    fn from(e: std::io::Error) -> Self {
        ParseError::Io(e)
    }
}

impl From<serde_json::Error> for ParseError {
    fn from(e: serde_json::Error) -> Self {
        ParseError::Json(e)
    }
}

/// Read and parse a results.json file. Drops cells/metadata/counters;
/// keeps only the four metrics per scenario.
pub fn parse_results_json(path: &Path) -> Result<ParsedResults, ParseError> {
    let contents = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&contents)?;

    let scenarios_obj = value
        .get("scenarios")
        .and_then(|v| v.as_object())
        .ok_or(ParseError::MissingScenariosKey)?;

    let mut scenarios = BTreeMap::new();
    for (key, entry) in scenarios_obj {
        let metrics = parse_scenario_entry(key, entry)?;
        scenarios.insert(key.clone(), metrics);
    }

    Ok(ParsedResults { scenarios })
}

fn parse_scenario_entry(
    key: &str,
    entry: &serde_json::Value,
) -> Result<ScenarioMetrics, ParseError> {
    let obj = entry
        .as_object()
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;

    let throughput = obj
        .get("throughput_ops_per_sec")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;
    let p50 = obj
        .get("p50_ns")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;
    let p95 = obj
        .get("p95_ns")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;
    let p99 = obj
        .get("p99_ns")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MalformedScenarioEntry(key.to_string()))?;

    Ok(ScenarioMetrics {
        throughput_ops_per_sec: throughput,
        p50_ns: p50,
        p95_ns: p95,
        p99_ns: p99,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp(json: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parse_valid_results_json() {
        let json = r#"{
          "metadata": {"timestamp": "2026-05-04T12:00:00Z"},
          "cells": {},
          "scenarios": {
            "ycsb-a/chisel-strict": {
              "total_wall_clock_ns": 15000000000,
              "op_count": 100000,
              "throughput_ops_per_sec": 6666.7,
              "p50_ns": 120000.0,
              "p95_ns": 180000.0,
              "p99_ns": 250000.0,
              "final_file_size_bytes": 100000000,
              "file_size_delta_bytes": 4194304,
              "counters": null
            }
          }
        }"#;
        let f = write_temp(json);
        let parsed = parse_results_json(f.path()).unwrap();
        assert_eq!(parsed.scenarios.len(), 1);
        let m = parsed.scenarios.get("ycsb-a/chisel-strict").unwrap();
        assert_eq!(m.throughput_ops_per_sec, 6666.7);
        assert_eq!(m.p50_ns, 120000.0);
        assert_eq!(m.p95_ns, 180000.0);
        assert_eq!(m.p99_ns, 250000.0);
    }

    #[test]
    fn parse_empty_scenarios_map() {
        let json = r#"{"metadata": {}, "cells": {}, "scenarios": {}}"#;
        let f = write_temp(json);
        let parsed = parse_results_json(f.path()).unwrap();
        assert!(parsed.scenarios.is_empty());
    }

    #[test]
    fn parse_missing_scenarios_key() {
        let json = r#"{"metadata": {}, "cells": {}}"#;
        let f = write_temp(json);
        let err = parse_results_json(f.path()).unwrap_err();
        assert!(matches!(err, ParseError::MissingScenariosKey));
    }

    #[test]
    fn parse_malformed_scenario_entry_missing_field() {
        // Entry present but missing throughput_ops_per_sec.
        let json = r#"{
          "scenarios": {
            "ycsb-a/chisel-strict": {
              "p50_ns": 120000.0,
              "p95_ns": 180000.0,
              "p99_ns": 250000.0
            }
          }
        }"#;
        let f = write_temp(json);
        let err = parse_results_json(f.path()).unwrap_err();
        assert!(matches!(
            err,
            ParseError::MalformedScenarioEntry(ref k) if k == "ycsb-a/chisel-strict"
        ));
    }
}
