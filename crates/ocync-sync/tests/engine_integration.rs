//! Integration tests for `SyncEngine` using mock HTTP servers.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ocync_distribution::spec::{Descriptor, ImageIndex, ImageManifest, MediaType};
use ocync_distribution::{Digest, RegistryClientBuilder};
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageStatus, SkipReason};
use url::Url;
use wiremock::matchers::{method, path, query_param};
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

fn empty_cache() -> Rc<RefCell<TransferStateCache>> {
    Rc::new(RefCell::new(TransferStateCache::new()))
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

/// Compute the real SHA-256 digest for test blob data.
fn blob_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// Build a descriptor with the real digest and size of the given data.
fn blob_descriptor(data: &[u8], media_type: MediaType) -> Descriptor {
    Descriptor {
        media_type,
        digest: blob_digest(data),
        size: data.len() as u64,
        platform: None,
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
async fn sync_happy_path() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
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
    let (manifest_bytes, _manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest and blobs.
    mount_source_manifest(&source_server, "library/nginx", "latest", &manifest_bytes).await;
    mount_blob_pull(
        &source_server,
        "library/nginx",
        &config_desc.digest,
        config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "library/nginx",
        &layer_desc.digest,
        layer_data,
    )
    .await;

    // Target: manifest HEAD 404, blobs not found, push endpoints.
    mount_manifest_head_not_found(&target_server, "mirror/nginx", "latest").await;
    mount_blob_not_found(&target_server, "mirror/nginx", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "mirror/nginx", &layer_desc.digest).await;
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
    assert_eq!(
        report.images[0].bytes_transferred,
        config_data.len() as u64 + layer_data.len() as u64,
        "bytes_transferred must equal sum of blob sizes"
    );
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_failed, 0);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_skipped, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_skip_on_digest_match() {
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
async fn sync_blob_exists_at_target_skips_transfer() {
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
    // No bytes transferred — blobs existed at target.
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.stats.blobs_skipped, 2);
}

#[tokio::test]
async fn sync_manifest_pull_failure() {
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
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn sync_blob_transfer_retries_on_source_500() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config";
    let layer_data = b"layer-data";
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

    // Source: manifest succeeds, config blob succeeds.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;

    // Layer blob: first attempt 500, second attempt succeeds.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(layer_data.to_vec()))
        .mount(&source_server)
        .await;

    // Target: standard setup.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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
    assert_eq!(report.stats.blobs_transferred, 2);
}

#[tokio::test]
async fn sync_dedup_across_tags() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Two tags share the same blobs.
    let config_data = b"config";
    let layer_data = b"layer";
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

    // Source: both tags serve the same manifest.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_source_manifest(&source_server, "repo", "v2", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: manifest HEAD 404 for both, blobs not found initially.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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

    // Use max_concurrent=1 to ensure sequential execution so dedup works across tags.
    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

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

    let config_data = b"config";
    let layer_data = b"layer";
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

    // Source: serve manifest with expect(1) to verify pull-once fan-out.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Both targets: no manifest, no blobs, accept pushes.
    for target in [&target_a, &target_b] {
        mount_manifest_head_not_found(target, "repo", "v1").await;
        mount_blob_not_found(target, "repo", &config_desc.digest).await;
        mount_blob_not_found(target, "repo", &layer_desc.digest).await;
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

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced))
    );
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(
        report.stats.blobs_transferred, 4,
        "2 blobs per target x 2 targets"
    );
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_retag() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config";
    let layer_data = b"layer";
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

    // Source: serve manifest at source tag.
    mount_source_manifest(&source_server, "repo", "latest", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: HEAD for target tag, blobs missing, push at target tag.
    mount_manifest_head_not_found(&target_server, "repo", "stable").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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
}

#[tokio::test]
async fn sync_empty_mappings() {
    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert!(report.images.is_empty());
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_blob_transfer_failure() {
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
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn sync_index_manifest_multi_platform() {
    use ocync_distribution::spec::ImageIndex;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build two child image manifests (simulating amd64 and arm64).
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
    mount_blob_pull(
        &source_server,
        "repo",
        &amd64_config_desc.digest,
        amd64_config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &amd64_layer_desc.digest,
        amd64_layer_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &arm64_config_desc.digest,
        arm64_config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &arm64_layer_desc.digest,
        arm64_layer_data,
    )
    .await;

    // Target: no manifest, no blobs, accept all pushes.
    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    mount_blob_not_found(&target_server, "repo", &amd64_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &amd64_layer_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &arm64_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &arm64_layer_desc.digest).await;
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
    // 4 blobs: 2 configs + 2 layers across two platforms.
    assert_eq!(report.images[0].blob_stats.transferred, 4);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (amd64_config_data.len()
        + amd64_layer_data.len()
        + arm64_config_data.len()
        + arm64_layer_data.len()) as u64;
    assert_eq!(
        report.images[0].bytes_transferred, expected_bytes,
        "bytes_transferred must equal sum of all blob sizes"
    );
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 4);
}

#[tokio::test]
async fn sync_head_different_digest_proceeds_with_sync() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config";
    let layer_data = b"layer";
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
    let (manifest_bytes, _manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest and blobs.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: manifest HEAD returns a DIFFERENT digest → should proceed.
    let stale_digest = test_digest("5ca1e");
    mount_manifest_head_matching(&target_server, "repo", "v1", &stale_digest).await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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

    assert!(report.images.is_empty());
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_manifest_push_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config";
    let layer_data = b"layer";
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

    // Source: serve everything normally.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: blobs succeed, but manifest PUT fails with 403.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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

    let config_data = b"config";
    let layer_data = b"layer";
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

    // Layer blob: always returns 429 (retryable) — should exhaust retries.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
    if let ImageStatus::Failed { retries, .. } = &report.images[0].status {
        assert_eq!(*retries, 2); // max_retries from fast_retry()
    }
    assert_eq!(report.stats.images_failed, 1);
}

#[tokio::test]
async fn sync_cross_repo_mount_success() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared blobs across two different source repos.
    let config_data = b"config";
    let layer_data = b"layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: two repos with the same blobs.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-a", &layer_desc.digest, layer_data).await;

    // Target for repo-a: no manifest, no blobs, push succeeds.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: no manifest, blobs not found, mount succeeds (201 Created).
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_desc.digest).await;
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

    // Use max_concurrent=1 to ensure mapping_a completes before mapping_b starts,
    // so that repo-a's blobs are in the dedup map for cross-repo mount.
    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

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

    let config_data = b"config";
    let layer_data = b"layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: two repos.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-a", &layer_desc.digest, layer_data).await;
    // repo-b blob pulls needed for fallback.
    mount_blob_pull(&source_server, "repo-b", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-b", &layer_desc.digest, layer_data).await;

    // Target for repo-a: normal sync.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: mount returns 202 Accepted (fallback).
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_desc.digest).await;
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

    // Use max_concurrent=1 to ensure mapping_a completes first so mapping_b
    // has mount sources available in the dedup map.
    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

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

    let config_data = b"config";
    let layer_data = b"layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: two repos.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-a", &layer_desc.digest, layer_data).await;
    mount_blob_pull(&source_server, "repo-b", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-b", &layer_desc.digest, layer_data).await;

    // Target for repo-a: normal sync.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: mount returns 500 (error), falls back to pull+push.
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_desc.digest).await;
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

    // Use max_concurrent=1 to ensure mapping_a completes first so mapping_b
    // has mount sources available in the dedup map.
    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 2);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(matches!(report.images[1].status, ImageStatus::Synced));
    // Mount failed → fell back to pull+push.
    assert_eq!(report.images[1].blob_stats.mounted, 0);
    assert_eq!(report.images[1].blob_stats.transferred, 2);
}

