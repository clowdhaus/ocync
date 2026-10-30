//! Tag filtering pipeline: glob -> semver -> exclude -> sort + latest.

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::Error;
use crate::version::Range;

/// Sort order for the final tag list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Sort by semantic version (highest first).
    Semver,
    /// Sort alphabetically (highest first).
    Alpha,
}

/// Prerelease markers excluded by default, matched case-insensitively.
/// Users override per tag via `include:`.
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
/// Stages narrow in order: `glob → semver → exclude → sort → latest →
/// min_tags`. Literal `include:` patterns bypass the whole pipeline except the
/// hard exclude tier. Glob `include:` patterns bypass `glob:` and the soft
/// exclude tier, then run through the rest of the pipeline.
///
/// # Exclude tiers
///
/// - **Soft tier** (built-in `SYSTEM_EXCLUDE` plus
///   [`defaults_exclude`](Self::defaults_exclude)): bypassable by `include:`,
///   for project-wide opinions like "drop `*-dev` unless a mapping says
///   otherwise."
/// - **Hard tier** ([`exclude`](Self::exclude)): blocks `include:` on the same
///   config, for absolute per-mapping denies.
#[derive(Debug, Default)]
pub struct FilterConfig {
    /// Always-include glob patterns. Literal patterns bypass the whole
    /// pipeline and are never capped by `sort:`/`latest:`. Glob patterns
    /// bypass `glob:` and the soft exclude tier, then run through the rest of
    /// the pipeline. Both are subject to [`exclude`](Self::exclude).
    pub include: Vec<String>,
    /// Glob patterns (OR semantics). An empty list passes all tags through.
    pub glob: Vec<String>,
    /// Semver version range constraint (e.g. `>=1.18.0`), parsed once so a
    /// bad range is refused when the config is read, not on every run.
    pub semver: Option<Range>,
    /// Soft-tier exclude patterns inherited from a `defaults:` block,
    /// bypassed by [`include`](Self::include) like `SYSTEM_EXCLUDE`.
    pub defaults_exclude: Vec<String>,
    /// Hard-tier exclude patterns (OR deny); block
    /// [`include`](Self::include).
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
    /// requires. For per-stage attribution (used by `--dry-run`), call
    /// [`apply_with_report`](Self::apply_with_report).
    pub fn apply(&self, tags: &[&str]) -> Result<Vec<String>, Error> {
        // No drop attribution: avoids allocating dropped names every
        // watch-mode cycle.
        let filtered = self.run_pipeline(tags, false)?;
        if let Some(min) = self.min_tags {
            if filtered.kept.len() < min {
                return Err(Error::BelowMinTags {
                    matched: filtered.kept.len(),
                    minimum: min,
                });
            }
        }
        Ok(filtered.kept)
    }

    /// Run the full filter pipeline and return the kept tags with a trace of
    /// how the pipeline arrived at them.
    ///
    /// Does NOT enforce [`min_tags`](Self::min_tags), so `--dry-run` can show
    /// what survived; the report carries the configured limit for the caller
    /// to compare against `kept.len()`.
    pub fn apply_with_report(&self, tags: &[&str]) -> Result<Filtered, Error> {
        self.run_pipeline(tags, true)
    }

