//! Integration tests for `SyncEngine` using mock HTTP servers.

use std::sync::Arc;

use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType};
use ocync_distribution::{Digest, RegistryClientBuilder};
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;
use ocync_sync::{ImageStatus, SkipReason};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a valid sha256 digest from a short suffix, zero-padded to 64 hex chars.
fn test_digest(suffix: &str) -> Digest {
    format!("sha256:{suffix:0>64}").parse().unwrap()
}

fn mock_url(server: &MockServer) -> Url {
    Url::parse(&server.uri()).unwrap()
}

fn mock_client(server: &MockServer) -> Arc<ocync_distribution::RegistryClient> {
    Arc::new(
        RegistryClientBuilder::new(mock_url(server))
            .build()
            .unwrap(),
    )
}

fn fast_retry() -> RetryConfig {
    RetryConfig {
        max_retries: 2,
        initial_backoff: std::time::Duration::from_millis(1),
        max_backoff: std::time::Duration::from_millis(10),
        backoff_multiplier: 2,
    }
}

/// Serialize an `ImageManifest` to JSON bytes and compute its digest.
fn serialize_manifest(manifest: &ImageManifest) -> (Vec<u8>, Digest) {
    let bytes = serde_json::to_vec(manifest).unwrap();
    let hash = ocync_distribution::sha256::Sha256::digest(&bytes);
    let digest = Digest::from_sha256(hash);
    (bytes, digest)
}

fn test_descriptor(digest: Digest, media_type: MediaType) -> Descriptor {
    Descriptor {
        media_type,
        digest,
        size: 100,
        platform: None,
        artifact_type: None,
        annotations: None,
    }
}

fn simple_image_manifest(config_digest: &Digest, layer_digest: &Digest) -> ImageManifest {
    ImageManifest {
        schema_version: 2,
        media_type: None,
        config: test_descriptor(config_digest.clone(), MediaType::OciConfig),
        layers: vec![test_descriptor(
            layer_digest.clone(),
            MediaType::OciLayerGzip,
        )],
        subject: None,
        artifact_type: None,
        annotations: None,
    }
}

/// Mount mock endpoints for a full image sync on the source server:
/// - GET manifest (returns the given JSON bytes with media type header)
async fn mount_source_manifest(server: &MockServer, repo: &str, tag: &str, bytes: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(server)
        .await;
}

/// Mount mock for blob pull (GET).
async fn mount_blob_pull(server: &MockServer, repo: &str, digest: &Digest, data: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(data.to_vec())
                .insert_header("content-length", data.len().to_string()),
        )
        .mount(server)
        .await;
}

/// Mount mock for blob HEAD (exists check) returning 404 (not found).
async fn mount_blob_not_found(server: &MockServer, repo: &str, digest: &Digest) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Mount mock for blob HEAD (exists check) returning 200 (exists).
async fn mount_blob_exists(server: &MockServer, repo: &str, digest: &Digest) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .mount(server)
        .await;
}

/// Mount mock for blob push: POST initiate + PATCH chunked data + PUT finalize.
async fn mount_blob_push(server: &MockServer, repo: &str) {
    // POST: initiate upload.
    Mock::given(method("POST"))
        .and(path(format!("/v2/{repo}/blobs/uploads/")))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", format!("/v2/{repo}/blobs/uploads/upload-id")),
        )
        .mount(server)
        .await;

    // PATCH: accept chunked data (may be called 1+ times).
    Mock::given(method("PATCH"))
        .and(path(format!("/v2/{repo}/blobs/uploads/upload-id")))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", format!("/v2/{repo}/blobs/uploads/upload-id")),
        )
        .mount(server)
        .await;

    // PUT: finalize with digest query param.
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{repo}/blobs/uploads/upload-id")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
}

/// Mount mock for manifest HEAD returning 404.
async fn mount_manifest_head_not_found(server: &MockServer, repo: &str, tag: &str) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Mount mock for manifest HEAD returning matching digest.
async fn mount_manifest_head_matching(server: &MockServer, repo: &str, tag: &str, digest: &Digest) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .mount(server)
        .await;
}

