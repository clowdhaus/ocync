//! Lenient tag-version parser, comparator, and range matcher.
//!
//! Real-world tag shapes (alpine, Chainguard `-rN`, Eclipse Temurin
//! `_<digits>`, EKS Distro compound suffixes) parse by treating the suffix as
//! opaque tokens used for ordering only. Range matching is prefix-only.

use std::cmp::Ordering;

/// A token in a tokenized tag suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    /// An all-digit run.
    Numeric(u64),
    /// Any other run.
    Alpha(&'a str),
}

/// A tag parsed into a numeric prefix and a tokenized suffix.
///
/// Empty `suffix` means the tag had no `-suffix` region (or the suffix
/// tokenized to nothing, e.g. `1.0-` or `1.0---`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TagVersion<'a> {
    /// Numeric components before the first `-`.
    pub(crate) prefix: Vec<u64>,
    /// Tokens after the first `-`.
    pub(crate) suffix: Vec<Token<'a>>,
}

impl<'a> TagVersion<'a> {
    /// Parse a tag into a `TagVersion`, or `None` if it has no parseable
    /// numeric prefix.
    pub(crate) fn parse(tag: &'a str) -> Option<Self> {
        if tag.is_empty() {
            return None;
        }

        let s = tag.strip_prefix('v').unwrap_or(tag);
        if s.is_empty() {
            return None;
        }

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

    /// Total order over `TagVersion`. Not `impl Ord` because zero-padding
    /// equality (`1.0` and `1.0.0` compare Equal) breaks `Ord`'s contract
    /// that `cmp(a, b) == Equal` implies `a == b`.
    pub(crate) fn compare(a: &TagVersion<'_>, b: &TagVersion<'_>) -> Ordering {
        // Numeric prefix, zero-padded.
        let max_len = a.prefix.len().max(b.prefix.len());
        for i in 0..max_len {
            let ai = a.prefix.get(i).copied().unwrap_or(0);
            let bi = b.prefix.get(i).copied().unwrap_or(0);
            match ai.cmp(&bi) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }

        // An empty suffix beats any suffix.
        match (a.suffix.is_empty(), b.suffix.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }

        // Token by token.
        for (at, bt) in a.suffix.iter().zip(b.suffix.iter()) {
            let ord = compare_tokens(at, bt);
            if ord != Ordering::Equal {
                return ord;
            }
        }

        // Shorter token list is less.
        a.suffix.len().cmp(&b.suffix.len())
    }
}

/// Numeric sorts below Alpha at the same position (semver identifier
/// precedence); tokens of the same kind compare by value.
fn compare_tokens(a: &Token<'_>, b: &Token<'_>) -> Ordering {
    match (a, b) {
        (Token::Numeric(x), Token::Numeric(y)) => x.cmp(y),
        (Token::Alpha(x), Token::Alpha(y)) => x.cmp(y),
        (Token::Numeric(_), Token::Alpha(_)) => Ordering::Less,
        (Token::Alpha(_), Token::Numeric(_)) => Ordering::Greater,
    }
}

/// Parse u64 components separated by '.' or '_'. Any non-digit character
/// fails the whole tag.
fn parse_prefix(s: &str) -> Option<Vec<u64>> {
    let mut components = Vec::new();
    for part in s.split(['.', '_']) {
        if part.is_empty() {
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

/// Split on '.', '-', '_', then at letter-digit boundaries. Empty pieces are
/// dropped; all-digit pieces become `Numeric`, the rest `Alpha`.
fn tokenize_suffix(s: &str) -> Vec<Token<'_>> {
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
                tokens.push(Token::Alpha(sub));
            }
        }
    }
    tokens
}

/// Split a string at every transition between digit and non-digit characters.
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

/// A parsed version range: one or more constraints joined by AND.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    constraints: Vec<Constraint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Constraint {
    op: Op,
    bound: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
}

/// Error returned when a version range string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RangeParseError {
    /// Operator-facing explanation of what was rejected.
    pub(crate) reason: String,
}

impl std::fmt::Display for RangeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for RangeParseError {}

