//! Platform filtering integration tests: index manifest platform selection
//! and nested index rejection.

mod helpers;

use ocync_distribution::Digest;
use ocync_distribution::spec::{Descriptor, ImageIndex, MediaType, Platform, PlatformFilter};
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

// ---------------------------------------------------------------------------
// Platform filtering tests
// ---------------------------------------------------------------------------

/// Platform filtering: only the matching platform's child manifest and blobs
/// are pulled from source and pushed to target. Non-matching platforms are
/// never touched.
#[tokio::test]
async fn sync_index_manifest_platform_filter() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // --- Build three child image manifests (linux/amd64, linux/arm64, windows/amd64) ---

    let amd64 = ManifestBuilder::new(b"amd64-config")
        .layer(b"amd64-layer")
        .build();
    let arm64 = ManifestBuilder::new(b"arm64-config")
        .layer(b"arm64-layer")
        .build();
    let win = ManifestBuilder::new(b"win-config")
        .layer(b"win-layer")
        .build();

    // --- Build index with platform-annotated descriptors ---

    let index = IndexBuilder::new()
        .manifest(
            &amd64,
            Some(Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
                os_version: None,
                os_features: None,
            }),
        )
        .manifest(
            &arm64,
            Some(Platform {
                os: "linux".into(),
                architecture: "arm64".into(),
                variant: None,
                os_version: None,
                os_features: None,
            }),
        )
        .manifest(
            &win,
            Some(Platform {
                os: "windows".into(),
                architecture: "amd64".into(),
                variant: None,
                os_version: None,
                os_features: None,
            }),
        )
        .build();

    // --- Source: serve index by tag, children by digest ---

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index.bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // amd64 child: expect exactly 1 pull (matching platform).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", amd64.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // arm64 child: expect 0 pulls (filtered out).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", arm64.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // windows child: expect 0 pulls (filtered out).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", win.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(win.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // Source blobs: amd64 blobs expect 1 pull each, others expect 0.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", amd64.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64.config_data.clone())
                .insert_header("content-length", amd64.config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            amd64.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64.layers_data[0].clone())
                .insert_header("content-length", amd64.layers_data[0].len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", arm64.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64.config_data.clone())
                .insert_header("content-length", arm64.config_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            arm64.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64.layers_data[0].clone())
                .insert_header("content-length", arm64.layers_data[0].len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", win.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(win.config_data.clone())
                .insert_header("content-length", win.config_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            win.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(win.layers_data[0].clone())
                .insert_header("content-length", win.layers_data[0].len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // --- Target: no existing manifest, no blobs, accept all pushes ---

    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    mount_blob_not_found(&target_server, "repo", &amd64.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &amd64.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "repo").await;

    // Accept amd64 child manifest push (by digest).
    mount_manifest_push(&target_server, "repo", &amd64.digest.to_string()).await;

    // Accept filtered index push (by tag).
    mount_manifest_push(&target_server, "repo", "latest").await;

    // arm64 and windows manifest pushes should NOT happen -- wiremock will
    // fail verification if unexpected requests arrive (no mock mounted).

    let mut mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("latest")],
    );
    mapping.platforms = Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]);

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Only 2 blobs transferred: amd64 config + amd64 layer.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (amd64.config_data.len() + amd64.layers_data[0].len()) as u64;
    assert_eq!(report.images[0].bytes_transferred, expected_bytes);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.bytes_transferred, expected_bytes);
    // wiremock .expect(N) assertions verify the platform filtering path.
}

// ---------------------------------------------------------------------------
// Nested index manifest rejection
// ---------------------------------------------------------------------------

/// An index manifest whose child is also an index should fail with an error
/// rather than silently producing incorrect results.
#[tokio::test]
async fn sync_nested_index_manifest_returns_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build a child descriptor that points to another index manifest.
    let child_index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let child_bytes = serde_json::to_vec(&child_index).unwrap();
    let child_hash = ocync_distribution::sha256::Sha256::digest(&child_bytes);
    let child_digest = Digest::from_sha256(child_hash);

    let parent_index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![Descriptor {
            media_type: MediaType::OciIndex,
            digest: child_digest.clone(),
            size: child_bytes.len() as u64,
            platform: None,
            artifact_type: None,
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let parent_bytes = serde_json::to_vec(&parent_index).unwrap();

    // Source: serve parent index and child index.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parent_bytes)
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{child_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(child_bytes)
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Failed { ref error, .. } if error.contains("nested index")),
        "should fail with nested index error, got: {:?}",
        report.images[0].status
    );
}
