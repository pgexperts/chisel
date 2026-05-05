// COV (coefficient of variation) computation: stddev / mean, expressed as
// a fraction (e.g., 0.02 = 2%). Used by the noise gate to score per-cell
// run-to-run variability across N runs.

#[derive(Debug, Clone, PartialEq)]
pub struct Cov {
    pub mean: f64,
    pub stddev: f64,
    pub cov: f64, // stddev / mean, fraction (NaN if mean is 0.0)
}

pub fn compute_cov(samples: &[f64]) -> Cov {
    let n = samples.len();
    if n == 0 {
        return Cov {
            mean: f64::NAN,
            stddev: 0.0,
            cov: f64::NAN,
        };
    }
    let mean = samples.iter().sum::<f64>() / n as f64;
    if n == 1 {
        // Sample stddev with N=1 is undefined (divide-by-zero on N-1).
        // Return 0.0 stddev so the noise gate doesn't false-fail.
        return Cov {
            mean,
            stddev: 0.0,
            cov: 0.0,
        };
    }
    // Sample variance with Bessel's correction (divide by N-1, not N).
    // Bessel-corrected because we're treating the runs as a sample of
    // the underlying noise process, not as the entire population.
    let variance = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    let stddev = variance.sqrt();
    let cov = if mean == 0.0 { f64::NAN } else { stddev / mean };
    Cov { mean, stddev, cov }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cov_of_constant_series_is_zero() {
        let c = compute_cov(&[100.0, 100.0, 100.0, 100.0, 100.0]);
        assert_eq!(c.mean, 100.0);
        assert_eq!(c.stddev, 0.0);
        assert_eq!(c.cov, 0.0);
    }

    #[test]
    fn cov_of_known_series_matches_hand_calc() {
        // 5 values with mean = 100, sample stddev = sqrt(((-2)^2 + (-1)^2 + 0^2 + 1^2 + 2^2) / 4) = sqrt(2.5) ≈ 1.5811
        let c = compute_cov(&[98.0, 99.0, 100.0, 101.0, 102.0]);
        assert!((c.mean - 100.0).abs() < 1e-9, "mean wrong: {}", c.mean);
        assert!(
            (c.stddev - 1.5811388300841898).abs() < 1e-9,
            "stddev wrong: {}",
            c.stddev
        );
        assert!(
            (c.cov - 0.015811388300841896).abs() < 1e-9,
            "cov wrong: {}",
            c.cov
        );
    }

    #[test]
    fn cov_of_zero_mean_series_is_nan() {
        let c = compute_cov(&[0.0, 0.0, 0.0]);
        assert_eq!(c.mean, 0.0);
        assert_eq!(c.stddev, 0.0);
        assert!(
            c.cov.is_nan(),
            "cov of zero mean should be NaN, got {}",
            c.cov
        );
    }

    #[test]
    fn cov_of_single_sample_returns_zero_stddev() {
        // Sample stddev with N=1 is undefined (divide-by-zero on N-1).
        // Convention: return 0.0 stddev and 0.0 cov so the noise gate
        // can't false-fail on a one-run "series".
        let c = compute_cov(&[42.0]);
        assert_eq!(c.mean, 42.0);
        assert_eq!(c.stddev, 0.0);
        assert_eq!(c.cov, 0.0);
    }
}