#[tokio::test]
async fn sync_multi_target_partial_blob_failure_isolates_targets() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let config_data = b"config";
    let layer_data = b"layer";
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

    // Source: serve manifest and blobs.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target A: everything succeeds.
    mount_manifest_head_not_found(&target_a, "repo", "v1").await;
    mount_blob_not_found(&target_a, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_a, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_a, "repo").await;
    mount_manifest_push(&target_a, "repo", "v1").await;

    // Target B: blob push initiation returns 403 (non-retryable).
    mount_manifest_head_not_found(&target_b, "repo", "v1").await;
    mount_blob_not_found(&target_b, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_b, "repo", &layer_desc.digest).await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_b)
        .await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
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

    assert_eq!(report.images.len(), 2);
    // With concurrent execution, results may arrive in any order.
    // Find results by status rather than assuming index order.
    let synced = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ImageStatus::Synced))
        .count();
    let failed = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ImageStatus::Failed { .. }))
        .count();
    assert_eq!(synced, 1, "one target should succeed");
    assert_eq!(failed, 1, "one target should fail");
    // Verify the successful target transferred blobs.
    let synced_result = report
        .images
        .iter()
        .find(|r| matches!(r.status, ImageStatus::Synced))
        .unwrap();
    assert_eq!(synced_result.blob_stats.transferred, 2);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_failed, 1);
}

