//! Tag filtering pipeline: glob -> semver -> exclude -> sort + latest.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::Error;

/// How to handle semver pre-release tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SemverPrerelease {
    /// Include pre-release tags in results.
    Include,
    /// Exclude pre-release tags from results.
    Exclude,
    /// Return only pre-release tags.
    Only,
}

/// Sort order for the final tag list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Sort by semantic version (highest first).
    Semver,
    /// Sort alphabetically (highest first).
    Alpha,
}

/// Configuration for the tag filter pipeline.
///
/// All stages are AND (narrowing). Each stage reduces the set:
/// `glob → semver → exclude → sort → latest → min_tags`.
#[derive(Debug, Default)]
pub struct FilterConfig {
    /// Glob patterns (OR semantics). An empty list passes all tags through.
    pub glob: Vec<String>,
    /// Semver version range constraint (e.g. `>=1.18.0`).
    pub semver: Option<String>,
    /// Pre-release handling. Defaults to [`SemverPrerelease::Exclude`] when
    /// a `semver` range is set.
    pub semver_prerelease: Option<SemverPrerelease>,
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
    /// requires.
    pub fn apply(&self, tags: &[&str]) -> Result<Vec<String>, Error> {
        // 0. Config validation
        if self.latest.is_some() && self.sort.is_none() {
            return Err(Error::LatestWithoutSort);
        }

        // 1. Glob include
        let mut current: Vec<&str> = if self.glob.is_empty() {
            tags.to_vec()
        } else {
            filter_glob(tags, &self.glob)?
        };

        // 2. Semver filter
        if let Some(ref range) = self.semver {
            current = filter_semver(&current, range)?;
        }

        // 3. Exclude
        if !self.exclude.is_empty() {
            current = filter_exclude(&current, &self.exclude)?;
        }

        // 4. Sort
        if let Some(order) = self.sort {
            sort_tags_in_place(&mut current, order);
        }

        // 5. Latest
        if let Some(n) = self.latest {
            current.truncate(n);
        }

        // 6. Min tags validation
        if let Some(min) = self.min_tags {
            if current.len() < min {
                return Err(Error::BelowMinTags {
                    matched: current.len(),
                    minimum: min,
                });
            }
        }

        Ok(current.into_iter().map(String::from).collect())
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

/// Exclude tags matching any of the given glob patterns.
fn filter_exclude<'a>(tags: &[&'a str], patterns: &[String]) -> Result<Vec<&'a str>, Error> {
    let set = build_glob_set(patterns)?;
    Ok(tags.iter().copied().filter(|t| !set.is_match(t)).collect())
}

/// Sort tags in-place in descending order (highest first).
fn sort_tags_in_place(tags: &mut [&str], order: SortOrder) {
    use crate::version::TagVersion;
    use std::cmp::Ordering;

    match order {
        SortOrder::Semver => {
            tags.sort_by(|a, b| {
                let va = TagVersion::parse(a);
                let vb = TagVersion::parse(b);
                match (va, vb) {
                    (Some(va), Some(vb)) => TagVersion::compare(&vb, &va), // descending
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => b.cmp(a), // alpha-descending fallback
                }
            });
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
        let result = filter_exclude(&tags, &["*-alpine".into()]).unwrap();
        assert_eq!(result, vec!["1.0-slim", "1.0"]);
    }

    #[test]
    fn exclude_multiple_patterns() {
        let tags = vec!["1.0-alpine", "1.0-slim", "1.0", "nightly"];
        let result = filter_exclude(&tags, &["*-alpine".into(), "nightly".into()]).unwrap();
        assert_eq!(result, vec!["1.0-slim", "1.0"]);
    }

    #[test]
    fn exclude_invalid_pattern_returns_error() {
        let tags = vec!["1.0"];
        let err = filter_exclude(&tags, &["[bad".into()]).unwrap_err();
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

    /// Pre-release tags sort below their base version: `1.0.0-rc1 < 1.0.0`.
    #[test]
    fn sort_semver_prerelease_ordering() {
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
}
