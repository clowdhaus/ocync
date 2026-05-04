//! Tag-version parser, comparator, and range matcher.
//!
//! Replaces the strict-SemVer parser previously used by the filter pipeline.
//! See `docs/superpowers/specs/2026-05-04-tag-version-parser-design.md`
//! for the design rationale.

use std::cmp::Ordering;

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

    /// Total order over `TagVersion`. Free associated function rather than
    /// `impl Ord` because zero-padding equality (`1.0` and `1.0.0` compare
    /// Equal) is incompatible with `Ord`'s contract that
    /// `cmp(a, b) == Equal` implies `a == b`.
    #[allow(dead_code)]
    pub(crate) fn compare(a: &Self, b: &Self) -> Ordering {
        // Step 1: numeric prefix, zero-padded to the longer length.
        let max_len = a.prefix.len().max(b.prefix.len());
        for i in 0..max_len {
            let ai = a.prefix.get(i).copied().unwrap_or(0);
            let bi = b.prefix.get(i).copied().unwrap_or(0);
            match ai.cmp(&bi) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }

        // Step 2: suffix presence. Empty suffix wins.
        match (a.suffix.is_empty(), b.suffix.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }

        // Step 3: token-by-token comparison.
        for (at, bt) in a.suffix.iter().zip(b.suffix.iter()) {
            let ord = compare_tokens(at, bt);
            if ord != Ordering::Equal {
                return ord;
            }
        }

        // Length-mismatch tie-breaker: shorter token list is less.
        a.suffix.len().cmp(&b.suffix.len())
    }
}