/// Mount mock for manifest push (PUT) returning 201.
async fn mount_manifest_push(server: &MockServer, repo: &str, reference: &str) {
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_image_happy_path() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c0c0");
    let layer_digest = test_digest("1a1a");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest and blobs.
    mount_source_manifest(&source_server, "library/nginx", "latest", &manifest_bytes).await;
    mount_blob_pull(
        &source_server,
        "library/nginx",
        &config_digest,
        b"config-data",
    )
    .await;
    mount_blob_pull(
        &source_server,
        "library/nginx",
        &layer_digest,
        b"layer-data",
    )
    .await;

    // Target: manifest HEAD 404, blobs not found, push endpoints.
    mount_manifest_head_not_found(&target_server, "mirror/nginx", "latest").await;
    mount_blob_not_found(&target_server, "mirror/nginx", &config_digest).await;
    mount_blob_not_found(&target_server, "mirror/nginx", &layer_digest).await;
    mount_blob_push(&target_server, "mirror/nginx").await;
    mount_manifest_push(&target_server, "mirror/nginx", "latest").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "library/nginx".into(),
        target_repo: "mirror/nginx".into(),
        targets: vec![TargetEntry {
            name: "target-reg".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("latest")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(report.images[0].bytes_transferred > 0);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_failed, 0);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_skipped, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_image_skip_on_digest_match() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("cc");
    let layer_digest = test_digest("11");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD returns matching digest → should skip.
    mount_manifest_head_matching(&target_server, "repo", "v1", &manifest_digest).await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Skipped {
            reason: SkipReason::DigestMatch,
        }
    ));
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_image_blob_exists_at_target_skips_transfer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c1");
    let layer_digest = test_digest("b1");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest (blobs NOT served — they shouldn't be pulled).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD 404, but both blobs already exist.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_exists(&target_server, "repo", &config_digest).await;
    mount_blob_exists(&target_server, "repo", &layer_digest).await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // No bytes transferred — blobs existed at target.
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.stats.blobs_skipped, 2);
}

#[tokio::test]
async fn sync_image_manifest_pull_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Source: 403 on manifest pull (non-retryable).
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&source_server)
        .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn sync_image_blob_pull_retries_on_500() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c2");
    let layer_digest = test_digest("b2");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: manifest succeeds, config blob succeeds.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;

    // Layer blob: first attempt 500, second attempt succeeds.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{layer_digest}")))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{layer_digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"layer-data".to_vec()))
        .mount(&source_server)
        .await;

    // Target: standard setup.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.stats.blobs_transferred, 2);
}

#[tokio::test]
async fn sync_dedup_across_tags() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Two tags share the same blobs.
    let config_digest = test_digest("aa00");
    let layer_digest = test_digest("bb00");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: both tags serve the same manifest.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_source_manifest(&source_server, "repo", "v2", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo", &layer_digest, b"layer").await;

    // Target: manifest HEAD 404 for both, blobs not found initially.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 2);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(matches!(report.images[1].status, ImageStatus::Synced));

    // First tag transfers blobs; second tag skips them (dedup).
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[1].blob_stats.skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_skipped, 2);
}

#[tokio::test]
async fn sync_multiple_targets() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let config_digest = test_digest("c3");
    let layer_digest = test_digest("b3");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo", &layer_digest, b"layer").await;

    // Both targets: no manifest, no blobs, accept pushes.
    for target in [&target_a, &target_b] {
        mount_manifest_head_not_found(target, "repo", "v1").await;
        mount_blob_not_found(target, "repo", &config_digest).await;
        mount_blob_not_found(target, "repo", &layer_digest).await;
        mount_blob_push(target, "repo").await;
        mount_manifest_push(target, "repo", "v1").await;
    }

    let source_client = mock_client(&source_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![
            TargetEntry {
                name: "target-a".into(),
                client: mock_client(&target_a),
            },
            TargetEntry {
                name: "target-b".into(),
                client: mock_client(&target_b),
            },
        ],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 2);
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_retag() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c4");
    let layer_digest = test_digest("b4");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest at source tag.
    mount_source_manifest(&source_server, "repo", "latest", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo", &layer_digest, b"layer").await;

    // Target: HEAD for target tag, blobs missing, push at target tag.
    mount_manifest_head_not_found(&target_server, "repo", "stable").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "stable").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::retag("latest", "stable")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
}

