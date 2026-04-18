//! Baseline comparison for CI regression detection.
//!
//! Compares the current ocync run against a stored baseline JSON file.
//! Exits non-zero if wall-clock time or request count regresses beyond
//! a configurable threshold.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Stored performance baseline for a benchmark run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Baseline {
    /// Wall-clock time for the run in seconds.
    pub(crate) wall_clock_secs: f64,
    /// Total number of HTTP requests made.
    pub(crate) total_requests: u64,
    /// Total bytes transferred.
    pub(crate) total_bytes: u64,
}

/// Result of comparing a current run against a stored baseline.
pub(crate) struct RegressionResult {
    /// Percentage change in wall-clock time: positive means slower.
    pub(crate) wall_clock_delta_pct: f64,
    /// Percentage change in request count: positive means more requests.
    pub(crate) requests_delta_pct: f64,
    /// Percentage change in bytes transferred: positive means more bytes.
    pub(crate) bytes_delta_pct: f64,
    /// True if any tracked metric exceeds the configured threshold.
    pub(crate) regressed: bool,
}

/// Compares `current` against `baseline` using `threshold_pct` as the regression limit.
///
/// Returns percentage deltas computed as `(current - baseline) / baseline * 100`.
/// Zero baselines return a delta of `0.0` to avoid division by zero.
/// `regressed` is `true` when either `wall_clock_delta_pct` or `requests_delta_pct`
/// exceeds `threshold_pct`.
pub(crate) fn compare(
    current: &Baseline,
    baseline: &Baseline,
    threshold_pct: u32,
) -> RegressionResult {
    let wall_clock_delta_pct = pct_delta(current.wall_clock_secs, baseline.wall_clock_secs);
    let requests_delta_pct = pct_delta(
        current.total_requests as f64,
        baseline.total_requests as f64,
    );
    let bytes_delta_pct = pct_delta(current.total_bytes as f64, baseline.total_bytes as f64);

    let threshold = threshold_pct as f64;
    let regressed = wall_clock_delta_pct > threshold || requests_delta_pct > threshold;

    RegressionResult {
        wall_clock_delta_pct,
        requests_delta_pct,
        bytes_delta_pct,
        regressed,
    }
}

