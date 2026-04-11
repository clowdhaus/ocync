//! OCI registry sync orchestration — tag filtering, transfer planning, and execution.

/// Tag filtering pipeline: glob, semver, exclude, sort, and latest.
pub mod filter;
