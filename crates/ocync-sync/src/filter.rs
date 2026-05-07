//! Tag filtering pipeline: glob -> semver -> exclude -> sort + latest.

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use tracing::debug;

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
/// Tags matching `include:` bypass the glob/semver pipeline AND the soft
/// exclude tier (built-in + defaults). They are still subject to mapping
/// `exclude:` (the hard tier).
///
/// # Exclude tiers
///
/// Exclusion has two tiers:
///
/// - **Soft tier** (built-in `SYSTEM_EXCLUDE` + caller-provided
///   [`defaults_exclude`](Self::defaults_exclude)): bypassable by
///   `include:`. Use for project-wide opinions like "drop `*-dev` unless
///   I say otherwise on a specific mapping."
/// - **Hard tier** ([`exclude`](Self::exclude)): blocks `include:` on the
///   same config. Use for absolute per-mapping denies.
#[derive(Debug, Default)]
pub struct FilterConfig {
    /// Always-include glob patterns. Tags matching any pattern survive
    /// `glob:`/`semver:` filters and the soft exclude tier (system + defaults).
    /// Not subject to `sort:` or `latest:` truncation (those only cap the
    /// `glob:`/`semver:` pipeline side). Subject to mapping
    /// [`exclude`](Self::exclude). Same glob syntax as `exclude:`.
    pub include: Vec<String>,
    /// Glob patterns (OR semantics). An empty list passes all tags through.
    pub glob: Vec<String>,
    /// Semver version range constraint (e.g. `>=1.18.0`).
    pub semver: Option<String>,
    /// Soft-tier exclude patterns inherited from a `defaults:` block.
    /// Bypassed by [`include`](Self::include), unlike
    /// [`exclude`](Self::exclude). Behaves the same as the built-in
    /// `SYSTEM_EXCLUDE` list.
    pub defaults_exclude: Vec<String>,
    /// Hard-tier exclude patterns (OR deny). Blocks
    /// [`include`](Self::include) on the same config.
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

    /// Run the full filter pipeline and return both the kept tags and a
    /// trace of how the pipeline arrived at them.
    ///
    /// Does NOT enforce [`min_tags`](Self::min_tags); the caller checks it
    /// against `filtered.kept.len()`. This lets `--dry-run` show what
    /// survived even when `min_tags` would otherwise turn the run into an
    /// error. The report carries `min_tags` so callers can render the
    /// configured limit alongside the actual count.
    pub fn apply_with_report(&self, tags: &[&str]) -> Result<Filtered, Error> {
        self.run_pipeline(tags, true)
    }

    /// One-line summary of the active filter clauses, e.g.
    /// `semver >=1.0.0, latest=5`. Returns `None` when no filtering applies.
    ///
    /// Sole formatter for filter rationale shown in non-dry-run logs; uses
    /// the same `FilterConfig` fields the pipeline operates on so adding a
    /// new field to the config will fail tests here before it ships.
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.glob.is_empty() {
            parts.push(format!("glob {}", self.glob.join(",")));
        }
        if let Some(ref s) = self.semver {
            parts.push(format!("semver {s}"));
        }
        if !self.exclude.is_empty() || !self.defaults_exclude.is_empty() {
            // Defaults- and mapping-tier patterns share one summary clause.
            // Dry-run carries the tier attribution; the INFO line stays tight.
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

    /// Shared pipeline implementation. When `track` is false (the real-sync
    /// hot path), per-stage `StageDelta` and per-reason `DropReason` are not
    /// constructed; the resulting `Filtered.report` carries empty vectors.
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

        // Partition include patterns by shape. Literals bypass the entire
        // pipeline (union after). Globs rescue from `glob:` and the soft-tier
        // excludes, then run through the rest of the pipeline.
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

        if let Some(ref range) = self.semver {
            let before = pipeline.len();
            let req =
                crate::version::Range::parse(range).map_err(|e| Error::InvalidVersionRange {
                    range: range.to_owned(),
                    reason: e.to_string(),
                })?;
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
                DropKind::Semver {
                    range: range.clone(),
                },
                dropped,
            );
            pipeline = kept;
            if track {
                pipeline_stages.push(StageDelta {
                    label: format!("semver \"{range}\""),
                    count_in: before,
                    count_out: pipeline.len(),
                });
            }
        }