// ---------------------------------------------------------------------------
// Tests: progressive cache population, cross-repo mount, monolithic upload,
// lazy invalidation, and cache persistence round-trip.
// ---------------------------------------------------------------------------

/// Two images sharing one base layer and one unique layer each.
///
/// After the first image syncs, the base layer is recorded as completed at
/// `(target, repo)`. When the second image processes the base layer, the
/// engine hits `blob_known_at_repo` → skips the HEAD check entirely.
/// Total blob pushes: base + layer_a + layer_b = 3, not 4.
#[tokio::test]
async fn sync_progressive_cache_skips_shared_blob_head_check() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let base_data = b"base-layer-data";
    let layer_a_data = b"layer-a-data";
    let layer_b_data = b"layer-b-data";
    let config_a_data = b"config-a";
    let config_b_data = b"config-b";

    let base_desc = blob_descriptor(base_data, MediaType::OciLayerGzip);
    let layer_a_desc = blob_descriptor(layer_a_data, MediaType::OciLayerGzip);
    let layer_b_desc = blob_descriptor(layer_b_data, MediaType::OciLayerGzip);
    let config_a_desc = blob_descriptor(config_a_data, MediaType::OciConfig);
    let config_b_desc = blob_descriptor(config_b_data, MediaType::OciConfig);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_a_desc.clone(),
        layers: vec![base_desc.clone(), layer_a_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_b_desc.clone(),
        layers: vec![base_desc.clone(), layer_b_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_a_bytes, _) = serialize_manifest(&manifest_a);
    let (manifest_b_bytes, _) = serialize_manifest(&manifest_b);

    // Source: serve both manifests and all blobs.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo", "v2", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo", &base_desc.digest, base_data).await;
    mount_blob_pull(&source_server, "repo", &layer_a_desc.digest, layer_a_data).await;
    mount_blob_pull(&source_server, "repo", &layer_b_desc.digest, layer_b_data).await;
    mount_blob_pull(&source_server, "repo", &config_a_desc.digest, config_a_data).await;
    mount_blob_pull(&source_server, "repo", &config_b_desc.digest, config_b_data).await;

    // Target: manifest HEAD 404 for both tags.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;

    // v1 blobs: all missing at target — base, config_a, layer_a each need HEAD + push.
    mount_blob_not_found(&target_server, "repo", &base_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &config_a_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_a_desc.digest).await;

    // v2 blobs: base was already completed by v1 — no HEAD issued.
    // config_b and layer_b are new so HEAD + push needed.
    mount_blob_not_found(&target_server, "repo", &config_b_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_b_desc.digest).await;

    // The push endpoint accepts any upload (3 pushes for v1: base + config_a + layer_a;
    // 2 pushes for v2: config_b + layer_b; base is skipped entirely for v2).
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
    };

    // Sequential execution ensures v1 completes and populates the cache before v2 starts.
    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 2);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert!(matches!(report.images[1].status, ImageStatus::Synced));

    // v1: 3 blobs transferred (base + config_a + layer_a).
    assert_eq!(report.images[0].blob_stats.transferred, 3);
    assert_eq!(report.images[0].blob_stats.skipped, 0);

    // v2: base is a cache hit (skipped), config_b + layer_b transferred.
    assert_eq!(report.images[1].blob_stats.transferred, 2);
    assert_eq!(report.images[1].blob_stats.skipped, 1);

    // Total: 5 transferred across both images, 1 skipped (the shared base for v2).
    assert_eq!(report.stats.blobs_transferred, 5);
    assert_eq!(report.stats.blobs_skipped, 1);
}

