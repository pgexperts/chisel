// Threshold-based comparison of baseline vs PR ParsedResults.
// Produces a DiffReport with per-scenario per-metric MetricDelta
// values. The "bad-direction-positive" sign convention on
// delta_pct (see spec §3.3) means every regression check is
// uniformly `delta_pct > threshold_pct` regardless of metric.