    /// One-line summary of the active filter clauses, e.g.
    /// `semver >=1.0.0, latest=5`. Returns `None` when no filtering applies.
    ///
    /// Sole formatter for filter rationale in non-dry-run logs.
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.glob.is_empty() {
            parts.push(format!("glob {}", self.glob.join(",")));
        }
        if let Some(ref s) = self.semver {
            parts.push(format!("semver {s}"));
        }
        if !self.exclude.is_empty() || !self.defaults_exclude.is_empty() {
            // One clause for both tiers; dry-run carries the attribution.
            let mut combined: Vec<&str> =
                self.defaults_exclude.iter().map(String::as_str).collect();
            combined.extend(self.exclude.iter().map(String::as_str));
            parts.push(format!("exclude {}", combined.join(",")));
        }
        if !self.include.is_empty() {
            parts.push(format!("include {}", self.include.join(",")));
        }
        if let Some(order) = self.sort {
            parts.push(format!(
                "sort {}",
                match order {
                    SortOrder::Semver => "semver",
                    SortOrder::Alpha => "alpha",
                }
            ));
        }
        if let Some(n) = self.latest {
            parts.push(format!("latest={n}"));
        }
        if let Some(n) = self.min_tags {
            parts.push(format!("min_tags={n}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }

    /// Shared pipeline. When `track` is false (the real-sync hot path), no
    /// `StageDelta` or `DropReason` is built and the report vectors are empty.
    fn run_pipeline(&self, tags: &[&str], track: bool) -> Result<Filtered, Error> {
        if self.latest.is_some() && self.sort.is_none() {
            return Err(Error::LatestWithoutSort);
        }

        let candidate_count = tags.len();
        let mut pipeline_stages: Vec<StageDelta> = Vec::new();
        let mut drop_reasons: Vec<DropReason> = Vec::new();

        let user_exclude_set = if self.exclude.is_empty() {
            None
        } else {
            Some(build_glob_set(&self.exclude)?)
        };
        let defaults_exclude_set = if self.defaults_exclude.is_empty() {
            None
        } else {
            Some(build_glob_set(&self.defaults_exclude)?)
        };
        let sys_exclude = system_exclude_set();

        // Literals bypass the entire pipeline (unioned after). Globs rescue
        // from `glob:` and the soft tier, then run through the rest.
        let (include_literals, include_globs): (Vec<String>, Vec<String>) = self
            .include
            .iter()
            .cloned()
            .partition(|p| is_literal_pattern(p));

        let include_glob_set = if include_globs.is_empty() {
            None
        } else {
            Some(build_glob_set(&include_globs)?)
        };

        let include_kept_refs: Vec<&str> = if include_literals.is_empty() {
            Vec::new()
        } else {
            let inc_set = build_glob_set(&include_literals)?;
            tags.iter()
                .copied()
                .filter(|t| inc_set.is_match(t))
                .filter(|t| user_exclude_set.as_ref().is_none_or(|s| !s.is_match(t)))
                .collect()
        };

        let mut pipeline: Vec<&str> = if self.glob.is_empty() {
            tags.to_vec()
        } else {
            let glob_set = build_glob_set(&self.glob)?;
            let (kept, dropped) = partition_with_drop(tags, track, |t| {
                glob_set.is_match(t) || include_glob_set.as_ref().is_some_and(|s| s.is_match(t))
            });
            push_drop_reason(
                &mut drop_reasons,
                track,
                DropKind::Glob {
                    patterns: self.glob.clone(),
                },
                dropped,
            );
            kept
        };
        if track && !self.glob.is_empty() {
            pipeline_stages.push(StageDelta {
                label: format!("glob {}", patterns_label(&self.glob)),
                count_in: candidate_count,
                count_out: pipeline.len(),
            });
        }

        if let Some(ref req) = self.semver {
            let before = pipeline.len();
            let (kept, dropped) = partition_with_drop(&pipeline, track, |t| {
                if is_referrers_fallback_tag(t) {
                    return false;
                }
                match crate::version::TagVersion::parse(t) {
                    Some(ver) => req.matches(&ver),
                    None => {
                        debug!(tag = t, "tag is not parseable as a version, dropping");
                        false
                    }
                }
            });
            push_drop_reason(
                &mut drop_reasons,
                track,
                DropKind::Semver { range: req.clone() },
                dropped,
            );
            pipeline = kept;
            if track {
                pipeline_stages.push(StageDelta {
                    label: format!("semver \"{req}\""),
                    count_in: before,
                    count_out: pipeline.len(),
                });
            }
        }

        // Tier order only decides which DropKind a drop is reported under,
        // not which tags are kept. Mapping (hard) is checked first as the
        // most specific user intent.
        let before_exclude = pipeline.len();
        let mut mapping_dropped: Vec<String> = Vec::new();
        let mut defaults_dropped: Vec<String> = Vec::new();
        let mut builtin_dropped: Vec<String> = Vec::new();
        pipeline.retain(|t| {
            // Hard tier always applies, including to include-glob matches.
            if let Some(ref s) = user_exclude_set {
                if s.is_match(t) {
                    if track {
                        mapping_dropped.push((*t).to_owned());
                    }
                    return false;
                }
            }
            // Soft tier is bypassed by a glob `include:` match. Literal
            // includes never reach this path.
            let rescued_from_soft_tier = include_glob_set.as_ref().is_some_and(|s| s.is_match(t));
            if !rescued_from_soft_tier {
                if let Some(ref s) = defaults_exclude_set {
                    if s.is_match(t) {
                        if track {
                            defaults_dropped.push((*t).to_owned());
                        }
                        return false;
                    }
                }
                if sys_exclude.is_match(t) {
                    if track {
                        builtin_dropped.push((*t).to_owned());
                    }
                    return false;
                }
            }
            true
        });
        if track {
            if !mapping_dropped.is_empty() {
                drop_reasons.push(DropReason {
                    kind: DropKind::MappingExclude {
                        patterns: self.exclude.clone(),
                    },
                    count: mapping_dropped.len(),
                    samples: mapping_dropped,
                });
            }
            if !defaults_dropped.is_empty() {
                drop_reasons.push(DropReason {
                    kind: DropKind::DefaultsExclude {
                        patterns: self.defaults_exclude.clone(),
                    },
                    count: defaults_dropped.len(),
                    samples: defaults_dropped,
                });
            }
            if !builtin_dropped.is_empty() {
                drop_reasons.push(DropReason {
                    kind: DropKind::BuiltinExclude,
                    count: builtin_dropped.len(),
                    samples: builtin_dropped,
                });
            }
            if before_exclude != pipeline.len() {
                pipeline_stages.push(StageDelta {
                    label: "exclude (mapping + defaults + built-in)".to_string(),
                    count_in: before_exclude,
                    count_out: pipeline.len(),
                });
            }
        }

        if let Some(order) = self.sort {
            let before = pipeline.len();
            sort_tags_in_place(&mut pipeline, order);
            if track {
                let label = match order {
                    SortOrder::Semver => "sort semver desc",
                    SortOrder::Alpha => "sort alpha desc",
                };
                pipeline_stages.push(StageDelta {
                    label: label.to_string(),
                    count_in: before,
                    count_out: pipeline.len(),
                });
            }
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
                pipeline_stages.push(StageDelta {
                    label: format!("keep latest {n}"),
                    count_in: before,
                    count_out: pipeline.len(),
                });
            }
        }

        // Include first (the dry-run formatter's header relies on this
        // order), then pipeline tags not already included.
        let mut seen: HashSet<&str> = include_kept_refs.iter().copied().collect();
        let mut final_set: Vec<&str> = include_kept_refs.clone();
        for t in pipeline {
            if seen.insert(t) {
                final_set.push(t);
            }
        }

        if track {
            drop_reasons.sort_by(|a, b| b.count.cmp(&a.count));
        }

        // Track-only: the hot path discards the report.
        let include_kept: Vec<String> = if track {
            include_kept_refs.iter().map(|s| (*s).to_owned()).collect()
        } else {
            Vec::new()
        };

        Ok(Filtered {
            kept: final_set.into_iter().map(String::from).collect(),
            report: FilterReport {
                candidate_count,
                include_kept,
                pipeline: pipeline_stages,
                dropped: drop_reasons,
                min_tags: self.min_tags,
            },
        })
    }
}

/// Result of [`FilterConfig::apply_with_report`]: the tags
/// [`FilterConfig::apply`] would return, plus how the pipeline got there.
#[derive(Debug)]
pub struct Filtered {
    /// Tags that survive the full pipeline.
    pub kept: Vec<String>,
    /// Stage-by-stage attrition and per-reason drop attribution.
    pub report: FilterReport,
}

/// Per-mapping filter pipeline trace consumed by `--dry-run`. Distinct from
/// the run-level `SyncReport`.
#[derive(Debug)]
pub struct FilterReport {
    /// Number of source tags fed into the pipeline.
    pub candidate_count: usize,
    /// Literal-`include:` tag names, in input order. They bypass
    /// glob/semver/sort/latest but not user `exclude:`.
    pub include_kept: Vec<String>,
    /// Stage-by-stage attrition along the pipeline (top-down order).
    pub pipeline: Vec<StageDelta>,
    /// Drop reasons sorted by count descending.
    pub dropped: Vec<DropReason>,
    /// Configured `min_tags:`, so dry-run can show whether a real sync would
    /// fail with `BelowMinTags`.
    pub min_tags: Option<usize>,
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

/// What rejected the dropped tags. The dry-run formatter prefixes most
/// reasons with "by " but renders [`LatestCap`](Self::LatestCap) as a
/// self-contained clause.
#[derive(Debug, Clone)]
pub enum DropKind {
    /// Tag did not match any configured `glob:` pattern.
    Glob {
        /// Configured glob patterns (one or more).
        patterns: Vec<String>,
    },
    /// Tag did not satisfy the configured `semver:` range.
    Semver {
        /// The configured version range, e.g. `">=1.18.0"`.
        range: Range,
    },
    /// Tag matched a per-mapping `exclude:` pattern (hard tier; blocks
    /// `include:` on the same mapping).
    MappingExclude {
        /// Mapping-level exclude patterns (one or more).
        patterns: Vec<String>,
    },
    /// Tag matched a `defaults.tags.exclude:` pattern (soft tier; bypassable
    /// by `include:`).
    DefaultsExclude {
        /// Defaults-level exclude patterns (one or more).
        patterns: Vec<String>,
    },
    /// Tag matched the built-in prerelease exclude list (soft tier;
    /// bypassable by `include:`).
    BuiltinExclude,
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
            Self::MappingExclude { patterns } => {
                write!(f, "exclude (mapping) {}", patterns_label(patterns))
            }
            Self::DefaultsExclude { patterns } => {
                write!(f, "exclude (defaults) {}", patterns_label(patterns))
            }
            Self::BuiltinExclude => f.write_str("exclude (built-in)"),
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
    /// All dropped tag names, uncapped so `--dry-run -v` can list them all.
    pub samples: Vec<String>,
}

fn patterns_label(patterns: &[String]) -> String {
    match patterns.len() {
        1 => format!("\"{}\"", patterns[0]),
        _ => {
            let quoted: Vec<String> = patterns.iter().map(|p| format!("\"{p}\"")).collect();
            quoted.join(", ")
        }
    }
}

/// Partition `input` into kept references and, only when `track`, owned
/// dropped names. Used by the glob and semver stages; the exclude stage
/// buckets drops by tier instead.
fn partition_with_drop<'a>(
    input: &[&'a str],
    track: bool,
    keep: impl Fn(&'a str) -> bool,
) -> (Vec<&'a str>, Vec<String>) {
    let mut kept = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for &t in input {
        if keep(t) {
            kept.push(t);
        } else if track {
            dropped.push(t.to_owned());
        }
    }
    (kept, dropped)
}

/// Append a `DropReason` when `track` is set and `samples` is non-empty.
fn push_drop_reason(
    drop_reasons: &mut Vec<DropReason>,
    track: bool,
    kind: DropKind,
    samples: Vec<String>,
) {
    if track && !samples.is_empty() {
        drop_reasons.push(DropReason {
            count: samples.len(),
            kind,
            samples,
        });
    }
}

/// True for OCI 1.1 referrers fallback tags (`<algo>-<hex>` and the cosign
/// `.sig`/`.sbom`/`.att` variants). They point at artifacts, never satisfy a
/// semver range, and are skipped so they stay out of the unparseable-tag log.
///
/// Public so the CLI's "no tags matched" warning shares this one prefix check.
pub fn is_referrers_fallback_tag(tag: &str) -> bool {
    tag.starts_with("sha256-") || tag.starts_with("sha512-")
}

/// True when `pattern` contains no glob metacharacters (`*`, `?`, `[`).
pub fn is_literal_pattern(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '['])
}

/// Build a [`GlobSet`] from patterns, or `Error::InvalidGlob` for a bad one.
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

/// Sort tags in place, highest first. Unparseable tags sort after parseable
/// ones, alphabetically descending among themselves.
fn sort_tags_in_place(tags: &mut [&str], order: SortOrder) {
    use crate::version::TagVersion;
    use std::cmp::Ordering;

    match order {
        SortOrder::Semver => {
            // Parse each tag once, not once per comparison.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn range(s: &str) -> Range {
        Range::parse(s).expect("valid range")
    }

    fn semver(range_str: &str) -> FilterConfig {
        FilterConfig {
            semver: Some(range(range_str)),
            ..FilterConfig::default()
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Runs `apply` on each `(name, config, tags, want)` row and compares the
    /// full ordered result.
    fn assert_applies(cases: Vec<(&str, FilterConfig, Vec<&str>, Vec<&str>)>) {
        for (name, config, tags, want) in cases {
            let got = config
                .apply(&tags)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(got, strs(&want), "{name}");
        }
    }

    #[test]
    fn glob_stage_keeps_matching_tags() {
        let glob = |pats: &[&str]| FilterConfig {
            glob: strs(pats),
            ..FilterConfig::default()
        };
        assert_applies(vec![
            (
                "star matches all",
                glob(&["*"]),
                vec!["1.0", "1.1", "2.0", "latest"],
                vec!["1.0", "1.1", "2.0", "latest"],
            ),
            (
                "prefix pattern",
                glob(&["v1.*"]),
                vec!["v1.0", "v1.1", "v2.0", "latest"],
                vec!["v1.0", "v1.1"],
            ),
            (
                "multiple patterns are OR",
                glob(&["v1.*", "v2.*"]),
                vec!["v1.0", "v2.0", "v3.0", "nightly"],
                vec!["v1.0", "v2.0"],
            ),
        ]);
    }

    #[test]
    fn semver_stage_keeps_in_range_tags() {
        assert_applies(vec![
            (
                "range filter",
                semver(">=1.18.0"),
                vec!["1.18.0", "1.19.0", "1.20.0", "1.17.0"],
                vec!["1.18.0", "1.19.0", "1.20.0"],
            ),
            (
                "v prefix stripped",
                semver(">=2.0.0"),
                vec!["v1.0.0", "v2.0.0", "v3.0.0"],
                vec!["v2.0.0", "v3.0.0"],
            ),
            (
                "two-part tags normalised",
                semver(">=1.19.0"),
                vec!["1.18", "1.19", "1.20"],
                vec!["1.19", "1.20"],
            ),
            (
                "non-parseable tags dropped",
                semver(">=1.0.0"),
                vec!["latest", "nightly", "1.0.0", "2.0.0"],
                vec!["1.0.0", "2.0.0"],
            ),
            ("empty tags", semver(">=1.0.0"), vec![], vec![]),
            (
                "empty string and bare v dropped",
                semver(">=1.0.0"),
                vec!["", "v", "1.0.0"],
                vec!["1.0.0"],
            ),
            (
                // They drop silently so the unparseable-tag log does not fire
                // once per artifact tag per image.
                "referrers fallback tags skipped",
                semver(">=1.0.0"),
                vec![
                    "1.0.0",
                    "sha256-abc123def456.sig",
                    "sha256-abc123def456.sbom",
                    "sha256-abc123def456.att",
                    "sha256-abc123def456",
                    "sha512-deadbeef.sig",
                    "2.0.0",
                ],
                vec!["1.0.0", "2.0.0"],
            ),
            (
                "alpine variant survives a two-part range",
                semver(">=15.0"),
                vec!["15.10-alpine", "15.10", "14.0-alpine"],
                vec!["15.10-alpine", "15.10"],
            ),
            (
                "chainguard revisions survive",
                semver(">=1.25.0"),
                vec!["1.25.5-r0", "1.25.5", "1.24.0-r5"],
                vec!["1.25.5-r0", "1.25.5"],
            ),
            (
                "temurin underscore build is a prefix component",
                semver(">=25.0"),
                vec!["25.0.3_9-jre-alpine-3.23", "24.0.1-jre", "25.0.0"],
                vec!["25.0.3_9-jre-alpine-3.23", "25.0.0"],
            ),
            (
                "date tags parse as components",
                semver(">=2024.0.0"),
                vec!["2024.01.15", "2023.12.31", "1.0.0"],
                vec!["2024.01.15"],
            ),
        ]);
    }

    #[test]
    fn mapping_exclude_drops_matching_tags() {
        let exclude = |pats: &[&str]| FilterConfig {
            exclude: strs(pats),
            ..FilterConfig::default()
        };
        assert_applies(vec![
            (
                "single pattern",
                exclude(&["*-alpine"]),
                vec!["1.0-alpine", "1.0-slim", "1.0", "1.1-alpine"],
                vec!["1.0-slim", "1.0"],
            ),
            (
                "multiple patterns",
                exclude(&["*-alpine", "nightly"]),
                vec!["1.0-alpine", "1.0-slim", "1.0", "nightly"],
                vec!["1.0-slim", "1.0"],
            ),
        ]);
    }

    #[test]
    fn sort_tags_descending() {
        use SortOrder::{Alpha, Semver};
        // (name, order, input, want)
        let cases: &[(&str, SortOrder, &[&str], &[&str])] = &[
            (
                "semver",
                Semver,
                &["1.0.0", "1.2.0", "1.1.0", "2.0.0"],
                &["2.0.0", "1.2.0", "1.1.0", "1.0.0"],
            ),
            (
                "alpha",
                Alpha,
                &["banana", "apple", "cherry"],
                &["cherry", "banana", "apple"],
            ),
            (
                "semver: non-parseable last, alphabetical descending",
                Semver,
                &["latest", "1.0.0", "nightly", "2.0.0"],
                &["2.0.0", "1.0.0", "nightly", "latest"],
            ),
            (
                "semver: v prefix",
                Semver,
                &["v1.0.0", "v3.0.0", "v2.0.0"],
                &["v3.0.0", "v2.0.0", "v1.0.0"],
            ),
            (
                "semver: mixed v prefix and bare sort by parsed value",
                Semver,
                &["v1.0.0", "2.0.0", "v1.5.0"],
                &["2.0.0", "v1.5.0", "v1.0.0"],
            ),
        ];
        for (name, order, input, want) in cases {
            let mut got = input.to_vec();
            sort_tags_in_place(&mut got, *order);
            assert_eq!(got, *want, "{name}");
        }
    }

    #[test]
    fn apply_pipeline_outputs() {
        let sorted = |order, latest| FilterConfig {
            sort: Some(order),
            latest,
            ..FilterConfig::default()
        };
        assert_applies(vec![
            (
                "default config passes all",
                FilterConfig::default(),
                vec!["1.0.0", "latest", "nightly"],
                vec!["1.0.0", "latest", "nightly"],
            ),
            ("empty tags", FilterConfig::default(), vec![], vec![]),
            (
                "glob, semver, exclude, sort, latest together",
                FilterConfig {
                    glob: strs(&["1.*"]),
                    semver: Some(range(">=1.18.0")),
                    exclude: strs(&["*-rc*"]),
                    sort: Some(SortOrder::Semver),
                    latest: Some(2),
                    ..FilterConfig::default()
                },
                vec![
                    "1.18.0",
                    "1.19.0",
                    "1.20.0",
                    "1.20.1-rc1",
                    "1.17.0",
                    "latest",
                    "nightly",
                ],
                vec!["1.20.0", "1.19.0"],
            ),
            (
                "latest truncates",
                sorted(SortOrder::Semver, Some(2)),
                vec!["1.0.0", "2.0.0", "3.0.0"],
                vec!["3.0.0", "2.0.0"],
            ),
            (
                "latest greater than length keeps all",
                sorted(SortOrder::Alpha, Some(10)),
                vec!["a", "b"],
                vec!["b", "a"],
            ),
            (
                "latest zero keeps none",
                sorted(SortOrder::Alpha, Some(0)),
                vec!["a", "b"],
                vec![],
            ),
            (
                "min_tags satisfied",
                FilterConfig {
                    min_tags: Some(2),
                    ..FilterConfig::default()
                },
                vec!["a", "b", "c"],
                vec!["a", "b", "c"],
            ),
        ]);
    }

    #[test]
    fn invalid_glob_is_rejected() {
        let cases = [
            (
                "glob stage",
                FilterConfig {
                    glob: strs(&["[invalid"]),
                    ..FilterConfig::default()
                },
            ),
            (
                "exclude stage",
                FilterConfig {
                    exclude: strs(&["[bad"]),
                    ..FilterConfig::default()
                },
            ),
        ];
        for (name, config) in cases {
            let err = config.apply(&["1.0"]).unwrap_err();
            assert!(matches!(err, Error::InvalidGlob { .. }), "{name}");
        }
        let err = build_glob_set(&["[bad".into()]).unwrap_err();
        assert!(matches!(err, Error::InvalidGlob { .. }), "build_glob_set");
    }

    #[test]
    fn describe_prints_the_range_as_written() {
        assert_eq!(
            semver(" >=1.0 , <2.0 ").describe().as_deref(),
            Some("semver  >=1.0 , <2.0 ")
        );
    }

    #[test]
    fn latest_without_sort_is_rejected() {
        let config = FilterConfig {
            latest: Some(3),
            ..FilterConfig::default()
        };
        let err = config.apply(&["a", "b", "c"]).unwrap_err();
        assert!(matches!(err, Error::LatestWithoutSort));
    }

    #[test]
    fn below_min_tags_reports_counts() {
        // (name, config, matched, minimum)
        let cases = [
            (
                "glob matches nothing",
                FilterConfig {
                    glob: strs(&["z-*"]),
                    min_tags: Some(1),
                    ..FilterConfig::default()
                },
                0,
                1,
            ),
            (
                // min_tags is checked after latest truncation.
                "checked after latest",
                FilterConfig {
                    sort: Some(SortOrder::Alpha),
                    latest: Some(1),
                    min_tags: Some(2),
                    ..FilterConfig::default()
                },
                1,
                2,
            ),
        ];
        for (name, config, want_matched, want_minimum) in cases {
            match config.apply(&["a", "b", "c"]) {
                Err(Error::BelowMinTags { matched, minimum }) => {
                    assert_eq!((matched, minimum), (want_matched, want_minimum), "{name}");
                }
                other => panic!("{name}: expected BelowMinTags, got {other:?}"),
            }
        }
    }

    #[test]
    fn system_exclude_drops_prereleases() {
        assert_applies(vec![
            (
                "rc, alpha, beta, snapshot, nightly, pre",
                semver(">=1.0"),
                vec![
                    "1.0.0",
                    "1.0.0-rc1",
                    "1.0.0-alpha",
                    "1.0.0-beta",
                    "1.0.0-snapshot",
                    "1.0.0-nightly",
                    "1.0.0-pre",
                ],
                vec!["1.0.0"],
            ),
            (
                "case-insensitive",
                semver(">=1.0"),
                vec!["5.0.0-SNAPSHOT", "5.0.0-RC1", "5.0.0-BETA1", "5.0.0"],
                vec!["5.0.0"],
            ),
            (
                "dev, r-revision, edge and final are kept",
                semver(">=1.0"),
                vec!["1.0.0-dev", "1.0.0-r0", "1.0.0-edge", "1.0.0-final"],
                vec!["1.0.0-dev", "1.0.0-r0", "1.0.0-edge", "1.0.0-final"],
            ),
            (
                // The wildcard catches the embedded date stamp.
                "alpha with date stamp",
                semver(">=1.0"),
                vec!["1.10.0", "1.10.0-alpha20241016", "1.9.0-alpha20240501"],
                vec!["1.10.0"],
            ),
        ]);
    }

    #[test]
    fn defaults_exclude_and_mapping_exclude_tiers() {
        let defaults = |pats: &[&str]| FilterConfig {
            defaults_exclude: strs(pats),
            ..FilterConfig::default()
        };
        assert_applies(vec![
            (
                "defaults_exclude drops tags",
                defaults(&["*-dev", "*-r[0-9]*"]),
                vec!["1.0.0", "1.0.0-dev", "1.0.0-r0"],
                vec!["1.0.0"],
            ),
            (
                // Defaults and mapping exclude stack; neither replaces the other.
                "defaults and mapping exclude stack",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    exclude: strs(&["*-slim"]),
                    ..FilterConfig::default()
                },
                vec!["1.0.0", "1.0.0-dev", "1.0.0-slim"],
                vec!["1.0.0"],
            ),
            (
                // The recipe suggested in `configuration.md` for suffixes
                // absent from the built-in soft tier.
                "character class drops r counters at any digit width",
                defaults(&["*-r[0-9]*"]),
                vec![
                    "1.25.5",
                    "1.25.5-r0",
                    "1.25.5-r9",
                    "1.25.5-r10",
                    "1.25.5-r123",
                ],
                vec!["1.25.5"],
            ),
            (
                // The glob: stage is upstream of exclude; only include: rescues.
                "glob: does not rescue from the soft tier",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    glob: strs(&["*-dev"]),
                    semver: Some(range(">=8.0.0, <9.0.0")),
                    ..FilterConfig::default()
                },
                vec!["8.0.0", "8.5.0-dev", "10.0.0-dev"],
                vec![],
            ),
            (
                // Stable and dev tags both constrained by semver:, -rN denied.
                "dev tags constrained by semver end to end",
                FilterConfig {
                    defaults_exclude: strs(&["*-r[0-9]*"]),
                    semver: Some(range(">=8.0.0, <9.0.0")),
                    sort: Some(SortOrder::Semver),
                    latest: Some(10),
                    ..FilterConfig::default()
                },
                vec![
                    "8.0.0",
                    "8.5.0",
                    "8.9.9",
                    "8.5.0-dev",
                    "8.9.9-dev",
                    "9.0.0",
                    "9.0.0-dev",
                    "10.0.0",
                    "10.0.0-dev",
                    "8.5.0-r0",
                    "8.5.0-r12",
                ],
                vec!["8.9.9", "8.9.9-dev", "8.5.0", "8.5.0-dev", "8.0.0"],
            ),
        ]);
    }

    #[test]
    fn include_literal_bypasses_pipeline() {
        assert_applies(vec![
            (
                "overrides system exclude",
                FilterConfig {
                    include: strs(&["1.25.0-rc1"]),
                    semver: Some(range(">=1.0")),
                    ..FilterConfig::default()
                },
                // 1.0.0-rc2 drops via system exclude; 1.25.0 passes the pipeline.
                vec!["1.25.0-rc1", "1.0.0-rc2", "1.25.0"],
                vec!["1.25.0-rc1", "1.25.0"],
            ),
            (
                // `latest` has no numeric prefix, so semver: alone would drop it.
                "overrides semver",
                FilterConfig {
                    include: strs(&["latest"]),
                    semver: Some(range(">=1.0")),
                    ..FilterConfig::default()
                },
                vec!["latest", "1.0.0", "0.9.0"],
                vec!["latest", "1.0.0"],
            ),
            (
                "overrides glob",
                FilterConfig {
                    include: strs(&["latest"]),
                    glob: strs(&["*-alpine"]),
                    ..FilterConfig::default()
                },
                vec!["latest", "1.0.0-alpine", "1.0.0"],
                vec!["latest", "1.0.0-alpine"],
            ),
            (
                "mapping exclude wins over include",
                FilterConfig {
                    include: strs(&["latest"]),
                    exclude: strs(&["latest"]),
                    ..FilterConfig::default()
                },
                vec!["latest", "1.0.0"],
                vec!["1.0.0"],
            ),
            (
                "overrides defaults_exclude",
                FilterConfig {
                    include: strs(&["latest-dev"]),
                    defaults_exclude: strs(&["*-dev"]),
                    ..FilterConfig::default()
                },
                // 1.0.0-dev does not match include and is dropped.
                vec!["latest", "latest-dev", "1.0.0", "1.0.0-dev"],
                vec!["latest-dev", "latest", "1.0.0"],
            ),
            (
                // Mapping exclude is the hard tier even with defaults_exclude set.
                "mapping exclude blocks include with defaults set",
                FilterConfig {
                    include: strs(&["latest", "latest-dev"]),
                    defaults_exclude: strs(&["*-dev"]),
                    exclude: strs(&["latest"]),
                    ..FilterConfig::default()
                },
                vec!["latest", "latest-dev"],
                vec!["latest-dev"],
            ),
            (
                "literal include is not capped by latest",
                FilterConfig {
                    include: strs(&["latest", "latest-dev"]),
                    semver: Some(range(">=1.0")),
                    sort: Some(SortOrder::Semver),
                    latest: Some(2),
                    ..FilterConfig::default()
                },
                vec![
                    "latest",
                    "latest-dev",
                    "1.5.0",
                    "1.4.0",
                    "1.3.0",
                    "1.2.0",
                    "1.1.0",
                ],
                vec!["latest", "latest-dev", "1.5.0", "1.4.0"],
            ),
            (
                // include and pipeline tags both count toward min_tags.
                "min_tags counts include and pipeline",
                FilterConfig {
                    include: strs(&["latest"]),
                    semver: Some(range(">=1.0")),
                    min_tags: Some(5),
                    ..FilterConfig::default()
                },
                vec!["latest", "1.0.0", "1.1.0", "1.2.0", "1.3.0"],
                vec!["latest", "1.0.0", "1.1.0", "1.2.0", "1.3.0"],
            ),
            (
                "pins a non-parseable tag alongside a glob include",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    include: strs(&["latest-dev", "*-dev"]),
                    semver: Some(range(">=2.0.0")),
                    ..FilterConfig::default()
                },
                vec!["latest-dev", "8.5.0-dev"],
                vec!["latest-dev", "8.5.0-dev"],
            ),
            (
                // Matches `recipes/semver-tracking.md`, "To pin a specific RC
                // alongside stable releases".
                "pins a parseable RC outside the semver range",
                FilterConfig {
                    include: strs(&["1.0.0-rc1"]),
                    semver: Some(range(">=2.0.0")),
                    ..FilterConfig::default()
                },
                vec!["1.0.0-rc1", "2.5.0", "3.0.0"],
                vec!["1.0.0-rc1", "2.5.0", "3.0.0"],
            ),
            (
                "literal include and glob include handled independently",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    include: strs(&["latest", "*-dev"]),
                    semver: Some(range(">=8.0.0, <9.0.0")),
                    ..FilterConfig::default()
                },
                // latest bypasses semver; 10.0.0-dev is rescued then fails it.
                vec!["latest", "8.5.0", "8.5.0-dev", "10.0.0-dev"],
                vec!["latest", "8.5.0", "8.5.0-dev"],
            ),
            (
                "empty include behaves like no include",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    include: vec![],
                    ..FilterConfig::default()
                },
                vec!["1.0.0", "1.0.0-dev"],
                vec!["1.0.0"],
            ),
        ]);
    }