#[tokio::test]
async fn sync_empty_mappings() {
    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![], &NullProgress).await;

    assert!(report.images.is_empty());
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_blob_push_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c5");
    let layer_digest = test_digest("b5");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;

    // Target: manifest HEAD 404, config blob not found.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;

    // Blob push: POST initiate returns 403 (non-retryable).
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    if let ImageStatus::Failed { error, .. } = &report.images[0].status {
        assert!(error.contains("blob transfer"));
    }
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn sync_index_manifest_multi_platform() {
    use ocync_distribution::spec::ImageIndex;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build two child image manifests (simulating amd64 and arm64).
    let amd64_config = test_digest("a0");
    let amd64_layer = test_digest("a1");
    let amd64_manifest = simple_image_manifest(&amd64_config, &amd64_layer);
    let (amd64_bytes, amd64_digest) = serialize_manifest(&amd64_manifest);

    let arm64_config = test_digest("b0");
    let arm64_layer = test_digest("b1");
    let arm64_manifest = simple_image_manifest(&arm64_config, &arm64_layer);
    let (arm64_bytes, arm64_digest) = serialize_manifest(&arm64_manifest);

    // Build the index manifest referencing both children.
    let index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![
            test_descriptor(amd64_digest.clone(), MediaType::OciManifest),
            test_descriptor(arm64_digest.clone(), MediaType::OciManifest),
        ],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let index_bytes = serde_json::to_vec(&index).unwrap();

    // Source: serve the index by tag and children by digest.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index_bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{amd64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{arm64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // Source: blobs.
    mount_blob_pull(&source_server, "repo", &amd64_config, b"amd64-config").await;
    mount_blob_pull(&source_server, "repo", &amd64_layer, b"amd64-layer").await;
    mount_blob_pull(&source_server, "repo", &arm64_config, b"arm64-config").await;
    mount_blob_pull(&source_server, "repo", &arm64_layer, b"arm64-layer").await;

    // Target: no manifest, no blobs, accept all pushes.
    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    mount_blob_not_found(&target_server, "repo", &amd64_config).await;
    mount_blob_not_found(&target_server, "repo", &amd64_layer).await;
    mount_blob_not_found(&target_server, "repo", &arm64_config).await;
    mount_blob_not_found(&target_server, "repo", &arm64_layer).await;
    mount_blob_push(&target_server, "repo").await;

    // Accept child manifest pushes (by digest) and index push (by tag).
    mount_manifest_push(&target_server, "repo", &amd64_digest.to_string()).await;
    mount_manifest_push(&target_server, "repo", &arm64_digest.to_string()).await;
    mount_manifest_push(&target_server, "repo", "latest").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("latest")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // 4 blobs: 2 configs + 2 layers across two platforms.
    assert_eq!(report.images[0].blob_stats.transferred, 4);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    assert!(report.images[0].bytes_transferred > 0);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 4);
}

#[tokio::test]
async fn sync_head_different_digest_proceeds_with_sync() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("d0");
    let layer_digest = test_digest("d1");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest and blobs.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo", &layer_digest, b"layer").await;

    // Target: manifest HEAD returns a DIFFERENT digest → should proceed.
    let stale_digest = test_digest("5ca1e");
    mount_manifest_head_matching(&target_server, "repo", "v1", &stale_digest).await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
}

#[tokio::test]
async fn sync_empty_tags_produces_no_images() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert!(report.images.is_empty());
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_manifest_push_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c6");
    let layer_digest = test_digest("b6");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve everything normally.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo", &layer_digest, b"layer").await;

    // Target: blobs succeed, but manifest PUT fails with 403.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    mount_blob_push(&target_server, "repo").await;

    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    if let ImageStatus::Failed { error, .. } = &report.images[0].status {
        assert!(
            error.contains("manifest"),
            "error should mention manifest: {error}"
        );
    }
    assert_eq!(report.stats.images_failed, 1);
}

#[tokio::test]
async fn sync_retry_exhaustion_returns_final_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("c7");
    let layer_digest = test_digest("b7");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_digest, b"config").await;

    // Layer blob: always returns 429 (retryable) — should exhaust retries.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{layer_digest}")))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    mount_blob_push(&target_server, "repo").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping], &NullProgress).await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    if let ImageStatus::Failed { error, retries } = &report.images[0].status {
        assert!(
            error.contains("blob transfer"),
            "error should mention blob transfer: {error}"
        );
        assert_eq!(*retries, 2); // max_retries from fast_retry()
    }
    assert_eq!(report.stats.images_failed, 1);
}

