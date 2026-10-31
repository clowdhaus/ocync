//! Baseline comparison for CI regression detection.
//!
//! Compares the current ocync run against a stored baseline JSON file.
//! Exits non-zero if wall-clock time or request count regresses beyond
//! a configurable threshold.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bench::lock::LockedAt;
use crate::bench::report::CorpusInfo;

/// Stored performance baseline for a benchmark run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Baseline {
    /// Wall-clock time for the run in seconds.
    pub(crate) wall_clock_secs: f64,
    /// Total number of HTTP requests made.
    pub(crate) requests: u64,
    /// Total bytes transferred.
    pub(crate) total_bytes: u64,
    /// The corpus lock the run synced against. Absent from baselines saved
    /// before the lock existed, which therefore compare against no run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) corpus_locked_at: Option<LockedAt>,
    /// Images and tags the run synced. `--limit` and `--skip-registries`
    /// shrink the corpus under the same lock, so the lock alone does not say
    /// two runs synced the same content. Zero in baselines saved before this
    /// was recorded, which therefore compare against no run.
    #[serde(default)]
    pub(crate) corpus: CorpusInfo,
    /// Rounds the figures are the median of. A median over other rounds has
    /// another spread, so two are not compared. One in baselines saved before
    /// rounds, which were a single run.
    #[serde(default = "one_round")]
    pub(crate) rounds: u32,
}

