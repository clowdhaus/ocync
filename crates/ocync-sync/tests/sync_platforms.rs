//! Platform filtering integration tests: index platform selection and nested
//! index rejection.

mod helpers;

use ocync_distribution::spec::{ImageIndex, MediaType, PlatformFilter};
use ocync_sync::ImageStatus;
use ocync_sync::engine::TagPair;
use wiremock::MockServer;

use helpers::*;

/// Only the matching platform's child manifest and blobs are pulled from the
/// source and pushed to the target; non-matching platforms are never touched.
#[tokio::test]
async fn sync_index_manifest_platform_filter() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let amd64 = ManifestBuilder::new(b"amd64-config")
        .layer(b"amd64-layer")
        .build();
    let arm64 = ManifestBuilder::new(b"arm64-config")
        .layer(b"arm64-layer")
        .build();
    let win = ManifestBuilder::new(b"win-config")
        .layer(b"win-layer")
        .build();
    let index = IndexBuilder::new()
        .manifest(&amd64, Some(linux("amd64")))
        .manifest(&arm64, Some(linux("arm64")))
        .manifest(&win, Some(platform("windows", "amd64")))
        .build();

    // The index and amd64 are pulled once each; arm64 and windows never.
    index
        .mount_source_expecting(
            &source_server,
            "repo",
            "latest",
            &[1, 0, 0],
            "platform filter",
        )
        .await;

    // Pushes of the filtered-out children have no mock, so they would 404 and fail the sync.
    amd64
        .mount_target(&target_server, "repo", &amd64.digest.to_string())
        .await;
    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    mount_manifest_push(&target_server, "repo", "latest").await;

    let mut mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("latest")],
    );
    mapping.platforms = Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]);

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // The amd64 config and layer only.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (amd64.config_data.len() + amd64.layers_data[0].len()) as u64;
    assert_eq!(report.images[0].bytes_transferred, expected_bytes);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.bytes_transferred, expected_bytes);
}

/// An index whose child is also an index fails with an error rather than
/// silently producing incorrect results.
#[tokio::test]
async fn sync_nested_index_manifest_returns_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let child_index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let child_bytes = serde_json::to_vec(&child_index).unwrap();
    let child_digest = blob_digest(&child_bytes);

    let parent_index = ImageIndex {
        manifests: vec![blob_descriptor(&child_bytes, MediaType::OciIndex)],
        ..child_index
    };
    let parent_bytes = serde_json::to_vec(&parent_index).unwrap();

    source_manifest_mock("repo", "v1", &parent_bytes, MediaType::OciIndex)
        .mount(&source_server)
        .await;
    source_manifest_mock(
        "repo",
        &child_digest.to_string(),
        &child_bytes,
        MediaType::OciIndex,
    )
    .mount(&source_server)
    .await;
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Failed { ref error, .. } if error.contains("nested index")),
        "should fail with nested index error, got: {:?}",
        report.images[0].status
    );
}
