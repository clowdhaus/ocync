//! Tag filtering pipeline: glob -> semver -> exclude -> sort + latest.

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::Error;

/// Sort order for the final tag list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Sort by semantic version (highest first).
    Semver,
    /// Sort alphabetically (highest first).
    Alpha,
}

/// Glob patterns excluded from the `glob:`/`semver:` pipeline by default.
/// Common prerelease markers, matched case-insensitively. Users override
/// per-tag via `include:`.
const SYSTEM_EXCLUDE: &[&str] = &[
    "*-rc*",
    "*-alpha*",
    "*-beta*",
    "*-pre*",
    "*-snapshot*",
    "*-nightly*",
];

fn system_exclude_set() -> &'static GlobSet {
    static SET: OnceLock<GlobSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut builder = GlobSetBuilder::new();
        for pat in SYSTEM_EXCLUDE {
            let g = GlobBuilder::new(pat)
                .case_insensitive(true)
                .build()
                .expect("system-exclude pattern is statically valid");
            builder.add(g);
        }
        builder
            .build()
            .expect("system-exclude GlobSet is statically valid")
    })
}

/// Configuration for the tag filter pipeline.
///
/// All stages are AND (narrowing). Each stage reduces the set:
/// `glob → semver → exclude → sort → latest → min_tags`.
/// Tags matching `include:` bypass the glob/semver pipeline (but are still
/// subject to user `exclude:`).
#[derive(Debug, Default)]
pub struct FilterConfig {
    /// Always-include glob patterns. Tags matching any pattern survive
    /// `glob:`/`semver:` filters and the system-exclude defaults. Not
    /// subject to `sort:` or `latest:` truncation (those only cap the
    /// `glob:`/`semver:` pipeline side). Subject to user `exclude:`. Same
    /// syntax as `exclude:`.
    pub include: Vec<String>,
    /// Glob patterns (OR semantics). An empty list passes all tags through.
    pub glob: Vec<String>,
    /// Semver version range constraint (e.g. `>=1.18.0`).
    pub semver: Option<String>,
    /// Exclude patterns (OR deny).
    pub exclude: Vec<String>,
    /// Sort order.
    pub sort: Option<SortOrder>,
    /// Keep only the first N after sorting.
    pub latest: Option<usize>,
    /// Minimum number of tags that must survive the pipeline.
    pub min_tags: Option<usize>,
}

impl FilterConfig {
    /// Run the full filter pipeline and return matching tags.
    ///
    /// Returns an error if any pattern is invalid, `latest` is set without
    /// `sort`, or fewer tags survive than [`min_tags`](Self::min_tags)
    /// requires. For per-stage attribution and drop reasons (used by
    /// `--dry-run`), call [`apply_with_report`](Self::apply_with_report).
    pub fn apply(&self, tags: &[&str]) -> Result<Vec<String>, Error> {
        // The hot path skips drop attribution to avoid allocating per-stage
        // `Vec<String>` of dropped tag names every watch-mode cycle.
        let outcome = self.run_pipeline(tags, false)?;
        if let Some(min) = self.min_tags {
            if outcome.kept.len() < min {
                return Err(Error::BelowMinTags {
                    matched: outcome.kept.len(),
                    minimum: min,
                });
            }
        }
        Ok(outcome.kept)
    }

    /// Run the full filter pipeline and return both the kept tags and a
    /// trace of how the pipeline arrived at them.
    ///
    /// Does NOT enforce [`min_tags`](Self::min_tags); the caller checks it
    /// against `outcome.kept.len()`. This lets `--dry-run` show what survived
    /// even when `min_tags` would otherwise turn the run into an error.
    pub fn apply_with_report(&self, tags: &[&str]) -> Result<Outcome, Error> {
        self.run_pipeline(tags, true)
    }

