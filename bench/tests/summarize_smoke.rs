// Integration smoke test: invoke the summarize binary against the
// committed fixtures and verify the three output artifacts are produced
// with sensible sizes + structure. PR 6 extension: also exercise the
// scenarios-metrics file path.

use assert_cmd::Command;
use std::path::PathBuf;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn summarize_smoke_runs_against_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("out");

    let mut cmd = Command::cargo_bin("summarize").unwrap();
    cmd.arg("--out").arg(&out_dir);
    cmd.arg("--criterion")
        .arg(fixtures_root().join("criterion"));
    cmd.arg("--aux")
        .arg(fixtures_root().join("aux_metrics.jsonl"));
    cmd.arg("--scenarios")
        .arg(fixtures_root().join("scenarios_metrics.jsonl"));

    let output = cmd.output().expect("failed to run binary");
    assert!(
        output.status.success(),
        "summarize exited non-zero. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let md_path = out_dir.join("summary.md");
    let json_path = out_dir.join("results.json");
    let raw_dir = out_dir.join("raw");
    assert!(md_path.exists(), "summary.md missing");
    assert!(json_path.exists(), "results.json missing");
    assert!(raw_dir.is_dir(), "raw/ directory missing");

    // PR 5 cells assertions (unchanged).
    let json_content = std::fs::read_to_string(&json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
    assert!(parsed["metadata"].is_object());
    assert!(parsed["cells"].is_object());
    let cells_obj = parsed["cells"].as_object().unwrap();
    assert!(
        cells_obj.len() >= 2,
        "expected at least 2 cells in fixture output, got {}",
        cells_obj.len()
    );

    // PR 6 scenarios assertions.
    assert!(
        parsed["scenarios"].is_object(),
        "results.json missing scenarios top-level key"
    );
    let scenarios_obj = parsed["scenarios"].as_object().unwrap();
    assert_eq!(
        scenarios_obj.len(),
        2,
        "expected 2 scenarios from fixture, got {}",
        scenarios_obj.len()
    );
    let s1 = &parsed["scenarios"]["ycsb-a/chisel-strict"];
    assert_eq!(s1["op_count"], 100000);
    assert_eq!(s1["counters"]["cache_hits"], 99000);
    let s2 = &parsed["scenarios"]["ycsb-a/redb-strict"];
    assert!(s2["counters"].is_null());

    // Markdown should include the Scenario tier section.
    let md_content = std::fs::read_to_string(&md_path).unwrap();
    assert!(
        md_content.contains("## Scenario tier"),
        "summary.md missing Scenario tier section"
    );
    assert!(
        md_content.contains("ycsb-a"),
        "summary.md missing ycsb-a row"
    );

    let chisel_raw = raw_dir
        .join("allocate-1pertx")
        .join("chisel-strict")
        .join("32B");
    assert!(
        chisel_raw.join("sample.json").exists(),
        "raw chisel-strict sample.json missing"
    );
    assert!(
        chisel_raw.join("estimates.json").exists(),
        "raw chisel-strict estimates.json missing"
    );

    // PR 8: cross-engine.md is unconditionally produced alongside the others.
    let cross_engine_path = out_dir.join("cross-engine.md");
    assert!(
        cross_engine_path.exists(),
        "cross-engine.md should be written to {}",
        out_dir.display()
    );
    let cross_engine_content = std::fs::read_to_string(&cross_engine_path).unwrap();
    assert!(
        cross_engine_content.starts_with("# Chisel Bench: Cross-engine comparison\n"),
        "cross-engine.md first line wrong:\n{cross_engine_content}"
    );
    // All three engine column labels and all four scenario names should appear.
    for label in ["Chisel", "redb", "SQLite"] {
        assert!(
            cross_engine_content.contains(label),
            "cross-engine.md missing engine label {label}"
        );
    }
    for scenario in ["ycsb-a", "ycsb-b", "mutation-log", "document-store"] {
        assert!(
            cross_engine_content.contains(scenario),
            "cross-engine.md missing scenario {scenario}"
        );
    }
}
