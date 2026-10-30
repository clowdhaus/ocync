//! Staging integration tests: pull-once fan-out to several targets, staged
//! files on disk, and shutdown drain deadlines.

mod helpers;

use std::time::Duration;

use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use wiremock::MockServer;

use helpers::*;

/// With staging on and two targets, each source request is made exactly once
/// (`expect(1)`) and both targets are pushed from the staged files, which
/// exist on disk with the pulled content.
#[tokio::test]
async fn sync_staging_writes_blobs_to_disk() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"staging-disk-config")
        .layer(b"staging-disk-layer")
        .build();
    parts
        .mount_source_expecting(&source_server, "repo", "v1", 1)
        .await;
    for target in [&target_a, &target_b] {
        parts.mount_target(target, "repo", "v1").await;
    }

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());
    let mapping = mapping_to_targets(
        &source_server,
        &[("target-a", &target_a), ("target-b", &target_b)],
        "repo",
        vec![TagPair::same("v1")],
    );

    // Sequential, so target-a stages the blobs before target-b reads them.
    let report = run_sync_sequential_with_staging(vec![mapping], staging).await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced))
    );
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_transferred, 4, "2 blobs x 2 targets");

    for (digest, data) in [
        (&parts.config_desc.digest, &parts.config_data),
        (&parts.layer_descs[0].digest, &parts.layers_data[0]),
    ] {
        let staged = staging_dir
            .path()
            .join("blobs")
            .join(digest.algorithm())
            .join(digest.hex());
        assert!(staged.exists(), "blob should be staged at {staged:?}");
        assert_eq!(&std::fs::read(&staged).unwrap(), data);
    }
}

/// Shutdown fires while blob GETs are delayed 60s. The 25s drain deadline
/// expires first, so the engine abandons the transfers and returns.
///
/// `start_paused` lets tokio auto-advance through the deadline without waiting.
#[tokio::test(start_paused = true)]
async fn sync_shutdown_deadline_abandons_stuck_transfers() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"stuck-config")
        .layer(b"stuck-layer")
        .build();
    parts
        .mount_source_delayed(&source_server, "repo", "v1", Duration::from_secs(60))
        .await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let shutdown = ShutdownSignal::new();
    trigger_shutdown_after(&shutdown, Duration::from_millis(10));

    let report = run_sync_with_shutdown(vec![mapping], &shutdown).await;

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
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"drain-cfg")
        .layer(b"drain-layer")
        .build();
    parts
        .mount_source_delayed(&source_server, "repo", "v1", Duration::from_secs(3))
        .await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let shutdown = ShutdownSignal::new();
    trigger_shutdown_after(&shutdown, Duration::from_millis(10));

    let report = SyncEngine::new(fast_retry(), concurrency(50))
        .with_drain_deadline(Duration::from_secs(1))
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        )
        .await;

    assert_eq!(
        report.stats.images_synced, 0,
        "no images should succeed when drain deadline expires before blob transfer"
    );
}
