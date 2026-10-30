//! Shared helpers for the `ocync-sync` integration tests.
//!
//! - `builders.rs`: `ManifestBuilder`, `ArtifactBuilder`, `IndexBuilder` and
//!   `ReferrersIndexBuilder` own fixture data with real digests; their
//!   `*Parts` mount the common case (fresh target, every blob missing).
//! - `mocks.rs`: free functions for tests where the mock topology is the
//!   subject (partial existence, rate limits, `expect(N)` push counts).
//! - `fixtures.rs`: mapping constructors, clients and retry config.
//!   `simple_image_manifest` is for HEAD-check tests over fake digests.
//! - `registry2.rs`: real `registry:2` containers and a corpus to fill them.

// Each test file is its own crate, so `pub` is the visibility for
// cross-module access within a test binary.
#![allow(dead_code, unused_imports, unused_macros, unreachable_pub)]

use std::cell::RefCell;
use std::rc::Rc;

use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{ResolvedMapping, SyncEngine};
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageStatus, SyncReport, shutdown::ShutdownSignal};

pub mod builders;
pub mod fixtures;
pub mod mocks;
pub mod registry2;

pub use builders::{
    ArtifactBuilder, ArtifactParts, IndexBuilder, IndexParts, ManifestBuilder, ManifestParts,
    ReferrersIndexBuilder, SHARED_TAGS, linux, mount_shared_index, platform,
};
pub use fixtures::*;
pub use mocks::*;
pub use registry2::*;

/// Run the engine with every knob explicit; the `run_sync*` wrappers fix most of them.
pub async fn run_engine(
    mappings: Vec<ResolvedMapping>,
    retry: RetryConfig,
    max_concurrent: usize,
    cache: Rc<RefCell<TransferStateCache>>,
    stage: BlobStage,
    shutdown: Option<&ShutdownSignal>,
) -> SyncReport {
    SyncEngine::new(retry, concurrency(max_concurrent))
        .run(mappings, cache, stage, &NullProgress, shutdown)
        .await
}

/// Run with `max_concurrent=50`, no cache, no staging, no shutdown.
pub async fn run_sync(mappings: Vec<ResolvedMapping>) -> SyncReport {
    run_engine(
        mappings,
        fast_retry(),
        50,
        empty_cache(),
        BlobStage::disabled(),
        None,
    )
    .await
}

/// Run with `max_concurrent=1` so tasks execute in order.
pub async fn run_sync_sequential(mappings: Vec<ResolvedMapping>) -> SyncReport {
    run_engine(
        mappings,
        fast_retry(),
        1,
        empty_cache(),
        BlobStage::disabled(),
        None,
    )
    .await
}

/// Run sequentially with `stage`, so the first target stages blobs before the
/// next one reads them.
pub async fn run_sync_sequential_with_staging(
    mappings: Vec<ResolvedMapping>,
    stage: BlobStage,
) -> SyncReport {
    run_engine(mappings, fast_retry(), 1, empty_cache(), stage, None).await
}

/// Run with `max_concurrent`, `stage`, no cache and a live (untriggered)
/// shutdown signal.
pub async fn run_sync_with_concurrency(
    mappings: Vec<ResolvedMapping>,
    max_concurrent: usize,
    stage: BlobStage,
) -> SyncReport {
    run_engine(
        mappings,
        fast_retry(),
        max_concurrent,
        empty_cache(),
        stage,
        Some(&ShutdownSignal::new()),
    )
    .await
}

/// Run with a caller-provided cache.
pub async fn run_sync_with_cache(
    mappings: Vec<ResolvedMapping>,
    cache: Rc<RefCell<TransferStateCache>>,
) -> SyncReport {
    run_engine(
        mappings,
        fast_retry(),
        50,
        cache,
        BlobStage::disabled(),
        None,
    )
    .await
}

/// Run with a caller-provided shutdown signal.
pub async fn run_sync_with_shutdown(
    mappings: Vec<ResolvedMapping>,
    shutdown: &ShutdownSignal,
) -> SyncReport {
    run_engine(
        mappings,
        fast_retry(),
        50,
        empty_cache(),
        BlobStage::disabled(),
        Some(shutdown),
    )
    .await
}

/// Trigger `shutdown` from a real thread after `delay`. A thread, not a
/// tokio timer, so it fires on wall-clock time even under `start_paused`.
pub fn trigger_shutdown_after(shutdown: &ShutdownSignal, delay: std::time::Duration) {
    let signal = shutdown.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        signal.trigger();
    });
}

/// Assert the report holds exactly `want` images, all `Synced`; `name` labels a failure.
pub fn assert_all_synced(report: &SyncReport, want: usize, name: &str) {
    assert_eq!(report.images.len(), want, "{name}: every tag has a result");
    assert_eq!(report.stats.images_synced, want as u64, "{name}");
    for img in &report.images {
        assert!(
            matches!(img.status, ImageStatus::Synced),
            "{name}: image {} should be Synced, got {:?}",
            img.source,
            img.status,
        );
    }
}

/// Assert `report.images[idx].status` matches a pattern.
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

pub(crate) use assert_status;
