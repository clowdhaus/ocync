//! Shared test helpers for `ocync-sync` integration tests.
//!
//! # Architecture
//!
//! - **`builders.rs`** -- Builder types that own fixture data with real digests.
//!   Use `ManifestBuilder`, `ArtifactBuilder`, `IndexBuilder`, `ReferrersIndexBuilder`.
//!   Output types (`*Parts`) have `.mount_source()` / `.mount_target()` for the
//!   80% case (fresh target, all blobs missing).
//!
//! - **`mocks.rs`** -- Free functions for HTTP mock setup. Use directly when
//!   mock topology IS the test subject (partial existence, rate limits, push-count
//!   assertions with `expect(N)`).
//!
//! - **`fixtures.rs`** -- Mapping constructors, client helpers, retry config.
//!   `simple_image_manifest` is for HEAD-check tests using fake digests.
//!
//! # When to use what
//!
//! | Scenario | Use |
//! |----------|-----|
//! | Standard transfer test | `ManifestBuilder` + `mount_source/target` + `run_sync` |
//! | Need push-count precision | Builders for data, manual mocks with `expect(N)` |
//! | Discovery/HEAD-check test | `simple_image_manifest` + `make_digest` |
//! | Custom engine config | `SyncEngine::new(...)` directly |

// Each test file is its own crate, so `pub` is the correct visibility for
// cross-module access within a test binary (not `pub(crate)`).
#![allow(dead_code, unused_imports, unused_macros, unreachable_pub)]

use std::cell::RefCell;
use std::rc::Rc;

use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{ResolvedMapping, SyncEngine};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{SyncReport, shutdown::ShutdownSignal};

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

// ---------------------------------------------------------------------------
// Engine run helpers
// ---------------------------------------------------------------------------

/// Run the sync engine with default settings (`max_concurrent=50`, no cache, no staging, no shutdown).
pub async fn run_sync(mappings: Vec<ResolvedMapping>) -> SyncReport {
    SyncEngine::new(fast_retry(), 50)
        .run(
            mappings,
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await
}

/// Run the sync engine sequentially (`max_concurrent=1`) for deterministic ordering tests.
pub async fn run_sync_sequential(mappings: Vec<ResolvedMapping>) -> SyncReport {
    SyncEngine::new(fast_retry(), 1)
        .run(
            mappings,
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await
}

/// Run the sync engine with a pre-populated cache.
pub async fn run_sync_with_cache(
    mappings: Vec<ResolvedMapping>,
    cache: Rc<RefCell<TransferStateCache>>,
) -> SyncReport {
    SyncEngine::new(fast_retry(), 50)
        .run(mappings, cache, BlobStage::disabled(), &NullProgress, None)
        .await
}

/// Run the sync engine with a shutdown signal.
pub async fn run_sync_with_shutdown(
    mappings: Vec<ResolvedMapping>,
    shutdown: &ShutdownSignal,
) -> SyncReport {
    SyncEngine::new(fast_retry(), 50)
        .run(
            mappings,
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(shutdown),
        )
        .await
}

// ---------------------------------------------------------------------------
// Assertion macros
// ---------------------------------------------------------------------------

/// Assert an image report's status matches a pattern, with diagnostic output on failure.
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

// Make the macro available to all test files that `mod helpers; use helpers::*;`
pub(crate) use assert_status;
