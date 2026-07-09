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
use rand_distr::{Distribution, WeightedIndex};

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
///
/// Unlike YCSB-A/B/document-store (which have no Deletes and so can
/// use the stateless `mix_operations` over a precomputed access vector),
/// this workload must be generated with a live-index pool. A naive
/// independent-uniform sampler over `[0, prepop_count)` would emit
/// Read/Update/Delete ops referencing already-deleted indices, which
/// engines reject at apply time with `InvalidHandle`. The pool tracks
/// which indices in apply_op's `(snapshot_ids ++ new_ids)` resolve
/// view are currently allocated; non-Allocate ops sample only from
/// it, and Delete removes its target. With symmetric 25/25 alloc /
/// delete weights, E[|live|] stays at prepop_count, so the
/// "demote to Allocate when live is empty" branch is purely defensive
/// against pathological RNG sequences and never triggers in practice.
pub fn gen_mutation_log(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let op_count = 100_000;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let weighted = WeightedIndex::new([0.25, 0.25, 0.25, 0.25]).unwrap();

    // Sizes for Allocate and Update — uniform [64, 4096] inclusive,
    // drawn from a separate stream (seed+1) to match the earlier
    // generator's seeding convention and keep the size sequence
    // independent of the op-kind / position sampling.
    let mut size_rng = ChaCha8Rng::seed_from_u64(seed.wrapping_add(1));

    let mut live: Vec<usize> = (0..prepop_count).collect();
    let mut next_alloc_index = prepop_count;
    let mut ops: Vec<Operation> = Vec::with_capacity(op_count);

    for _ in 0..op_count {
        let mut kind = weighted.sample(&mut rng);
        // Defensive: Read/Update/Delete need at least one live index.
        if kind != 0 && live.is_empty() {
            kind = 0;
        }
        let op = match kind {
            0 => {
                let idx = next_alloc_index;
                next_alloc_index += 1;
                live.push(idx);
                Operation::Allocate {
                    size: size_rng.gen_range(64..=4096),
                }
            }
            1 => {
                let j = rng.gen_range(0..live.len());
                Operation::Read {
                    alloc_index: live[j],
                }
            }
            2 => {
                let j = rng.gen_range(0..live.len());
                Operation::Update {
                    alloc_index: live[j],
                    size: size_rng.gen_range(64..=4096),
                }
            }
            3 => {
                // swap_remove is O(1); ordering is irrelevant since
                // selection above is uniform-random.
                let j = rng.gen_range(0..live.len());
                let alloc_index = live.swap_remove(j);
                Operation::Delete { alloc_index }
            }
            _ => unreachable!("WeightedIndex over 4 weights"),
        };
        ops.push(op);
    }

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
    fn mutation_log_op_sequence_is_engine_applicable() {
        // Walk the generated workload simulating apply_op's view of the
        // live-index set. Reads/Updates/Deletes that reference a dead or
        // never-allocated index would panic the engine — assert here that
        // the generator never produces such references. Catches the bug
        // class where a stateful workload's live-set tracking gets out of
        // sync with the index space apply_op resolves through.
        let seed = seed_for("mutation-log");
        let prepop = gen_mutation_log_prepopulate(seed);
        let workload = gen_mutation_log(seed);
        let prepop_count = prepop.ops.len();

        let mut live: std::collections::HashSet<usize> = (0..prepop_count).collect();
        let mut next_alloc_index = prepop_count;

        for (i, op) in workload.ops.iter().enumerate() {
            match op {
                Operation::Allocate { .. } => {
                    assert!(live.insert(next_alloc_index));
                    next_alloc_index += 1;
                }
                Operation::Read { alloc_index } | Operation::Update { alloc_index, .. } => {
                    assert!(
                        live.contains(alloc_index),
                        "op {i} ({op:?}) references dead index {alloc_index}"
                    );
                }
                Operation::Delete { alloc_index } => {
                    assert!(
                        live.remove(alloc_index),
                        "op {i} (Delete) references dead index {alloc_index}"
                    );
                }
                Operation::DeleteMany { .. } => {
                    panic!("mutation-log workload should not emit DeleteMany");
                }
            }
        }
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