    /// Shared pipeline implementation. When `track` is false (the real-sync
    /// hot path), per-stage `StageDelta` and per-reason `DropReason` are not
    /// constructed; the resulting `Outcome.report` carries empty vectors.
    fn run_pipeline(&self, tags: &[&str], track: bool) -> Result<Outcome, Error> {
        if self.latest.is_some() && self.sort.is_none() {
            return Err(Error::LatestWithoutSort);
        }

        let candidates = tags.len();
        let mut pipeline_stages: Vec<StageDelta> = Vec::new();
        let mut drop_reasons: Vec<DropReason> = Vec::new();

        let user_exclude_set = if self.exclude.is_empty() {
            None
        } else {
            Some(build_glob_set(&self.exclude)?)
        };
        let sys_exclude = system_exclude_set();

        let include_kept: Vec<&str> = if self.include.is_empty() {
            Vec::new()
        } else {
            let inc_set = build_glob_set(&self.include)?;
            tags.iter()
                .copied()
                .filter(|t| inc_set.is_match(t))
                .filter(|t| user_exclude_set.as_ref().is_none_or(|s| !s.is_match(t)))
                .collect()
        };

        let mut pipeline: Vec<&str> = if self.glob.is_empty() {
            tags.to_vec()
        } else {
            let kept = filter_glob(tags, &self.glob)?;
            if track {
                let kept_set: HashSet<&str> = kept.iter().copied().collect();
                let dropped: Vec<String> = tags
                    .iter()
                    .filter(|t| !kept_set.contains(*t))
                    .map(|t| (*t).to_owned())
                    .collect();
                if !dropped.is_empty() {
                    drop_reasons.push(DropReason {
                        kind: DropKind::Glob {
                            patterns: self.glob.clone(),
                        },
                        count: dropped.len(),
                        samples: dropped,
                    });
                }
            }
            kept
        };
        if track && !self.glob.is_empty() {
            pipeline_stages.push(StageDelta {
                label: format!("glob {}", patterns_label(&self.glob)),
                count_in: candidates,
                count_out: pipeline.len(),
            });
        }

        if let Some(ref range) = self.semver {
            let before = pipeline.len();
            let kept = filter_semver(&pipeline, range)?;
            if track {
                let kept_set: HashSet<&str> = kept.iter().copied().collect();
                let dropped: Vec<String> = pipeline
                    .iter()
                    .filter(|t| !kept_set.contains(*t))
                    .map(|t| (*t).to_owned())
                    .collect();
                if !dropped.is_empty() {
                    drop_reasons.push(DropReason {
                        kind: DropKind::Semver {
                            range: range.clone(),
                        },
                        count: dropped.len(),
                        samples: dropped,
                    });
                }
            }
            pipeline = kept;
            if track {
                pipeline_stages.push(StageDelta {
                    label: format!("semver \"{range}\""),
                    count_in: before,
                    count_out: pipeline.len(),
                });
            }
        }

        let before_exclude = pipeline.len();
        let mut user_dropped: Vec<String> = Vec::new();
        let mut sys_dropped: Vec<String> = Vec::new();
        pipeline.retain(|t| {
            if let Some(ref s) = user_exclude_set {
                if s.is_match(t) {
                    if track {
                        user_dropped.push((*t).to_owned());
                    }
                    return false;
                }
            }
            if sys_exclude.is_match(t) {
                if track {
                    sys_dropped.push((*t).to_owned());
                }
                return false;
            }
            true
        });
        if track {
            if !user_dropped.is_empty() {
                drop_reasons.push(DropReason {
                    kind: DropKind::UserExclude {
                        patterns: self.exclude.clone(),
                    },
                    count: user_dropped.len(),
                    samples: user_dropped,
                });
            }
            if !sys_dropped.is_empty() {
                drop_reasons.push(DropReason {
                    kind: DropKind::SystemExclude,
                    count: sys_dropped.len(),
                    samples: sys_dropped,
                });
            }
            if before_exclude != pipeline.len() {
                pipeline_stages.push(StageDelta {
                    label: "exclude (user + system)".to_string(),
                    count_in: before_exclude,
                    count_out: pipeline.len(),
                });
            }
        }

        if let Some(order) = self.sort {
            sort_tags_in_place(&mut pipeline, order);
        }
        if let Some(n) = self.latest {
            let before = pipeline.len();
            if pipeline.len() > n {
                if track {
                    let dropped: Vec<String> =
                        pipeline[n..].iter().map(|t| (*t).to_owned()).collect();
                    drop_reasons.push(DropReason {
                        kind: DropKind::LatestCap { limit: n },
                        count: dropped.len(),
                        samples: dropped,
                    });
                }
                pipeline.truncate(n);
            }
            if track {
                let label = match self.sort {
                    Some(SortOrder::Semver) => format!("sort semver desc, latest {n}"),
                    Some(SortOrder::Alpha) => format!("sort alpha desc, latest {n}"),
                    None => format!("latest {n}"),
                };
                pipeline_stages.push(StageDelta {
                    label,
                    count_in: before,
                    count_out: pipeline.len(),
                });
            }
        }

        // Union: include first (preserves include input order), then pipeline
        // tags not already in include. The order matters for the dry-run
        // formatter's "include first, then pipeline" header.
        let mut seen: HashSet<&str> = include_kept.iter().copied().collect();
        let mut final_set: Vec<&str> = include_kept.clone();
        for t in pipeline {
            if seen.insert(t) {
                final_set.push(t);
            }
        }

        if track {
            drop_reasons.sort_by(|a, b| b.count.cmp(&a.count));
        }

        Ok(Outcome {
            kept: final_set.into_iter().map(String::from).collect(),
            report: FilterReport {
                candidates,
                include_kept: include_kept.len(),
                pipeline: pipeline_stages,
                dropped: drop_reasons,
            },
        })
    }
}

/// Result of applying a [`FilterConfig`] with reporting attached.
///
/// `kept` is the same `Vec<String>` that [`FilterConfig::apply`] returns.
/// `report` describes how the pipeline arrived at it.
#[derive(Debug)]
pub struct Outcome {
    /// Tags that survive the full pipeline.
    pub kept: Vec<String>,
    /// Stage-by-stage attrition and per-reason drop attribution.
    pub report: FilterReport,
}

