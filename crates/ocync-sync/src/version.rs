//! Tag-version parser, comparator, and range matcher.
//!
//! Replaces the strict-SemVer parser previously used by the filter pipeline.
//! See `docs/superpowers/specs/2026-05-04-tag-version-parser-design.md`
//! for the design rationale.

/// A token in a tokenized tag suffix.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Numeric(u64),
    Alpha(String),
}

/// A tag parsed into a numeric prefix and a tokenized suffix.
///
/// Empty `suffix` means the tag had no `-suffix` region (or the suffix
/// tokenized to nothing, e.g. `1.0-` or `1.0---`).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TagVersion {
    pub(crate) prefix: Vec<u64>,
    pub(crate) suffix: Vec<Token>,
}

impl TagVersion {
    /// Parse a tag string into a `TagVersion`.
    /// Returns `None` if the tag has no parseable numeric prefix.
    #[allow(dead_code)]
    pub(crate) fn parse(tag: &str) -> Option<Self> {
        if tag.is_empty() {
            return None;
        }

        // Optional leading 'v' is stripped before prefix parsing.
        let s = tag.strip_prefix('v').unwrap_or(tag);
        if s.is_empty() {
            return None;
        }

        // Prefix region: everything before the first '-'.
        let (prefix_str, suffix_str) = match s.find('-') {
            Some(i) => (&s[..i], Some(&s[i + 1..])),
            None => (s, None),
        };

        if prefix_str.is_empty() {
            return None;
        }

        let prefix = parse_prefix(prefix_str)?;

        let suffix = match suffix_str {
            Some(s) if !s.is_empty() => tokenize_suffix(s),
            _ => Vec::new(),
        };

        Some(TagVersion { prefix, suffix })
    }
}

/// Parse the prefix region: u64 components separated by '.' or '_'.
/// Any non-digit character causes the whole tag to fail to parse.
#[allow(dead_code)]
fn parse_prefix(s: &str) -> Option<Vec<u64>> {
    let mut components = Vec::new();
    for part in s.split(['.', '_']) {
        if part.is_empty() {
            return None;
        }
        // Reject if any non-digit character is present.
        if !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let n: u64 = part.parse().ok()?;
        components.push(n);
    }
    if components.is_empty() {
        None
    } else {
        Some(components)
    }
}

/// Tokenize the suffix region. Two passes:
///   1. Split on '.', '-', '_'.
///   2. For each token, split at letter-digit boundaries.
///
/// Empty strings are dropped. Each surviving string becomes Numeric(u64) if
/// all-digit, otherwise Alpha(String).
#[allow(dead_code)]
fn tokenize_suffix(s: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for part in s.split(['.', '-', '_']) {
        if part.is_empty() {
            continue;
        }
        for sub in split_letter_digit(part) {
            if sub.is_empty() {
                continue;
            }
            if let Ok(n) = sub.parse::<u64>() {
                tokens.push(Token::Numeric(n));
            } else {
                tokens.push(Token::Alpha(sub.to_string()));
            }
        }
    }
    tokens
}

/// Split a string at every transition between digit and non-digit characters.
#[allow(dead_code)]
fn split_letter_digit(s: &str) -> Vec<&str> {
    let mut splits = Vec::new();
    let mut start = 0;
    let mut prev_is_digit: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let is_digit = c.is_ascii_digit();
        if let Some(prev) = prev_is_digit {
            if prev != is_digit {
                splits.push(&s[start..i]);
                start = i;
            }
        }
        prev_is_digit = Some(is_digit);
    }
    splits.push(&s[start..]);
    splits
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_three_part_numeric() {
        let v = TagVersion::parse("1.21.5").unwrap();
        assert_eq!(v.prefix, vec![1, 21, 5]);
        assert!(v.suffix.is_empty());
    }

    #[test]
    fn parses_two_part_numeric() {
        let v = TagVersion::parse("15.10").unwrap();
        assert_eq!(v.prefix, vec![15, 10]);
        assert!(v.suffix.is_empty());
    }

    #[test]
    fn parses_one_part_numeric() {
        let v = TagVersion::parse("21").unwrap();
        assert_eq!(v.prefix, vec![21]);
        assert!(v.suffix.is_empty());
    }

    #[test]
    fn strips_v_prefix() {
        let v = TagVersion::parse("v1.27.6").unwrap();
        assert_eq!(v.prefix, vec![1, 27, 6]);
    }

    #[test]
    fn parses_v0() {
        let v = TagVersion::parse("v0").unwrap();
        assert_eq!(v.prefix, vec![0]);
    }

    #[test]
    fn parses_underscore_separator() {
        let v = TagVersion::parse("21.0.5_11").unwrap();
        assert_eq!(v.prefix, vec![21, 0, 5, 11]);
        assert!(v.suffix.is_empty());
    }

    #[test]
    fn rejects_empty() {
        assert!(TagVersion::parse("").is_none());
    }

    #[test]
    fn rejects_bare_v() {
        assert!(TagVersion::parse("v").is_none());
    }

    #[test]
    fn rejects_non_numeric_prefix() {
        assert!(TagVersion::parse("latest").is_none());
        assert!(TagVersion::parse("lts-iron").is_none());
    }

    #[test]
    fn rejects_letter_in_prefix_region() {
        // postgres `17rc1` has 'r' in the prefix region (before any '-') -> drop.
        assert!(TagVersion::parse("17rc1").is_none());
        assert!(TagVersion::parse("1.24rc2").is_none());
    }
}
