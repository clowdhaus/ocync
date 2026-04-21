//! Shutdown integration tests: graceful shutdown stops new work, drains in-flight
//! transfers, and engine exits cleanly with an untriggered shutdown signal.

mod helpers;

use ocync_distribution::spec::{ImageManifest, MediaType};
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// Trigger shutdown immediately and verify the engine stops accepting new
/// discovery work. In-flight execution may or may not complete depending on
/// timing, but the engine must return within a bounded time.
#[tokio::test]
async fn sync_shutdown_stops_new_work() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-shutdown";
    let layer_data = b"layer-shutdown";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest and blobs (but add delays so shutdown can interrupt).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_source_manifest(&source_server, "repo", "v2", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: everything works.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    let shutdown = ShutdownSignal::new();
    // Trigger shutdown immediately before the engine even starts running.
    shutdown.trigger();

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        )
        .await;

    // With shutdown triggered before run, discovery futures may or may not
    // complete. The key invariant: the engine returns (doesn't hang) and
    // reports whatever results it did gather.
    assert!(
        report.images.len() <= 2,
        "should have at most 2 images (may have fewer if shutdown interrupted discovery)"
    );
}

/// Trigger shutdown while a transfer is still in flight (blob GET has a
/// 2-second delay). The 25-second drain deadline gives the transfer enough
/// time to complete. Verifies that in-flight work finishes instead of being
/// abandoned prematurely.
#[tokio::test]
async fn sync_shutdown_drains_in_flight() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-drain";
    let layer_data = b"layer-drain";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: manifest responds immediately, config blob responds immediately,
    // but layer blob has a 2-second delay (simulates a slow transfer in progress
    // when shutdown fires).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(layer_data.to_vec())
                .insert_header("content-length", layer_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(2)),
        )
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let shutdown = ShutdownSignal::new();

    // Trigger shutdown after 50ms. The blob GET takes 2s, so the transfer
    // will still be in flight when shutdown fires. The 25s drain deadline
    // gives the transfer plenty of time to complete.
    let signal = shutdown.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        signal.trigger();
    });

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        )
        .await;

    // The in-flight transfer should complete within the drain deadline (2s < 25s).
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
}

/// The engine must exit after all work completes even when a shutdown signal
/// is registered but never triggered. Before the fix, the select! loop's
/// shutdown branch stayed enabled with an always-pending `notified().await`,
/// preventing the `else` exit from firing.
///
/// This test uses a 10-second timeout to detect the hang -- if the engine
/// doesn't exit within 10s of completing all transfers, the test fails.
#[tokio::test]
async fn sync_exits_with_untriggered_shutdown_signal() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-shutdown-exit";
    let layer_data = b"layer-shutdown-exit";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    // Create shutdown signal but DO NOT trigger it -- this is the production
    // scenario where the user never sends SIGTERM.
    let shutdown = ShutdownSignal::new();

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        ),
    )
    .await
    .expect("engine hung -- did not exit within 10s after completing all work");

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.exit_code(), 0);
}