/// Loads a [`Baseline`] from a JSON file at `path`.
pub(crate) fn load_baseline(path: &Path) -> Result<Baseline, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&contents).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Saves a [`Baseline`] as pretty-printed JSON to `path`.
pub(crate) fn save_baseline(path: &Path, baseline: &Baseline) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(baseline).map_err(|e| format!("serialize baseline: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Formats a [`RegressionResult`] as a human-readable multi-line string.
///
/// Example output:
/// ```text
///   wall-clock: +25.0% (threshold: 20%) REGRESSED
///   requests:   +10.0% (threshold: 20%)
///   bytes:      +15.0%
/// ```
pub(crate) fn format_result(result: &RegressionResult, threshold_pct: u32) -> String {
    let threshold = threshold_pct as f64;

    let wall_regressed = result.wall_clock_delta_pct > threshold;
    let req_regressed = result.requests_delta_pct > threshold;

    let wall_line = format_metric_line(
        "wall-clock",
        result.wall_clock_delta_pct,
        Some(threshold_pct),
        wall_regressed,
    );
    let req_line = format_metric_line(
        "requests",
        result.requests_delta_pct,
        Some(threshold_pct),
        req_regressed,
    );
    let bytes_line = format_metric_line("bytes", result.bytes_delta_pct, None, false);

    format!("{wall_line}\n{req_line}\n{bytes_line}")
}

/// Computes percentage delta: `(current - baseline) / baseline * 100`.
///
/// Returns `0.0` when `baseline` is zero.
fn pct_delta(current: f64, baseline: f64) -> f64 {
    if baseline == 0.0 {
        return 0.0;
    }
    (current - baseline) / baseline * 100.0
}

/// Formats a single metric line with optional threshold and regression tag.
fn format_metric_line(
    name: &str,
    delta_pct: f64,
    threshold_pct: Option<u32>,
    regressed: bool,
) -> String {
    let sign = if delta_pct >= 0.0 { "+" } else { "" };
    let value = format!("{sign}{delta_pct:.1}%");

    let mut line = format!("  {name:<12}{value}");

    if let Some(t) = threshold_pct {
        line.push_str(&format!(" (threshold: {t}%)"));
    }

    if regressed {
        line.push_str(" REGRESSED");
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_regression_within_threshold() {
        let baseline = Baseline {
            wall_clock_secs: 100.0,
            total_requests: 1000,
            total_bytes: 1_000_000,
        };
        let current = Baseline {
            wall_clock_secs: 115.0,
            total_requests: 1100,
            total_bytes: 1_100_000,
        };
        let result = compare(&current, &baseline, 20);
        assert!(!result.regressed);
        assert!((result.wall_clock_delta_pct - 15.0).abs() < 0.01);
        assert!((result.requests_delta_pct - 10.0).abs() < 0.01);
    }

    #[test]
    fn regression_detected_when_over_threshold() {
        let baseline = Baseline {
            wall_clock_secs: 100.0,
            total_requests: 1000,
            total_bytes: 1_000_000,
        };
        let current = Baseline {
            wall_clock_secs: 125.0,
            total_requests: 1000,
            total_bytes: 1_000_000,
        };
        let result = compare(&current, &baseline, 20);
        assert!(result.regressed);
        assert!((result.wall_clock_delta_pct - 25.0).abs() < 0.01);
    }

    #[test]
    fn improvement_is_negative_delta() {
        let baseline = Baseline {
            wall_clock_secs: 100.0,
            total_requests: 1000,
            total_bytes: 1_000_000,
        };
        let current = Baseline {
            wall_clock_secs: 80.0,
            total_requests: 800,
            total_bytes: 800_000,
        };
        let result = compare(&current, &baseline, 20);
        assert!(!result.regressed);
        assert!((result.wall_clock_delta_pct - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn format_result_shows_regression_tag() {
        let result = RegressionResult {
            wall_clock_delta_pct: 25.0,
            requests_delta_pct: 10.0,
            bytes_delta_pct: 15.0,
            regressed: true,
        };
        let output = format_result(&result, 20);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "  wall-clock  +25.0% (threshold: 20%) REGRESSED");
        assert_eq!(lines[1], "  requests    +10.0% (threshold: 20%)");
        assert_eq!(lines[2], "  bytes       +15.0%");
    }

    #[test]
    fn format_result_shows_improvement() {
        let result = RegressionResult {
            wall_clock_delta_pct: -15.0,
            requests_delta_pct: -8.0,
            bytes_delta_pct: -12.0,
            regressed: false,
        };
        let output = format_result(&result, 20);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "  wall-clock  -15.0% (threshold: 20%)");
        assert_eq!(lines[1], "  requests    -8.0% (threshold: 20%)");
        assert_eq!(lines[2], "  bytes       -12.0%");
    }

    #[test]
    fn zero_baseline_returns_zero_delta() {
        let baseline = Baseline {
            wall_clock_secs: 0.0,
            total_requests: 0,
            total_bytes: 0,
        };
        let current = Baseline {
            wall_clock_secs: 50.0,
            total_requests: 500,
            total_bytes: 1_000_000,
        };
        let result = compare(&current, &baseline, 20);
        assert!(!result.regressed);
        assert_eq!(result.wall_clock_delta_pct, 0.0);
        assert_eq!(result.requests_delta_pct, 0.0);
        assert_eq!(result.bytes_delta_pct, 0.0);
    }

    #[test]
    fn regression_triggered_by_request_count_only() {
        let baseline = Baseline {
            wall_clock_secs: 100.0,
            total_requests: 1000,
            total_bytes: 1_000_000,
        };
        let current = Baseline {
            wall_clock_secs: 105.0, // 5% -- within threshold
            total_requests: 1250,   // 25% -- exceeds threshold
            total_bytes: 1_000_000,
        };
        let result = compare(&current, &baseline, 20);
        assert!(result.regressed);
        assert!((result.wall_clock_delta_pct - 5.0).abs() < 0.01);
        assert!((result.requests_delta_pct - 25.0).abs() < 0.01);
    }

    #[test]
    fn threshold_boundary_is_exclusive() {
        // Delta exactly equal to threshold should NOT trigger regression.
        let baseline = Baseline {
            wall_clock_secs: 100.0,
            total_requests: 1000,
            total_bytes: 1_000_000,
        };
        let current = Baseline {
            wall_clock_secs: 120.0, // exactly 20%
            total_requests: 1200,   // exactly 20%
            total_bytes: 1_200_000,
        };
        let result = compare(&current, &baseline, 20);
        assert!(!result.regressed);
        assert!((result.wall_clock_delta_pct - 20.0).abs() < 0.01);
        assert!((result.requests_delta_pct - 20.0).abs() < 0.01);
    }

    #[test]
    fn baseline_roundtrip() {
        let baseline = Baseline {
            wall_clock_secs: 47.5,
            total_requests: 847,
            total_bytes: 2_100_000_000,
        };
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        save_baseline(tmpfile.path(), &baseline).unwrap();
        let loaded = load_baseline(tmpfile.path()).unwrap();
        assert_eq!(loaded.wall_clock_secs, 47.5);
        assert_eq!(loaded.total_requests, 847);
        assert_eq!(loaded.total_bytes, 2_100_000_000);
    }

    #[test]
    fn load_baseline_missing_file() {
        let err = load_baseline(Path::new("/nonexistent/baseline.json")).unwrap_err();
        assert!(err.starts_with("read /nonexistent/baseline.json:"));
    }

    #[test]
    fn load_baseline_invalid_json() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmpfile.path(), "not json").unwrap();
        let err = load_baseline(tmpfile.path()).unwrap_err();
        assert!(err.starts_with("parse "));
    }
}
