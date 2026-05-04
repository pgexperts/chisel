// Pure-function helpers for the summary post-processor: magnitude-adaptive
// formatters for durations and byte counts, a numpy-style linear-interpolation
// percentile, and a parser for size labels like "32B" / "1MB" into byte counts.
//
// All functions are pure — easy to unit-test, no I/O, no allocation beyond
// the returned String/Option. format_duration_ns and format_bytes use 2
// decimal places at every magnitude switch so cells line up uniformly in
// table columns.

/// Format a duration in nanoseconds as a human-readable string.
/// Auto-picks ns / µs / ms / s based on magnitude. Two decimal places
/// at every switch point so cells line up uniformly in tables.
pub fn format_duration_ns(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{:.0} ns", ns)
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

/// Format a byte count (positive, negative, or zero) with sign prefix.
/// Auto-picks B / KB / MB / GB based on magnitude. 1024-base (matches
/// `ls -lh` and human intuition for filesystem sizes).
pub fn format_bytes(bytes: i64) -> String {
    let abs = bytes.unsigned_abs();
    let sign = if bytes < 0 {
        "-"
    } else if bytes > 0 {
        "+"
    } else {
        ""
    };
    if abs < 1_024 {
        format!("{}{} B", sign, abs)
    } else if abs < 1_024 * 1_024 {
        format!("{}{:.1} KB", sign, abs as f64 / 1_024.0)
    } else if abs < 1_024 * 1_024 * 1_024 {
        format!("{}{:.2} MB", sign, abs as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{}{:.2} GB", sign, abs as f64 / 1_024.0_f64.powi(3))
    }
}

/// Format a byte count using binary IEC suffixes (B / KiB / MiB / GiB).
/// Positive values only; no sign prefix. Distinct from `format_bytes`
/// which signs the output and uses 1024-base "KB/MB/GB" labels — the
/// cross-engine comparison tables (PR 8) need IEC labels for technical
/// clarity ("MiB" is unambiguously 2^20, "MB" can mean 10^6 in some
/// contexts).
///
/// Boundary convention: switch to the next-larger unit at exactly 1024
/// of the smaller unit. So 1023 B → "1023 B", 1024 B → "1.0 KiB".
/// One decimal for KiB and MiB; two decimals for GiB (since GiB values
/// are typically 2-3 digits and the extra precision matters more).
pub fn format_bytes_iec(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes < KIB {
        format!("{} B", bytes)
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    }
}

/// Parse a size label like "32B", "2KB", "1MB" into the corresponding
/// byte count. Returns None for unrecognized formats. Used by the
/// markdown renderer to sort size columns numerically rather than
/// lexically (which would put "1MB" before "32B").
pub fn parse_size_to_bytes(label: &str) -> Option<u64> {
    // Find the boundary between digits and unit
    let unit_start = label.find(|c: char| !c.is_ascii_digit())?;
    if unit_start == 0 {
        return None; // no leading digits
    }
    let (num_str, unit) = label.split_at(unit_start);
    let num: u64 = num_str.parse().ok()?;
    let multiplier: u64 = match unit {
        "B" => 1,
        "KB" => 1_024,
        "MB" => 1_024 * 1_024,
        "GB" => 1_024 * 1_024 * 1_024,
        _ => return None,
    };
    Some(num * multiplier)
}

/// Numpy-style percentile via linear interpolation. `q` is in [0.0, 1.0]
/// (0.0 = min, 1.0 = max). `sorted` MUST be sorted ascending; behavior
/// is undefined otherwise. Returns None if `sorted` is empty.
///
/// At `q = 0.99` with `n = 100` samples (Criterion's default), the
/// percentile lands at index 99.0, which is exactly sorted[99] (the
/// max). At `q = 0.99` with `n = 5`, the percentile lands at index
/// 3.96, which is interpolated between sorted[3] and sorted[4].
pub fn percentile_linear_interp(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let idx_f = q * (n - 1) as f64;
    let lower = idx_f.floor() as usize;
    let upper = idx_f.ceil() as usize;
    if lower == upper {
        Some(sorted[lower])
    } else {
        let frac = idx_f - lower as f64;
        Some(sorted[lower] + frac * (sorted[upper] - sorted[lower]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_ns_picks_unit_correctly() {
        // Sub-µs stays in ns
        assert_eq!(format_duration_ns(500.0), "500 ns");
        // µs range
        assert_eq!(format_duration_ns(1500.0), "1.50 µs");
        assert_eq!(format_duration_ns(45_678.0), "45.68 µs");
        // ms range
        assert_eq!(format_duration_ns(1_500_000.0), "1.50 ms");
        // s range
        assert_eq!(format_duration_ns(1_500_000_000.0), "1.50 s");
    }

    #[test]
    fn format_duration_ns_handles_boundary_values() {
        // Just under each boundary uses the smaller unit
        assert_eq!(format_duration_ns(999.0), "999 ns");
        assert_eq!(format_duration_ns(999_999.0), "1000.00 µs");
        // At-and-above the boundary uses the bigger unit
        assert_eq!(format_duration_ns(1000.0), "1.00 µs");
        assert_eq!(format_duration_ns(1_000_000.0), "1.00 ms");
        assert_eq!(format_duration_ns(1_000_000_000.0), "1.00 s");
    }

    #[test]
    fn format_bytes_picks_unit_correctly() {
        assert_eq!(format_bytes(500), "+500 B");
        assert_eq!(format_bytes(2048), "+2.0 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "+2.00 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "+2.00 GB");
    }

    #[test]
    fn format_bytes_handles_negative_and_zero() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(-1024), "-1.0 KB");
        assert_eq!(format_bytes(-2_097_152), "-2.00 MB");
    }

    #[test]
    fn parse_size_to_bytes_round_trips_known_sizes() {
        // The PR 4b SIZES table (master spec §3.2)
        assert_eq!(parse_size_to_bytes("32B"), Some(32));
        assert_eq!(parse_size_to_bytes("256B"), Some(256));
        assert_eq!(parse_size_to_bytes("2KB"), Some(2048));
        assert_eq!(parse_size_to_bytes("16KB"), Some(16_384));
        assert_eq!(parse_size_to_bytes("128KB"), Some(131_072));
        assert_eq!(parse_size_to_bytes("1MB"), Some(1_048_576));
    }

    #[test]
    fn parse_size_to_bytes_unknown_label_returns_none() {
        assert_eq!(parse_size_to_bytes("foobar"), None);
        assert_eq!(parse_size_to_bytes("32"), None); // missing unit
        assert_eq!(parse_size_to_bytes(""), None);
    }

    #[test]
    fn format_bytes_iec_under_1k_uses_bytes() {
        assert_eq!(format_bytes_iec(0), "0 B");
        assert_eq!(format_bytes_iec(1), "1 B");
        assert_eq!(format_bytes_iec(512), "512 B");
        assert_eq!(format_bytes_iec(1023), "1023 B");
    }

    #[test]
    fn format_bytes_iec_uses_binary_suffixes_at_each_boundary() {
        // Exact boundaries — 1024 of the smaller unit becomes 1.0 of larger.
        assert_eq!(format_bytes_iec(1024), "1.0 KiB");
        assert_eq!(format_bytes_iec(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes_iec(1024_u64.pow(3)), "1.00 GiB");
    }

    #[test]
    fn format_bytes_iec_intermediate_values_round_correctly() {
        // 1.5 KiB
        assert_eq!(format_bytes_iec(1536), "1.5 KiB");
        // 100 MiB exactly
        assert_eq!(format_bytes_iec(100 * 1024 * 1024), "100.0 MiB");
        // 4.2 MiB approximately
        assert_eq!(format_bytes_iec(4_404_019), "4.2 MiB");
        // 8 GiB
        assert_eq!(format_bytes_iec(8 * 1024_u64.pow(3)), "8.00 GiB");
    }

    #[test]
    fn percentile_linear_interp_known_values() {
        // sorted = [10, 20, 30, 40, 50], n=5 so idx = q * 4
        let sorted = [10.0, 20.0, 30.0, 40.0, 50.0];
        // p0 = sorted[0] = 10.0
        assert!((percentile_linear_interp(&sorted, 0.0).unwrap() - 10.0).abs() < 1e-9);
        // p50 = idx 2.0 = sorted[2] = 30.0
        assert!((percentile_linear_interp(&sorted, 0.50).unwrap() - 30.0).abs() < 1e-9);
        // p95 = idx 3.8 → 40 + 0.8 * (50-40) = 48.0
        assert!((percentile_linear_interp(&sorted, 0.95).unwrap() - 48.0).abs() < 1e-9);
        // p99 = idx 3.96 → 40 + 0.96 * (50-40) = 49.6
        assert!((percentile_linear_interp(&sorted, 0.99).unwrap() - 49.6).abs() < 1e-9);
        // p100 = sorted[4] = 50.0
        assert!((percentile_linear_interp(&sorted, 1.0).unwrap() - 50.0).abs() < 1e-9);
        // Empty input
        assert_eq!(percentile_linear_interp(&[], 0.5), None);
        // Single element
        assert_eq!(percentile_linear_interp(&[42.0], 0.99), Some(42.0));
    }
}
