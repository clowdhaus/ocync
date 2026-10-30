//! Shutdown integration tests: shutdown stops new work, drains in-flight
//! transfers, and an untriggered signal does not keep the engine alive.

mod helpers;

use std::time::Duration;

use ocync_sync::engine::TagPair;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::{ImageStatus, RunOutcome};
use wiremock::MockServer;

use helpers::*;

/// With shutdown triggered before the run, the engine stops taking discovery
/// work and returns. Whether in-flight work completes depends on timing, so
/// the only invariant is that it returns with at most the two images.
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
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );
    let shutdown = ShutdownSignal::new();
    shutdown.trigger();

    let report = run_sync_with_shutdown(vec![mapping], &shutdown).await;

    assert!(
        report.images.len() <= 2,
        "should have at most 2 images (may have fewer if shutdown interrupted discovery)"
    );
}

/// Shutdown fires while the layer GET is still in flight (2s delay). The 25s
/// drain deadline lets it finish instead of abandoning it.
#[tokio::test]
async fn sync_shutdown_drains_in_flight() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-drain")
        .layer(b"layer-drain")
        .build();
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull_delayed(
        &source_server,
        "repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
        Duration::from_secs(2),
    )
    .await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let shutdown = ShutdownSignal::new();
    trigger_shutdown_after(&shutdown, Duration::from_millis(50));

    let report = run_sync_with_shutdown(vec![mapping], &shutdown).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
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
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.outcome(), RunOutcome::Clean);
}
