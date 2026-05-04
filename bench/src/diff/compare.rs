// Threshold-based comparison of baseline vs PR ParsedResults.
// Produces a DiffReport with per-scenario per-metric MetricDelta
// values. The "bad-direction-positive" sign convention on
// delta_pct (see spec §3.3) means every regression check is
// uniformly `delta_pct > threshold_pct` regardless of metric.

use crate::diff::parse::{ParsedResults, ScenarioMetrics};
use std::collections::BTreeSet;
use std::path::PathBuf;

// Threshold constants (spec §3.4). Module-level, not config: tuning
// is a future change that should come with concrete data behind it.
pub const THRESHOLD_PCT_THROUGHPUT: f64 = 5.0;
pub const THRESHOLD_PCT_P50: f64 = 5.0;
pub const THRESHOLD_PCT_P95: f64 = 10.0;
pub const THRESHOLD_PCT_P99: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Throughput,
    P50,
    P95,
    P99,
}

impl Metric {
    pub fn label(&self) -> &'static str {
        match self {
            Metric::Throughput => "throughput",
            Metric::P50 => "p50",
            Metric::P95 => "p95",
            Metric::P99 => "p99",
        }
    }

    pub fn threshold_pct(&self) -> f64 {
        match self {
            Metric::Throughput => THRESHOLD_PCT_THROUGHPUT,
            Metric::P50 => THRESHOLD_PCT_P50,
            Metric::P95 => THRESHOLD_PCT_P95,
            Metric::P99 => THRESHOLD_PCT_P99,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeltaStatus {
    /// Metric improved (PR is faster / higher throughput). Not flagged.
    Improved,
    /// Within threshold. Not flagged.
    Unchanged,
    /// Regressed beyond threshold. Flagged.
    Regressed { pct: f64, threshold_pct: f64 },
    /// Cell present on PR side but absent on baseline.
    BaselineMissing,
    /// Cell present on baseline but absent on PR side.
    PrMissing,
}

#[derive(Debug, Clone)]
pub struct MetricDelta {
    pub metric: Metric,
    pub baseline: Option<f64>,
    pub pr: Option<f64>,
    /// Signed in the bad direction: positive = PR slower / lower
    /// throughput. None when either side is missing.
    pub delta_pct: Option<f64>,
    pub status: DeltaStatus,
}

#[derive(Debug, Clone)]
pub struct ScenarioDiff {
    pub scenario: String,
    pub mode: String,
    pub metrics: [MetricDelta; 4],
    /// The single worst regression in `metrics`, if any. Populates
    /// the summary-table "Worst Δ" column.
    pub worst_regression: Option<MetricDelta>,
}

#[derive(Debug)]
pub struct DiffReport {
    pub scenarios: Vec<ScenarioDiff>,
    pub regression_count: usize,
    pub baseline_path: PathBuf,
    pub pr_path: PathBuf,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

/// Compare baseline against PR. Iterates the union of scenario keys
/// from both sides; produces one ScenarioDiff per key with four
/// MetricDelta entries. Regression count is summed across all flagged
/// metrics in all scenarios.
pub fn compare(
    baseline: &ParsedResults,
    pr: &ParsedResults,
    baseline_path: PathBuf,
    pr_path: PathBuf,
    generated_at: chrono::DateTime<chrono::Utc>,
) -> DiffReport {
    let mut all_keys: BTreeSet<String> = BTreeSet::new();
    all_keys.extend(baseline.scenarios.keys().cloned());
    all_keys.extend(pr.scenarios.keys().cloned());

    let mut scenarios = Vec::new();
    let mut regression_count = 0;

    for key in all_keys {
        let baseline_m = baseline.scenarios.get(&key);
        let pr_m = pr.scenarios.get(&key);
        let diff = compare_scenario(&key, baseline_m, pr_m);
        regression_count += diff
            .metrics
            .iter()
            .filter(|m| matches!(m.status, DeltaStatus::Regressed { .. }))
            .count();
        scenarios.push(diff);
    }

    DiffReport {
        scenarios,
        regression_count,
        baseline_path,
        pr_path,
        generated_at,
    }
}

fn compare_scenario(
    key: &str,
    baseline: Option<&ScenarioMetrics>,
    pr: Option<&ScenarioMetrics>,
) -> ScenarioDiff {
    // Split "scenario/mode" key. If somehow there's no slash, treat
    // the full key as scenario and "<unknown>" as mode — schema
    // should always produce well-formed keys, so this is defensive.
    let (scenario, mode) = match key.split_once('/') {
        Some((s, m)) => (s.to_string(), m.to_string()),
        None => (key.to_string(), "<unknown>".to_string()),
    };

    let metrics = [
        compare_metric(Metric::Throughput, baseline, pr),
        compare_metric(Metric::P50, baseline, pr),
        compare_metric(Metric::P95, baseline, pr),
        compare_metric(Metric::P99, baseline, pr),
    ];

    let worst_regression = metrics
        .iter()
        .filter(|m| matches!(m.status, DeltaStatus::Regressed { .. }))
        .max_by(|a, b| {
            a.delta_pct
                .unwrap_or(0.0)
                .partial_cmp(&b.delta_pct.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();

    ScenarioDiff {
        scenario,
        mode,
        metrics,
        worst_regression,
    }
}

fn compare_metric(
    metric: Metric,
    baseline: Option<&ScenarioMetrics>,
    pr: Option<&ScenarioMetrics>,
) -> MetricDelta {
    let extract = |m: &ScenarioMetrics| -> f64 {
        match metric {
            Metric::Throughput => m.throughput_ops_per_sec,
            Metric::P50 => m.p50_ns,
            Metric::P95 => m.p95_ns,
            Metric::P99 => m.p99_ns,
        }
    };
    match (baseline, pr) {
        (None, None) => unreachable!(
            "compare_metric: both sides None; key came from the union so at least one must be Some"
        ),
        (Some(b), None) => MetricDelta {
            metric,
            baseline: Some(extract(b)),
            pr: None,
            delta_pct: None,
            status: DeltaStatus::PrMissing,
        },
        (None, Some(p)) => MetricDelta {
            metric,
            baseline: None,
            pr: Some(extract(p)),
            delta_pct: None,
            status: DeltaStatus::BaselineMissing,
        },
        (Some(b), Some(p)) => {
            let bv = extract(b);
            let pv = extract(p);
            // Bad-direction-positive sign convention. For throughput,
            // bad = lower, so delta_pct = (baseline - pr) / baseline * 100.
            // For latency, bad = higher, so delta_pct = (pr - baseline) / baseline * 100.
            let delta_pct = match metric {
                Metric::Throughput => (bv - pv) / bv * 100.0,
                Metric::P50 | Metric::P95 | Metric::P99 => (pv - bv) / bv * 100.0,
            };
            let status = if delta_pct > metric.threshold_pct() {
                DeltaStatus::Regressed {
                    pct: delta_pct,
                    threshold_pct: metric.threshold_pct(),
                }
            } else if delta_pct < 0.0 {
                DeltaStatus::Improved
            } else {
                DeltaStatus::Unchanged
            };
            MetricDelta {
                metric,
                baseline: Some(bv),
                pr: Some(pv),
                delta_pct: Some(delta_pct),
                status,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixed_metrics() -> ScenarioMetrics {
        ScenarioMetrics {
            throughput_ops_per_sec: 1000.0,
            p50_ns: 100_000.0,
            p95_ns: 200_000.0,
            p99_ns: 500_000.0,
        }
    }

    fn one_scenario(key: &str, m: ScenarioMetrics) -> ParsedResults {
        let mut s = BTreeMap::new();
        s.insert(key.to_string(), m);
        ParsedResults { scenarios: s }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-04T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn identical_inputs_produce_no_regressions() {
        let m = fixed_metrics();
        let baseline = one_scenario("ycsb-a/chisel-strict", m);
        let pr = one_scenario("ycsb-a/chisel-strict", m);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("baseline.json"),
            PathBuf::from("pr.json"),
            now(),
        );
        assert_eq!(report.regression_count, 0);
        assert_eq!(report.scenarios.len(), 1);
        let s = &report.scenarios[0];
        assert_eq!(s.scenario, "ycsb-a");
        assert_eq!(s.mode, "chisel-strict");
        assert!(s.worst_regression.is_none());
        for md in &s.metrics {
            assert!(matches!(md.status, DeltaStatus::Unchanged));
            assert_eq!(md.delta_pct, Some(0.0));
        }
    }

    #[test]
    fn pr_throughput_10pct_lower_is_regressed() {
        // Throughput threshold is 5%; 10% lower trips it.
        let baseline = one_scenario(
            "ycsb-a/chisel-strict",
            ScenarioMetrics {
                throughput_ops_per_sec: 1000.0,
                p50_ns: 100_000.0,
                p95_ns: 200_000.0,
                p99_ns: 500_000.0,
            },
        );
        let pr = one_scenario(
            "ycsb-a/chisel-strict",
            ScenarioMetrics {
                throughput_ops_per_sec: 900.0, // 10% lower
                p50_ns: 100_000.0,
                p95_ns: 200_000.0,
                p99_ns: 500_000.0,
            },
        );
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 1);
        let s = &report.scenarios[0];
        let throughput_md = &s.metrics[0];
        assert_eq!(throughput_md.metric, Metric::Throughput);
        match &throughput_md.status {
            DeltaStatus::Regressed { pct, threshold_pct } => {
                assert!((pct - 10.0).abs() < 0.001, "expected ~10.0, got {pct}");
                assert_eq!(*threshold_pct, 5.0);
            }
            other => panic!("expected Regressed, got {other:?}"),
        }
        assert!(s.worst_regression.is_some());
        assert_eq!(
            s.worst_regression.as_ref().unwrap().metric,
            Metric::Throughput
        );
    }

    #[test]
    fn pr_p99_6pct_higher_is_unchanged() {
        // p99 threshold is 10%; 6% does not trip it.
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr_metrics = ScenarioMetrics {
            p99_ns: 530_000.0, // 6% higher than 500_000
            ..fixed_metrics()
        };
        let pr = one_scenario("ycsb-a/chisel-strict", pr_metrics);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 0);
        let p99_md = &report.scenarios[0].metrics[3];
        assert_eq!(p99_md.metric, Metric::P99);
        assert!(matches!(p99_md.status, DeltaStatus::Unchanged));
    }

    #[test]
    fn pr_p99_12pct_higher_is_regressed() {
        // p99 threshold is 10%; 12% trips it.
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr_metrics = ScenarioMetrics {
            p99_ns: 560_000.0, // 12% higher than 500_000
            ..fixed_metrics()
        };
        let pr = one_scenario("ycsb-a/chisel-strict", pr_metrics);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 1);
        let p99_md = &report.scenarios[0].metrics[3];
        match &p99_md.status {
            DeltaStatus::Regressed { pct, threshold_pct } => {
                assert!((pct - 12.0).abs() < 0.001);
                assert_eq!(*threshold_pct, 10.0);
            }
            other => panic!("expected Regressed, got {other:?}"),
        }
    }
}
