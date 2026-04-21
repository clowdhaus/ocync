//! Platform filtering integration tests: index manifest platform selection
//! and nested index rejection.

mod helpers;

use ocync_distribution::Digest;
use ocync_distribution::spec::{
    Descriptor, ImageIndex, ImageManifest, MediaType, Platform, PlatformFilter,
};
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

    let amd64_config_data = b"amd64-config";
    let amd64_layer_data = b"amd64-layer";
    let amd64_config_desc = blob_descriptor(amd64_config_data, MediaType::OciConfig);
    let amd64_layer_desc = blob_descriptor(amd64_layer_data, MediaType::OciLayerGzip);
    let amd64_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: amd64_config_desc.clone(),
        layers: vec![amd64_layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (amd64_bytes, amd64_digest) = serialize_manifest(&amd64_manifest);

    let arm64_config_data = b"arm64-config";
    let arm64_layer_data = b"arm64-layer";
    let arm64_config_desc = blob_descriptor(arm64_config_data, MediaType::OciConfig);
    let arm64_layer_desc = blob_descriptor(arm64_layer_data, MediaType::OciLayerGzip);
    let arm64_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: arm64_config_desc.clone(),
        layers: vec![arm64_layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (arm64_bytes, arm64_digest) = serialize_manifest(&arm64_manifest);

    let win_config_data = b"win-config";
    let win_layer_data = b"win-layer";
    let win_config_desc = blob_descriptor(win_config_data, MediaType::OciConfig);
    let win_layer_desc = blob_descriptor(win_layer_data, MediaType::OciLayerGzip);
    let win_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: win_config_desc.clone(),
        layers: vec![win_layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (win_bytes, win_digest) = serialize_manifest(&win_manifest);

    // --- Build index with platform-annotated descriptors ---

    let index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![
            Descriptor {
                media_type: MediaType::OciManifest,
                digest: amd64_digest.clone(),
                size: amd64_bytes.len() as u64,
                platform: Some(Platform {
                    architecture: "amd64".into(),
                    os: "linux".into(),
                    variant: None,
                    os_version: None,
                    os_features: None,
                }),
                artifact_type: None,
                annotations: None,
            },
            Descriptor {
                media_type: MediaType::OciManifest,
                digest: arm64_digest.clone(),
                size: arm64_bytes.len() as u64,
                platform: Some(Platform {
                    architecture: "arm64".into(),
                    os: "linux".into(),
                    variant: None,
                    os_version: None,
                    os_features: None,
                }),
                artifact_type: None,
                annotations: None,
            },
            Descriptor {
                media_type: MediaType::OciManifest,
                digest: win_digest.clone(),
                size: win_bytes.len() as u64,
                platform: Some(Platform {
                    architecture: "amd64".into(),
                    os: "windows".into(),
                    variant: None,
                    os_version: None,
                    os_features: None,
                }),
                artifact_type: None,
                annotations: None,
            },
        ],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let index_bytes = serde_json::to_vec(&index).unwrap();

    // --- Source: serve index by tag, children by digest ---

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index_bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // amd64 child: expect exactly 1 pull (matching platform).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{amd64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // arm64 child: expect 0 pulls (filtered out).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{arm64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // windows child: expect 0 pulls (filtered out).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{win_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(win_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // Source blobs: amd64 blobs expect 1 pull each, others expect 0.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", amd64_config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_config_data.to_vec())
                .insert_header("content-length", amd64_config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", amd64_layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_layer_data.to_vec())
                .insert_header("content-length", amd64_layer_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", arm64_config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64_config_data.to_vec())
                .insert_header("content-length", arm64_config_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", arm64_layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64_layer_data.to_vec())
                .insert_header("content-length", arm64_layer_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", win_config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(win_config_data.to_vec())
                .insert_header("content-length", win_config_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", win_layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(win_layer_data.to_vec())
                .insert_header("content-length", win_layer_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // --- Target: no existing manifest, no blobs, accept all pushes ---

    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    mount_blob_not_found(&target_server, "repo", &amd64_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &amd64_layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;

    // Accept amd64 child manifest push (by digest).
    mount_manifest_push(&target_server, "repo", &amd64_digest.to_string()).await;

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
    let expected_bytes = (amd64_config_data.len() + amd64_layer_data.len()) as u64;
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
