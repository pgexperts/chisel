// Scenario tier — four end-to-end YCSB-style workloads from master
// spec §4. Each scenario is a thin composition of the workload-data
// primitives (zipfian_indices, lognormal_sizes, mix_operations) plus
// the spec's parameters.
//
// Each scenario has a paired `gen_<name>_prepopulate` generator that
// produces the pre-population workload (allocate ops at the right
// sizes); `run_scenario_cell` runs prepopulate untimed before
// running the scenario workload timed.
//
// Hardcoded per-scenario seeds (see `seed_for`) — DefaultHasher
// randomizes per-process, so derived seeds wouldn't reproduce.

use crate::workload::{
    lognormal_sizes, mix_operations, zipfian_indices, OpKind, Operation, Workload,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Per-scenario seeds. Hardcoded rather than hashed — Rust's
/// DefaultHasher randomizes per-process state so derived seeds
/// would not reproduce across runs.
pub fn seed_for(scenario: &str) -> u64 {
    match scenario {
        "ycsb-a" => 0x6001,
        "ycsb-b" => 0x6002,
        "mutation-log" => 0x6003,
        "document-store" => 0x6004,
        _ => panic!("unknown scenario: {scenario}"),
    }
}

/// S1: YCSB-A — 100K records × 1KB pre-pop, 100K ops 50/50 read/update,
/// Zipfian θ=0.99 (heavy skew, ~75% of accesses to ~10% of records).
/// Master spec §4.1.
pub fn gen_ycsb_a(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let op_count = 100_000;
    let theta = 0.99;
    // Each Read or Update consumes one access entry; total = op_count.
    let access = zipfian_indices(seed, op_count, prepop_count, theta);
    // Each Update consumes one size; ~50K of them. To stay simple,
    // pre-allocate op_count sizes (extras are unused). All 1KB fixed.
    let sizes = vec![1024usize; op_count];
    let ops = mix_operations(
        seed,
        op_count,
        &[(OpKind::Read, 0.5), (OpKind::Update, 0.5)],
        &access,
        &sizes,
    );
    Workload {
        name: "ycsb-a".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for YCSB-A: 100K Allocate ops of 1KB each.
pub fn gen_ycsb_a_prepopulate(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let ops: Vec<Operation> = (0..prepop_count)
        .map(|_| Operation::Allocate { size: 1024 })
        .collect();
    Workload {
        name: "ycsb-a-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}

/// S2: YCSB-B — same setup as S1, mix is 95% read / 5% update.
/// Master spec §4.2.
pub fn gen_ycsb_b(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let op_count = 100_000;
    let theta = 0.99;
    let access = zipfian_indices(seed, op_count, prepop_count, theta);
    let sizes = vec![1024usize; op_count];
    let ops = mix_operations(
        seed,
        op_count,
        &[(OpKind::Read, 0.95), (OpKind::Update, 0.05)],
        &access,
        &sizes,
    );
    Workload {
        name: "ycsb-b".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for YCSB-B: identical to YCSB-A (100K × 1KB).
pub fn gen_ycsb_b_prepopulate(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let ops: Vec<Operation> = (0..prepop_count)
        .map(|_| Operation::Allocate { size: 1024 })
        .collect();
    Workload {
        name: "ycsb-b-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}

/// S3: Mutation Log — 10K records, sizes uniform [64B, 4KB], 100K ops
/// 25%/25%/25%/25% allocate/read/update/delete, uniform random access.
/// Master spec §4.3.
pub fn gen_mutation_log(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let op_count = 100_000;
    // Uniform random access; a fresh Vec each call (deterministic via seed).
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let access: Vec<usize> = (0..op_count)
        .map(|_| rng.gen_range(0..prepop_count))
        .collect();
    // Sizes uniform [64, 4096] inclusive.
    let mut size_rng = ChaCha8Rng::seed_from_u64(seed.wrapping_add(1));
    let sizes: Vec<usize> = (0..op_count)
        .map(|_| size_rng.gen_range(64..=4096))
        .collect();
    let ops = mix_operations(
        seed,
        op_count,
        &[
            (OpKind::Allocate, 0.25),
            (OpKind::Read, 0.25),
            (OpKind::Update, 0.25),
            (OpKind::Delete, 0.25),
        ],
        &access,
        &sizes,
    );
    Workload {
        name: "mutation-log".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for Mutation Log: 10K Allocate ops, sizes uniform
/// [64B, 4KB] inclusive.
pub fn gen_mutation_log_prepopulate(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let mut rng = ChaCha8Rng::seed_from_u64(seed.wrapping_add(2));
    let ops: Vec<Operation> = (0..prepop_count)
        .map(|_| Operation::Allocate {
            size: rng.gen_range(64..=4096),
        })
        .collect();
    Workload {
        name: "mutation-log-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}

/// S4: Document Store — 10K records, log-normal sizes (median 4KB,
/// p99 ≈ 1MB), 50K ops 70%/20%/10% read/allocate/update, Zipfian
/// θ=0.7 (moderate skew, more spread than YCSB-A's 0.99).
/// Master spec §4.4.
pub fn gen_document_store(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let op_count = 50_000;
    let theta = 0.7;
    let access = zipfian_indices(seed, op_count, prepop_count, theta);
    // Sizes for Allocate (~10K of them) and Update (~5K of them):
    // lognormal with median 4KB, p99 1MB.
    let sizes = lognormal_sizes(seed.wrapping_add(1), op_count, 4096, 1_048_576);
    let ops = mix_operations(
        seed,
        op_count,
        &[
            (OpKind::Read, 0.70),
            (OpKind::Allocate, 0.20),
            (OpKind::Update, 0.10),
        ],
        &access,
        &sizes,
    );
    Workload {
        name: "document-store".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for Document Store: 10K Allocate ops with log-normal
/// sizes (median 4KB, p99 ≈ 1MB).
pub fn gen_document_store_prepopulate(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let sizes = lognormal_sizes(seed.wrapping_add(2), prepop_count, 4096, 1_048_576);
    let ops: Vec<Operation> = sizes
        .iter()
        .map(|&size| Operation::Allocate { size })
        .collect();
    Workload {
        name: "document-store-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_for_returns_distinct_seeds_per_scenario() {
        let names = ["ycsb-a", "ycsb-b", "mutation-log", "document-store"];
        let seeds: Vec<u64> = names.iter().map(|n| seed_for(n)).collect();
        let unique: std::collections::HashSet<u64> = seeds.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "scenario seeds must be distinct");
    }

    #[test]
    #[should_panic(expected = "unknown scenario")]
    fn seed_for_panics_on_unknown_scenario() {
        let _ = seed_for("not-a-real-scenario");
    }

    #[test]
    fn gen_ycsb_a_shape() {
        let seed = seed_for("ycsb-a");
        let prepop = gen_ycsb_a_prepopulate(seed);
        let workload = gen_ycsb_a(seed);

        // Pre-population: 100K Allocate ops of 1KB each
        assert_eq!(prepop.name, "ycsb-a-prepopulate");
        assert_eq!(prepop.ops.len(), 100_000);
        assert_eq!(prepop.prepop_count, 0);
        for op in &prepop.ops {
            assert!(matches!(op, Operation::Allocate { size: 1024 }));
        }

        // Main workload: 100K ops, 50/50 read/update over 100K records
        assert_eq!(workload.name, "ycsb-a");
        assert_eq!(workload.ops.len(), 100_000);
        assert_eq!(workload.prepop_count, 100_000);
        let reads = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let updates = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        // 50/50 mix over 100K → expect ~50K of each (binomial ±500)
        assert!((49_500..=50_500).contains(&reads));
        assert!((49_500..=50_500).contains(&updates));
        assert_eq!(reads + updates, 100_000);
    }

    #[test]
    fn gen_mutation_log_shape() {
        let seed = seed_for("mutation-log");
        let prepop = gen_mutation_log_prepopulate(seed);
        let workload = gen_mutation_log(seed);

        // Pre-population: 10K Allocate ops, sizes uniform [64, 4096]
        assert_eq!(prepop.name, "mutation-log-prepopulate");
        assert_eq!(prepop.ops.len(), 10_000);
        assert_eq!(prepop.prepop_count, 0);
        for op in &prepop.ops {
            match op {
                Operation::Allocate { size } => {
                    assert!(
                        (64..=4096).contains(size),
                        "prepop size {size} out of [64, 4096]"
                    );
                }
                _ => panic!("expected only Allocate ops in prepop"),
            }
        }

        // Main workload: 100K ops, 25/25/25/25 alloc/read/update/delete
        assert_eq!(workload.name, "mutation-log");
        assert_eq!(workload.ops.len(), 100_000);
        assert_eq!(workload.prepop_count, 10_000);
        let alloc = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Allocate { .. }))
            .count();
        let read = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let update = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        let delete = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Delete { .. }))
            .count();
        // 25/25/25/25 over 100K → expect ~25K each (multinomial ±500)
        assert!((24_500..=25_500).contains(&alloc));
        assert!((24_500..=25_500).contains(&read));
        assert!((24_500..=25_500).contains(&update));
        assert!((24_500..=25_500).contains(&delete));
        assert_eq!(alloc + read + update + delete, 100_000);
    }

    #[test]
    fn gen_document_store_shape() {
        let seed = seed_for("document-store");
        let prepop = gen_document_store_prepopulate(seed);
        let workload = gen_document_store(seed);

        // Pre-population: 10K Allocate ops with log-normal sizes
        assert_eq!(prepop.name, "document-store-prepopulate");
        assert_eq!(prepop.ops.len(), 10_000);
        assert_eq!(prepop.prepop_count, 0);
        for op in &prepop.ops {
            match op {
                Operation::Allocate { size } => {
                    // Sizes clamped to [16, 4_194_304] by lognormal_sizes
                    assert!(
                        (16usize..=4_194_304).contains(size),
                        "prepop size {size} out of clamp range"
                    );
                }
                _ => panic!("expected only Allocate ops in prepop"),
            }
        }

        // Main workload: 50K ops, 70/20/10 read/alloc/update
        assert_eq!(workload.name, "document-store");
        assert_eq!(workload.ops.len(), 50_000);
        assert_eq!(workload.prepop_count, 10_000);
        let read = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let alloc = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Allocate { .. }))
            .count();
        let update = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        // 70/20/10 over 50K → expect 35K/10K/5K (multinomial ±500)
        assert!((34_500..=35_500).contains(&read));
        assert!((9_500..=10_500).contains(&alloc));
        assert!((4_500..=5_500).contains(&update));
        assert_eq!(read + alloc + update, 50_000);
    }
}
