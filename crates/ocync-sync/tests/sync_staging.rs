//! Staging integration tests: pull-once semantics, shutdown drain deadline expiry,
//! custom drain deadline, and staged files exist on disk after sync.

mod helpers;

use ocync_distribution::spec::MediaType;
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

// ---------------------------------------------------------------------------
// Tests: staging pull-once semantics, shutdown drain deadline expiry
// ---------------------------------------------------------------------------

/// Multi-target test with staging enabled. Each source blob is pulled exactly
/// once (verified by `expect(1)`) and pushed to both targets from the staged
/// file on disk.
#[tokio::test]
async fn sync_staging_pulls_once_pushes_twice() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"staging-config-twice")
        .layer(b"staging-layer-twice")
        .build();

    // Source: serve manifest and blobs with expect(1) to verify pull-once.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", parts.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.config_data.clone())
                .insert_header("content-length", parts.config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.layers_data[0].clone())
                .insert_header("content-length", parts.layers_data[0].len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Target A: manifest HEAD 404, blobs HEAD 404, push endpoints.
    mount_manifest_head_not_found(&target_a, "repo", "v1").await;
    mount_blob_not_found(&target_a, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_a, "repo", &parts.layer_descs[0].digest).await;
    mount_blob_push(&target_a, "repo").await;
    mount_manifest_push(&target_a, "repo", "v1").await;

    // Target B: manifest HEAD 404, blobs HEAD 404, push endpoints.
    mount_manifest_head_not_found(&target_b, "repo", "v1").await;
    mount_blob_not_found(&target_b, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_b, "repo", &parts.layer_descs[0].digest).await;
    mount_blob_push(&target_b, "repo").await;
    mount_manifest_push(&target_b, "repo", "v1").await;

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        vec![TagPair::same("v1")],
    );

    // Use max_concurrent=1 so target-a completes and stages blobs before
    // target-b starts (reads from staging instead of pulling from source).
    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(vec![mapping], empty_cache(), staging, &NullProgress, None)
        .await;

    assert_eq!(report.images.len(), 2);
    let synced = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ImageStatus::Synced))
        .count();
    assert_eq!(synced, 2, "both targets should succeed");
    assert_eq!(report.stats.images_synced, 2);
    // wiremock expect(1) assertions verify each source blob was pulled exactly once.
}

/// Shutdown fires while a blob transfer is stuck behind a 60-second delay.
/// The 25-second drain deadline expires before the blob completes, so the
/// engine abandons the stuck transfer and returns.
///
/// Uses `start_paused = true` so tokio auto-advances through the 25-second
/// drain deadline without waiting real time. The blob's 60-second delay
/// exceeds the deadline, so it never completes.
#[tokio::test(start_paused = true)]
async fn sync_shutdown_deadline_abandons_stuck_transfers() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"stuck-config")
        .layer(b"stuck-layer")
        .build();

    // Source: manifest responds immediately. Config blob has a 60-second delay
    // (far beyond the 25-second drain deadline). Layer blob also delayed.
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", parts.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.config_data.clone())
                .insert_header("content-length", parts.config_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.layers_data[0].clone())
                .insert_header("content-length", parts.layers_data[0].len().to_string())
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .mount(&source_server)
        .await;

    // Target: standard setup (manifest HEAD 404, blobs HEAD 404, push endpoints).
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[0].digest).await;
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

    // Trigger shutdown after 10ms (real wall-clock via std::thread::sleep).
    // With start_paused=true, tokio auto-advances time: the engine's 25s
    // drain deadline resolves before the 60s blob delays. wiremock delays
    // use tokio::time::sleep internally, so they also auto-advance -- but
    // 60s > 25s, so the deadline fires first and the engine breaks out.
    let signal = shutdown.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
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

    // The drain deadline (25s) fires before the blob transfer completes (60s).
    // The engine abandons the stuck transfer and returns. The image may or
    // may not appear in results depending on whether discovery completed
    // before the deadline, but the key invariant is that the engine returns
    // (doesn't hang forever).
    assert!(
        report.images.len() <= 1,
        "should have at most 1 image result"
    );
}