impl Range {
    /// Parse comma-joined constraints such as `>=1.0, <2.0`.
    ///
    /// Operators: `>=`, `<=`, `>`, `<`, `=`, and bare (implies `=`).
    /// Caret, tilde, wildcards (`x`, `X`, `*`) and prerelease bounds are
    /// rejected.
    pub(crate) fn parse(s: &str) -> Result<Self, RangeParseError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(RangeParseError {
                reason: "empty range".into(),
            });
        }
        let mut constraints = Vec::new();
        for part in trimmed.split(',') {
            constraints.push(parse_constraint(part.trim())?);
        }
        Ok(Range { constraints })
    }

    /// `true` if every constraint matches `v.prefix`. Both sides are
    /// zero-padded to the longer length, so `>=15.0` matches `15.10.5`.
    pub(crate) fn matches(&self, v: &TagVersion<'_>) -> bool {
        self.constraints.iter().all(|c| c.matches(&v.prefix))
    }
}

fn parse_constraint(s: &str) -> Result<Constraint, RangeParseError> {
    if s.is_empty() {
        return Err(RangeParseError {
            reason: "empty constraint".into(),
        });
    }
    if s.starts_with('^')
        || s.starts_with('~')
        || s.contains('x')
        || s.contains('X')
        || s.contains('*')
    {
        return Err(RangeParseError {
            reason: format!(
                "unsupported range syntax '{}'; use a comma-joined combination of >=, <=, >, <, =",
                s
            ),
        });
    }

    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (Op::Ge, r.trim_start())
    } else if let Some(r) = s.strip_prefix("<=") {
        (Op::Le, r.trim_start())
    } else if let Some(r) = s.strip_prefix('>') {
        (Op::Gt, r.trim_start())
    } else if let Some(r) = s.strip_prefix('<') {
        (Op::Lt, r.trim_start())
    } else if let Some(r) = s.strip_prefix('=') {
        (Op::Eq, r.trim_start())
    } else {
        (Op::Eq, s)
    };

    let bound_str = rest.strip_prefix('v').unwrap_or(rest);
    let bound = parse_bound(bound_str)?;
    Ok(Constraint { op, bound })
}

fn parse_bound(s: &str) -> Result<Vec<u64>, RangeParseError> {
    if s.contains('-') {
        return Err(RangeParseError {
            reason: format!("prerelease suffix not allowed in range bound: '{}'", s),
        });
    }
    let mut components = Vec::new();
    for part in s.split('.') {
        if part.is_empty() {
            return Err(RangeParseError {
                reason: format!("malformed bound '{}'", s),
            });
        }
        let n: u64 = part.parse().map_err(|_| RangeParseError {
            reason: format!("malformed bound '{}'", s),
        })?;
        components.push(n);
    }
    if components.is_empty() {
        Err(RangeParseError {
            reason: format!("empty bound '{}'", s),
        })
    } else {
        Ok(components)
    }
}

