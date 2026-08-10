// CLI entry point for chisel-bench-noise-gate.
//
// Runs the scenario tier N times back-to-back and reports per-cell
// coefficient of variation. Used at provisioning time to qualify a
// candidate dedicated-bench machine before it goes into production.
//
// The orchestration logic (subprocess invocation of `cargo bench --bench
// scenarios` and parsing of bench/results/scenarios_metrics.jsonl) lives
// here; the COV computation and report rendering are in the
// chisel_bench::noise_gate library module.

use chisel_bench::noise_gate::report::{CellResult, GateResult, MIN_SAMPLES_PER_CELL};
use chisel_bench::noise_gate::{compute_cov, render_report};
use clap::Parser;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

#[derive(Parser, Debug)]
#[command(name = "chisel-bench-noise-gate", version)]
#[command(about = "Runs the scenario tier N times and reports per-cell COV")]
struct Cli {
    /// Provider name (e.g., "hetzner") — recorded in the report header.
    #[arg(long)]
    provider: String,

    /// Instance type (e.g., "ccx23") — recorded in the report header.
    #[arg(long)]
    instance_type: String,

    /// Number of back-to-back runs (minimum 2 — a COV needs two samples).
    #[arg(long, default_value = "5", value_parser = parse_runs)]
    runs: usize,

    /// Throughput COV threshold (fraction; 0.02 = 2%).
    #[arg(long, default_value = "0.02")]
    throughput_threshold: f64,

    /// p99 latency COV threshold (fraction; 0.05 = 5%).
    #[arg(long, default_value = "0.05")]
    p99_threshold: f64,

    /// Output path for the markdown report.
    #[arg(long, default_value = "noise-gate-report.md")]
    out: PathBuf,
}

