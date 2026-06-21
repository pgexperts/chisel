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

use chisel_bench::noise_gate::report::{CellResult, GateResult};
use chisel_bench::noise_gate::{compute_cov, render_report};
use clap::Parser;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

#[derive(Parser)]
#[command(name = "chisel-bench-noise-gate", version)]
#[command(about = "Runs the scenario tier N times and reports per-cell COV")]
struct Cli {
    /// Provider name (e.g., "hetzner") — recorded in the report header.
    #[arg(long)]
    provider: String,

    /// Instance type (e.g., "ccx23") — recorded in the report header.
    #[arg(long)]
    instance_type: String,

    /// Number of back-to-back runs.
    #[arg(long, default_value = "5")]
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
            let passes = throughput.cov <= cli.throughput_threshold
                && p99_latency_ns.cov <= cli.p99_threshold;
            CellResult {
                scenario,
                engine,
                mode,
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
