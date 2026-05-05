// Bench harness for Chisel. See `ARCHITECTURE.md` ("Benchmark
// infrastructure" section) at the repo root for the layered design:
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing

pub mod chisel_engine;
pub mod diff;
pub mod engine;
pub mod noise_gate;
pub mod redb_engine;
pub mod runner;
pub mod scenarios;
pub mod sqlite_engine;
pub mod summary;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use runner::{
    run_scenario_cell, AuxMetricsWriter, CellAuxMetrics, CellId, ChiselCountersDelta, EngineMode,
    PopulatedSnapshot, ScenarioResult, CACHE_SIZE_PAGES,
};
pub use scenarios::{
    gen_document_store, gen_document_store_prepopulate, gen_mutation_log,
    gen_mutation_log_prepopulate, gen_ycsb_a, gen_ycsb_a_prepopulate, gen_ycsb_b,
    gen_ycsb_b_prepopulate, seed_for,
};
pub use sqlite_engine::SqliteEngine;
pub use workload::{OpKind, Operation, Workload};