impl Constraint {
    fn matches(&self, prefix: &[u64]) -> bool {
        let max_len = prefix.len().max(self.bound.len());
        let cmp = (0..max_len)
            .map(|i| {
                let l = prefix.get(i).copied().unwrap_or(0);
                let r = self.bound.get(i).copied().unwrap_or(0);
                l.cmp(&r)
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal);
        match self.op {
            Op::Ge => cmp != Ordering::Less,
            Op::Le => cmp != Ordering::Greater,
            Op::Gt => cmp == Ordering::Greater,
            Op::Lt => cmp == Ordering::Less,
            Op::Eq => cmp == Ordering::Equal,
        }
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_prefix_and_suffix() {
        use Token::{Alpha, Numeric};
        // (name, tag, prefix, suffix)
        let cases: &[(&str, &str, &[u64], &[Token<'_>])] = &[
            ("three-part numeric", "1.21.5", &[1, 21, 5], &[]),
            ("two-part numeric", "15.10", &[15, 10], &[]),
            ("one-part numeric", "21", &[21], &[]),
            ("v prefix stripped", "v1.27.6", &[1, 27, 6], &[]),
            ("v0", "v0", &[0], &[]),
            (
                "underscore is a prefix separator",
                "21.0.5_11",
                &[21, 0, 5, 11],
                &[],
            ),
            ("date as one integer", "20240115", &[20240115], &[]),
            ("trailing dash collapses", "1.0-", &[1, 0], &[]),
            ("separator-only suffix collapses", "1.0---", &[1, 0], &[]),
            ("alpine", "15.10-alpine", &[15, 10], &[Alpha("alpine")]),
            (
                "chainguard revision",
                "1.25.5-r0",
                &[1, 25, 5],
                &[Alpha("r"), Numeric(0)],
            ),
            (
                "temurin compound",
                "25.0.3_9-jre-alpine-3.23",
                &[25, 0, 3, 9],
                &[Alpha("jre"), Alpha("alpine"), Numeric(3), Numeric(23)],
            ),
            (
                "eks distro",
                "v1.27.6-eks-1-27-14",
                &[1, 27, 6],
                &[Alpha("eks"), Numeric(1), Numeric(27), Numeric(14)],
            ),
            (
                "node alpine compound",
                "20.18.1-alpine3.20",
                &[20, 18, 1],
                &[Alpha("alpine"), Numeric(3), Numeric(20)],
            ),
            (
                "dotnet compound variant",
                "8.0-bookworm-slim",
                &[8, 0],
                &[Alpha("bookworm"), Alpha("slim")],
            ),
            (
                "hashicorp alpha date stamp",
                "1.10.0-alpha20241016",
                &[1, 10, 0],
                &[Alpha("alpha"), Numeric(20241016)],
            ),
            (
                "arm variant",
                "2.21.0-arm64v8",
                &[2, 21, 0],
                &[Alpha("arm"), Numeric(64), Alpha("v"), Numeric(8)],
            ),
        ];
        for (name, tag, prefix, suffix) in cases {
            let v = TagVersion::parse(tag).unwrap_or_else(|| panic!("{name}: {tag} must parse"));
            assert_eq!(v.prefix, *prefix, "{name}: prefix");
            assert_eq!(v.suffix, *suffix, "{name}: suffix");
        }
    }

    #[test]
    fn rejects_unparseable_tags() {
        let cases = [
            ("empty", ""),
            ("bare v", "v"),
            ("non-numeric prefix", "latest"),
            ("non-numeric prefix with dash", "lts-iron"),
            // A letter before any '-' (postgres `17rc1`) drops the tag.
            ("letter in prefix region", "17rc1"),
            ("letter in two-part prefix region", "1.24rc2"),
            ("u64 overflow", "1111111111111111111111111"),
        ];
        for (name, tag) in cases {
            assert!(TagVersion::parse(tag).is_none(), "{name}: {tag}");
        }
    }

    #[test]
    fn rc1_and_rc_dot_1_tokenize_identically() {
        let a = TagVersion::parse("1.0.0-rc1").unwrap();
        let b = TagVersion::parse("1.0.0-rc.1").unwrap();
        assert_eq!(a.suffix, b.suffix);
        assert_eq!(a.suffix, vec![Token::Alpha("rc"), Token::Numeric(1)]);
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    fn matches(range: &str, tag: &str) -> bool {
        let r = Range::parse(range).expect("valid range");
        let v = TagVersion::parse(tag).expect("valid tag");
        r.matches(&v)
    }

    #[test]
    fn range_matches_prefix() {
        // (name, range, tag, want)
        let cases = [
            ("ge at bound", ">=1.0", "1.0.0", true),
            ("ge above bound", ">=1.0", "1.5.0", true),
            ("ge below bound", ">=1.0", "0.9.0", false),
            ("lt below bound", "<2.0", "1.99.99", true),
            ("lt at bound", "<2.0", "2.0.0", false),
            // The suffix is opaque: 2.0.0-rc1 has prefix [2,0,0].
            ("lt at bound with suffix", "<2.0", "2.0.0-rc1", false),
            ("comma-joined inside", ">=1.0, <2.0", "1.5.0", true),
            ("comma-joined above", ">=1.0, <2.0", "2.0.0", false),
            ("comma-joined below", ">=1.0, <2.0", "0.9.0", false),
            ("whitespace tolerated", " >= 1.0 , < 2.0 ", "1.5.0", true),
            ("bare equals zero-padded", "1.0", "1.0.0", true),
            ("bare equals shorter tag", "1.0", "1", true),
            ("bare equals rejects higher", "1.0", "1.0.5", false),
            ("explicit eq one part", "=15.0", "15", true),
            ("explicit eq two part", "=15.0", "15.0", true),
            ("explicit eq three part", "=15.0", "15.0.0", true),
            ("explicit eq rejects higher", "=15.0", "15.0.5", false),
            (
                "two-part bound, three-part prefix",
                ">=15.0",
                "15.10.5",
                true,
            ),
        ];
        for (name, range, tag, want) in cases {
            assert_eq!(matches(range, tag), want, "{name}: {range} vs {tag}");
        }
    }

    #[test]
    fn rejects_unsupported_ranges() {
        let cases = [
            ("caret", "^2"),
            ("tilde", "~1"),
            ("wildcard x", "1.x"),
            ("wildcard star", "1.*"),
            ("wildcard X", "1.X"),
            ("empty", ""),
            ("prerelease bound", ">=1.0-rc1"),
            // No operator falls through to Op::Eq; same rejection path.
            ("bare prerelease bound", "1.0.0-r0"),
        ];
        for (name, range) in cases {
            assert!(Range::parse(range).is_err(), "{name}: {range}");
        }
    }
}

#[cfg(test)]
mod compare_tests {
    use super::*;
    use Ordering::{Equal, Greater, Less};

    fn compare(a: &str, b: &str) -> Ordering {
        let p = |s| TagVersion::parse(s).expect("valid tag");
        TagVersion::compare(&p(a), &p(b))
    }

    #[test]
    fn compare_orders() {
        // (name, a, b, want)
        let cases = [
            ("numeric less", "1.0.0", "2.0.0", Less),
            ("numeric greater", "2.0.0", "1.0.0", Greater),
            ("numeric equal", "1.21.5", "1.21.5", Equal),
            ("zero-pad equal", "1.0", "1.0.0", Equal),
            // 1.0.5 > 1.0, which pads to 1.0.0.
            (
                "zero-pad three-part beats two-part",
                "1.0.5",
                "1.0",
                Greater,
            ),
            ("no suffix beats suffix", "1.0.0", "1.0.0-rc1", Greater),
            ("suffix loses to none", "1.0.0-alpha", "1.0.0", Less),
            ("temurin build 10 above 9", "25.0.3_10", "25.0.3_9", Greater),
            // [25,0,3,0] > [25,0,2,999] after padding.
            (
                "temurin higher patch beats build",
                "25.0.3",
                "25.0.2_999",
                Greater,
            ),
            (
                "rc10 above rc9 via letter-digit split",
                "1.0.0-rc10",
                "1.0.0-rc9",
                Greater,
            ),
            ("rc.2 above rc.1", "1.0.0-rc.1", "1.0.0-rc.2", Less),
            (
                "eks distro compound suffix",
                "v1.27.6-eks-1-27-14",
                "v1.27.6-eks-1-27-9",
                Greater,
            ),
            (
                "bitnami release counter",
                "1.31.3-debian-12-r3",
                "1.31.3-debian-12-r2",
                Greater,
            ),
            // ASCII: 'R' (0x52) < 'r' (0x72); parse and compare preserve case.
            ("case-significant alpha", "1.0.0-RC1", "1.0.0-rc1", Less),
            (
                "uppercase SNAPSHOT below lowercase",
                "5.0.0-SNAPSHOT",
                "5.0.0-snapshot",
                Less,
            ),
            // SemVer identifier precedence.
            (
                "numeric below alpha at same position",
                "1.0.0-1",
                "1.0.0-alpha",
                Less,
            ),
            // Tokens tie at [rc, 1]; the length tie-breaker decides.
            (
                "longer suffix wins on tied tokens",
                "1.0.0-rc.1",
                "1.0.0-rc.1.hotfix",
                Less,
            ),
            (
                "longer suffix wins, reversed",
                "1.0.0-rc.1.hotfix",
                "1.0.0-rc.1",
                Greater,
            ),
            // The single integer 20240115 exceeds [2024,1,15] under zero-pad.
            (
                "date integer above date components",
                "20240115",
                "2024.01.15",
                Greater,
            ),
        ];
        for (name, a, b, want) in cases {
            assert_eq!(compare(a, b), want, "{name}: {a} vs {b}");
        }
    }
}