/// Compare two suffix tokens. Numeric < Alpha at the same position (`SemVer` rule).
#[allow(dead_code)]
fn compare_tokens(a: &Token, b: &Token) -> Ordering {
    match (a, b) {
        (Token::Numeric(x), Token::Numeric(y)) => x.cmp(y),
        (Token::Alpha(x), Token::Alpha(y)) => x.cmp(y),
        (Token::Numeric(_), Token::Alpha(_)) => Ordering::Less,
        (Token::Alpha(_), Token::Numeric(_)) => Ordering::Greater,
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

/// A parsed version range: one or more constraints joined by AND.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
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
    pub(crate) reason: String,
}

impl std::fmt::Display for RangeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for RangeParseError {}

#[allow(dead_code)]
impl Range {
    /// Parse a range string into a `Range`.
    ///
    /// Accepts comma-joined constraints of the form `>=1.0, <2.0`.
    /// Supported operators: `>=`, `<=`, `>`, `<`, `=`, and bare (implies `=`).
    /// Caret (`^`), tilde (`~`), and wildcards (`x`, `X`, `*`) are rejected.
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

    /// Returns `true` if `v` satisfies all constraints in the range.
    pub(crate) fn matches(&self, v: &TagVersion) -> bool {
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
        let lhs: Vec<u64> = (0..max_len)
            .map(|i| prefix.get(i).copied().unwrap_or(0))
            .collect();
        let rhs: Vec<u64> = (0..max_len)
            .map(|i| self.bound.get(i).copied().unwrap_or(0))
            .collect();
        match self.op {
            Op::Ge => lhs >= rhs,
            Op::Le => lhs <= rhs,
            Op::Gt => lhs > rhs,
            Op::Lt => lhs < rhs,
            Op::Eq => lhs == rhs,
        }
    }
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

    #[test]
    fn rejects_u64_overflow() {
        // 25 digits exceeds u64::MAX.
        let too_big = "1".repeat(25);
        assert!(TagVersion::parse(&too_big).is_none());
    }

    #[test]
    fn collapses_trailing_dash() {
        let v = TagVersion::parse("1.0-").unwrap();
        assert_eq!(v.prefix, vec![1, 0]);
        assert!(v.suffix.is_empty());
    }

    #[test]
    fn collapses_suffix_with_only_separators() {
        let v = TagVersion::parse("1.0---").unwrap();
        assert!(v.suffix.is_empty());
    }

    #[test]
    fn parses_suffix_alpine() {
        let v = TagVersion::parse("15.10-alpine").unwrap();
        assert_eq!(v.prefix, vec![15, 10]);
        assert_eq!(v.suffix, vec![Token::Alpha("alpine".into())]);
    }

    #[test]
    fn parses_suffix_chainguard_revision() {
        let v = TagVersion::parse("1.25.5-r0").unwrap();
        assert_eq!(v.prefix, vec![1, 25, 5]);
        assert_eq!(v.suffix, vec![Token::Alpha("r".into()), Token::Numeric(0)]);
    }

    #[test]
    fn parses_suffix_temurin_compound() {
        let v = TagVersion::parse("25.0.3_9-jre-alpine-3.23").unwrap();
        assert_eq!(v.prefix, vec![25, 0, 3, 9]);
        assert_eq!(
            v.suffix,
            vec![
                Token::Alpha("jre".into()),
                Token::Alpha("alpine".into()),
                Token::Numeric(3),
                Token::Numeric(23),
            ]
        );
    }

    #[test]
    fn parses_suffix_eks_distro() {
        let v = TagVersion::parse("v1.27.6-eks-1-27-14").unwrap();
        assert_eq!(v.prefix, vec![1, 27, 6]);
        assert_eq!(
            v.suffix,
            vec![
                Token::Alpha("eks".into()),
                Token::Numeric(1),
                Token::Numeric(27),
                Token::Numeric(14),
            ]
        );
    }

    #[test]
    fn parses_suffix_node_alpine_compound() {
        let v = TagVersion::parse("20.18.1-alpine3.20").unwrap();
        assert_eq!(v.prefix, vec![20, 18, 1]);
        assert_eq!(
            v.suffix,
            vec![
                Token::Alpha("alpine".into()),
                Token::Numeric(3),
                Token::Numeric(20),
            ]
        );
    }

    #[test]
    fn tokenization_equivalence_rc1_and_rc_dot_1() {
        // 1.0.0-rc1 and 1.0.0-rc.1 produce identical tokens.
        let a = TagVersion::parse("1.0.0-rc1").unwrap();
        let b = TagVersion::parse("1.0.0-rc.1").unwrap();
        assert_eq!(a.suffix, b.suffix);
        assert_eq!(a.suffix, vec![Token::Alpha("rc".into()), Token::Numeric(1)]);
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
    fn ge_simple() {
        assert!(matches(">=1.0", "1.0.0"));
        assert!(matches(">=1.0", "1.5.0"));
        assert!(!matches(">=1.0", "0.9.0"));
    }

    #[test]
    fn lt_simple() {
        assert!(matches("<2.0", "1.99.99"));
        assert!(!matches("<2.0", "2.0.0"));
        // suffix opaque: 2.0.0-rc1 has prefix [2,0,0]
        assert!(!matches("<2.0", "2.0.0-rc1"));
    }

    #[test]
    fn comma_joined_narrows() {
        assert!(matches(">=1.0, <2.0", "1.5.0"));
        assert!(!matches(">=1.0, <2.0", "2.0.0"));
        assert!(!matches(">=1.0, <2.0", "0.9.0"));
    }

    #[test]
    fn whitespace_tolerated() {
        assert!(matches(" >= 1.0 , < 2.0 ", "1.5.0"));
    }

    #[test]
    fn no_operator_implies_equal() {
        assert!(matches("1.0", "1.0.0"));
        assert!(matches("1.0", "1"));
        assert!(!matches("1.0", "1.0.5"));
    }

    #[test]
    fn equal_explicit() {
        assert!(matches("=15.0", "15"));
        assert!(matches("=15.0", "15.0"));
        assert!(matches("=15.0", "15.0.0"));
        assert!(!matches("=15.0", "15.0.5"));
    }

    #[test]
    fn two_part_bound_matches_three_part_prefix() {
        assert!(matches(">=15.0", "15.10.5"));
    }

    #[test]
    fn rejects_caret() {
        assert!(Range::parse("^2").is_err());
    }

    #[test]
    fn rejects_tilde() {
        assert!(Range::parse("~1").is_err());
    }

    #[test]
    fn rejects_wildcard() {
        assert!(Range::parse("1.x").is_err());
        assert!(Range::parse("1.*").is_err());
        assert!(Range::parse("1.X").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(Range::parse("").is_err());
    }

    #[test]
    fn rejects_prerelease_bound() {
        assert!(Range::parse(">=1.0-rc1").is_err());
        // Bare bound (no operator falls through to Op::Eq); same rejection path.
        assert!(Range::parse("1.0.0-r0").is_err());
    }
}

#[cfg(test)]
mod compare_tests {
    use super::*;

    fn p(s: &str) -> TagVersion {
        TagVersion::parse(s).expect("valid tag")
    }

    fn cmp(a: &str, b: &str) -> Ordering {
        TagVersion::compare(&p(a), &p(b))
    }

    #[test]
    fn three_part_numeric_descending() {
        assert_eq!(cmp("1.0.0", "2.0.0"), Ordering::Less);
        assert_eq!(cmp("2.0.0", "1.0.0"), Ordering::Greater);
        assert_eq!(cmp("1.21.5", "1.21.5"), Ordering::Equal);
    }

    #[test]
    fn zero_pad_one_dot_zero_equals_one_zero_zero() {
        assert_eq!(cmp("1.0", "1.0.0"), Ordering::Equal);
    }

    #[test]
    fn zero_pad_three_part_beats_two_part() {
        // 1.0.5 > 1.0 (which pads to 1.0.0)
        assert_eq!(cmp("1.0.5", "1.0"), Ordering::Greater);
    }

    #[test]
    fn suffix_less_wins() {
        assert_eq!(cmp("1.0.0", "1.0.0-rc1"), Ordering::Greater);
        assert_eq!(cmp("1.0.0-alpha", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn temurin_underscore_build_orders() {
        assert_eq!(cmp("25.0.3_10", "25.0.3_9"), Ordering::Greater);
        // 25.0.3 > 25.0.2_999: prefix [25,0,3,0] > [25,0,2,999] after padding.
        assert_eq!(cmp("25.0.3", "25.0.2_999"), Ordering::Greater);
    }

    #[test]
    fn rc10_above_rc9_via_letter_digit_split() {
        assert_eq!(cmp("1.0.0-rc10", "1.0.0-rc9"), Ordering::Greater);
    }

    #[test]
    fn rc1_and_rc_dot_1_compare_equal() {
        assert_eq!(cmp("1.0.0-rc1", "1.0.0-rc.1"), Ordering::Equal);
    }

    #[test]
    fn rc_dot_2_above_rc_dot_1() {
        assert_eq!(cmp("1.0.0-rc.1", "1.0.0-rc.2"), Ordering::Less);
    }

    #[test]
    fn eks_distro_compound_suffix_orders() {
        assert_eq!(
            cmp("v1.27.6-eks-1-27-14", "v1.27.6-eks-1-27-9"),
            Ordering::Greater
        );
    }

    #[test]
    fn bitnami_release_counter_orders() {
        assert_eq!(
            cmp("1.31.3-debian-12-r3", "1.31.3-debian-12-r2"),
            Ordering::Greater
        );
    }

    #[test]
    fn case_significant_alpha() {
        // ASCII: 'R' (0x52) < 'r' (0x72), so RC1 sorts before rc1.
        assert_eq!(cmp("1.0.0-RC1", "1.0.0-rc1"), Ordering::Less);
    }

    #[test]
    fn numeric_lt_alpha_at_same_position() {
        // SemVer identifier-precedence rule.
        let a = p("1.0.0-1");
        let b = p("1.0.0-alpha");
        assert_eq!(TagVersion::compare(&a, &b), Ordering::Less);
    }

    #[test]
    fn longer_suffix_beats_shorter_when_tokens_equal() {
        // Exercises the length tie-breaker: [rc, 1] vs [rc, 1, "hotfix"].
        // Tokens compare equal at positions 0 and 1; falls through to
        // length comparison.
        assert_eq!(cmp("1.0.0-rc.1", "1.0.0-rc.1.hotfix"), Ordering::Less);
        // Antisymmetry: the reverse comparison must yield Greater.
        assert_eq!(cmp("1.0.0-rc.1.hotfix", "1.0.0-rc.1"), Ordering::Greater);
    }
}
