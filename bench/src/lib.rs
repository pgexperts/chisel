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
// PR 2 (this PR) lands only the bottom layer: the Engine trait and
// ChiselEngine. Subsequent PRs add the other engines (PR 3), the
// workload + runner + micro grid (PR 4), the reporter (PR 5),
// scenarios (PR 6), and CI integration (PR 7).

pub mod chisel_engine;
pub mod engine;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
