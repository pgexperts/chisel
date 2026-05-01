// Bench binary: the 270-cell micro grid. Iterates EngineMode::ALL × SIZES
// × the 9 row groups, registering each cell as a Criterion benchmark
// inside a per-row BenchmarkGroup with Throughput::Elements(N) for
// per-op normalization. Aux metrics (file-size delta + Chisel internal
// counter deltas) are captured per cell into bench/results/aux_metrics.jsonl.
//
// The three Criterion-shaped cell-runner helpers (run_*_cell) live here
// rather than in src/runner.rs because Criterion is in [dev-dependencies]
// and src/ code can't import dev-deps. The helpers are private.
//
// Run the full grid: `cargo bench --bench micro_grid`. Filter to one row:
// `cargo bench --bench micro_grid read-warm`.

#![allow(unused_imports)] // task 11 will use the remaining workload generators

use chisel_bench::runner::{
    apply_op, capture_aux_metrics_snapshot_restore, capture_aux_metrics_warm_read,
    drive_workload_with_tx_granularity, populate_snapshot, AuxMetricsWriter, CellId, EngineMode,
    CACHE_SIZE_PAGES,
};
use chisel_bench::workload::{
    gen_allocate, gen_delete_many, gen_delete_random, gen_read_random, gen_update_random, Workload,
};
use chisel_bench::Engine;
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BatchSize, BenchmarkGroup, BenchmarkId,
    Criterion, Throughput,
};
use tempfile::NamedTempFile;

/// Six log-spaced sizes, one per Chisel internal regime (master spec §3.2).
/// `prepop_count` calibrated to ~25 MB raw payload per cell (master spec §3.4).
const SIZES: [(usize, &str, usize); 6] = [
    (32, "32B", 800_000),
    (256, "256B", 100_000),
    (2_048, "2KB", 12_500),
    (16_384, "16KB", 1_500),
    (131_072, "128KB", 200),
    (1_048_576, "1MB", 25),
];

/// Hardcoded per-row seeds for workload determinism. Hardcoded rather
/// than derived from row names because Rust's DefaultHasher randomizes
/// per-process — derived seeds would change between invocations.
fn seed_for(row_name: &str) -> u64 {
    match row_name {
        "allocate-1pertx" => 0x4001,
        "allocate-1000pertx" => 0x4002,
        "read-warm" => 0x4003,
        "read-cold" => 0x4004,
        "update-1pertx" => 0x4005,
        "update-1000pertx" => 0x4006,
        "delete-1pertx" => 0x4007,
        "delete-1000pertx" => 0x4008,
        "delete_many" => 0x4009,
        _ => panic!("unknown row name: {row_name}"),
    }
}

/// Snapshot-restore cell-runner — used by 8 of 9 rows (allocate, cold-read,
/// update, delete, delete_many). Each iteration copies the pre-built
/// snapshot, opens a fresh engine, runs the workload's ops grouped into
/// transactions of `ops_per_tx`, then drops engine + tempfile.
fn run_snapshot_restore_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    snapshot_path: &std::path::Path,
    snapshot_ids: &[u64],
    workload: &Workload,
    ops_per_tx: usize,
) {
    group.bench_with_input(BenchmarkId::new(mode.label(), size_label), &(), |b, _| {
        b.iter_batched(
            || {
                let working = NamedTempFile::new().unwrap();
                std::fs::copy(snapshot_path, working.path()).unwrap();
                let engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                (engine, working)
            },
            |(mut engine, _working)| {
                drive_workload_with_tx_granularity(
                    &mut *engine,
                    workload,
                    ops_per_tx,
                    snapshot_ids,
                );
            },
            BatchSize::PerIteration,
        );
    });
}

/// Warm-read cell-runner — row 3 only. Engine is opened once per cell
/// and reused across all iterations; the cache warms naturally during
/// Criterion's warmup phase. Reads don't mutate engine-visible state,
/// so persistent engine is safe.
fn run_warm_read_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    engine: &mut dyn Engine,
    workload: &Workload,
    snapshot_ids: &[u64],
) {
    group.bench_with_input(BenchmarkId::new(mode.label(), size_label), &(), |b, _| {
        b.iter(|| {
            for op in &workload.ops {
                apply_op(engine, op, snapshot_ids, &mut Vec::new());
            }
        });
    });
}

