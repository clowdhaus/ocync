//! Tag filtering pipeline: glob -> semver -> exclude -> sort + latest.

use globset::{Glob, GlobSetBuilder};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How to handle semver pre-release tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemverPrerelease {
    /// Include pre-release tags in results.
    Include,
    /// Exclude pre-release tags from results.
    Exclude,
    /// Return only pre-release tags.
    Only,
}

/// Sort order for the final tag list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Sort by semantic version (descending).
    Semver,
    /// Sort alphabetically (ascending).
    Alpha,
}

/// Configuration for the tag filter pipeline.
#[derive(Debug, Default)]
pub struct FilterConfig {
    /// Glob patterns (OR semantics).
    pub glob: Vec<String>,
    /// Semver version range constraint (e.g. `>=1.18.0`).
    pub semver: Option<String>,
    /// Pre-release handling.
    pub semver_prerelease: Option<SemverPrerelease>,
    /// Exclude patterns (OR deny).
    pub exclude: Vec<String>,
    /// Sort order.
    pub sort: Option<SortOrder>,
    /// Keep only the first N after sorting.
    pub latest: Option<usize>,
}

// ---------------------------------------------------------------------------
// Pipeline entry point
// ---------------------------------------------------------------------------

/// Run the full filter pipeline and return matching tags.
pub fn run_filter_pipeline(tags: &[&str], config: &FilterConfig) -> Vec<String> {
    // 1. Glob include
    let mut current: Vec<&str> = if config.glob.is_empty() {
        tags.to_vec()
    } else {
        filter_glob(tags, &config.glob)
    };

    // 2. Semver filter
    if let Some(ref range) = config.semver {
        current = filter_semver(
            &current,
            range,
            config
                .semver_prerelease
                .unwrap_or(SemverPrerelease::Exclude),
        );
    }

    // 3. Exclude
    if !config.exclude.is_empty() {
        current = filter_exclude(&current, &config.exclude);
    }

    // 4. Sort
    if let Some(order) = config.sort {
        current = sort_tags(&current, order);
    }

    // 5. Latest
    if let Some(n) = config.latest {
        current = apply_latest(&current, n);
    }

    current.into_iter().map(String::from).collect()
}

// ---------------------------------------------------------------------------
// Individual stages
// ---------------------------------------------------------------------------

/// Filter tags by glob patterns (OR: any pattern match keeps the tag).
pub fn filter_glob<'a>(tags: &[&'a str], patterns: &[String]) -> Vec<&'a str> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            builder.add(g);
        }
    }
    let set = match builder.build() {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    tags.iter().copied().filter(|t| set.is_match(t)).collect()
}

/// Filter tags by a semver version range, with pre-release handling.
pub fn filter_semver<'a>(
    tags: &[&'a str],
    range: &str,
    prerelease: SemverPrerelease,
) -> Vec<&'a str> {
    let req = match semver::VersionReq::parse(range) {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    tags.iter()
        .copied()
        .filter(|tag| {
            let Some(ver) = parse_semver(tag) else {
                return false;
            };
            let has_pre = !ver.pre.is_empty();
            match prerelease {
                SemverPrerelease::Exclude if has_pre => return false,
                SemverPrerelease::Only if !has_pre => return false,
                _ => {}
            }
            // semver crate by default doesn't match pre-releases against
            // ranges without pre-release specifiers, so we compare the
            // non-pre-release portion when the user wants to include them.
            if has_pre && prerelease != SemverPrerelease::Exclude {
                let base = semver::Version::new(ver.major, ver.minor, ver.patch);
                req.matches(&base)
            } else {
                req.matches(&ver)
            }
        })
        .collect()
}

/// Exclude tags matching any of the given glob patterns.
pub fn filter_exclude<'a>(tags: &[&'a str], patterns: &[String]) -> Vec<&'a str> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            builder.add(g);
        }
    }
    let set = match builder.build() {
        Ok(s) => s,
        Err(_) => return tags.to_vec(),
    };
    tags.iter().copied().filter(|t| !set.is_match(t)).collect()
}

/// Sort tags in descending order (highest first).
pub fn sort_tags<'a>(tags: &[&'a str], order: SortOrder) -> Vec<&'a str> {
    let mut sorted: Vec<&str> = tags.to_vec();
    match order {
        SortOrder::Semver => {
            sorted.sort_by(|a, b| {
                let va = parse_semver(a);
                let vb = parse_semver(b);
                match (va, vb) {
                    (Some(va), Some(vb)) => vb.cmp(&va), // descending
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => b.cmp(a),
                }
            });
        }
        SortOrder::Alpha => {
            sorted.sort_by(|a, b| b.cmp(a)); // descending
        }
    }
    sorted
}

/// Keep only the first `n` tags.
pub fn apply_latest<'a>(tags: &[&'a str], n: usize) -> Vec<&'a str> {
    tags.iter().copied().take(n).collect()
}

// ---------------------------------------------------------------------------
// Semver parse helper
// ---------------------------------------------------------------------------

