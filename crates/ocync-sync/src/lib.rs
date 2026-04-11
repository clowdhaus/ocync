//! OCI registry sync orchestration — tag filtering, transfer planning, and execution.

/// Error types for sync operations.
pub mod error;

/// Tag filtering pipeline: glob, semver, exclude, sort, and latest.
pub mod filter;

pub use error::Error;