    #[test]
    fn include_glob_runs_through_pipeline() {
        assert_applies(vec![
            (
                "rescues every match from defaults_exclude",
                FilterConfig {
                    include: strs(&["*-dev"]),
                    defaults_exclude: strs(&["*-dev"]),
                    ..FilterConfig::default()
                },
                // 1.0.0-rc1 is built-in soft tier, not matched by the `*-dev` include.
                vec![
                    "latest",
                    "latest-dev",
                    "1.0.0",
                    "1.0.0-dev",
                    "2.0.0-dev",
                    "1.0.0-rc1",
                ],
                vec!["latest", "latest-dev", "1.0.0", "1.0.0-dev", "2.0.0-dev"],
            ),
            (
                // Same suffix-presence ordering as `TagVersion::compare`:
                // 3.12.2 sorts before 3.12.2-dev.
                "constrained by semver, sort and latest",
                FilterConfig {
                    include: strs(&["*-dev"]),
                    defaults_exclude: strs(&["*-dev", "*-r[0-9]*"]),
                    semver: Some(range(">=3.12.0")),
                    sort: Some(SortOrder::Semver),
                    latest: Some(2),
                    ..FilterConfig::default()
                },
                vec![
                    "3.12.0",
                    "3.12.1",
                    "3.12.2",
                    "3.12.0-dev",
                    "3.12.1-dev",
                    "3.12.2-dev",
                    "3.12.0-r0",
                    "3.12.0-r10",
                ],
                vec!["3.12.2", "3.12.2-dev"],
            ),
            (
                // The hard tier (exclude:) still blocks include.
                "rescues from built-in and defaults soft tiers only",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    exclude: strs(&["*-debug-dev"]),
                    include: strs(&["*-rc*", "*-dev"]),
                    ..FilterConfig::default()
                },
                vec!["1.0.0-rc1", "1.0.0-dev", "1.0.0-debug-dev"],
                vec!["1.0.0-rc1", "1.0.0-dev"],
            ),
            (
                // Without the bypass, dev tags would drop at glob: before the
                // soft-tier exemption could apply.
                "rescues from the glob: filter",
                FilterConfig {
                    glob: strs(&["*-alpine"]),
                    include: strs(&["*-dev"]),
                    ..FilterConfig::default()
                },
                vec!["1.0.0-alpine", "1.0.0-dev", "1.0.0"],
                vec!["1.0.0-alpine", "1.0.0-dev"],
            ),
            (
                "dropped by semver when out of range",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    include: strs(&["*-dev"]),
                    semver: Some(range(">=8.0.0, <9.0.0")),
                    ..FilterConfig::default()
                },
                vec!["8.5.0", "8.5.0-dev", "10.0.0", "10.0.0-dev"],
                vec!["8.5.0", "8.5.0-dev"],
            ),
            (
                // Promote the include to a literal to keep such a tag.
                "non-parseable tag drops at semver",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    include: strs(&["*-dev"]),
                    semver: Some(range(">=2.0.0")),
                    ..FilterConfig::default()
                },
                vec!["latest-dev", "8.5.0-dev"],
                vec!["8.5.0-dev"],
            ),
            (
                "pin by glob drops the RC at semver",
                FilterConfig {
                    include: strs(&["1.0.0-*"]),
                    semver: Some(range(">=2.0.0")),
                    ..FilterConfig::default()
                },
                vec!["1.0.0-rc1", "2.5.0", "3.0.0"],
                vec!["2.5.0", "3.0.0"],
            ),
            (
                // Pins the claim in `recipes/semver-tracking.md` that
                // out-of-range prereleases drop.
                "out-of-range prereleases drop through semver",
                FilterConfig {
                    include: strs(&[
                        "*-rc*",
                        "*-alpha*",
                        "*-beta*",
                        "*-pre*",
                        "*-snapshot*",
                        "*-nightly*",
                    ]),
                    semver: Some(range(">=2.0")),
                    ..FilterConfig::default()
                },
                vec!["1.5.0-rc1", "2.0.0-rc1", "2.5.0-alpha1", "2.5.0", "1.5.0"],
                vec!["2.0.0-rc1", "2.5.0-alpha1", "2.5.0"],
            ),
        ]);
    }

    #[test]
    fn report_attributes_each_exclude_tier() {
        type IsKind = fn(&DropKind) -> bool;
        let defaults: IsKind = |k| matches!(k, DropKind::DefaultsExclude { .. });
        let mapping: IsKind = |k| matches!(k, DropKind::MappingExclude { .. });
        let builtin: IsKind = |k| matches!(k, DropKind::BuiltinExclude);
        // (name, config, tags, kinds that must appear in `dropped`)
        let cases = [
            (
                // The dry-run formatter renders (defaults)/(mapping)/(built-in)
                // from the variant, so each tier must stay distinct.
                "defaults, mapping and built-in",
                FilterConfig {
                    defaults_exclude: strs(&["*-dev"]),
                    exclude: strs(&["*-slim"]),
                    ..FilterConfig::default()
                },
                vec!["1.0.0", "1.0.0-dev", "1.0.0-rc1", "1.0.0-slim"],
                vec![defaults, mapping, builtin],
            ),
            (
                "mapping and built-in",
                FilterConfig {
                    exclude: strs(&["*-musl"]),
                    semver: Some(range(">=1.0")),
                    ..FilterConfig::default()
                },
                vec!["1.0.0", "1.0.0-musl", "1.1.0-rc1", "1.2.0"],
                vec![mapping, builtin],
            ),
        ];
        for (name, config, tags, want_kinds) in cases {
            let filtered = config.apply_with_report(&tags).unwrap();
            let kinds: Vec<&DropKind> = filtered.report.dropped.iter().map(|d| &d.kind).collect();
            for want in want_kinds {
                assert!(
                    kinds.iter().any(|k| want(k)),
                    "{name}: expected kind missing in {kinds:?}"
                );
            }
        }
    }

    #[test]
    fn report_kept_plus_dropped_equals_candidates() {
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
            glob: strs(&["1.*"]),
            semver: Some(range(">=1.18.0")),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        let total_dropped: usize = filtered.report.dropped.iter().map(|d| d.count).sum();
        assert_eq!(
            filtered.kept.len() + total_dropped,
            filtered.report.candidate_count
        );
    }

    #[test]
    fn report_with_include_path() {
        let tags = vec!["latest", "1.0.0", "1.1.0", "1.2.0", "0.9.0-rc1"];
        let config = FilterConfig {
            include: strs(&["latest"]),
            semver: Some(range(">=1.0")),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        assert_eq!(filtered.report.include_kept, vec!["latest".to_string()]);

        // Tags rescued only via `include:` still count as dropped from the
        // pipeline: they reach `kept` through the include union.
        let total_dropped: usize = filtered.report.dropped.iter().map(|d| d.count).sum();
        assert_eq!(filtered.report.candidate_count, 5, "candidate_count");
        assert_eq!(
            total_dropped, 3,
            "latest and 0.9.0-rc1 by semver, 1.0.0 by cap"
        );

        // latest is dropped by semver but rescued by include; 1.0.0 falls off
        // via latest=2; 0.9.0-rc1 fails semver.
        assert_eq!(filtered.kept, strs(&["latest", "1.2.0", "1.1.0"]));
    }

    #[test]
    fn report_include_kept_names() {
        // (name, config, tags, want)
        let cases = [
            (
                "carries rescued names in input order",
                FilterConfig {
                    include: strs(&["latest", "latest-dev"]),
                    semver: Some(range(">=1.0")),
                    ..FilterConfig::default()
                },
                vec!["latest", "latest-dev", "1.0.0", "1.1.0", "0.9.0-rc1"],
                vec!["latest", "latest-dev"],
            ),
            (
                "omits tags removed by mapping exclude",
                FilterConfig {
                    include: strs(&["latest"]),
                    exclude: strs(&["latest"]),
                    ..FilterConfig::default()
                },
                vec!["latest", "1.0.0"],
                vec![],
            ),
        ];
        for (name, config, tags, want) in cases {
            let filtered = config.apply_with_report(&tags).unwrap();
            assert_eq!(filtered.report.include_kept, strs(&want), "{name}");
        }
    }

    #[test]
    fn report_dropped_is_pareto_sorted() {
        let tags: Vec<String> = (0..20).map(|i| format!("1.{i}.0")).collect();
        let mut tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        tag_refs.extend(["edge", "latest", "nightly", "1.0.0-rc1", "1.0.0-alpha"]);
        let config = FilterConfig {
            glob: strs(&["1.*"]),
            semver: Some(range(">=1.10")),
            sort: Some(SortOrder::Semver),
            latest: Some(3),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tag_refs).unwrap();
        for window in filtered.report.dropped.windows(2) {
            assert!(
                window[0].count >= window[1].count,
                "not Pareto-sorted: {:?}",
                filtered.report.dropped
            );
        }
    }

    #[test]
    fn report_carries_min_tags_without_enforcing_it() {
        // (name, config, tags, want min_tags, want kept len, want apply ok)
        let cases = [
            (
                "violated: apply errors, report still returns",
                FilterConfig {
                    sort: Some(SortOrder::Semver),
                    min_tags: Some(10),
                    ..FilterConfig::default()
                },
                vec!["1.0.0", "1.1.0"],
                Some(10),
                2,
                false,
            ),
            (
                "satisfied",
                FilterConfig {
                    sort: Some(SortOrder::Semver),
                    min_tags: Some(2),
                    ..FilterConfig::default()
                },
                vec!["1.0.0", "1.1.0", "1.2.0"],
                Some(2),
                3,
                true,
            ),
            (
                "absent when not configured",
                FilterConfig::default(),
                vec!["1.0.0", "1.1.0"],
                None,
                2,
                true,
            ),
        ];
        for (name, config, tags, want_min, want_kept, want_ok) in cases {
            let filtered = config.apply_with_report(&tags).unwrap();
            assert_eq!(filtered.report.min_tags, want_min, "{name}: min_tags");
            assert_eq!(filtered.kept.len(), want_kept, "{name}: kept");
            let applied = config.apply(&tags);
            assert_eq!(applied.is_ok(), want_ok, "{name}: apply ok");
            if !want_ok {
                assert!(
                    matches!(applied, Err(Error::BelowMinTags { .. })),
                    "{name}: error kind"
                );
            }
        }
    }

    #[test]
    fn report_has_no_zero_count_reasons() {
        // A stage that drops nothing must not appear in `dropped`.
        let tags = vec!["1.0.0", "1.1.0", "1.2.0"];
        let config = FilterConfig {
            glob: strs(&["*"]),
            semver: Some(range(">=1.0")),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        assert!(filtered.report.dropped.is_empty());
        assert_eq!(filtered.kept.len(), filtered.report.candidate_count);
    }

    #[test]
    fn describe_default_returns_none() {
        assert!(FilterConfig::default().describe().is_none());
    }

    #[test]
    fn describe_combines_clauses_in_pipeline_order() {
        let config = FilterConfig {
            glob: strs(&["1.*"]),
            semver: Some(range(">=1.0.0")),
            exclude: strs(&["*-rc*"]),
            sort: Some(SortOrder::Semver),
            latest: Some(5),
            min_tags: Some(1),
            ..FilterConfig::default()
        };
        assert_eq!(
            config.describe().as_deref(),
            Some("glob 1.*, semver >=1.0.0, exclude *-rc*, sort semver, latest=5, min_tags=1")
        );
    }

    /// Every `FilterConfig` field that influences selection must contribute to
    /// `describe()`, or the INFO-line rationale goes stale.
    #[test]
    fn describe_covers_every_selection_field() {
        let cfg = FilterConfig {
            include: strs(&["latest"]),
            glob: strs(&["v*"]),
            semver: Some(range(">=1")),
            defaults_exclude: strs(&["*-dev"]),
            exclude: strs(&["nightly"]),
            sort: Some(SortOrder::Alpha),
            latest: Some(3),
            min_tags: Some(2),
        };
        let desc = cfg.describe().expect("non-empty config describes");
        for needle in [
            "include latest",
            "glob v*",
            "semver >=1",
            "exclude *-dev,nightly",
            "sort alpha",
            "latest=3",
            "min_tags=2",
        ] {
            assert!(desc.contains(needle), "missing {needle:?} in {desc:?}");
        }
    }

    #[test]
    fn referrers_fallback_detection() {
        // (tag, want)
        let cases = [
            ("sha256-abcdef", true),
            ("sha256-deadbeef.sig", true),
            ("sha512-cafef00d.sbom", true),
            ("v1.0.0", false),
            ("latest", false),
            ("sha256", false), // no dash
        ];
        for (tag, want) in cases {
            assert_eq!(is_referrers_fallback_tag(tag), want, "{tag}");
        }
    }

    #[test]
    fn is_literal_pattern_classifies() {
        let cases = [
            ("latest", true),
            ("1.25.5", true),
            ("latest-dev", true),
            ("v1.2.3-rc1", true),
            // No metacharacters, so literal, though the parser rejects empty
            // patterns before they reach the pipeline.
            ("", true),
            ("*-dev", false),
            ("v?.*.*", false),
            ("*-r[0-9]*", false),
            ("foo[abc]bar", false),
        ];
        for (pattern, want) in cases {
            assert_eq!(is_literal_pattern(pattern), want, "{pattern:?}");
        }
    }
}