#[tokio::test]
async fn sync_cross_repo_mount_success() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared blobs across two different source repos.
    let config_digest = test_digest("ac00");
    let layer_digest = test_digest("ac01");

    let manifest_a = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let manifest_b = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: two repos with the same blobs.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo-a", &layer_digest, b"layer").await;

    // Target for repo-a: no manifest, no blobs, push succeeds.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: no manifest, blobs not found, mount succeeds (201 Created).
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_digest).await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // First mapping: repo-a syncs normally (pull+push).
    let mapping_a = ResolvedMapping {
        source_client: source_client.clone(),
        source_repo: "repo-a".into(),
        target_repo: "repo-a".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client.clone(),
        }],
        tags: vec![TagPair::same("v1")],
    };

    // Second mapping: repo-b should mount from repo-a.
    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping_a, mapping_b], &NullProgress).await;

    assert_eq!(report.images.len(), 2);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(matches!(report.images[1].status, ImageStatus::Synced));
    // First mapping: 2 blobs transferred.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    // Second mapping: 2 blobs mounted (not transferred).
    assert_eq!(report.images[1].blob_stats.mounted, 2);
    assert_eq!(report.images[1].blob_stats.transferred, 0);
    assert_eq!(report.stats.blobs_mounted, 2);
}

#[tokio::test]
async fn sync_cross_repo_mount_fallback_to_pull_push() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("f0");
    let layer_digest = test_digest("f1");

    let manifest_a = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let manifest_b = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: two repos.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo-a", &layer_digest, b"layer").await;
    // repo-b blob pulls needed for fallback.
    mount_blob_pull(&source_server, "repo-b", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo-b", &layer_digest, b"layer").await;

    // Target for repo-a: normal sync.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: mount returns 202 Accepted (fallback).
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_digest).await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo-b/blobs/uploads/fallback-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v2/repo-b/blobs/uploads/fallback-id"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo-b/blobs/uploads/fallback-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo-b/blobs/uploads/fallback-id"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping_a = ResolvedMapping {
        source_client: source_client.clone(),
        source_repo: "repo-a".into(),
        target_repo: "repo-a".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client.clone(),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping_a, mapping_b], &NullProgress).await;

    assert_eq!(report.images.len(), 2);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(matches!(report.images[1].status, ImageStatus::Synced));
    // Second mapping: mount fallback → pull+push, so transferred not mounted.
    assert_eq!(report.images[1].blob_stats.mounted, 0);
    assert_eq!(report.images[1].blob_stats.transferred, 2);
}

#[tokio::test]
async fn sync_cross_repo_mount_failure_falls_back() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = test_digest("e0");
    let layer_digest = test_digest("e1");

    let manifest_a = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let manifest_b = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: two repos.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo-a", &layer_digest, b"layer").await;
    mount_blob_pull(&source_server, "repo-b", &config_digest, b"config").await;
    mount_blob_pull(&source_server, "repo-b", &layer_digest, b"layer").await;

    // Target for repo-a: normal sync.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: mount returns 500 (error), falls back to pull+push.
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_digest).await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2) // First two POSTs are mount attempts that fail.
        .mount(&target_server)
        .await;
    // Subsequent POSTs succeed (fallback upload initiation).
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo-b/blobs/uploads/retry-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v2/repo-b/blobs/uploads/retry-id"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo-b/blobs/uploads/retry-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo-b/blobs/uploads/retry-id"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping_a = ResolvedMapping {
        source_client: source_client.clone(),
        source_repo: "repo-a".into(),
        target_repo: "repo-a".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client.clone(),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let mut engine = SyncEngine::new(fast_retry());
    let report = engine.run(vec![mapping_a, mapping_b], &NullProgress).await;

    assert_eq!(report.images.len(), 2);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(matches!(report.images[1].status, ImageStatus::Synced));
    // Mount failed → fell back to pull+push.
    assert_eq!(report.images[1].blob_stats.mounted, 0);
    assert_eq!(report.images[1].blob_stats.transferred, 2);
}
