//! Shared test helpers for `ocync-sync` integration tests.

// During migration, not all helpers are consumed by every test binary.
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
