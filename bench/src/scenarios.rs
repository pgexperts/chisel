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

// Imports for the workload primitives (zipfian_indices, lognormal_sizes,
// mix_operations, OpKind, Operation, Workload) and rand types are
// added by Task 6 when the first scenario generator (gen_ycsb_a) lands.
// For Task 5's seed_for + tests, only std types are needed.

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
}
