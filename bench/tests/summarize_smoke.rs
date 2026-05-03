// Integration smoke test: invoke the summarize binary against the
// committed fixtures and verify the three output artifacts are produced
// with sensible sizes + structure. Catches end-to-end wiring bugs that
// pass unit tests but break the binary.

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

    let output = cmd.output().expect("failed to run binary");
    assert!(
        output.status.success(),
        "summarize exited non-zero. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the three artifacts.
    let md_path = out_dir.join("summary.md");
    let json_path = out_dir.join("results.json");
    let raw_dir = out_dir.join("raw");
    assert!(md_path.exists(), "summary.md missing");
    assert!(json_path.exists(), "results.json missing");
    assert!(raw_dir.is_dir(), "raw/ directory missing");

    // Sanity-check sizes.
    let md_size = std::fs::metadata(&md_path).unwrap().len();
    assert!(md_size > 200, "summary.md too small ({} bytes)", md_size);
    assert!(
        md_size < 100_000,
        "summary.md unexpectedly large ({} bytes)",
        md_size
    );

    let json_size = std::fs::metadata(&json_path).unwrap().len();
    assert!(
        json_size > 100,
        "results.json too small ({} bytes)",
        json_size
    );

    // Verify results.json parses + has expected structure.
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

    // Verify the raw/ archive copied at least the chisel-strict 32B sample + estimates.
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
}
