// CLI entry point for the chisel-bench-summarize post-processor.
// Reads PR 4b's bench output (Criterion sample.json + aux_metrics.jsonl)
// PLUS PR 6's scenarios_metrics.jsonl, and emits summary.md +
// results.json + raw/ under bench/results/<UTC>/.
//
// All logic lives in the chisel_bench::summary library module; this
// file is just argv parsing, error printing, and exit codes.

use chisel_bench::summary::{
    copy_raw_archive, discover_cells, gather_metadata, load_scenarios_jsonl, render_json,
    render_markdown, DiscoverError,
};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chisel-bench-summarize", version)]
#[command(about = "Post-process Criterion + aux-metrics + scenarios output")]
struct Cli {
    /// Output directory (default: bench/results/<UTC-ISO8601>/)
    #[arg(long)]
    out: Option<PathBuf>,

    /// Criterion output directory.
    #[arg(long, default_value = "target/criterion")]
    criterion: PathBuf,

    /// Aux-metrics JSONL produced by the micro-grid bench.
    #[arg(long, default_value = "bench/results/aux_metrics.jsonl")]
    aux: PathBuf,

    /// Scenarios-metrics JSONL produced by the scenario bench.
    #[arg(long, default_value = "bench/results/scenarios_metrics.jsonl")]
    scenarios: PathBuf,
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
    // 1. Discover cells (micro grid) and load scenarios (PR 6).
    //
    // discover_cells errors when the Criterion directory is missing or
    // empty. When the user has scenarios but hasn't run the micro grid
    // (a legitimate workflow once the tiers diverged), absorb those two
    // error variants as "no cells" and let the unified empty-check
    // below decide whether to bail. Other errors (I/O, parse) still
    // propagate.
    // DiscoverError has only the CriterionDirNotFound / NoCellsFound
    // variants today; if a new variant is added later, this match will
    // fail to compile and force an explicit decision rather than
    // silently swallowing it. unwrap_or_default() would silently swallow
    // any future new variant, so we suppress the lint here.
    #[allow(clippy::manual_unwrap_or_default)]
    let cells = match discover_cells(&cli.criterion, &cli.aux) {
        Ok(cells) => cells,
        Err(DiscoverError::CriterionDirNotFound(_) | DiscoverError::NoCellsFound(_)) => Vec::new(),
    };
    let scenarios = load_scenarios_jsonl(&cli.scenarios);
    if cells.is_empty() && scenarios.is_empty() {
        return Err("no cells or scenarios discovered — did you run cargo bench?".into());
    }

    // 2. Resolve output directory.
    let out_dir = cli.out.unwrap_or_else(|| {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
        PathBuf::from(format!("bench/results/{ts}"))
    });
    std::fs::create_dir_all(&out_dir)?;

    // 3. Gather metadata.
    let metadata = gather_metadata(&cli.criterion, &cli.aux, cells.len())?;

    // 4. Render markdown + JSON.
    let md = render_markdown(&cells, &scenarios, &metadata);
    let json = render_json(&cells, &scenarios, &metadata);

    // 5. Write output artifacts.
    std::fs::write(out_dir.join("summary.md"), &md)?;
    std::fs::write(
        out_dir.join("results.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    copy_raw_archive(&cli.criterion, &out_dir.join("raw"))?;

    // 6. Tell user where to find it.
    println!(
        "Wrote {} cells + {} scenarios to {}",
        cells.len(),
        scenarios.len(),
        out_dir.display()
    );
    println!(
        "  - summary.md  ({} bytes)",
        std::fs::metadata(out_dir.join("summary.md"))?.len()
    );
    println!(
        "  - results.json ({} bytes)",
        std::fs::metadata(out_dir.join("results.json"))?.len()
    );
    println!("  - raw/ (Criterion estimates.json + sample.json archive)");

    Ok(())
}