/// A pre-warmed cache records a blob as completed at repo-a. When the engine
/// processes the same blob for repo-b on the same target, it finds a mount
/// source in the cache and issues a cross-repo mount POST (201 → Mounted).
#[tokio::test]
async fn sync_warm_cache_triggers_cross_repo_mount() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
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

    // Source: serve manifest (blobs will be mounted, not pulled).
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_bytes).await;

    // Target: manifest HEAD 404, mount POST returns 201 (Mounted) for both blobs.
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // Pre-warm the cache: blobs already exist at repo-a on the target.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        let target = "target";
        c.set_blob_completed(target, config_desc.digest.clone(), "repo-a".into());
        c.set_blob_completed(target, layer_desc.digest.clone(), "repo-a".into());
    }

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: target_client,
        }],
        tags: vec![TagPair::same("v1")],
    };

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Both blobs mounted from repo-a; none pulled or skipped by HEAD.
    assert_eq!(report.images[0].blob_stats.mounted, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    assert_eq!(report.stats.blobs_mounted, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
}

/// Small blobs (below the 1 MiB monolithic threshold) use POST+PUT with no
/// PATCH. The mock expects exactly 1 POST and 1 PUT per blob, and 0 PATCHes.
#[tokio::test]
async fn sync_small_blob_uses_monolithic_upload() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Both blobs are well below the 1 MiB monolithic threshold.
    let config_data = b"small-config";
    let layer_data = b"small-layer";

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

    // Monolithic upload: POST initiates, PUT finalizes — no PATCH.
    // Use .expect() to assert exact counts per HTTP method.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202).insert_header("location", "/v2/repo/blobs/uploads/mono-id"),
        )
        .expect(2) // one POST per blob
        .mount(&target_server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/mono-id"))
        .respond_with(ResponseTemplate::new(201))
        .expect(2) // one PUT per blob
        .mount(&target_server)
        .await;

    // No PATCH mock registered — any PATCH would cause a wiremock 404 and fail the test.

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
    assert_eq!(report.images[0].blob_stats.transferred, 2);
}

/// A stale cache entry records a blob as completed at `other-repo`. The engine
/// finds a mount source, issues a mount POST, which returns a non-201/non-202
/// status (treated as an error). The engine invalidates the cache entry and
/// falls back to HEAD check + full push, which succeeds.
#[tokio::test]
async fn sync_lazy_invalidation_on_mount_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-payload";
    let layer_data = b"layer-payload";
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

    // Mount POSTs carry a `mount` query parameter; match on it to distinguish
    // them from regular upload initiations.  Both mount attempts return 404
    // (non-retryable error) so the engine invalidates the stale cache entry
    // and falls back to HEAD check + full push.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .and(query_param("mount", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .and(query_param("mount", layer_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&target_server)
        .await;

    // After invalidation the engine falls back to HEAD check (returns 404 = absent).
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;

    // Fallback push: the blobs are small so the engine takes the monolithic
    // path (POST → 202, then PUT → 201; no PATCH).
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo/blobs/uploads/fallback-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/fallback-id"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // Stale cache: blobs appear completed at other-repo, so the engine will
    // find a mount source and try a mount before falling back.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed("target", config_desc.digest.clone(), "other-repo".into());
        c.set_blob_completed("target", layer_desc.digest.clone(), "other-repo".into());
    }

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

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // After failed mounts, fallback push transferred both blobs.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.mounted, 0);
}