        // Exclude stage: three tiers, evaluated in order so the first match
        // attributes the drop. Order doesn't change kept tags (they're all
        // OR-deny); it only decides which DropKind a tag is reported under.
        // Mapping (hard) is checked first because it represents the most
        // specific user intent.
        let before_exclude = pipeline.len();
        let mut mapping_dropped: Vec<String> = Vec::new();
        let mut defaults_dropped: Vec<String> = Vec::new();
        let mut builtin_dropped: Vec<String> = Vec::new();
        pipeline.retain(|t| {
            // Hard tier (mapping `exclude:`) always applies, including to
            // include-glob matches. Most-specific user intent wins.
            if let Some(ref s) = user_exclude_set {
                if s.is_match(t) {
                    if track {
                        mapping_dropped.push((*t).to_owned());
                    }
                    return false;
                }
            }
            // Soft tier (defaults + built-in) is bypassed when a glob `include:`
            // pattern matches. Literal `include:` matches never reach this path
            // (they bypass the pipeline entirely via the literal union).
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

        // Union: include first (preserves include input order), then pipeline
        // tags not already in include. The order matters for the dry-run
        // formatter's "include first, then pipeline" header.
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

        // Track-only: hot path discards the report, so don't allocate names.
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

/// Result of applying a [`FilterConfig`] with reporting attached.
///
/// `kept` is the same `Vec<String>` that [`FilterConfig::apply`] returns.
/// `report` describes how the pipeline arrived at it.
#[derive(Debug)]
pub struct Filtered {
    /// Tags that survive the full pipeline.
    pub kept: Vec<String>,
    /// Stage-by-stage attrition and per-reason drop attribution.
    pub report: FilterReport,
}

/// Per-mapping filter pipeline trace.
///
/// `candidate_count` is the source-tag count fed in. `pipeline` lists each
/// stage (label + `count_in` -> `count_out`). `dropped` is Pareto-sorted by
/// drop count across all stages, suitable for "where did most of my tags go"
/// output. `include_kept` carries the names of tags rescued via the
/// `include:` path so the formatter can call them out by name.
///
/// Distinct from `ocync_sync::SyncReport` (the run-level engine report);
/// this is the per-mapping filter trace consumed by `--dry-run`.
#[derive(Debug)]
pub struct FilterReport {
    /// Number of source tags fed into the pipeline.
    pub candidate_count: usize,
    /// Tag names admitted via the `include:` path. Empty when `include:` is
    /// not configured or no tag matched. These tags bypass the
    /// glob/semver/sort/latest pipeline (but are still subject to user
    /// `exclude:`).
    pub include_kept: Vec<String>,
    /// Stage-by-stage attrition along the pipeline (top-down order).
    pub pipeline: Vec<StageDelta>,
    /// Drop reasons sorted by count descending (Pareto).
    pub dropped: Vec<DropReason>,
    /// Configured `min_tags:` value, if set. The dry-run formatter compares
    /// this against `kept.len()` so the user sees whether real-sync would
    /// fail with `BelowMinTags`. `apply()` enforces this directly;
    /// `apply_with_report()` does not (the report is the point of dry-run).
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

/// Single-pass partition of `input` into kept references and (when `track`)
/// owned dropped names. The `else if track` skips `to_owned` allocation on
/// the watch-mode hot path. Used by the glob and semver stages; the exclude
/// stage is structured differently because it splits drops between
/// user- and system-attributed buckets.
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

/// Append a `DropReason` to `drop_reasons` when both `track` is true and
/// at least one tag was dropped at this stage.
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

// ---------------------------------------------------------------------------
// Individual stages
// ---------------------------------------------------------------------------

/// True for OCI 1.1 referrers fallback tags (`<algo>-<hex>` and the cosign
/// `.sig`/`.sbom`/`.att` variants). These are pointers to artifacts, not image
/// versions, and will never satisfy a semver range -- skip parsing so they do
/// not appear in the unparseable-tag log channel.
///
/// Public so that observability/UX code outside the filter pipeline (e.g. the
/// CLI's "no tags matched" warn) can partition source tag lists without
/// reintroducing a duplicate prefix check that would drift over time.
pub fn is_referrers_fallback_tag(tag: &str) -> bool {
    tag.starts_with("sha256-") || tag.starts_with("sha512-")
}

/// Returns `true` when `pattern` contains no glob metacharacters
/// (`*`, `?`, `[`).
pub fn is_literal_pattern(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '['])
}

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
///
/// Test-only convenience over [`build_glob_set`] + [`Iterator::filter`].
/// Production callers go through [`FilterConfig::apply`], which inlines
/// the same logic with single-pass drop attribution.
#[cfg(test)]
fn filter_glob<'a>(tags: &[&'a str], patterns: &[String]) -> Result<Vec<&'a str>, Error> {
    let set = build_glob_set(patterns)?;
    Ok(tags.iter().copied().filter(|t| set.is_match(t)).collect())
}