/// The rounds of a baseline saved before rounds were recorded.
fn one_round() -> u32 {
    1
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
/// Refuses when the two were recorded under different corpus locks, the
/// baseline under none, or over a different number of images or tags: they
/// synced different content, so a delta is not a regression. Refuses a
/// different number of rounds too. `baseline_path` names the file to replace.
///
/// Returns percentage deltas computed as `(current - baseline) / baseline * 100`.
/// Zero baselines return a delta of `0.0` to avoid division by zero.
/// `regressed` is `true` when either `wall_clock_delta_pct` or `requests_delta_pct`
/// exceeds `threshold_pct`.
pub(crate) fn compare(
    current: &Baseline,
    baseline: &Baseline,
    threshold_pct: u32,
    baseline_path: &Path,
) -> Result<RegressionResult, String> {
    if current.corpus_locked_at != baseline.corpus_locked_at || current.corpus != baseline.corpus {
        let show =
            |at: &Option<LockedAt>| at.as_ref().map_or("(none)".to_owned(), ToString::to_string);
        return Err(format!(
            "{} was recorded under corpus lock {} over {} images and {} tags, this run \
             under {} over {} images and {} tags; record a new baseline by deleting {} \
             and rerunning",
            baseline_path.display(),
            show(&baseline.corpus_locked_at),
            baseline.corpus.images,
            baseline.corpus.tags,
            show(&current.corpus_locked_at),
            current.corpus.images,
            current.corpus.tags,
            baseline_path.display()
        ));
    }
    if current.rounds != baseline.rounds {
        return Err(format!(
            "{} was recorded over {} rounds, this run over {}; record a new baseline by \
             deleting {} and rerunning",
            baseline_path.display(),
            baseline.rounds,
            current.rounds,
            baseline_path.display()
        ));
    }
    let wall_clock_delta_pct = pct_delta(current.wall_clock_secs, baseline.wall_clock_secs);
    let requests_delta_pct = pct_delta(current.requests as f64, baseline.requests as f64);
    let bytes_delta_pct = pct_delta(current.total_bytes as f64, baseline.total_bytes as f64);

    let threshold = threshold_pct as f64;
    let regressed = wall_clock_delta_pct > threshold || requests_delta_pct > threshold;

    Ok(RegressionResult {
        wall_clock_delta_pct,
        requests_delta_pct,
        bytes_delta_pct,
        regressed,
    })
}

/// Loads a [`Baseline`] from a JSON file at `path`.
#[expect(
    clippy::disallowed_methods,
    reason = "reads a baseline the bench wrote"
)]
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
    use crate::bench::lock::fixtures::locked_at;

    fn baseline(wall_clock_secs: f64, requests: u64, total_bytes: u64) -> Baseline {
        Baseline {
            wall_clock_secs,
            requests,
            total_bytes,
            corpus_locked_at: None,
            corpus: CorpusInfo {
                images: 40,
                tags: 55,
            },
            rounds: 5,
        }
    }

    #[test]
    fn compare_flags_only_deltas_over_the_threshold() {
        let stored = baseline(100.0, 1000, 1_000_000);
        // (name, current, regressed, wall-clock, requests, bytes deltas in percent)
        for (name, current, regressed, wall, requests, bytes) in [
            (
                "within threshold",
                baseline(115.0, 1100, 1_100_000),
                false,
                15.0,
                10.0,
                10.0,
            ),
            (
                "wall-clock over threshold",
                baseline(125.0, 1000, 1_000_000),
                true,
                25.0,
                0.0,
                0.0,
            ),
            (
                "improvement is a negative delta",
                baseline(80.0, 800, 800_000),
                false,
                -20.0,
                -20.0,
                -20.0,
            ),
            (
                "request count alone regresses",
                baseline(105.0, 1250, 1_000_000),
                true,
                5.0,
                25.0,
                0.0,
            ),
            (
                "a delta equal to the threshold is not a regression",
                baseline(120.0, 1200, 1_200_000),
                false,
                20.0,
                20.0,
                20.0,
            ),
        ] {
            let result = compare(&current, &stored, 20, Path::new("baseline.json")).unwrap();
            assert_eq!(result.regressed, regressed, "{name}");
            assert!((result.wall_clock_delta_pct - wall).abs() < 0.01, "{name}");
            assert!(
                (result.requests_delta_pct - requests).abs() < 0.01,
                "{name}"
            );
            assert!((result.bytes_delta_pct - bytes).abs() < 0.01, "{name}");
        }
    }

    #[test]
    fn format_result_tags_regressions_and_shows_signed_deltas() {
        for (name, regressed, deltas, expected) in [
            (
                "regression",
                true,
                (25.0, 10.0, 15.0),
                [
                    "  wall-clock  +25.0% (threshold: 20%) REGRESSED",
                    "  requests    +10.0% (threshold: 20%)",
                    "  bytes       +15.0%",
                ],
            ),
            (
                "improvement",
                false,
                (-15.0, -8.0, -12.0),
                [
                    "  wall-clock  -15.0% (threshold: 20%)",
                    "  requests    -8.0% (threshold: 20%)",
                    "  bytes       -12.0%",
                ],
            ),
        ] {
            let result = RegressionResult {
                wall_clock_delta_pct: deltas.0,
                requests_delta_pct: deltas.1,
                bytes_delta_pct: deltas.2,
                regressed,
            };
            let output = format_result(&result, 20);
            assert_eq!(output.lines().collect::<Vec<_>>(), expected, "{name}");
        }
    }

    #[test]
    fn zero_baseline_returns_zero_delta() {
        let result = compare(
            &baseline(50.0, 500, 1_000_000),
            &baseline(0.0, 0, 0),
            20,
            Path::new("baseline.json"),
        )
        .unwrap();
        assert!(!result.regressed);
        assert_eq!(result.wall_clock_delta_pct, 0.0);
        assert_eq!(result.requests_delta_pct, 0.0);
        assert_eq!(result.bytes_delta_pct, 0.0);
    }

    #[test]
    fn baseline_roundtrip() {
        let baseline = Baseline {
            wall_clock_secs: 47.5,
            requests: 847,
            total_bytes: 2_100_000_000,
            corpus_locked_at: Some(locked_at("2026-10-30-214340")),
            corpus: CorpusInfo {
                images: 40,
                tags: 55,
            },
            rounds: 5,
        };
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        save_baseline(tmpfile.path(), &baseline).unwrap();
        let loaded = load_baseline(tmpfile.path()).unwrap();
        assert_eq!(loaded.wall_clock_secs, 47.5);
        assert_eq!(loaded.requests, 847);
        assert_eq!(loaded.total_bytes, 2_100_000_000);
        assert_eq!(
            loaded.corpus_locked_at,
            Some(locked_at("2026-10-30-214340"))
        );
    }

    #[test]
    fn a_baseline_over_other_content_is_not_compared() {
        // Catches: reading a delta between runs that synced different content
        // (another lock, no lock, or a `--limit` run under the same lock) as a
        // regression, and a refusal that does not say which file to replace.
        let this_run = Some(locked_at("2026-10-30-214340"));
        let full = CorpusInfo {
            images: 40,
            tags: 55,
        };
        let cases = [
            (Some(locked_at("2026-10-30-214340")), full, true),
            (
                Some(locked_at("2026-09-01-000000")),
                CorpusInfo {
                    images: 40,
                    tags: 55,
                },
                false,
            ),
            (
                None,
                CorpusInfo {
                    images: 40,
                    tags: 55,
                },
                false,
            ),
            (
                Some(locked_at("2026-10-30-214340")),
                CorpusInfo { images: 5, tags: 5 },
                false,
            ),
        ];
        for (recorded, corpus, compared) in cases {
            let stored = Baseline {
                corpus_locked_at: recorded.clone(),
                corpus,
                ..baseline(1.0, 1, 1)
            };
            let current = Baseline {
                corpus_locked_at: this_run.clone(),
                ..baseline(1.0, 1, 1)
            };
            let got = compare(&current, &stored, 20, Path::new("results/b.json"));
            assert_eq!(
                got.is_ok(),
                compared,
                "baseline lock {recorded:?}, {:?}",
                stored.corpus
            );
            if let Err(e) = got {
                assert!(e.starts_with("results/b.json was recorded"), "{e}");
                assert!(e.contains("deleting results/b.json"), "{e}");
            }
        }
    }

    #[test]
    fn a_baseline_over_other_rounds_is_not_compared_and_an_old_one_is_one_round() {
        // Catches: a median of 5 rounds compared with one run, and a baseline
        // saved before rounds failing to load.
        let stored = Baseline {
            rounds: 3,
            ..baseline(1.0, 1, 1)
        };
        let err = compare(
            &baseline(1.0, 1, 1),
            &stored,
            20,
            Path::new("results/b.json"),
        )
        .err()
        .unwrap();
        assert_eq!(
            err,
            "results/b.json was recorded over 3 rounds, this run over 5; record a new \
             baseline by deleting results/b.json and rerunning"
        );
        assert!(
            compare(
                &baseline(1.0, 1, 1),
                &baseline(1.0, 1, 1),
                20,
                Path::new("b")
            )
            .is_ok()
        );

        let old = r#"{"wall_clock_secs": 1.0, "requests": 1, "total_bytes": 1}"#;
        assert_eq!(serde_json::from_str::<Baseline>(old).unwrap().rounds, 1);
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