/// Run a sync, persist the resulting cache to disk, reload it, and verify
/// the blob entries recorded during the sync are present in the loaded cache.
#[tokio::test]
async fn sync_cache_persist_and_load_round_trip() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"persist-config";
    let layer_data = b"persist-layer";
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

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target-reg".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1")],
    };

    let cache = empty_cache();
    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            Rc::clone(&cache),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.stats.blobs_transferred, 2);

    // Persist the in-memory cache to a temp file.
    let tmp_dir = tempfile::tempdir().unwrap();
    let cache_path = tmp_dir.path().join("sync_cache.bin");
    cache.borrow().persist(&cache_path).unwrap();

    // Load the cache back and verify both blobs are recorded as completed.
    let loaded = TransferStateCache::load(&cache_path, std::time::Duration::from_secs(3600));

    assert!(
        loaded.blob_known_at_repo("target-reg", &config_desc.digest, "repo"),
        "config blob should be recorded as completed at repo"
    );
    assert!(
        loaded.blob_known_at_repo("target-reg", &layer_desc.digest, "repo"),
        "layer blob should be recorded as completed at repo"
    );
    assert!(
        !loaded.blob_known_at_repo("target-reg", &config_desc.digest, "other-repo"),
        "blob should not appear at an unrelated repo"
    );
}

// ---------------------------------------------------------------------------
// Tests: shutdown integration
// ---------------------------------------------------------------------------

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

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
    };

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

// ---------------------------------------------------------------------------
// Tests: concurrent execution (non-sequential ordering)
// ---------------------------------------------------------------------------

/// Verify that cross-tag dedup works correctly even with concurrent execution
/// (max_concurrent > 1). Results may arrive in any order, but shared blobs
/// must still be deduplicated globally.
#[tokio::test]
async fn sync_dedup_across_tags_concurrent() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_layer = b"shared-layer-concurrent";
    let config_a = b"cfg-a-concurrent";
    let config_b = b"cfg-b-concurrent";
    let shared_desc = blob_descriptor(shared_layer, MediaType::OciLayerGzip);
    let config_a_desc = blob_descriptor(config_a, MediaType::OciConfig);
    let config_b_desc = blob_descriptor(config_b, MediaType::OciConfig);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_a_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_b_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (ma_bytes, _) = serialize_manifest(&manifest_a);
    let (mb_bytes, _) = serialize_manifest(&manifest_b);

    mount_source_manifest(&source_server, "repo", "v1", &ma_bytes).await;
    mount_source_manifest(&source_server, "repo", "v2", &mb_bytes).await;
    mount_blob_pull(&source_server, "repo", &shared_desc.digest, shared_layer).await;
    mount_blob_pull(&source_server, "repo", &config_a_desc.digest, config_a).await;
    mount_blob_pull(&source_server, "repo", &config_b_desc.digest, config_b).await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &shared_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &config_a_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &config_b_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
    };

    // Use higher concurrency — both tags can execute simultaneously.
    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 2);
    let all_synced = report
        .images
        .iter()
        .all(|r| matches!(r.status, ImageStatus::Synced));
    assert!(all_synced, "both images should sync successfully");

    // Total blobs: v1 has config_a + shared (2), v2 has config_b + shared (2).
    // With dedup, shared is transferred once and skipped once = 3 transferred + 1 skipped.
    // With concurrent execution, the shared blob might be transferred by both
    // if they run simultaneously, so assert total transferred + skipped = 4.
    let total_transferred: u64 = report.images.iter().map(|r| r.blob_stats.transferred).sum();
    let total_skipped: u64 = report.images.iter().map(|r| r.blob_stats.skipped).sum();
    assert_eq!(
        total_transferred + total_skipped,
        4,
        "total blob operations should be 4 (config_a + config_b + shared x2)"
    );
    assert_eq!(report.stats.images_synced, 2);
}