/// Verify that `with_drain_deadline` is respected. Blob transfers take 5s.
/// With the default 25s drain deadline the transfer would succeed during
/// drain, but with a custom 2s deadline it is abandoned.
///
/// Uses `start_paused = true` so tokio auto-advances through deadlines
/// without waiting real time.
#[tokio::test(start_paused = true)]
async fn sync_custom_drain_deadline_abandons_before_default_would() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"drain-cfg")
        .layer(b"drain-layer")
        .build();

    // Source: manifest responds immediately. Blob delays are 5s -- between
    // our custom 2s drain deadline and the default 25s deadline. This means
    // the transfer would succeed with the default but fails with the custom.
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", parts.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.config_data.clone())
                .insert_header("content-length", parts.config_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.layers_data[0].clone())
                .insert_header("content-length", parts.layers_data[0].len().to_string())
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[0].digest).await;
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
    let signal = shutdown.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        signal.trigger();
    });

    // 2-second drain deadline: shorter than the 5s blob delay, so the
    // engine should abandon the transfer. With the default 25s deadline,
    // the transfer would complete successfully.
    let engine =
        SyncEngine::new(fast_retry(), 50).with_drain_deadline(std::time::Duration::from_secs(2));
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        )
        .await;

    // The 2s drain deadline fires before the 5s blob transfer completes.
    // No images should have synced successfully.
    assert_eq!(
        report.stats.images_synced, 0,
        "no images should succeed when drain deadline expires before blob transfer"
    );
}

// ---------------------------------------------------------------------------
// Tests: staging verifies files exist on disk
// ---------------------------------------------------------------------------

/// Multi-target staging with disk verification and pull-once assertions.
/// Each source endpoint is hit exactly once (verified by `.expect(1)`),
/// and staged blob files exist on disk with correct content.
#[tokio::test]
async fn sync_staging_writes_blobs_to_disk() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"staging-disk-config")
        .layer(b"staging-disk-layer")
        .build();

    // Source: serve manifest and blobs with expect(1) to verify pull-once.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", parts.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.config_data.clone())
                .insert_header("content-length", parts.config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.layers_data[0].clone())
                .insert_header("content-length", parts.layers_data[0].len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    for target in [&target_a, &target_b] {
        mount_manifest_head_not_found(target, "repo", "v1").await;
        mount_blob_not_found(target, "repo", &parts.config_desc.digest).await;
        mount_blob_not_found(target, "repo", &parts.layer_descs[0].digest).await;
        mount_blob_push(target, "repo").await;
        mount_manifest_push(target, "repo", "v1").await;
    }

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(vec![mapping], empty_cache(), staging, &NullProgress, None)
        .await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced))
    );
    // Both targets should transfer 2 blobs each (from staging, not from source).
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_transferred, 4, "2 blobs x 2 targets");
    // wiremock expect(1) verifies each source blob was pulled exactly once.

    // Verify staged files exist on disk at the expected content-addressable paths.
    let config_path = staging_dir
        .path()
        .join("blobs")
        .join(parts.config_desc.digest.algorithm())
        .join(parts.config_desc.digest.hex());
    let layer_path = staging_dir
        .path()
        .join("blobs")
        .join(parts.layer_descs[0].digest.algorithm())
        .join(parts.layer_descs[0].digest.hex());
    assert!(
        config_path.exists(),
        "config blob should be staged on disk at {config_path:?}"
    );
    assert!(
        layer_path.exists(),
        "layer blob should be staged on disk at {layer_path:?}"
    );

    // Verify content matches what was pulled from source.
    assert_eq!(std::fs::read(&config_path).unwrap(), parts.config_data);
    assert_eq!(std::fs::read(&layer_path).unwrap(), parts.layers_data[0]);
}