/// Parse a tag as semver, stripping an optional `v` prefix and normalising
/// two-part versions (`X.Y` -> `X.Y.0`).
fn parse_semver(tag: &str) -> Option<semver::Version> {
    let s = tag.strip_prefix('v').unwrap_or(tag);
    if let Ok(v) = semver::Version::parse(s) {
        return Some(v);
    }
    // Try X.Y -> X.Y.0
    let parts: Vec<&str> = s.splitn(3, '.').collect();
    if parts.len() == 2 {
        let normalised = format!("{}.{}.0", parts[0], parts[1]);
        return semver::Version::parse(&normalised).ok();
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- glob tests --

    #[test]
    fn test_glob_star() {
        let tags = vec!["1.0", "1.1", "2.0", "latest"];
        let result = filter_glob(&tags, &["*".to_string()]);
        assert_eq!(result, vec!["1.0", "1.1", "2.0", "latest"]);
    }

    #[test]
    fn test_glob_prefix() {
        let tags = vec!["v1.0", "v1.1", "v2.0", "latest"];
        let result = filter_glob(&tags, &["v1.*".to_string()]);
        assert_eq!(result, vec!["v1.0", "v1.1"]);
    }

    #[test]
    fn test_glob_or_semantics() {
        let tags = vec!["v1.0", "v2.0", "v3.0", "nightly"];
        let result = filter_glob(&tags, &["v1.*".to_string(), "v2.*".to_string()]);
        assert_eq!(result, vec!["v1.0", "v2.0"]);
    }

    // -- semver tests --

    #[test]
    fn test_semver_filter() {
        let tags = vec!["1.18.0", "1.19.0", "1.20.0", "1.17.0"];
        let result = filter_semver(&tags, ">=1.18.0", SemverPrerelease::Exclude);
        assert_eq!(result, vec!["1.18.0", "1.19.0", "1.20.0"]);
    }

    #[test]
    fn test_semver_v_prefix() {
        let tags = vec!["v1.0.0", "v2.0.0", "v3.0.0"];
        let result = filter_semver(&tags, ">=2.0.0", SemverPrerelease::Exclude);
        assert_eq!(result, vec!["v2.0.0", "v3.0.0"]);
    }

    #[test]
    fn test_semver_two_part() {
        let tags = vec!["1.18", "1.19", "1.20"];
        let result = filter_semver(&tags, ">=1.19.0", SemverPrerelease::Exclude);
        assert_eq!(result, vec!["1.19", "1.20"]);
    }

    // -- prerelease tests --

    #[test]
    fn test_prerelease_include() {
        let tags = vec!["1.0.0", "1.1.0-rc1", "1.2.0"];
        let result = filter_semver(&tags, ">=1.0.0", SemverPrerelease::Include);
        assert_eq!(result, vec!["1.0.0", "1.1.0-rc1", "1.2.0"]);
    }

    #[test]
    fn test_prerelease_exclude() {
        let tags = vec!["1.0.0", "1.1.0-rc1", "1.2.0"];
        let result = filter_semver(&tags, ">=1.0.0", SemverPrerelease::Exclude);
        assert_eq!(result, vec!["1.0.0", "1.2.0"]);
    }

    #[test]
    fn test_prerelease_only() {
        let tags = vec!["1.0.0", "1.1.0-rc1", "1.2.0-beta2"];
        let result = filter_semver(&tags, ">=1.0.0", SemverPrerelease::Only);
        assert_eq!(result, vec!["1.1.0-rc1", "1.2.0-beta2"]);
    }

    // -- exclude test --

    #[test]
    fn test_exclude_filter() {
        let tags = vec!["1.0-alpine", "1.0-slim", "1.0", "1.1-alpine"];
        let result = filter_exclude(&tags, &["*-alpine".to_string()]);
        assert_eq!(result, vec!["1.0-slim", "1.0"]);
    }

    // -- sort tests --

    #[test]
    fn test_sort_semver() {
        let tags = vec!["1.0.0", "1.2.0", "1.1.0", "2.0.0"];
        let result = sort_tags(&tags, SortOrder::Semver);
        assert_eq!(result, vec!["2.0.0", "1.2.0", "1.1.0", "1.0.0"]);
    }

    #[test]
    fn test_sort_alpha() {
        let tags = vec!["banana", "apple", "cherry"];
        let result = sort_tags(&tags, SortOrder::Alpha);
        assert_eq!(result, vec!["cherry", "banana", "apple"]);
    }

    // -- latest test --

    #[test]
    fn test_latest_cap() {
        let tags = vec!["3.0.0", "2.0.0", "1.0.0"];
        let result = apply_latest(&tags, 2);
        assert_eq!(result, vec!["3.0.0", "2.0.0"]);
    }

    // -- pipeline tests --

    #[test]
    fn test_pipeline_empty_config() {
        let tags = vec!["1.0.0", "latest", "nightly"];
        let config = FilterConfig::default();
        let result = run_filter_pipeline(&tags, &config);
        assert_eq!(result, vec!["1.0.0", "latest", "nightly"]);
    }

    #[test]
    fn test_full_pipeline() {
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
            glob: vec!["1.*".to_string()],
            semver: Some(">=1.18.0".to_string()),
            semver_prerelease: Some(SemverPrerelease::Exclude),
            exclude: vec![],
            sort: Some(SortOrder::Semver),
            latest: Some(2),
        };
        let result = run_filter_pipeline(&tags, &config);
        assert_eq!(result, vec!["1.20.0", "1.19.0"]);
    }
}
