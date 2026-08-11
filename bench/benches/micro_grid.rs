// Bench binary: the micro grid. Iterates EngineMode::ALL (5) × SIZES (6)
// × 8 row groups, registering each cell as a Criterion benchmark
// inside a per-row BenchmarkGroup with Throughput::Elements(N) for
// per-op normalization. Aux metrics (file-size delta + Chisel internal
// counter deltas) are captured per cell into bench/results/aux_metrics.jsonl.
//
// 230 cells, not the 5 × 6 × 8 = 240 the dimensions suggest: the two
// 1000-per-tx write rows skip their 1MB column on the bench-runtime budget
// (see TX_BUDGET_BYTES). Rows and their cell counts:
//
//   allocate-1pertx     30    read-warm          30    update-1pertx     30
//   allocate-1000pertx  25    read-cold          30    update-1000pertx  25
//   delete-1pertx       30    delete-1000pertx   30
//
// The count is worth keeping honest: a reader checking that a run completed
// compares against it, and it feeds the raw-archive size estimate in
// summary/discover.rs.
//
// The three Criterion-shaped cell-runner helpers (run_*_cell) live here
// rather than in src/runner.rs because Criterion is in [dev-dependencies]
// and src/ code can't import dev-deps. The helpers are private.
//
// Run the full grid: `cargo bench --bench micro_grid`. Filter to one row:
// `cargo bench --bench micro_grid read-warm`.

use chisel_bench::runner::{
    apply_op, capture_aux_metrics_snapshot_restore, capture_aux_metrics_warm_read,
    drive_workload_with_tx_granularity, populate_snapshot, AuxMetricsWriter, CellId, EngineMode,
    CACHE_SIZE_PAGES,
};
use chisel_bench::workload::{
    gen_allocate, gen_delete_random, gen_read_random, gen_update_random, Workload,
};
use chisel_bench::Engine;
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BatchSize, BenchmarkGroup, BenchmarkId,
    Criterion, Throughput,
};
use tempfile::NamedTempFile;

// I93: pin mimalloc as this bench binary's process allocator. Chisel is
// allocation-heavy by construction (a boxed 8 KB page per cache entry, a Vec
// per read), as is redb; the system malloc taxes them unevenly versus SQLite's
// C core, which does far less Rust-side heap traffic. A fast, fixed allocator
// removes that variable and reports each engine's realistic best case. Scoped
// to the bench binary (publish=false) — a #[global_allocator] is process-global
// and a library must never impose one on its consumers.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

/// Per-transaction byte budget for the 1000-per-tx rows. Cells where
/// `ops_per_tx * size_bytes` exceeds this are skipped.
///
/// This is a BENCH-RUNTIME budget, not a Chisel capacity limit. It used to be
/// the latter — 8 MiB, justified as avoiding "Chisel's CacheFull ... cache
/// hard ceiling is ~16 MB" — but that ceiling was removed when the bench
/// engines gained the spillway at production-default scale (1024 × the cache
/// budget; see `ChiselEngine::open_file` and `POPULATE_TX_BUDGET_BYTES`).
/// Measured against the current engine at `CACHE_SIZE_PAGES`, every cell the
/// old budget excluded completes:
///
///   allocate-1000pertx  16KB (16 MB/tx)    0.71 s
///   allocate-1000pertx 128KB (128 MB/tx)   2.70 s
///   allocate-1000pertx   1MB (1 GB/tx)    18.51 s
///
/// So capacity is no longer the constraint; wall clock is. 128 MiB admits the
/// 16KB and 128KB columns, which were losing real coverage for no reason, and
/// still excludes 1MB × 1000 — at ~18.5 s per iteration that one cell would
/// add hours to every micro-grid run across the five engine modes.
///
/// Affects allocate-1000pertx and update-1000pertx at 1MB only.
/// delete-1000pertx does not consult this: deleting frees rather than writes
/// `ops * size` bytes, and it clamps its op count to `prepop_count` anyway.
const TX_BUDGET_BYTES: usize = 128 * 1024 * 1024;

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
        _ => panic!("unknown row name: {row_name}"),
    }
}