/// Per-mapping filter pipeline trace.
///
/// `candidates` is the source-tag count fed in. `pipeline` lists each stage
/// (label + `count_in` -> `count_out`). `dropped` is Pareto-sorted by drop
/// count across all stages, suitable for "where did most of my tags go"
/// output.
///
/// Distinct from `ocync_sync::SyncReport` (the run-level engine report);
/// this is the per-mapping filter trace consumed by `--dry-run`.
#[derive(Debug)]
pub struct FilterReport {
    /// Number of source tags fed into the pipeline.
    pub candidates: usize,
    /// Number of tags admitted via the `include:` path. `0` when `include:`
    /// is not configured.
    pub include_kept: usize,
    /// Stage-by-stage attrition along the pipeline (top-down order).
    pub pipeline: Vec<StageDelta>,
    /// Drop reasons sorted by count descending (Pareto).
    pub dropped: Vec<DropReason>,
}

/// One pipeline stage's count delta.
#[derive(Debug)]
pub struct StageDelta {
    /// Human-readable label, e.g. `glob "3.*"` or `semver ">=3.18"`.
    pub label: String,
    /// Tag count entering this stage.
    pub count_in: usize,
    /// Tag count leaving this stage.
    pub count_out: usize,
}

/// What rejected the dropped tags. Carries enough structure for both human
/// rendering and downstream pattern-matching (e.g., the dry-run formatter
/// prefixes most reasons with "by " but renders [`LatestCap`](Self::LatestCap)
/// as a self-contained clause).
#[derive(Debug, Clone)]
pub enum DropKind {
    /// Tag did not match any configured `glob:` pattern.
    Glob {
        /// Configured glob patterns (one or more).
        patterns: Vec<String>,
    },
    /// Tag did not satisfy the configured `semver:` range.
    Semver {
        /// The configured version range string, e.g. `">=1.18.0"`.
        range: String,
    },
    /// Tag matched a user-configured `exclude:` pattern.
    UserExclude {
        /// User-configured exclude patterns (one or more).
        patterns: Vec<String>,
    },
    /// Tag matched the built-in prerelease exclude list.
    SystemExclude,
    /// Tag fell off the end of the `latest: N` truncation.
    LatestCap {
        /// The configured `latest: N` value.
        limit: usize,
    },
}

impl fmt::Display for DropKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Glob { patterns } => write!(f, "glob {}", patterns_label(patterns)),
            Self::Semver { range } => write!(f, "semver \"{range}\""),
            Self::UserExclude { patterns } => {
                write!(f, "user-exclude {}", patterns_label(patterns))
            }
            Self::SystemExclude => f.write_str("system-exclude"),
            Self::LatestCap { limit } => write!(f, "over latest={limit} limit"),
        }
    }
}

/// One drop reason with sample tag names.
#[derive(Debug)]
pub struct DropReason {
    /// Which pipeline stage rejected these tags.
    pub kind: DropKind,
    /// How many tags this stage dropped.
    pub count: usize,
    /// All dropped tag names. Stored uncapped so `--dry-run -v` can render
    /// the full list; the default formatter caps display at 5 (display-only,
    /// not stored).
    pub samples: Vec<String>,
}

/// Format a list of patterns for stage/drop labels.
fn patterns_label(patterns: &[String]) -> String {
    match patterns.len() {
        1 => format!("\"{}\"", patterns[0]),
        _ => {
            let quoted: Vec<String> = patterns.iter().map(|p| format!("\"{p}\"")).collect();
            quoted.join(", ")
        }
    }
}

// ---------------------------------------------------------------------------
// Individual stages
// ---------------------------------------------------------------------------

/// Build a [`GlobSet`] from patterns, returning an error on invalid patterns.
pub fn build_glob_set(patterns: &[String]) -> Result<GlobSet, Error> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).map_err(|e| Error::InvalidGlob {
            pattern: p.clone(),
            reason: e.to_string(),
        })?;
        builder.add(g);
    }
    builder.build().map_err(|e| Error::InvalidGlob {
        pattern: patterns.join(", "),
        reason: e.to_string(),
    })
}

/// Filter tags by glob patterns (OR: any pattern match keeps the tag).
fn filter_glob<'a>(tags: &[&'a str], patterns: &[String]) -> Result<Vec<&'a str>, Error> {
    let set = build_glob_set(patterns)?;
    Ok(tags.iter().copied().filter(|t| set.is_match(t)).collect())
}

/// Filter tags by a version range.
///
/// Tags that cannot be parsed as a version are dropped with a warning.
fn filter_semver<'a>(tags: &[&'a str], range: &str) -> Result<Vec<&'a str>, Error> {
    let req = crate::version::Range::parse(range).map_err(|e| Error::InvalidVersionRange {
        range: range.to_owned(),
        reason: e.to_string(),
    })?;

    Ok(tags
        .iter()
        .copied()
        .filter(|tag| {
            let Some(ver) = crate::version::TagVersion::parse(tag) else {
                warn!(tag, "tag is not parseable as a version, dropping");
                return false;
            };
            req.matches(&ver)
        })
        .collect())
}

