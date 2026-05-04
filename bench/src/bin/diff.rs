// CLI entry point for chisel-bench-diff. Argv parsing + file I/O
// only; all logic lives in the chisel_bench::diff library module.
//
// Output: markdown to stdout, intended for capture by the bench
// workflow and posting as a PR comment via peter-evans/create-or-
// update-comment with the marker `<!-- chisel-bench-diff -->`.

use chisel_bench::diff::{compare, parse, render};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chisel-bench-diff", version)]
#[command(about = "Diff two results.json files and render a PR-comment markdown report")]
struct Cli {
    /// Path to the baseline results.json (typically from main).
    #[arg(long)]
    baseline: PathBuf,

    /// Path to the PR HEAD results.json.
    #[arg(long)]
    pr: PathBuf,

    /// Reserved for when micro-grid diffing is added in a future PR.
    /// Currently a no-op; included so the workflow YAML doesn't need
    /// to change when that future PR lands.
    #[arg(long, default_value_t = false)]
    #[allow(dead_code)]
    scenarios_only: bool,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let baseline = parse::parse_results_json(&cli.baseline)?;
    let pr = parse::parse_results_json(&cli.pr)?;
    let report = compare::compare(
        &baseline,
        &pr,
        cli.baseline.clone(),
        cli.pr.clone(),
        chrono::Utc::now(),
    );
    let md = render::render_markdown(&report);
    print!("{md}");
    Ok(())
}
