// Bench-binary entry for the scenario tier. Iterates the 16 cells
// (4 scenarios × 4 modes: chisel-strict, chisel-mem, redb-strict,
// sqlite-strict), calls run_scenario_cell for each,
// streams ScenarioResult rows to bench/results/scenarios_metrics.jsonl
// (one JSON object per line, flushed after each cell for crash
// resilience).
//
// We do NOT use Criterion here — the master-spec runtime budget of
// 1-6 minutes per full tier rules out Criterion's many-samples-per-bench
// model. Inline Instant::now() timing inside run_scenario_cell gives
// per-op latency distribution from a single 100K-op run, which is
// already self-averaging.
//
// Cargo wiring: this file is the source for [[bench]] name = "scenarios"
// with harness = false in bench/Cargo.toml. Run via:
//
//   cargo bench --bench scenarios

use chisel_bench::runner::{run_scenario_cell, EngineMode};
use chisel_bench::scenarios::{
    gen_document_store, gen_document_store_prepopulate, gen_mutation_log,
    gen_mutation_log_prepopulate, gen_ycsb_a, gen_ycsb_a_prepopulate, gen_ycsb_b,
    gen_ycsb_b_prepopulate, seed_for,
};
use chisel_bench::workload::Workload;
use std::io::Write;

// Modes compared per scenario. chisel-mem (in-memory Chisel) sits directly
// after chisel-strict so the per-scenario fsync/file-I/O tax reads off the two
// columns' delta (I92). redb/sqlite stay strict here — their unsafe variants
// are a micro-grid concern, not a scenario one.
const SCENARIO_MODES: &[EngineMode] = &[
    EngineMode::ChiselStrict,
    EngineMode::ChiselMemory,
    EngineMode::RedbStrict,
    EngineMode::SqliteStrict,
];

fn main() -> std::io::Result<()> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = format!("{manifest_dir}/results/scenarios_metrics.jsonl");
    if let Some(p) = std::path::Path::new(&out_path).parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut writer = std::fs::File::create(&out_path)?;

    let mut cells_written = 0usize;
    for (scenario_name, prepop, workload) in build_scenarios() {
        for &mode in SCENARIO_MODES {
            eprintln!("running {scenario_name} on {} ...", mode.label());
            let result = run_scenario_cell(mode, scenario_name, &prepop, &workload);
            serde_json::to_writer(&mut writer, &result)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            eprintln!(
                "  total {:.2}s, p50 {:.0} ns, p99 {:.0} ns, throughput {:.1} ops/s",
                result.total_wall_clock_ns as f64 / 1e9,
                result.p50_ns,
                result.p99_ns,
                result.throughput_ops_per_sec,
            );
            cells_written += 1;
        }
    }
    eprintln!("Wrote {cells_written} cells to {out_path}");
    Ok(())
}

fn build_scenarios() -> Vec<(&'static str, Workload, Workload)> {
    let s1 = seed_for("ycsb-a");
    let s2 = seed_for("ycsb-b");
    let s3 = seed_for("mutation-log");
    let s4 = seed_for("document-store");
    vec![
        ("ycsb-a", gen_ycsb_a_prepopulate(s1), gen_ycsb_a(s1)),
        ("ycsb-b", gen_ycsb_b_prepopulate(s2), gen_ycsb_b(s2)),
        (
            "mutation-log",
            gen_mutation_log_prepopulate(s3),
            gen_mutation_log(s3),
        ),
        (
            "document-store",
            gen_document_store_prepopulate(s4),
            gen_document_store(s4),
        ),
    ]
}
