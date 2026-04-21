//! Shared test helpers for `ocync-sync` integration tests.

// During migration, not all helpers are consumed by every test binary.
// Each test file is its own crate, so `pub` is the correct visibility for
// cross-module access within a test binary (not `pub(crate)`).
#![allow(dead_code, unused_imports, unreachable_pub)]

pub mod builders;
pub mod fixtures;
pub mod mocks;

// Re-export commonly used items for ergonomic `use helpers::*` imports.
pub use builders::{ManifestBuilder, ManifestParts};
pub use fixtures::*;
pub use mocks::*;