/// Cold-read cell-runner — row 4 only. Engine open is INSIDE the timed
/// routine: cold means "fresh open, no values touched, first read is
/// the timed call" (master spec §5.2). File copy stays in setup.
fn run_cold_read_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    snapshot_path: &std::path::Path,
    snapshot_ids: &[u64],
    workload: &Workload,
) {
    group.bench_with_input(BenchmarkId::new(mode.label(), size_label), &(), |b, _| {
        b.iter_batched(
            || {
                let working = NamedTempFile::new().unwrap();
                std::fs::copy(snapshot_path, working.path()).unwrap();
                working
            },
            |working| {
                let mut engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                apply_op(
                    &mut *engine,
                    &workload.ops[0],
                    snapshot_ids,
                    &mut Vec::new(),
                );
            },
            BatchSize::PerIteration,
        );
    });
}

/// Rows 1 and 2: allocate, 1-per-tx and 1000-per-tx.
/// Empty pre-populated DB; workload is `ops_per_tx` Allocate ops; cell
/// runs one tx of `ops_per_tx` ops. Throughput::Elements(ops_per_tx).
fn bench_row_allocate_n_per_tx(
    c: &mut Criterion,
    aux: &mut AuxMetricsWriter,
    group_name: &str,
    ops_per_tx: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(ops_per_tx as u64));

    for (size_bytes, size_label, _) in SIZES {
        let workload = gen_allocate(ops_per_tx, size_bytes);
        for mode in EngineMode::ALL {
            // "Empty snapshot" = a fresh tempfile with a freshly-opened-and-closed
            // engine. populate_snapshot with prepop_count=0 gives exactly that.
            let snap = populate_snapshot(mode, size_bytes, 0).unwrap();
            run_snapshot_restore_cell(
                &mut group,
                mode,
                size_label,
                snap.path(),
                snap.ids(),
                &workload,
                ops_per_tx,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId {
                    row: leak_str(group_name),
                    mode: mode.label(),
                    size: size_label,
                },
                mode,
                snap.path(),
                snap.ids(),
                &workload,
                ops_per_tx,
            ))
            .unwrap();
        }
    }

    group.finish();
}

/// Row 3: read warm. Persistent engine across iterations — cache warms
/// naturally. Workload is 64 random reads (cycled per iteration).
fn bench_row_read_warm(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    let mut group = c.benchmark_group("read-warm");
    group.throughput(Throughput::Elements(64));

    for (size_bytes, size_label, prepop_count) in SIZES {
        let workload = gen_read_random(seed_for("read-warm"), prepop_count, 64);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            let mut engine = mode.open(snap.path(), CACHE_SIZE_PAGES).unwrap();
            run_warm_read_cell(
                &mut group,
                mode,
                size_label,
                &mut *engine,
                &workload,
                snap.ids(),
            );
            aux.append(&capture_aux_metrics_warm_read(
                CellId {
                    row: "read-warm",
                    mode: mode.label(),
                    size: size_label,
                },
                &mut *engine,
                &workload,
                snap.ids(),
            ))
            .unwrap();
            // engine drops here, before snap drops at end of for body
        }
    }

    group.finish();
}

/// Row 4: read cold. Fresh engine opened inside the timed routine; "cold"
/// means first read after open. Workload is 1 read.
fn bench_row_read_cold(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    let mut group = c.benchmark_group("read-cold");
    group.throughput(Throughput::Elements(1));

    for (size_bytes, size_label, prepop_count) in SIZES {
        let workload = gen_read_random(seed_for("read-cold"), prepop_count, 1);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
            run_cold_read_cell(
                &mut group,
                mode,
                size_label,
                snap.path(),
                snap.ids(),
                &workload,
            );
            aux.append(&capture_aux_metrics_snapshot_restore(
                CellId {
                    row: "read-cold",
                    mode: mode.label(),
                    size: size_label,
                },
                mode,
                snap.path(),
                snap.ids(),
                &workload,
                /*ops_per_tx*/ 1,
            ))
            .unwrap();
        }
    }

    group.finish();
}

/// `CellId.row` is `&'static str`. The two allocate rows have group names
/// passed in dynamically as a parameter — we leak them to satisfy the
/// `'static` requirement. There are exactly 2 such leaks per process
/// (one per allocate row); negligible memory.
fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn micro_grid(c: &mut Criterion) {
    let mut aux = AuxMetricsWriter::create(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/results/aux_metrics.jsonl"
    ))
    .unwrap();

    bench_row_allocate_n_per_tx(c, &mut aux, "allocate-1pertx", 1);
    bench_row_allocate_n_per_tx(c, &mut aux, "allocate-1000pertx", 1000);
    bench_row_read_warm(c, &mut aux);
    bench_row_read_cold(c, &mut aux);
    // Tasks 11 will add the remaining 5 calls (update × 2, delete × 2, delete_many).
}

criterion_group!(benches, micro_grid);
criterion_main!(benches);
