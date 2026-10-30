//! Shared helpers for the `ocync-sync` integration tests.
//!
//! - `builders.rs`: `ManifestBuilder`, `ArtifactBuilder`, `IndexBuilder` and
//!   `ReferrersIndexBuilder` own fixture data with real digests; their
//!   `*Parts` mount the common case (fresh target, every blob missing).
//! - `mocks.rs`: free functions for tests where the mock topology is the
//!   subject (partial existence, rate limits, `expect(N)` push counts), and
//!   the mock batch checkers.
//! - `fixtures.rs`: mapping constructors, clients, retry config and seeded
//!   caches. `simple_image_manifest` is for HEAD-check tests over fake digests.
//! - `digests.rs`: digest and repository fixtures with no wiremock dependency;
//!   filesystem-only test binaries include it alone through `#[path]`.
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
pub mod digests;
pub mod fixtures;
pub mod mocks;
pub mod registry2;

pub use builders::{
    ArtifactBuilder, ArtifactParts, IndexBuilder, IndexParts, ManifestBuilder, ManifestParts,
    ReferrersIndexBuilder, SHARED_TAGS, linux, mount_shared_index, platform,
};
pub use digests::*;
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

/// Drive `run` to completion, triggering `shutdown` once `received` counts
/// `want` requests so it lands mid-transfer however slow the host is. Panics
/// if `run` returns before shutdown fired, or after 10s of wall-clock time if
/// `run` is still going and `want` was never reached, which also bounds
/// `start_paused` runs.
pub async fn run_with_shutdown_at<T: std::fmt::Debug>(
    run: impl Future<Output = T>,
    shutdown: &ShutdownSignal,
    want: usize,
    received: impl AsyncFn() -> usize,
) -> T {
    let trigger = async {
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while received().await < want {
            assert!(
                std::time::Instant::now() < give_up,
                "the server never received {want} requests to trigger shutdown on"
            );
            // Yield rather than sleep: an idle runtime lets a paused clock jump
            // to the engine's next timeout while real requests are in flight.
            tokio::task::yield_now().await;
        }
        shutdown.trigger();
        std::future::pending::<()>().await;
    };
    let out = tokio::select! {
        out = run => out,
        () = trigger => unreachable!("the trigger never completes"),
    };
    assert!(
        shutdown.is_triggered(),
        "the run returned before shutdown was triggered: {out:?}"
    );
    out
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

/// Assert `report.images[idx].status` matches a pattern. A trailing format
/// string names the table row in the failure.
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
    ($report:expr, $idx:expr, $pattern:pat, $($label:tt)+) => {
        assert!(
            matches!($report.images[$idx].status, $pattern),
            "{}: image[{}]: expected {}, got {:?}",
            format!($($label)+),
            $idx,
            stringify!($pattern),
            $report.images[$idx].status,
        );
    };
}

pub(crate) use assert_status;