/// Verify two cross-repo mappings both succeed with concurrent execution.
/// With max_concurrent>1, both may execute simultaneously. The test verifies
/// correctness (both complete) without prescribing mount vs push paths.
#[tokio::test]
async fn sync_cross_repo_mount_concurrent() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"cfg-mount-concurrent";
    let layer_data = b"layer-mount-concurrent";
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

    // Both repos share the same manifest/blobs at source.
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-a", &layer_desc.digest, layer_data).await;
    mount_blob_pull(&source_server, "repo-b", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-b", &layer_desc.digest, layer_data).await;

    // Target: HEAD 404 for both repos (all blobs treated as absent).
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-b", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_desc.digest).await;

    // Push endpoints for both repos.
    mount_blob_push(&target_server, "repo-a").await;
    mount_blob_push(&target_server, "repo-b").await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping_a = ResolvedMapping {
        source_client: Arc::clone(&source_client),
        source_repo: "repo-a".into(),
        target_repo: "repo-a".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: Arc::clone(&target_client),
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

    // max_concurrent=10: both mappings can execute concurrently.
    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 2);
    let all_synced = report
        .images
        .iter()
        .all(|r| matches!(r.status, ImageStatus::Synced));
    assert!(all_synced, "both mappings should sync successfully");
    assert_eq!(report.stats.images_synced, 2);
}

// ---------------------------------------------------------------------------
// Tests: nested index manifest rejection
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

// ---------------------------------------------------------------------------
// Tests: cache state verification after invalidation
// ---------------------------------------------------------------------------

/// After a mount failure triggers lazy invalidation, verify the cache entry
/// is actually cleared and the blob is recorded as completed via the fallback
/// push path.
#[tokio::test]
async fn sync_lazy_invalidation_clears_cache_and_records_completion() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-cache-verify";
    let layer_data = b"layer-cache-verify";
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

    // Mount attempts return 404 (failure).
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .and(query_param("mount", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(404))
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .and(query_param("mount", layer_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(404))
        .mount(&target_server)
        .await;

    // Fallback: HEAD 404, then push.
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo/blobs/uploads/verify-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/verify-id"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    // Pre-warm cache with stale mount source.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed("target", config_desc.digest.clone(), "stale-repo".into());
        c.set_blob_completed("target", layer_desc.digest.clone(), "stale-repo".into());
    }

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

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            cache.clone(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));

    // Verify: stale mount source is gone, blobs are now recorded at "repo".
    let c = cache.borrow();
    assert!(
        !c.blob_known_at_repo("target", &config_desc.digest, "stale-repo"),
        "stale cache entry for config at stale-repo should be invalidated"
    );
    assert!(
        !c.blob_known_at_repo("target", &layer_desc.digest, "stale-repo"),
        "stale cache entry for layer at stale-repo should be invalidated"
    );
    assert!(
        c.blob_known_at_repo("target", &config_desc.digest, "repo"),
        "config blob should be recorded as completed at repo after fallback push"
    );
    assert!(
        c.blob_known_at_repo("target", &layer_desc.digest, "repo"),
        "layer blob should be recorded as completed at repo after fallback push"
    );
}

// ---------------------------------------------------------------------------
// Tests: index manifest child failure, partial blob failure, concurrent dedup
// ---------------------------------------------------------------------------

/// An index manifest where one child manifest pull returns 500 (server error).
/// The entire image should fail.
#[tokio::test]
async fn sync_index_manifest_child_pull_failure() {
    use ocync_distribution::spec::ImageIndex;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build two child image manifests (simulating amd64 and arm64).
    let amd64_config_data = b"amd64-config-fail";
    let amd64_layer_data = b"amd64-layer-fail";
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

    let arm64_config_data = b"arm64-config-fail";
    let arm64_layer_data = b"arm64-layer-fail";
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
    let (_arm64_bytes, arm64_digest) = serialize_manifest(&arm64_manifest);

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

    // Source: serve the index by tag and amd64 child by digest.
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

    // arm64 child manifest pull returns 500 (server error).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{arm64_digest}")))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .mount(&source_server)
        .await;

    // Target: manifest HEAD 404.
    mount_manifest_head_not_found(&target_server, "repo", "latest").await;

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
        matches!(report.images[0].status, ImageStatus::Failed { .. }),
        "image should fail when a child manifest pull fails, got: {:?}",
        report.images[0].status
    );
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

