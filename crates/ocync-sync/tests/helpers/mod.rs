//! Shared test helpers for `ocync-sync` integration tests.

// During migration, not all helpers are consumed by every test binary.
// Each test file is its own crate, so `pub` is the correct visibility for
// cross-module access within a test binary (not `pub(crate)`).
#![allow(dead_code, unused_imports, unreachable_pub)]

pub mod builders;
pub mod fixtures;
pub mod mocks;

// Re-export commonly used items for ergonomic `use helpers::*` imports.
pub use builders::{
    ArtifactBuilder, ArtifactParts, IndexBuilder, IndexParts, ManifestBuilder, ManifestParts,
    ReferrersIndexBuilder,
};
pub use fixtures::*;
pub use mocks::*;

/// Assert an image report's status matches a pattern, with diagnostic output on failure.
#[macro_export]
macro_rules! assert_status {
    ($report:expr, $idx:expr, $pattern:pat) => {
        assert!(
            matches!($report.images[$idx].status, $pattern),
            "image[{}]: expected {}, got {:?}",
            $idx,
            stringify!($pattern),
            $report.images[$idx].status,
        );
    };
}