/// Snapshot-restore cell-runner — used by every row except warm-read
/// (allocate, cold-read, update, delete). Each iteration copies the pre-built
/// snapshot, opens a fresh engine, and runs the workload's ops grouped into
/// transactions of `ops_per_tx`. Engine + tempfile are RETURNED from the timed
/// routine, not dropped inside it, so their teardown lands outside the
/// measured span.
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
            |(mut engine, working)| {
                drive_workload_with_tx_granularity(
                    &mut *engine,
                    workload,
                    ops_per_tx,
                    snapshot_ids,
                );
                // Hand engine + tempfile back to Criterion instead of dropping
                // them here. `iter_batched` does `drop(black_box(output))`
                // AFTER `measurement.end`, so returning the tuple moves engine
                // teardown out of the timed span. Dropping it inside charged
                // this row for SQLite's close-time WAL checkpoint (up to ~8 MB
                // of frames) and a NamedTempFile unlink of a file up to ~25 MB
                // — neither of which is the operation the row is named after,
                // and both of which cost differently per engine.
                (engine, working)
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
                // Open stays inside the span deliberately (that IS cold read);
                // CLOSE does not. Returning the pair defers both the engine
                // teardown and the tempfile unlink until after
                // `measurement.end` — see `run_snapshot_restore_cell`.
                (engine, working)
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
        if ops_per_tx * size_bytes > TX_BUDGET_BYTES {
            continue; // over the bench-runtime budget; see TX_BUDGET_BYTES
        }
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
            // Ordering matters: aux capture reuses the SAME engine the bench just
            // warmed, so the counter delta it records is a cache-hit-dominated
            // read — exactly the warm-read regime this row is meant to measure.
            // Capturing before the bench would instead sample a cold first read.
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

/// Rows 5 and 6: update, 1-per-tx and 1000-per-tx.
/// Same skip-when-too-big pattern as `bench_row_allocate_n_per_tx`:
/// 1000 × 16KB updates exceed Chisel's per-tx cache budget.
fn bench_row_update_n_per_tx(
    c: &mut Criterion,
    aux: &mut AuxMetricsWriter,
    group_name: &str,
    ops_per_tx: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(ops_per_tx as u64));

    for (size_bytes, size_label, prepop_count) in SIZES {
        if ops_per_tx * size_bytes > TX_BUDGET_BYTES {
            continue; // over the bench-runtime budget; see TX_BUDGET_BYTES
        }
        let workload =
            gen_update_random(seed_for(group_name), prepop_count, ops_per_tx, size_bytes);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
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

/// Rows 7 and 8: delete, 1-per-tx and 1000-per-tx.
/// Deletes don't accumulate dirty pages the same way as allocates/updates,
/// so no TX_BUDGET_BYTES skip is needed. The only constraint is
/// gen_delete_random's count <= prepop_count assertion: clamp to fit.
fn bench_row_delete_n_per_tx(
    c: &mut Criterion,
    aux: &mut AuxMetricsWriter,
    group_name: &str,
    ops_per_tx: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(ops_per_tx as u64));

    for (size_bytes, size_label, prepop_count) in SIZES {
        // For the 1000-per-tx variant at the 1MB row, prepop_count=25 < 1000,
        // so we'd violate gen_delete_random's count <= prepop_count assert.
        // Clamp to the smaller of the two; reported throughput stays at
        // ops_per_tx for cross-row comparability, the actual delete count
        // is just smaller.
        let workload_count = ops_per_tx.min(prepop_count);
        if workload_count == 0 {
            continue;
        }
        let workload = gen_delete_random(seed_for(group_name), prepop_count, workload_count);
        for mode in EngineMode::ALL {
            let snap = populate_snapshot(mode, size_bytes, prepop_count).unwrap();
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

/// `CellId.row` is `&'static str`, but the allocate, update, and delete
/// rows receive their group name as a dynamic `&str` parameter — so each
/// cell's CellId is built by leaking that name to satisfy the `'static`
/// bound. leak_str therefore runs once per emitted (size × mode) cell of
/// those three row kinds (the read rows use `&'static str` literals and do
/// not leak), not the "2 per process" an earlier note claimed. Each leak is
/// a short row-name string, so total leaked memory stays negligible.
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
    bench_row_update_n_per_tx(c, &mut aux, "update-1pertx", 1);
    // update-1000pertx and delete-1000pertx were previously not registered at
    // all, on the rationale that "1000 random updates/deletes pin ~1000
    // distinct dirty data pages, exceeding Chisel's 2048-page cache ceiling"
    // and so "are not measurable under default cache settings". Both halves of
    // that are stale: CACHE_SIZE_PAGES is 256, not 2048, and the strict cache
    // ceiling itself is gone now that the bench engines run with the spillway
    // at production-default scale. Measured at 1000 ops per tx, largest size
    // each row admits: update 1.27 s, delete 0.80 s. They are measurable, so
    // they are registered.
    bench_row_update_n_per_tx(c, &mut aux, "update-1000pertx", 1000);
    bench_row_delete_n_per_tx(c, &mut aux, "delete-1pertx", 1);
    bench_row_delete_n_per_tx(c, &mut aux, "delete-1000pertx", 1000);
}

criterion_group!(benches, micro_grid);
criterion_main!(benches);