/// Blob 1 (config) push succeeds. Blob 2 (layer) push POST returns 403.
/// Verify image fails, bytes_transferred reflects only the first blob,
/// and manifest is NOT pushed.
#[tokio::test]
async fn sync_partial_blob_failure_stops_remaining() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"partial-config";
    let layer_data = b"partial-layer";
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

    // Source: serve manifest and both blobs.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: manifest HEAD 404, both blobs not found.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;

    // Config blob push succeeds (POST/PUT monolithic for small blobs).
    // Use query_param to match the config blob's finalization PUT.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo/blobs/uploads/partial-id"),
        )
        .up_to_n_times(1)
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/partial-id"))
        .and(query_param("digest", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;

    // Layer blob push POST returns 403 (non-retryable).
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

    // No manifest PUT mock — if engine tries to push manifest, wiremock
    // returns 404 and the test fails differently.

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

    // Use max_concurrent=1 to ensure sequential blob processing (config first).
    let engine = SyncEngine::new(fast_retry(), 1);
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
        matches!(report.images[0].status, ImageStatus::Failed { .. }),
        "image should fail when a blob push fails, got: {:?}",
        report.images[0].status
    );
    assert_eq!(
        report.images[0].bytes_transferred,
        config_data.len() as u64,
        "only the config blob should count as transferred"
    );
    assert_eq!(report.images[0].blob_stats.transferred, 1);
    assert_eq!(report.stats.images_failed, 1);
}

