//! Tag-version parser, comparator, and range matcher.
//!
//! Replaces the strict-SemVer parser previously used by the filter pipeline.
//! See `docs/superpowers/specs/2026-05-04-tag-version-parser-design.md`
//! for the design rationale.

use std::cmp::Ordering;

/// A token in a tokenized tag suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Numeric(u64),
    Alpha(String),
}
