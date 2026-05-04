// PR 7: regression-diff library. Consumes two results.json files
// (PR 5 schema), computes per-metric deltas with threshold-based
// flagging, renders the result as a markdown PR-comment body.
//
// Library/binary split: this module is unit-testable; the binary
// at src/bin/diff.rs is just argv parsing, file I/O, and stdout.

pub mod compare;
pub mod parse;
pub mod render;