/// Filter tags by a version range.
///
/// Tags that cannot be parsed as a version are dropped with a warning.
/// Test-only; production callers go through [`FilterConfig::apply`], which
/// inlines the same logic with single-pass drop attribution.
#[cfg(test)]
fn filter_semver<'a>(tags: &[&'a str], range: &str) -> Result<Vec<&'a str>, Error> {
    let req = crate::version::Range::parse(range).map_err(|e| Error::InvalidVersionRange {
        range: range.to_owned(),
        reason: e.to_string(),
    })?;

    Ok(tags
        .iter()
        .copied()
        .filter(|tag| {
            if is_referrers_fallback_tag(tag) {
                return false;
            }
            let Some(ver) = crate::version::TagVersion::parse(tag) else {
                debug!(tag, "tag is not parseable as a version, dropping");
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

    /// Referrers fallback tags (cosign signatures, SBOMs, attestations) bypass
    /// the version parser. They drop silently so noisy unparseable-tag logs do
    /// not fire once per artifact tag per image.
    #[test]
    fn semver_skips_referrers_fallback_tags() {
        let tags = vec![
            "1.0.0",
            "sha256-abc123def456.sig",
            "sha256-abc123def456.sbom",
            "sha256-abc123def456.att",
            "sha256-abc123def456",
            "sha512-deadbeef.sig",
            "2.0.0",
        ];
        let result = filter_semver(&tags, ">=1.0.0").unwrap();
        assert_eq!(result, vec!["1.0.0", "2.0.0"]);
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

    // - defaults_exclude tier --------------------------------------------

    /// `defaults_exclude` drops tags from the pipeline just like the
    /// built-in system exclude. The mapping has no `exclude:` of its own,
    /// so this proves defaults flow through.
    #[test]
    fn defaults_exclude_drops_tags() {
        let tags = vec!["1.0.0", "1.0.0-dev", "1.0.0-r0"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into(), "*-r[0-9]*".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.0.0".to_string()]);
    }

    /// `include:` rescues a tag that `defaults_exclude` would drop. This is
    /// the user-facing escape hatch for the project-wide exclude floor.
    #[test]
    fn include_overrides_defaults_exclude() {
        let tags = vec!["latest", "latest-dev", "1.0.0", "1.0.0-dev"];
        let config = FilterConfig {
            include: vec!["latest-dev".into()],
            defaults_exclude: vec!["*-dev".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"latest".to_string()));
        assert!(
            result.contains(&"latest-dev".to_string()),
            "include should rescue latest-dev from defaults_exclude"
        );
        assert!(result.contains(&"1.0.0".to_string()));
        // 1.0.0-dev does not match include and is dropped by defaults_exclude.
        assert!(!result.contains(&"1.0.0-dev".to_string()));
    }

    /// Mapping-level `exclude:` is the hard tier: it blocks `include:` even
    /// when a `defaults_exclude` is also configured. Negative assertion
    /// preserving existing semantics.
    #[test]
    fn mapping_exclude_blocks_include_with_defaults_set() {
        let tags = vec!["latest", "latest-dev"];
        let config = FilterConfig {
            include: vec!["latest".into(), "latest-dev".into()],
            defaults_exclude: vec!["*-dev".into()],
            exclude: vec!["latest".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        // mapping.exclude = ["latest"] blocks include of "latest"
        assert!(!result.contains(&"latest".to_string()));
        // include still rescues latest-dev (matches defaults_exclude soft tier only)
        assert!(result.contains(&"latest-dev".to_string()));
    }

    /// `defaults_exclude` and `exclude` (mapping) both apply: their union
    /// drops tags. Stacking, not replacement.
    #[test]
    fn defaults_and_mapping_exclude_stack() {
        let tags = vec!["1.0.0", "1.0.0-dev", "1.0.0-slim"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            exclude: vec!["*-slim".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.0.0".to_string()]);
    }

    /// `include:` is glob-matched, not literal-matched: `include: ["*-dev"]`
    /// rescues every `-dev` tag from a `defaults_exclude: ["*-dev"]` soft
    /// tier. Companion to `include_overrides_defaults_exclude`, which only
    /// covers the literal-include case.
    #[test]
    fn include_glob_rescues_all_matching_from_defaults_exclude() {
        let tags = vec![
            "latest",
            "latest-dev",
            "1.0.0",
            "1.0.0-dev",
            "2.0.0-dev",
            "1.0.0-rc1", // built-in soft tier, not matched by `*-dev` include
        ];
        let config = FilterConfig {
            include: vec!["*-dev".into()],
            defaults_exclude: vec!["*-dev".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"latest-dev".to_string()));
        assert!(result.contains(&"1.0.0-dev".to_string()));
        assert!(result.contains(&"2.0.0-dev".to_string()));
        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"1.0.0".to_string()));
        assert!(
            !result.contains(&"1.0.0-rc1".to_string()),
            "soft-tier patterns the include glob does NOT match are still dropped"
        );
    }

    /// Glob character classes work against real tag strings. `*-r[0-9]*`
    /// drops Chainguard/Bitnami `-r<N>` build counters at any digit width.
    /// Documents the recipe shape suggested in `configuration.md` for
    /// suffixes deliberately absent from the built-in soft tier.
    #[test]
    fn defaults_exclude_character_class_drops_r_counters() {
        let tags = vec![
            "1.25.5",
            "1.25.5-r0",
            "1.25.5-r9",
            "1.25.5-r10",
            "1.25.5-r123",
        ];
        let config = FilterConfig {
            defaults_exclude: vec!["*-r[0-9]*".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.25.5".to_string()]);
    }

    /// Glob `include:` is constrained by the pipeline. Stable + dev tags share
    /// one pipeline; `sort:` orders them together; `latest:` caps the union.
    /// Same suffix-presence ordering as `version::TagVersion::compare`: empty
    /// suffix wins, so `3.12.2` sorts before `3.12.2-dev` at the same prefix.
    #[test]
    fn include_glob_constrained_by_semver_sort_latest() {
        let tags = vec![
            "3.12.0",
            "3.12.1",
            "3.12.2",
            "3.12.0-dev",
            "3.12.1-dev",
            "3.12.2-dev",
            "3.12.0-r0",
            "3.12.0-r10",
        ];
        let config = FilterConfig {
            include: vec!["*-dev".into()],
            defaults_exclude: vec!["*-dev".into(), "*-r[0-9]*".into()],
            semver: Some(">=3.12.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        // Top 2 by semver desc with empty-suffix winning: 3.12.2 then 3.12.2-dev.
        assert_eq!(result, vec!["3.12.2".to_string(), "3.12.2-dev".to_string()]);
    }

    /// Prescription 1: drop `*-dev` from `defaults_exclude` and let
    /// `semver:` constrain the dev tags through the pipeline. The lenient
    /// parser admits `8.5.0-dev` as prefix `[8,5,0]` (suffix opaque), so
    /// the range `>=8.0.0, <9.0.0` keeps `8.5.0-dev` and rejects
    /// `10.0.0-dev`. No `include:` involved.
    #[test]
    fn semver_constrains_dev_tags_when_not_in_defaults_exclude() {
        let tags = vec!["8.0.0", "8.5.0", "8.5.0-dev", "10.0.0", "10.0.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-r[0-9]*".into()],
            semver: Some(">=8.0.0, <9.0.0".into()),
            sort: Some(SortOrder::Semver),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"8.0.0".to_string()));
        assert!(result.contains(&"8.5.0".to_string()));
        assert!(
            result.contains(&"8.5.0-dev".to_string()),
            "8.5.0-dev should pass: prefix [8,5,0] matches >=8.0.0, <9.0.0"
        );
        assert!(
            !result.contains(&"10.0.0".to_string()),
            "10.0.0 should be rejected by <9.0.0"
        );
        assert!(
            !result.contains(&"10.0.0-dev".to_string()),
            "10.0.0-dev should be rejected: prefix [10,0,0] fails <9.0.0"
        );
    }

    /// `glob:` does NOT rescue from soft-tier excludes. The `glob:` stage
    /// is upstream of the exclude stage; even when `glob:` admits a tag,
    /// `defaults_exclude` drops it downstream. Only `include:` rescues
    /// from soft tier (literal halves bypass the entire pipeline; glob
    /// halves bypass `glob:` and soft tier and then run through the rest
    /// of the pipeline).
    #[test]
    fn glob_does_not_rescue_dev_from_defaults_exclude() {
        let tags = vec!["8.0.0", "8.5.0-dev", "10.0.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            glob: vec!["*-dev".into()],
            semver: Some(">=8.0.0, <9.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            !result.contains(&"8.5.0-dev".to_string()),
            "soft tier still drops -dev even when glob admits it"
        );
        assert!(
            !result.contains(&"10.0.0-dev".to_string()),
            "soft tier still drops -dev regardless of semver intent"
        );
        assert_eq!(
            result,
            Vec::<String>::new(),
            "two-mapping pattern produces empty result, not a bounded -dev set"
        );
    }

    /// End-to-end Chainguard-style mirror: stable releases + dev variants
    /// both constrained to a semver range, with `-r<N>` build counters
    /// always denied. The only working shape, per the surrounding tests:
    /// `*-dev` is NOT in `defaults_exclude`, `semver:` does the work for
    /// both stable and dev tags, and per-mapping hard-tier `exclude:` on
    /// other mappings denies `-dev` where unwanted.
    ///
    /// This test models the dev-wanting mapping. The companion stable-only
    /// mapping is covered by `defaults_and_mapping_exclude_stack` plus a
    /// mapping-level `exclude: ["*-dev"]`.
    #[test]
    fn dev_constrained_by_semver_end_to_end() {
        let tags = vec![
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
        ];
        let config = FilterConfig {
            defaults_exclude: vec!["*-r[0-9]*".into()],
            semver: Some(">=8.0.0, <9.0.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(10),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();

        for t in ["8.0.0", "8.5.0", "8.9.9", "8.5.0-dev", "8.9.9-dev"] {
            assert!(result.contains(&t.to_string()), "{t} should be kept");
        }
        for t in [
            "9.0.0",
            "9.0.0-dev",
            "10.0.0",
            "10.0.0-dev",
            "8.5.0-r0",
            "8.5.0-r12",
        ] {
            assert!(!result.contains(&t.to_string()), "{t} should be dropped");
        }
    }

    /// Dry-run attribution: defaults-tier drops surface as
    /// `DropKind::DefaultsExclude`, distinct from `MappingExclude` and
    /// `BuiltinExclude`. The formatter relies on the variant to render
    /// `(defaults)` / `(mapping)` / `(built-in)`.
    #[test]
    fn report_attributes_defaults_exclude_separately() {
        let tags = vec!["1.0.0", "1.0.0-dev", "1.0.0-rc1", "1.0.0-slim"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            exclude: vec!["*-slim".into()],
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        let kinds: Vec<&DropKind> = filtered.report.dropped.iter().map(|d| &d.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, DropKind::DefaultsExclude { .. })),
            "missing DefaultsExclude in {kinds:?}"
        );
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, DropKind::MappingExclude { .. })),
            "missing MappingExclude in {kinds:?}"
        );
        assert!(
            kinds.iter().any(|k| matches!(k, DropKind::BuiltinExclude)),
            "missing BuiltinExclude (1.0.0-rc1 should hit it) in {kinds:?}"
        );
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
        let filtered = config.apply_with_report(&tags).unwrap();
        let total_dropped: usize = filtered.report.dropped.iter().map(|d| d.count).sum();
        assert_eq!(
            filtered.kept.len() + total_dropped,
            filtered.report.candidate_count
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
        let filtered = config.apply_with_report(&tags).unwrap();

        // include_kept now carries names: just `latest` was rescued.
        assert_eq!(filtered.report.include_kept, vec!["latest".to_string()]);

        // Pipeline accounting (closed): every candidate either survives the
        // pipeline or appears in `dropped`. Tags rescued only via `include:`
        // still count as dropped here -- they reach `kept` via the include
        // path, not the pipeline.
        let total_dropped: usize = filtered.report.dropped.iter().map(|d| d.count).sum();
        let pipeline_survivors = filtered.report.candidate_count - total_dropped;

        // Union semantics: final kept is `include_kept + pipeline_survivors`
        // minus their overlap (tags that both include and the pipeline kept).
        // So `kept` is bounded above by the unduplicated sum and below by
        // pipeline survivors alone.
        assert!(
            filtered.kept.len() <= filtered.report.include_kept.len() + pipeline_survivors,
            "kept={} > include_kept={} + pipeline_survivors={}",
            filtered.kept.len(),
            filtered.report.include_kept.len(),
            pipeline_survivors,
        );
        assert!(
            filtered.kept.len() >= pipeline_survivors,
            "kept={} < pipeline_survivors={}",
            filtered.kept.len(),
            pipeline_survivors,
        );

        // Concrete shape for this fixture: latest is dropped by semver but
        // rescued by include; 1.0.0 falls off via latest=2; 0.9.0-rc1 fails
        // semver. Final kept = {latest, 1.2.0, 1.1.0}.
        assert_eq!(filtered.kept.len(), 3);
        assert!(filtered.kept.contains(&"latest".to_string()));
        assert!(filtered.kept.contains(&"1.2.0".to_string()));
        assert!(filtered.kept.contains(&"1.1.0".to_string()));
        assert!(!filtered.kept.contains(&"1.0.0".to_string()));
        assert!(!filtered.kept.contains(&"0.9.0-rc1".to_string()));
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
    fn report_min_tags_does_not_block_apply_with_report() {
        // apply() returns BelowMinTags; apply_with_report() returns Filtered anyway.
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
        // apply_with_report succeeds with partial data, and surfaces min_tags
        // in the report so the caller can render the gap to the user.
        let filtered = config.apply_with_report(&tags).unwrap();
        assert_eq!(filtered.kept.len(), 2);
        assert_eq!(filtered.report.min_tags, Some(10));
    }

    #[test]
    fn report_min_tags_satisfied_round_trip() {
        // When min_tags is satisfied, apply_with_report still surfaces the
        // configured value so the formatter can show "satisfied" status.
        let tags = vec!["1.0.0", "1.1.0", "1.2.0"];
        let config = FilterConfig {
            sort: Some(SortOrder::Semver),
            min_tags: Some(2),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        assert_eq!(filtered.report.min_tags, Some(2));
        assert!(filtered.kept.len() >= 2);
        // And apply() succeeds (kept >= min):
        assert!(config.apply(&tags).is_ok());
    }

    #[test]
    fn report_min_tags_absent_when_not_configured() {
        let tags = vec!["1.0.0", "1.1.0"];
        let config = FilterConfig::default();
        let filtered = config.apply_with_report(&tags).unwrap();
        assert_eq!(filtered.report.min_tags, None);
    }

    #[test]
    fn report_drop_reasons_split_user_and_system_exclude() {
        let tags = vec!["1.0.0", "1.0.0-musl", "1.1.0-rc1", "1.2.0"];
        let config = FilterConfig {
            exclude: vec!["*-musl".into()],
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        let kinds: Vec<&DropKind> = filtered.report.dropped.iter().map(|d| &d.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, DropKind::MappingExclude { .. })),
            "missing MappingExclude in {kinds:?}"
        );
        assert!(
            kinds.iter().any(|k| matches!(k, DropKind::BuiltinExclude)),
            "missing BuiltinExclude in {kinds:?}"
        );
    }

    /// `include_kept` carries the actual rescued tag names so the dry-run
    /// formatter can call them out by name. Order matches input order
    /// (preserved by the union step).
    #[test]
    fn report_include_kept_carries_rescued_names() {
        let tags = vec!["latest", "latest-dev", "1.0.0", "1.1.0", "0.9.0-rc1"];
        let config = FilterConfig {
            include: vec!["latest".into(), "latest-dev".into()],
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        assert_eq!(
            filtered.report.include_kept,
            vec!["latest".to_string(), "latest-dev".to_string()]
        );
    }

    /// Tags matched by user-exclude do not show up in `include_kept` even
    /// when an include pattern would have rescued them: user-exclude wins.
    #[test]
    fn report_include_kept_omits_user_excluded() {
        let tags = vec!["latest", "1.0.0"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            exclude: vec!["latest".into()],
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        assert!(filtered.report.include_kept.is_empty());
    }

    /// Single-pass attribution invariant: every drop attributed to a stage
    /// has a non-zero count. A stage that drops nothing must not appear in
    /// `dropped` at all.
    #[test]
    fn report_drop_reasons_have_non_zero_counts() {
        let tags = vec!["1.0.0", "1.1.0", "1.2.0"];
        let config = FilterConfig {
            glob: vec!["*".into()],
            semver: Some(">=1.0".into()),
            ..FilterConfig::default()
        };
        let filtered = config.apply_with_report(&tags).unwrap();
        // Nothing dropped: no entries.
        assert!(filtered.report.dropped.is_empty());
        // Sanity: nothing dropped means counts add up to candidate_count.
        assert_eq!(filtered.kept.len(), filtered.report.candidate_count);
        for reason in &filtered.report.dropped {
            assert!(reason.count > 0, "stage attributed zero drops: {reason:?}");
            assert_eq!(reason.count, reason.samples.len());
        }
    }

    // - describe ----------------------------------------------------------

    #[test]
    fn describe_default_returns_none() {
        assert!(FilterConfig::default().describe().is_none());
    }

    #[test]
    fn describe_combines_clauses_in_pipeline_order() {
        let config = FilterConfig {
            glob: vec!["1.*".into()],
            semver: Some(">=1.0.0".into()),
            exclude: vec!["*-rc*".into()],
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

    /// Regression guard: every `FilterConfig` field that influences selection
    /// must contribute to `describe()`. Adding a new field without updating
    /// `describe()` would silently leave INFO-line filter rationale stale.
    #[test]
    fn describe_covers_every_selection_field() {
        let cfg = FilterConfig {
            include: vec!["latest".into()],
            glob: vec!["v*".into()],
            semver: Some("^1".into()),
            defaults_exclude: vec!["*-dev".into()],
            exclude: vec!["nightly".into()],
            sort: Some(SortOrder::Alpha),
            latest: Some(3),
            min_tags: Some(2),
        };
        let desc = cfg.describe().expect("non-empty config describes");
        for needle in [
            "include latest",
            "glob v*",
            "semver ^1",
            "exclude *-dev,nightly",
            "sort alpha",
            "latest=3",
            "min_tags=2",
        ] {
            assert!(desc.contains(needle), "missing {needle:?} in {desc:?}");
        }
    }

    // - is_referrers_fallback_tag ----------------------------------------

    #[test]
    fn referrers_fallback_detection() {
        assert!(is_referrers_fallback_tag("sha256-abcdef"));
        assert!(is_referrers_fallback_tag("sha256-deadbeef.sig"));
        assert!(is_referrers_fallback_tag("sha512-cafef00d.sbom"));
        assert!(!is_referrers_fallback_tag("v1.0.0"));
        assert!(!is_referrers_fallback_tag("latest"));
        assert!(!is_referrers_fallback_tag("sha256")); // no dash
    }

    /// Glob `include:` rescues tags from the soft tier (built-in
    /// `SYSTEM_EXCLUDE` plus `defaults_exclude`). Hard tier (`exclude:`) is
    /// unchanged: it still blocks include.
    #[test]
    fn include_glob_rescues_from_soft_tier_only() {
        let tags = vec!["1.0.0-rc1", "1.0.0-dev", "1.0.0-debug-dev"];
        let config = FilterConfig {
            // `*-rc*` is in built-in SYSTEM_EXCLUDE; `*-dev` we add as defaults.
            defaults_exclude: vec!["*-dev".into()],
            // Hard tier denies one specific dev variant.
            exclude: vec!["*-debug-dev".into()],
            include: vec!["*-rc*".into(), "*-dev".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            result.contains(&"1.0.0-rc1".to_string()),
            "rescued from built-in soft tier"
        );
        assert!(
            result.contains(&"1.0.0-dev".to_string()),
            "rescued from defaults soft tier"
        );
        assert!(
            !result.contains(&"1.0.0-debug-dev".to_string()),
            "hard tier still blocks include"
        );
    }

    /// Glob `include:` rescues tags from the `glob:` positive filter. Without
    /// this bypass, dev tags would be dropped at the `glob:` stage before
    /// soft-tier exemption could apply.
    #[test]
    fn include_glob_rescues_from_glob_filter() {
        let tags = vec!["1.0.0-alpine", "1.0.0-dev", "1.0.0"];
        let config = FilterConfig {
            glob: vec!["*-alpine".into()],
            include: vec!["*-dev".into()],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"1.0.0-alpine".to_string()));
        assert!(
            result.contains(&"1.0.0-dev".to_string()),
            "include glob bypasses the glob: positive filter"
        );
        assert!(
            !result.contains(&"1.0.0".to_string()),
            "tags matching neither glob nor include glob are dropped"
        );
    }

    /// Partition is internal but observable through the apply path: a pure
    /// literal include retains today's bypass-everything semantics, including
    /// surviving a `semver:` filter that the literal does not parse against.
    #[test]
    fn literal_include_partition_bypasses_semver() {
        let tags = vec!["latest", "1.0.0", "2.0.0"];
        let config = FilterConfig {
            include: vec!["latest".into()],
            semver: Some(">=2.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"2.0.0".to_string()));
        assert!(!result.contains(&"1.0.0".to_string()));
    }

    /// Glob include constrained by semver: rescues tags from soft tier, then
    /// drops them if `semver:` rejects. The case-6 fix in concrete form.
    #[test]
    fn include_glob_dropped_by_semver_when_out_of_range() {
        let tags = vec!["8.5.0", "8.5.0-dev", "10.0.0", "10.0.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            include: vec!["*-dev".into()],
            semver: Some(">=8.0.0, <9.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(result.contains(&"8.5.0".to_string()));
        assert!(result.contains(&"8.5.0-dev".to_string()));
        assert!(!result.contains(&"10.0.0".to_string()));
        assert!(
            !result.contains(&"10.0.0-dev".to_string()),
            "include glob is constrained by semver"
        );
    }

    /// Mixed include: literal halves bypass everything, glob halves go
    /// through the pipeline. The two halves union into the final result
    /// and are handled independently from each other.
    #[test]
    fn mixed_include_handles_literals_and_globs_independently() {
        let tags = vec!["latest", "8.5.0", "8.5.0-dev", "10.0.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            include: vec!["latest".into(), "*-dev".into()],
            semver: Some(">=8.0.0, <9.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            result.contains(&"latest".to_string()),
            "literal include bypasses semver"
        );
        assert!(result.contains(&"8.5.0".to_string()));
        assert!(
            result.contains(&"8.5.0-dev".to_string()),
            "glob include rescued and admitted by semver"
        );
        assert!(
            !result.contains(&"10.0.0-dev".to_string()),
            "glob include rejected by semver"
        );
    }

    /// Glob include matching a non-parseable tag is dropped at semver --
    /// globs go through the pipeline, and the pipeline drops anything the
    /// version parser cannot read. Companion test
    /// `literal_include_pins_non_parseable_tag` shows how to keep such a
    /// tag (promote to a literal).
    #[test]
    fn include_glob_drops_non_parseable_tag_at_semver() {
        let tags = vec!["latest-dev", "8.5.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            include: vec!["*-dev".into()],
            semver: Some(">=2.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            result.contains(&"8.5.0-dev".to_string()),
            "parseable in-range tag survives"
        );
        assert!(
            !result.contains(&"latest-dev".to_string()),
            "non-parseable tag drops at semver even when matched by include glob"
        );
    }

    /// Literal include pins a non-parseable tag through `semver:`. The
    /// literal half of include bypasses the entire pipeline, so a tag
    /// like `latest-dev` that the version parser cannot read still lands
    /// in the result. Workaround for the case covered by
    /// `include_glob_drops_non_parseable_tag_at_semver`.
    #[test]
    fn literal_include_pins_non_parseable_tag() {
        let tags = vec!["latest-dev", "8.5.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            include: vec!["latest-dev".into(), "*-dev".into()],
            semver: Some(">=2.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            result.contains(&"latest-dev".to_string()),
            "literal include bypasses semver, pinning the non-parseable tag"
        );
        assert!(
            result.contains(&"8.5.0-dev".to_string()),
            "glob include still admits parseable in-range tags alongside the literal"
        );
    }

    /// Literal include pins a parseable tag that `semver:` would otherwise
    /// reject (out-of-range RC). Matches the recipe at
    /// `recipes/semver-tracking.md` "To pin a specific RC alongside stable
    /// releases" -- proves the literal-bypass contract for tags that DO
    /// parse as semver but fail the range constraint.
    #[test]
    fn literal_include_pins_parseable_rc_against_semver_range() {
        let tags = vec!["1.0.0-rc1", "2.5.0", "3.0.0"];
        let config = FilterConfig {
            include: vec!["1.0.0-rc1".into()],
            semver: Some(">=2.0.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            result.contains(&"1.0.0-rc1".to_string()),
            "literal RC pinned via include bypasses the semver range"
        );
        assert!(result.contains(&"2.5.0".to_string()));
        assert!(result.contains(&"3.0.0".to_string()));
        // Sanity: with a glob include of the same RC pattern, the bypass is
        // gone and the RC drops at semver.
        let glob_config = FilterConfig {
            include: vec!["1.0.0-*".into()],
            semver: Some(">=2.0.0".into()),
            ..FilterConfig::default()
        };
        let glob_result = glob_config.apply(&tags).unwrap();
        assert!(
            !glob_result.contains(&"1.0.0-rc1".to_string()),
            "glob include runs through semver; the RC drops"
        );
    }

    /// Glob include for prerelease patterns + `semver:` keeps in-range
    /// prereleases and drops out-of-range ones. Matches the corrected
    /// prose in `recipes/semver-tracking.md` ("opt back into prereleases
    /// for in-range versions") -- the docs claim that out-of-range
    /// prereleases drop. This test pins that claim.
    #[test]
    fn include_glob_drops_out_of_range_prereleases_through_semver() {
        let tags = vec![
            "1.5.0-rc1", // below range, even though include pattern matches
            "2.0.0-rc1", // in range, in pattern -> kept
            "2.5.0-alpha1",
            "2.5.0",
            "1.5.0",
        ];
        let config = FilterConfig {
            include: vec![
                "*-rc*".into(),
                "*-alpha*".into(),
                "*-beta*".into(),
                "*-pre*".into(),
                "*-snapshot*".into(),
                "*-nightly*".into(),
            ],
            semver: Some(">=2.0".into()),
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert!(
            result.contains(&"2.0.0-rc1".to_string()),
            "in-range RC rescued from built-in soft tier and admitted by semver"
        );
        assert!(
            result.contains(&"2.5.0-alpha1".to_string()),
            "in-range alpha rescued and admitted"
        );
        assert!(result.contains(&"2.5.0".to_string()));
        assert!(
            !result.contains(&"1.5.0-rc1".to_string()),
            "out-of-range RC rescued from built-in soft tier but dropped by semver"
        );
        assert!(
            !result.contains(&"1.5.0".to_string()),
            "out-of-range stable still drops as usual"
        );
    }

    /// Empty `include:` produces no rescue and no literal union. Regression
    /// coverage for the partition logic returning empty halves.
    #[test]
    fn empty_include_behaves_like_no_include() {
        let tags = vec!["1.0.0", "1.0.0-dev"];
        let config = FilterConfig {
            defaults_exclude: vec!["*-dev".into()],
            include: vec![],
            ..FilterConfig::default()
        };
        let result = config.apply(&tags).unwrap();
        assert_eq!(result, vec!["1.0.0".to_string()]);
    }

    // - is_literal_pattern ------------------------------------------------

    #[test]
    fn is_literal_pattern_detects_literals() {
        assert!(is_literal_pattern("latest"));
        assert!(is_literal_pattern("1.25.5"));
        assert!(is_literal_pattern("latest-dev"));
        assert!(is_literal_pattern("v1.2.3-rc1"));
    }

    #[test]
    fn is_literal_pattern_detects_globs() {
        assert!(!is_literal_pattern("*-dev"));
        assert!(!is_literal_pattern("v?.*.*"));
        assert!(!is_literal_pattern("*-r[0-9]*"));
        assert!(!is_literal_pattern("foo[abc]bar"));
    }

    #[test]
    fn is_literal_pattern_empty_is_literal() {
        // Empty string has no glob metacharacters; treat as literal.
        // The pipeline will never see this in practice (parser rejects empty),
        // but pin down the contract for future edits.
        assert!(is_literal_pattern(""));
    }
}