/// Sort tags in-place in descending order (highest first).
fn sort_tags_in_place(tags: &mut [&str], order: SortOrder) {
    use crate::version::TagVersion;
    use std::cmp::Ordering;

    match order {
        SortOrder::Semver => {
            // Parse each tag once, sort on the parsed value, then write back.
            let mut decorated: Vec<(Option<TagVersion<'_>>, &str)> =
                tags.iter().map(|t| (TagVersion::parse(t), *t)).collect();
            decorated.sort_by(|(va, ta), (vb, tb)| match (va, vb) {
                (Some(a), Some(b)) => TagVersion::compare(b, a), // descending
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => tb.cmp(ta), // alpha-descending fallback
            });
            for (i, (_, tag)) in decorated.into_iter().enumerate() {
                tags[i] = tag;
            }
        }
        SortOrder::Alpha => {
            tags.sort_by(|a, b| b.cmp(a));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // - glob tests --------------------------------------------------------

    #[test]
    fn glob_star_matches_all() {
        let tags = vec!["1.0", "1.1", "2.0", "latest"];
        let result = filter_glob(&tags, &["*".into()]).unwrap();
        assert_eq!(result, vec!["1.0", "1.1", "2.0", "latest"]);
    }

    #[test]
    fn glob_prefix_pattern() {
        let tags = vec!["v1.0", "v1.1", "v2.0", "latest"];
        let result = filter_glob(&tags, &["v1.*".into()]).unwrap();
        assert_eq!(result, vec!["v1.0", "v1.1"]);
    }

    #[test]
    fn glob_or_semantics() {
        let tags = vec!["v1.0", "v2.0", "v3.0", "nightly"];
        let result = filter_glob(&tags, &["v1.*".into(), "v2.*".into()]).unwrap();
        assert_eq!(result, vec!["v1.0", "v2.0"]);
    }

    #[test]
    fn glob_invalid_pattern_returns_error() {
        let tags = vec!["1.0"];
        let err = filter_glob(&tags, &["[invalid".into()]).unwrap_err();
        assert!(matches!(err, Error::InvalidGlob { .. }));
    }

    // - semver tests ------------------------------------------------------

    #[test]
    fn semver_range_filter() {
        let tags = vec!["1.18.0", "1.19.0", "1.20.0", "1.17.0"];
        let result = filter_semver(&tags, ">=1.18.0").unwrap();
        assert_eq!(result, vec!["1.18.0", "1.19.0", "1.20.0"]);
    }

    #[test]
    fn semver_v_prefix_stripped() {
        let tags = vec!["v1.0.0", "v2.0.0", "v3.0.0"];
        let result = filter_semver(&tags, ">=2.0.0").unwrap();
        assert_eq!(result, vec!["v2.0.0", "v3.0.0"]);
    }

    #[test]
    fn semver_two_part_normalised() {
        let tags = vec!["1.18", "1.19", "1.20"];
        let result = filter_semver(&tags, ">=1.19.0").unwrap();
        assert_eq!(result, vec!["1.19", "1.20"]);
    }

    #[test]
    fn semver_invalid_range_returns_error() {
        let tags = vec!["1.0.0"];
        let err = filter_semver(&tags, ">= not_a_version").unwrap_err();
        assert!(matches!(err, Error::InvalidVersionRange { .. }));
    }

    #[test]
    fn semver_non_parseable_tags_dropped() {
        let tags = vec!["latest", "nightly", "1.0.0", "2.0.0"];
        let result = filter_semver(&tags, ">=1.0.0").unwrap();
        assert_eq!(result, vec!["1.0.0", "2.0.0"]);
    }

    #[test]
    fn semver_empty_tags() {
        let result = filter_semver(&[], ">=1.0.0").unwrap();
        assert!(result.is_empty());
    }

    // - exclude tests -----------------------------------------------------

    #[test]
    fn exclude_basic() {
        let tags = vec!["1.0-alpine", "1.0-slim", "1.0", "1.1-alpine"];
        let set = build_glob_set(&["*-alpine".into()]).unwrap();
        let result: Vec<&str> = tags.into_iter().filter(|t| !set.is_match(t)).collect();
        assert_eq!(result, vec!["1.0-slim", "1.0"]);
    }

    #[test]
    fn exclude_multiple_patterns() {
        let tags = vec!["1.0-alpine", "1.0-slim", "1.0", "nightly"];
        let set = build_glob_set(&["*-alpine".into(), "nightly".into()]).unwrap();
        let result: Vec<&str> = tags.into_iter().filter(|t| !set.is_match(t)).collect();
        assert_eq!(result, vec!["1.0-slim", "1.0"]);
    }

    #[test]
    fn exclude_invalid_pattern_returns_error() {
        let err = build_glob_set(&["[bad".into()]).unwrap_err();
        assert!(matches!(err, Error::InvalidGlob { .. }));
    }

    // - sort tests --------------------------------------------------------

    #[test]
    fn sort_semver_descending() {
        let mut tags = vec!["1.0.0", "1.2.0", "1.1.0", "2.0.0"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(tags, vec!["2.0.0", "1.2.0", "1.1.0", "1.0.0"]);
    }

    #[test]
    fn sort_alpha_descending() {
        let mut tags = vec!["banana", "apple", "cherry"];
        sort_tags_in_place(&mut tags, SortOrder::Alpha);
        assert_eq!(tags, vec!["cherry", "banana", "apple"]);
    }

    /// Non-parseable semver tags sort after all valid semver tags,
    /// in descending alphabetical order among themselves.
    #[test]
    fn sort_semver_non_parseable_tags_last() {
        let mut tags = vec!["latest", "1.0.0", "nightly", "2.0.0"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(tags, vec!["2.0.0", "1.0.0", "nightly", "latest"]);
    }

    #[test]
    fn sort_semver_with_v_prefix() {
        let mut tags = vec!["v1.0.0", "v3.0.0", "v2.0.0"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(tags, vec!["v3.0.0", "v2.0.0", "v1.0.0"]);
    }

    /// Tags with `-rc1`/`-alpha`/`-beta` suffixes sort below their base
    /// version and in descending suffix order within the same base.
    #[test]
    fn sort_semver_suffix_tags_descending() {
        let mut tags = vec!["1.0.0", "1.0.0-rc1", "1.0.0-alpha", "1.1.0-beta1", "1.1.0"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(
            tags,
            vec!["1.1.0", "1.1.0-beta1", "1.0.0", "1.0.0-rc1", "1.0.0-alpha"]
        );
    }

    /// Two-part versions (`X.Y`) sort correctly via normalisation to `X.Y.0`.
    #[test]
    fn sort_semver_two_part_versions() {
        let mut tags = vec!["1.20", "1.18", "1.19"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(tags, vec!["1.20", "1.19", "1.18"]);
    }

    /// Mixed `v`-prefix and bare versions sort by parsed semver, not string.
    #[test]
    fn sort_semver_mixed_v_prefix_and_bare() {
        let mut tags = vec!["v1.0.0", "2.0.0", "v1.5.0"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(tags, vec!["2.0.0", "v1.5.0", "v1.0.0"]);
    }

    /// Empty string and bare `v` are not parseable as a version.
    #[test]
    fn semver_empty_and_bare_v_dropped() {
        let tags = vec!["", "v", "1.0.0"];
        let result = filter_semver(&tags, ">=1.0.0").unwrap();
        assert_eq!(result, vec!["1.0.0"]);
    }

    // - pipeline tests ----------------------------------------------------

    #[test]
    fn pipeline_default_config_passes_all() {
        let tags = vec!["1.0.0", "latest", "nightly"];
        let config = FilterConfig::default();
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.0.0", "latest", "nightly"]);
    }

    #[test]
    fn pipeline_full() {
        let tags = vec![
            "1.18.0",
            "1.19.0",
            "1.20.0",
            "1.20.1-rc1",
            "1.17.0",
            "latest",
            "nightly",
        ];
        let config = FilterConfig {
            glob: vec!["1.*".into()],
            semver: Some(">=1.18.0".into()),
            exclude: vec!["*-rc*".into()],
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            min_tags: None,
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.20.0", "1.19.0"]);
    }

    #[test]
    fn pipeline_invalid_glob_returns_error() {
        let config = FilterConfig {
            glob: vec!["[invalid".into()],
            ..FilterConfig::default()
        };
        let err = config.apply(&["1.0"]).unwrap_err();
        assert!(matches!(err, Error::InvalidGlob { .. }));
    }

    #[test]
    fn pipeline_invalid_exclude_returns_error() {
        let config = FilterConfig {
            exclude: vec!["[bad".into()],
            ..FilterConfig::default()
        };
        let err = config.apply(&["1.0"]).unwrap_err();
        assert!(matches!(err, Error::InvalidGlob { .. }));
    }

    #[test]
    fn pipeline_invalid_semver_range_returns_error() {
        let config = FilterConfig {
            semver: Some("not_valid".into()),
            ..FilterConfig::default()
        };
        let err = config.apply(&["1.0.0"]).unwrap_err();
        assert!(matches!(err, Error::InvalidVersionRange { .. }));
    }

    #[test]
    fn pipeline_empty_tags() {
        let config = FilterConfig::default();
        let result = config.apply(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn pipeline_latest_truncates() {
        let config = FilterConfig {
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let tags = vec!["1.0.0", "2.0.0", "3.0.0"];
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["3.0.0", "2.0.0"]);
    }

    #[test]
    fn pipeline_latest_greater_than_len() {
        let config = FilterConfig {
            sort: Some(SortOrder::Alpha),
            latest: Some(10),
            ..FilterConfig::default()
        };
        let result = config.apply(&["a", "b"]).unwrap();
        assert_eq!(result, vec!["b", "a"]);
    }

    #[test]
    fn pipeline_latest_zero_returns_empty() {
        let config = FilterConfig {
            sort: Some(SortOrder::Alpha),
            latest: Some(0),
            ..FilterConfig::default()
        };
        let result = config.apply(&["a", "b"]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn pipeline_latest_without_sort_returns_error() {
        let config = FilterConfig {
            latest: Some(3),
            ..FilterConfig::default()
        };
        let err = config.apply(&["a", "b", "c"]).unwrap_err();
        assert!(matches!(err, Error::LatestWithoutSort));
    }

    #[test]
    fn pipeline_min_tags_satisfied() {
        let config = FilterConfig {
            min_tags: Some(2),
            ..FilterConfig::default()
        };
        let result = config.apply(&["a", "b", "c"]).unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn pipeline_min_tags_violated() {
        let config = FilterConfig {
            glob: vec!["z-*".into()],
            min_tags: Some(1),
            ..FilterConfig::default()
        };
        let err = config.apply(&["a", "b"]).unwrap_err();
        assert!(matches!(
            err,
            Error::BelowMinTags {
                matched: 0,
                minimum: 1
            }
        ));
    }

    #[test]
    fn pipeline_min_tags_after_latest() {
        // min_tags is checked AFTER latest truncation.
        let config = FilterConfig {
            sort: Some(SortOrder::Alpha),
            latest: Some(1),
            min_tags: Some(2),
            ..FilterConfig::default()
        };
        let err = config.apply(&["a", "b", "c"]).unwrap_err();
        assert!(matches!(
            err,
            Error::BelowMinTags {
                matched: 1,
                minimum: 2
            }
        ));
    }

    // - build_glob_set tests -------------------------------------------------

    #[test]
    fn glob_set_semver_with_v_prefix() {
        let gs = build_glob_set(&["v[0-9]*.[0-9]*.[0-9]*".into()]).unwrap();
        assert!(gs.is_match("v1.2.3"));
        assert!(gs.is_match("v10.20.30"));
        assert!(!gs.is_match("latest"));
        assert!(!gs.is_match("nightly"));
    }

    #[test]
    fn glob_set_bare_semver() {
        let gs = build_glob_set(&["[0-9]*.[0-9]*.[0-9]*".into()]).unwrap();
        assert!(gs.is_match("1.2.3"));
        assert!(gs.is_match("10.0.0"));
        assert!(!gs.is_match("v1.2.3"));
        assert!(!gs.is_match("latest"));
    }

    #[test]
    fn glob_set_invalid_pattern() {
        let err = build_glob_set(&["[bad".into()]).unwrap_err();
        assert!(matches!(err, Error::InvalidGlob { .. }));
    }

    // - lenient-parser regression tests ----------------------------------

    /// Headline regression: `15.10-alpine` survives `>=15.0` under the
    /// lenient parser. Today's strict-SemVer parser drops it.
    #[test]
    fn filter_semver_keeps_alpine_with_two_part_range() {
        let tags = vec!["15.10-alpine", "15.10", "14.0-alpine"];
        let result = filter_semver(&tags, ">=15.0").unwrap();
        assert_eq!(result, vec!["15.10-alpine", "15.10"]);
        // Negative assertion: 14.0-alpine drops (below range).
        assert!(!result.contains(&"14.0-alpine"));
    }

    /// Chainguard `-rN` build revisions survive `>=N.M.K` directly.
    #[test]
    fn filter_semver_keeps_chainguard_revisions() {
        let tags = vec!["1.25.5-r0", "1.25.5", "1.24.0-r5"];
        let result = filter_semver(&tags, ">=1.25.0").unwrap();
        assert_eq!(result, vec!["1.25.5-r0", "1.25.5"]);
        assert!(!result.contains(&"1.24.0-r5"));
    }

    /// Eclipse Temurin underscore build is treated as a numeric prefix component.
    #[test]
    fn filter_semver_keeps_temurin_underscore_build() {
        let tags = vec!["25.0.3_9-jre-alpine-3.23", "24.0.1-jre", "25.0.0"];
        let result = filter_semver(&tags, ">=25.0").unwrap();
        assert_eq!(result, vec!["25.0.3_9-jre-alpine-3.23", "25.0.0"]);
        assert!(!result.contains(&"24.0.1-jre"));
    }

    /// EKS Distro compound suffix sorts numerically on the trailing components.
    #[test]
    fn sort_semver_eks_distro_compound_suffix() {
        let mut tags = vec![
            "v1.27.6-eks-1-27-9",
            "v1.27.6-eks-1-27-14",
            "v1.27.6-eks-1-27-12",
        ];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(
            tags,
            vec![
                "v1.27.6-eks-1-27-14",
                "v1.27.6-eks-1-27-12",
                "v1.27.6-eks-1-27-9",
            ]
        );
    }

    /// Letter-digit split makes `rc10` sort above `rc9`.
    #[test]
    fn sort_semver_rc_above_9() {
        let mut tags = vec!["1.0.0-rc9", "1.0.0-rc10", "1.0.0-rc1"];
        sort_tags_in_place(&mut tags, SortOrder::Semver);
        assert_eq!(tags, vec!["1.0.0-rc10", "1.0.0-rc9", "1.0.0-rc1"]);
    }

    /// Date-style tags now parse as N-component prefixes (replaces the old
    /// `semver_date_tags_dropped` test which asserted the opposite).
    #[test]
    fn semver_date_tags_parse_as_components() {
        let tags = vec!["2024.01.15", "2023.12.31", "1.0.0"];
        let result = filter_semver(&tags, ">=2024.0.0").unwrap();
        assert_eq!(result, vec!["2024.01.15"]);
        assert!(!result.contains(&"2023.12.31"));
        assert!(!result.contains(&"1.0.0"));
    }

    // - system-exclude tests ---------------------------------------------

    #[test]
    fn system_exclude_drops_rc_by_default() {
        let tags = vec![
            "1.0.0",
            "1.0.0-rc1",
            "1.0.0-alpha",
            "1.0.0-beta",
            "1.0.0-snapshot",
            "1.0.0-nightly",
            "1.0.0-pre",
        ];
        let config = FilterConfig {
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.0.0"]);
    }

    #[test]
    fn system_exclude_case_insensitive() {
        let tags = vec!["5.0.0-SNAPSHOT", "5.0.0-RC1", "5.0.0-BETA1", "5.0.0"];
        let config = FilterConfig {
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["5.0.0"]);
    }

    #[test]
    fn system_exclude_keeps_dev_and_r_revisions() {
        let tags = vec!["1.0.0-dev", "1.0.0-r0", "1.0.0-edge", "1.0.0-final"];
        let config = FilterConfig {
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        // Order matches input; none of these match the default-exclude list.
        assert!(result.contains(&"1.0.0-dev".to_string()));
        assert!(result.contains(&"1.0.0-r0".to_string()));
        assert!(result.contains(&"1.0.0-edge".to_string()));
        assert!(result.contains(&"1.0.0-final".to_string()));
        assert_eq!(result.len(), 4);
    }

    // - include tests -----------------------------------------------------

    #[test]
    fn include_pin_overrides_system_exclude() {
        let tags = vec!["1.25.0-rc1", "1.0.0-rc2", "1.25.0"];
        let config = FilterConfig {
            include: vec!["1.25.0-rc1".into()],
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        // 1.25.0-rc1 survives via include; 1.0.0-rc2 drops via system-exclude;
        // 1.25.0 survives via the pipeline.
        assert!(result.contains(&"1.25.0-rc1".to_string()));
        assert!(result.contains(&"1.25.0".to_string()));
        assert!(!result.contains(&"1.0.0-rc2".to_string()));
    }

    /// `include` survives even when the tag would fail the `semver:` filter
    /// (e.g., literal `latest` has no numeric prefix).
    #[test]
    fn include_pin_overrides_semver() {
        let tags = vec!["latest", "1.0.0", "0.9.0"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"1.0.0".to_string()));
        assert!(!result.contains(&"0.9.0".to_string()));
    }

    /// `include` survives even when the tag doesn't match `glob:` (which
    /// would otherwise restrict the pool).
    #[test]
    fn include_pin_overrides_glob() {
        // glob restricts pool to alpine variants; "latest" wouldn't match.
        let tags = vec!["latest", "1.0.0-alpine", "1.0.0"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            glob: vec!["*-alpine".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"1.0.0-alpine".to_string()));
        // 1.0.0 fails glob; not in include; not kept.
        assert!(!result.contains(&"1.0.0".to_string()));
    }

    #[test]
    fn user_exclude_wins_over_include() {
        let tags = vec!["latest", "1.0.0"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            exclude: vec!["latest".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(!result.contains(&"latest".to_string()));
    }

    #[test]
    fn latest_n_does_not_cap_include() {
        // Pipeline has 5 candidates; latest:2 should keep only the top 2 of
        // the pipeline side. Includes pass through uncapped.
        let tags = vec![
            "latest",
            "latest-dev",
            "1.5.0",
            "1.4.0",
            "1.3.0",
            "1.2.0",
            "1.1.0",
        ];
        let config = FilterConfig {
            include: vec!["latest".into(), "latest-dev".into()],
            semver: Some(">=1.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        // 2 includes + top 2 of pipeline = 4 tags.
        assert_eq!(result.len(), 4);
        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"latest-dev".to_string()));
        // Top 2 of pipeline kept; lower versions dropped by latest:N truncation.
        assert!(result.contains(&"1.5.0".to_string()));
        assert!(result.contains(&"1.4.0".to_string()));
        assert!(!result.contains(&"1.3.0".to_string()));
        assert!(!result.contains(&"1.2.0".to_string()));
        assert!(!result.contains(&"1.1.0".to_string()));
    }

    #[test]
    fn min_tags_counts_include_and_pipeline() {
        let tags = vec!["latest", "1.0.0", "1.1.0", "1.2.0", "1.3.0"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            semver: Some(">=1.0".into()),
            min_tags: Some(5),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result.len(), 5);
    }

    /// `HashiCorp` `alpha20241016` tags drop via system-exclude `*-alpha*`
    /// (the wildcard catches the embedded date stamp).
    #[test]
    fn system_exclude_drops_alpha_with_date_stamp() {
        let tags = vec!["1.10.0", "1.10.0-alpha20241016", "1.9.0-alpha20240501"];
        let config = FilterConfig {
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.10.0"]);
        assert!(!result.contains(&"1.10.0-alpha20241016".to_string()));
    }

    // - apply_with_report tests --------------------------------------------

    #[test]
    fn report_invariant_kept_plus_dropped_equals_candidates() {
        let tags = vec![
            "1.18.0",
            "1.19.0",
            "1.20.0",
            "1.20.1-rc1",
            "1.17.0",
            "latest",
            "nightly",
        ];
        let config = FilterConfig {
            glob: vec!["1.*".into()],
            semver: Some(">=1.18.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let outcome = config.apply_with_report(&tags).unwrap();
        let total_dropped: usize = outcome.report.dropped.iter().map(|d| d.count).sum();
        assert_eq!(
            outcome.kept.len() + total_dropped,
            outcome.report.candidates
        );
    }

    #[test]
    fn report_invariant_with_include_path() {
        let tags = vec!["latest", "1.0.0", "1.1.0", "1.2.0", "0.9.0-rc1"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            semver: Some(">=1.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let outcome = config.apply_with_report(&tags).unwrap();

        assert_eq!(outcome.report.include_kept, 1);

        // Pipeline accounting (closed): every candidate either survives the
        // pipeline or appears in `dropped`. Tags rescued only via `include:`
        // still count as dropped here -- they reach `kept` via the include
        // path, not the pipeline.
        let total_dropped: usize = outcome.report.dropped.iter().map(|d| d.count).sum();
        let pipeline_survivors = outcome.report.candidates - total_dropped;

        // Union semantics: final kept is `include_kept + pipeline_survivors`
        // minus their overlap (tags that both include and the pipeline kept).
        // So `kept` is bounded above by the unduplicated sum and below by
        // pipeline survivors alone.
        assert!(
            outcome.kept.len() <= outcome.report.include_kept + pipeline_survivors,
            "kept={} > include_kept={} + pipeline_survivors={}",
            outcome.kept.len(),
            outcome.report.include_kept,
            pipeline_survivors,
        );
        assert!(
            outcome.kept.len() >= pipeline_survivors,
            "kept={} < pipeline_survivors={}",
            outcome.kept.len(),
            pipeline_survivors,
        );

        // Concrete shape for this fixture: latest is dropped by semver but
        // rescued by include; 1.0.0 falls off via latest=2; 0.9.0-rc1 fails
        // semver. Final kept = {latest, 1.2.0, 1.1.0}.
        assert_eq!(outcome.kept.len(), 3);
        assert!(outcome.kept.contains(&"latest".to_string()));
        assert!(outcome.kept.contains(&"1.2.0".to_string()));
        assert!(outcome.kept.contains(&"1.1.0".to_string()));
        assert!(!outcome.kept.contains(&"1.0.0".to_string()));
        assert!(!outcome.kept.contains(&"0.9.0-rc1".to_string()));
    }

    #[test]
    fn report_pareto_sorted_by_drop_count() {
        let tags: Vec<String> = (0..20).map(|i| format!("1.{i}.0")).collect();
        let mut tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        tag_refs.extend(["edge", "latest", "nightly", "1.0.0-rc1", "1.0.0-alpha"]);
        let config = FilterConfig {
            glob: vec!["1.*".into()],
            semver: Some(">=1.10".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(3),
            ..FilterConfig::default()
        };
        let outcome = config.apply_with_report(&tag_refs).unwrap();
        for window in outcome.report.dropped.windows(2) {
            assert!(
                window[0].count >= window[1].count,
                "not Pareto-sorted: {:?}",
                outcome.report.dropped
            );
        }
    }

    #[test]
    fn report_min_tags_does_not_block_apply_with_report() {
        // apply() returns BelowMinTags; apply_with_report() returns Outcome anyway.
        let tags = vec!["1.0.0", "1.1.0"];
        let config = FilterConfig {
            sort: Some(SortOrder::Semver),
            min_tags: Some(10),
            ..FilterConfig::default()
        };
        // apply errors:
        assert!(matches!(
            config.apply(&tags),
            Err(Error::BelowMinTags { .. })
        ));
        // apply_with_report succeeds with partial data:
        let outcome = config.apply_with_report(&tags).unwrap();
        assert_eq!(outcome.kept.len(), 2);
    }

    #[test]
    fn report_drop_reasons_split_user_and_system_exclude() {
        let tags = vec!["1.0.0", "1.0.0-musl", "1.1.0-rc1", "1.2.0"];
        let config = FilterConfig {
            exclude: vec!["*-musl".into()],
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let outcome = config.apply_with_report(&tags).unwrap();
        let kinds: Vec<&DropKind> = outcome.report.dropped.iter().map(|d| &d.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, DropKind::UserExclude { .. })),
            "missing UserExclude in {kinds:?}"
        );
        assert!(
            kinds.iter().any(|k| matches!(k, DropKind::SystemExclude)),
            "missing SystemExclude in {kinds:?}"
        );
    }
}
