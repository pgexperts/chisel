// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PRs 1–2 + PR-A + PR 3 (this PR) land the Engine trait and all
// three engine impls. Subsequent PRs add the workload + runner +
// micro grid (PR 4), the reporter (PR 5), scenarios (PR 6), and
// CI integration (PR 7).

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod sqlite_engine;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use sqlite_engine::SqliteEngine;