/// clap value parser for `--runs`. Refuses anything below
/// `MIN_SAMPLES_PER_CELL` at argv-parse time rather than letting the run
/// proceed to a vacuous verdict: `--runs 0` executes no benchmark and yields
/// an empty cell set, and `--runs 1` yields cells whose COV is `compute_cov`'s
/// N=1 placeholder 0.0, so every cell clears every threshold by construction.
/// Both used to print "Noise gate PASSED" and exit 0 — on the one tool whose
/// entire purpose is to refuse hardware that cannot hold a stable number.
fn parse_runs(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|_| format!("`{s}` is not a non-negative whole number"))?;
    if n < MIN_SAMPLES_PER_CELL {
        return Err(format!(
            "must be at least {MIN_SAMPLES_PER_CELL}; a coefficient of variation \
             is undefined below {MIN_SAMPLES_PER_CELL} samples, so --runs {n} \
             would qualify a machine on evidence that cannot detect noise"
        ));
    }
    Ok(n)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(passed) => {
            if passed {
                println!("Noise gate PASSED — see {}", cli.out.display());
                ExitCode::SUCCESS
            } else {
                eprintln!("Noise gate FAILED — see {}", cli.out.display());
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("Noise gate error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<bool, Box<dyn std::error::Error>> {
    // Collect per-run cell metrics: keyed by (scenario, engine, mode).
    let mut samples: BTreeMap<(String, String, String), Vec<(f64, f64)>> = BTreeMap::new();

    for run_idx in 0..cli.runs {
        eprintln!("Run {} of {} ...", run_idx + 1, cli.runs);

        // Truncate scenarios_metrics.jsonl before each run so we read
        // only this run's results. Path is anchored at the bench
        // crate's manifest dir (resolved at compile time) so the
        // binary works regardless of the caller's cwd. The matching
        // write path is in bench/benches/scenarios.rs (also rooted at
        // CARGO_MANIFEST_DIR).
        //
        // The harness opens the file with File::create, which already
        // truncates on each `cargo bench`, so this remove is not what
        // prevents cross-run bleed. Its real value is fail-loud: if a
        // run produces no file at all (harness crashed before writing),
        // the read_to_string below errors instead of silently reusing
        // the previous run's rows and averaging in stale samples.
        let metrics_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("results")
            .join("scenarios_metrics.jsonl");
        if metrics_path.exists() {
            fs::remove_file(&metrics_path)?;
        }

        // Pin the cargo subprocess's cwd to the bench crate so it
        // finds the right Cargo.toml; otherwise `cargo bench --bench
        // scenarios` would fail when this binary is invoked from
        // outside bench/.
        let status = Command::new("cargo")
            .args(["bench", "--bench", "scenarios"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()?;
        if !status.success() {
            return Err(
                format!("Run {} failed: cargo bench exited {}", run_idx + 1, status).into(),
            );
        }

        // Parse the JSONL written by this run. Field names mirror
        // ScenarioResult's serde-serialized layout in
        // bench/src/runner.rs (`scenario`, `mode`,
        // `throughput_ops_per_sec`, `p99_ns`). `mode` is the combined
        // EngineMode::label() — `<engine>-<durability>` — so we split
        // on the first hyphen to recover the per-cell key
        // (scenario, engine, durability). Errors propagate with the
        // offending line number to surface schema regressions loudly
        // rather than silently averaging zeros.
        let contents = fs::read_to_string(&metrics_path)?;
        for (lineno, line) in contents.lines().enumerate() {
            let entry: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| format!("line {} in metrics jsonl: {}", lineno + 1, e))?;
            let scenario = entry["scenario"]
                .as_str()
                .ok_or("missing 'scenario' field")?
                .to_string();
            let mode_label = entry["mode"].as_str().ok_or("missing 'mode' field")?;
            let (engine, mode) = mode_label.split_once('-').ok_or_else(|| {
                format!(
                    "malformed mode label '{}': expected '<engine>-<durability>'",
                    mode_label
                )
            })?;
            let engine = engine.to_string();
            let mode = mode.to_string();
            let throughput = entry["throughput_ops_per_sec"]
                .as_f64()
                .ok_or("missing 'throughput_ops_per_sec' field")?;
            let p99_ns = entry["p99_ns"].as_f64().ok_or("missing 'p99_ns' field")?;
            samples
                .entry((scenario, engine, mode))
                .or_default()
                .push((throughput, p99_ns));
        }
    }

    // Compute per-cell COV and assemble the GateResult.
    let cells: Vec<CellResult> = samples
        .into_iter()
        .map(|((scenario, engine, mode), pairs)| {
            let throughput_samples: Vec<f64> = pairs.iter().map(|(t, _)| *t).collect();
            let p99_samples: Vec<f64> = pairs.iter().map(|(_, p)| *p).collect();
            let throughput = compute_cov(&throughput_samples);
            let p99_latency_ns = compute_cov(&p99_samples);
            // `--runs >= 2` is enforced at parse time, but a cell can still
            // fall short of that: a scenario present in only some runs'
            // metrics (a partially-written JSONL, a scenario list that
            // changed mid-sweep) yields fewer samples than `cli.runs`. Such a
            // cell must not pass on `compute_cov`'s N=1 placeholder 0.0.
            let samples = pairs.len();
            let passes = samples >= MIN_SAMPLES_PER_CELL
                && throughput.cov <= cli.throughput_threshold
                && p99_latency_ns.cov <= cli.p99_threshold;
            CellResult {
                scenario,
                engine,
                mode,
                samples,
                throughput,
                p99_latency_ns,
                passes,
            }
        })
        .collect();

    let result = GateResult {
        provider: cli.provider.clone(),
        instance_type: cli.instance_type.clone(),
        run_count: cli.runs,
        throughput_threshold: cli.throughput_threshold,
        p99_threshold: cli.p99_threshold,
        cells,
    };

    let report = render_report(&result);
    fs::write(&cli.out, report)?;

    Ok(result.all_pass())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── BENCH-10 (issue #109) ──
    // `--runs 0` measured nothing and `--runs 1` measured no variance, and
    // both exited 0 printing "Noise gate PASSED". Refusing at parse time is
    // what keeps a machine from being qualified on non-evidence.

    #[test]
    fn parse_runs_rejects_zero_and_one() {
        for bad in ["0", "1"] {
            let err = parse_runs(bad).expect_err("--runs {bad} must be rejected");
            assert!(
                err.contains("at least 2"),
                "error should name the floor, got: {err}"
            );
        }
    }

    #[test]
    fn parse_runs_accepts_two_and_above() {
        assert_eq!(parse_runs("2").unwrap(), 2);
        assert_eq!(parse_runs("5").unwrap(), 5);
    }

    #[test]
    fn parse_runs_rejects_non_numeric() {
        assert!(parse_runs("many").is_err());
        assert!(parse_runs("-1").is_err());
    }

    #[test]
    fn cli_rejects_runs_below_the_floor() {
        // End-to-end through clap, so the wiring of `value_parser` is
        // covered too — not just the bare function.
        let err = Cli::try_parse_from([
            "chisel-bench-noise-gate",
            "--provider",
            "hetzner",
            "--instance-type",
            "ccx23",
            "--runs",
            "1",
        ])
        .expect_err("clap must reject --runs 1");
        assert!(
            err.to_string().contains("at least 2"),
            "clap error should carry the reason, got: {err}"
        );
    }
}
