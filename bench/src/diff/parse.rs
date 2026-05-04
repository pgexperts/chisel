// JSON parser for the diff's input. Reads PR 5's results.json
// schema (top-level "scenarios" map keyed by "<scenario>/<mode>")
// and produces a typed view containing only the four metrics the
// diff cares about (throughput + p50/p95/p99). All other fields
// (cells, metadata, counters, file size) are ignored.
