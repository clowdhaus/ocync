//! Shutdown integration tests: shutdown stops new work, drains in-flight
//! transfers within the drain deadline, abandons them past it, and an
//! untriggered signal does not keep the engine alive.

mod helpers;

use std::time::Duration;

use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageStatus, RunOutcome};
use wiremock::MockServer;
use wiremock::http::Method;

use helpers::*;

/// One image whose source blob GETs answer only after a delay, so a shutdown
/// triggered on their arrival lands while they are in flight.
struct DelayedPull {
    source: MockServer,
    target: MockServer,
}

impl DelayedPull {
    async fn new(blob_delay: Duration) -> Self {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        let parts = ManifestBuilder::new(b"delayed-config")
            .layer(b"delayed-layer")
            .build();
        parts
            .mount_source_delayed(&source, "repo", "v1", blob_delay)
            .await;
        parts.mount_target(&target, "repo", "v1").await;
        Self { source, target }
    }

    fn mappings(&self) -> Vec<ResolvedMapping> {
        vec![mapping_from_servers(
            &self.source,
            &self.target,
            "repo",
            vec![TagPair::same("v1")],
        )]
    }

    /// Blob GETs the source has received.
    async fn blob_gets(&self) -> usize {
        count_requests(
            &self.source,
            Some(&Method::GET),
            PathMatch::Contains("/blobs/"),
        )
        .await
    }
}

/// With shutdown triggered before the run, the engine stops taking discovery
/// work: it returns with no image reported and no request made to the source.
#[tokio::test]
async fn sync_shutdown_stops_new_work() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-shutdown")
        .layer(b"layer-shutdown")
        .build();
    for tag in ["v1", "v2"] {
        parts.mount_source(&source_server, "repo", tag).await;
        parts.mount_target(&target_server, "repo", tag).await;
    }

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        same_tags(&["v1", "v2"]),
    );
    let shutdown = ShutdownSignal::new();
    shutdown.trigger();

    let report = run_sync_with_shutdown(vec![mapping], &shutdown).await;

    assert!(report.images.is_empty(), "{:?}", report.images);
    assert_eq!(
        count_requests(&source_server, None, PathMatch::Any).await,
        0,
        "shutdown before the run must stop discovery before it asks the source"
    );
}

/// Shutdown fires while the blob GETs are still in flight (2s delay). The 25s
/// drain deadline lets them finish instead of abandoning them.
#[tokio::test]
async fn sync_shutdown_drains_in_flight() {
    let pull = DelayedPull::new(Duration::from_secs(2)).await;
    let shutdown = ShutdownSignal::new();

    let (report, ()) = tokio::join!(
        run_sync_with_shutdown(pull.mappings(), &shutdown),
        trigger_shutdown_at(&shutdown, 2, async || pull.blob_gets().await),
    );

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
}

/// Shutdown fires while blob GETs are delayed 60s. The 25s drain deadline
/// expires first, so the engine abandons the transfers and returns.
///
/// `start_paused` lets tokio auto-advance through the deadline without waiting.
#[tokio::test(start_paused = true)]
async fn sync_shutdown_deadline_abandons_stuck_transfers() {
    let pull = DelayedPull::new(Duration::from_secs(60)).await;
    let shutdown = ShutdownSignal::new();

    let (report, ()) = tokio::join!(
        run_sync_with_shutdown(pull.mappings(), &shutdown),
        trigger_shutdown_at(&shutdown, 2, async || pull.blob_gets().await),
    );

    // A 60s blob that completed would sync the image: the deadline must cut it.
    assert_eq!(
        report.stats.images_synced, 0,
        "the drain deadline should abandon the stuck transfer"
    );
}

/// `with_drain_deadline` is honoured: blob GETs take 3s, which the default 25s
/// deadline would let finish, but a 1s deadline abandons them.
///
/// Real time: a paused clock would elapse either deadline at once.
#[tokio::test]
async fn sync_custom_drain_deadline_abandons_before_default_would() {
    let pull = DelayedPull::new(Duration::from_secs(3)).await;
    let shutdown = ShutdownSignal::new();
    let engine =
        SyncEngine::new(fast_retry(), concurrency(50)).with_drain_deadline(Duration::from_secs(1));

    let (report, ()) = tokio::join!(
        engine.run(
            pull.mappings(),
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        ),
        trigger_shutdown_at(&shutdown, 2, async || pull.blob_gets().await),
    );

    assert_eq!(
        report.stats.images_synced, 0,
        "no images should succeed when drain deadline expires before blob transfer"
    );
}

/// The engine exits after all work completes even though a registered shutdown
/// signal never fires. Breaks if the select loop's shutdown branch stays armed
/// on an always-pending wait, which blocks the `else` exit. The 10s timeout
/// detects the hang.
#[tokio::test]
async fn sync_exits_with_untriggered_shutdown_signal() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-shutdown-exit")
        .layer(b"layer-shutdown-exit")
        .build();
    parts.mount_source(&source_server, "repo", "v1").await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    // The production case: the user never sends SIGTERM.
    let shutdown = ShutdownSignal::new();

    let report = tokio::time::timeout(
        Duration::from_secs(10),
        run_sync_with_shutdown(vec![mapping], &shutdown),
    )
    .await
    .expect("engine hung: did not exit within 10s after completing all work");

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.outcome(), RunOutcome::Clean);
}