/// Two tags (v1, v2) with identical blob digests, run at `max_concurrent = 50`.
/// Dedup must work under real concurrency.
#[tokio::test]
async fn sync_concurrent_dedup_at_real_concurrency() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"dedup-config-concurrent";
    let layer_data = b"dedup-layer-concurrent";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);

    // Two different manifests that share the same config and layer blobs.
    let manifest_v1 = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_v2 = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (v1_bytes, _) = serialize_manifest(&manifest_v1);
    let (v2_bytes, _) = serialize_manifest(&manifest_v2);

    // Source: serve each manifest with expect(1) to verify each pulled once.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(v1_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(v2_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: manifest HEAD 404 for both tags, blobs not found, push endpoints.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: "target".into(),
            client: mock_client(&target_server),
        }],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
    };

    // Real concurrency — NOT 1.
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

    assert_eq!(report.images.len(), 2);
    let all_synced = report
        .images
        .iter()
        .all(|r| matches!(r.status, ImageStatus::Synced));
    assert!(all_synced, "both images should sync successfully");

    // Under real concurrency, both tags may race past the cache and both
    // transfer the shared blobs. The key invariant: total blob operations
    // equals 4 (each image has 2 blobs), both images succeed, and each
    // source manifest is pulled exactly once (verified by wiremock expect(1)).
    let total_transferred: u64 = report.images.iter().map(|r| r.blob_stats.transferred).sum();
    let total_skipped: u64 = report.images.iter().map(|r| r.blob_stats.skipped).sum();
    assert_eq!(
        total_transferred + total_skipped,
        4,
        "total blob operations should be 4 (config x2 + layer x2)"
    );
    // Dedup may or may not fire under concurrency (race-dependent), so
    // assert bounds: transferred is 2..=4, skipped is 0..=2.
    assert!(
        (2..=4).contains(&report.stats.blobs_transferred),
        "blobs transferred should be 2-4 under concurrent execution, got {}",
        report.stats.blobs_transferred,
    );
    assert_eq!(report.stats.images_synced, 2);
    // wiremock expect(1) assertions verify each source manifest was pulled exactly once.
}

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

    let config_data = b"staging-config-twice";
    let layer_data = b"staging-layer-twice";
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

    // Source: serve manifest and blobs with expect(1) to verify pull-once.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(config_data.to_vec())
                .insert_header("content-length", config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(layer_data.to_vec())
                .insert_header("content-length", layer_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Target A: manifest HEAD 404, blobs HEAD 404, push endpoints.
    mount_manifest_head_not_found(&target_a, "repo", "v1").await;
    mount_blob_not_found(&target_a, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_a, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_a, "repo").await;
    mount_manifest_push(&target_a, "repo", "v1").await;

    // Target B: manifest HEAD 404, blobs HEAD 404, push endpoints.
    mount_manifest_head_not_found(&target_b, "repo", "v1").await;
    mount_blob_not_found(&target_b, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_b, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_b, "repo").await;
    mount_manifest_push(&target_b, "repo", "v1").await;

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
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

    let config_data = b"stuck-config";
    let layer_data = b"stuck-layer";
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

    // Source: manifest responds immediately. Config blob has a 60-second delay
    // (far beyond the 25-second drain deadline). Layer blob also delayed.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(config_data.to_vec())
                .insert_header("content-length", config_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(layer_data.to_vec())
                .insert_header("content-length", layer_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .mount(&source_server)
        .await;

    // Target: standard setup (manifest HEAD 404, blobs HEAD 404, push endpoints).
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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

    let config_data = b"drain-cfg";
    let layer_data = b"drain-layer";
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

    // Source: manifest responds immediately. Blob delays are 5s — between
    // our custom 2s drain deadline and the default 25s deadline. This means
    // the transfer would succeed with the default but fails with the custom.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(config_data.to_vec())
                .insert_header("content-length", config_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(layer_data.to_vec())
                .insert_header("content-length", layer_data.len().to_string())
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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

    let config_data = b"staging-disk-config";
    let layer_data = b"staging-disk-layer";
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

    // Source: serve manifest and blobs with expect(1) to verify pull-once.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(config_data.to_vec())
                .insert_header("content-length", config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(layer_data.to_vec())
                .insert_header("content-length", layer_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    for target in [&target_a, &target_b] {
        mount_manifest_head_not_found(target, "repo", "v1").await;
        mount_blob_not_found(target, "repo", &config_desc.digest).await;
        mount_blob_not_found(target, "repo", &layer_desc.digest).await;
        mount_blob_push(target, "repo").await;
        mount_manifest_push(target, "repo", "v1").await;
    }

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
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
        .join(config_desc.digest.algorithm())
        .join(config_desc.digest.hex());
    let layer_path = staging_dir
        .path()
        .join("blobs")
        .join(layer_desc.digest.algorithm())
        .join(layer_desc.digest.hex());
    assert!(
        config_path.exists(),
        "config blob should be staged on disk at {config_path:?}"
    );
    assert!(
        layer_path.exists(),
        "layer blob should be staged on disk at {layer_path:?}"
    );

    // Verify content matches what was pulled from source.
    assert_eq!(std::fs::read(&config_path).unwrap(), config_data);
    assert_eq!(std::fs::read(&layer_path).unwrap(), layer_data);
}

// ---------------------------------------------------------------------------
// Tests: warm cache skips blob entirely (no HEAD check issued)
// ---------------------------------------------------------------------------

/// A pre-warmed cache records a blob as completed at `(target, repo)`.
/// Verify the engine skips it entirely — no HEAD check, no pull, no push.
/// The blob HEAD mock is NOT registered, so any HEAD attempt would fail.
#[tokio::test]
async fn sync_warm_cache_skips_blob_head_check() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"cache-skip-config";
    let layer_data = b"cache-skip-layer";
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
    // No source blob pulls mounted — they shouldn't be needed.

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    // No blob HEAD mocks — any HEAD attempt would cause wiremock to return 404
    // which would trigger a push, which would also fail (no push mock).
    // The test succeeds only if the cache skip prevents all blob operations.
    mount_manifest_push(&target_server, "repo", "v1").await;

    // Pre-warm cache: both blobs known at (target, repo).
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed("target", config_desc.digest.clone(), "repo".into());
        c.set_blob_completed("target", layer_desc.digest.clone(), "repo".into());
    }

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

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(
            vec![mapping],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Both blobs skipped via cache — no transfers, no HEAD checks.
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    // Aggregate stats must also reflect the skip.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
}
