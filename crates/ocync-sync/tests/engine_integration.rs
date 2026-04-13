//! Integration tests for `SyncEngine` using mock HTTP servers.

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ocync_distribution::spec::{
    Descriptor, ImageIndex, ImageManifest, MediaType, Platform, PlatformFilter, RepositoryName,
};
use ocync_distribution::{BatchBlobChecker, Digest, RegistryClientBuilder};
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{RegistryName, ResolvedMapping, SyncEngine, TagPair, TargetEntry};
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

/// Shorthand for a [`TargetEntry`] without a batch checker.
fn target_entry(name: &str, client: Arc<ocync_distribution::RegistryClient>) -> TargetEntry {
    TargetEntry {
        name: RegistryName::new(name),
        client,
        batch_checker: None,
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

/// Mock implementation of [`BatchBlobChecker`] for testing the batch pre-population path.
///
/// Returns a pre-configured set of existing digests, filtered by input.
/// Verifies the caller passes the expected repository name (per mock
/// contract fidelity — a bug where the engine passes the wrong repo
/// would be invisible without this check).
struct MockBatchChecker {
    /// Expected repository name — panics if the caller passes a different repo.
    expected_repo: String,
    /// Set of digests that the mock reports as existing at the target.
    existing: HashSet<Digest>,
    /// Tracks how many times `check_blob_existence` was called.
    call_count: Arc<AtomicUsize>,
}

impl MockBatchChecker {
    fn new(expected_repo: &str, existing: HashSet<Digest>) -> (Self, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (
            Self {
                expected_repo: expected_repo.to_owned(),
                existing,
                call_count: Arc::clone(&count),
            },
            count,
        )
    }
}

impl BatchBlobChecker for MockBatchChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<HashSet<Digest>, ocync_distribution::Error>> + 'a>>
    {
        assert_eq!(
            repo.as_str(),
            self.expected_repo,
            "mock: batch checker called with wrong repo"
        );
        Box::pin(async {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(digests
                .iter()
                .filter(|d| self.existing.contains(d))
                .cloned()
                .collect())
        })
    }
}

/// Mock batch checker that always returns an error (for testing fallback path).
///
/// Verifies the caller passes the expected repository name.
struct FailingBatchChecker {
    /// Expected repository name.
    expected_repo: String,
    /// Tracks how many times `check_blob_existence` was called.
    call_count: Arc<AtomicUsize>,
}

impl FailingBatchChecker {
    fn new(expected_repo: &str) -> (Self, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (
            Self {
                expected_repo: expected_repo.to_owned(),
                call_count: Arc::clone(&count),
            },
            count,
        )
    }
}

impl BatchBlobChecker for FailingBatchChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        _digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<HashSet<Digest>, ocync_distribution::Error>> + 'a>>
    {
        assert_eq!(
            repo.as_str(),
            self.expected_repo,
            "mock: failing batch checker called with wrong repo"
        );
        Box::pin(async {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Err(ocync_distribution::Error::Other(
                "batch API unavailable".into(),
            ))
        })
    }
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
        targets: vec![target_entry("target-reg", target_client)],
        tags: vec![TagPair::same("latest")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
        platforms: None,
        skip_existing: false,
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
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::retag("latest", "stable")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("latest")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client.clone())],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
    };

    // Second mapping: repo-b should mount from repo-a.
    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client.clone())],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
    };

    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client.clone())],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
    };

    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
/// Total blob pushes: base + `layer_a` + `layer_b` = 3, not 4.
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
/// PATCH. The mock expects exactly 1 POST and 1 PUT per blob, and 0 PATCH requests.
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target-reg", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        loaded.blob_known_at_repo(
            "target-reg",
            &config_desc.digest,
            &RepositoryName::from("repo")
        ),
        "config blob should be recorded as completed at repo"
    );
    assert!(
        loaded.blob_known_at_repo(
            "target-reg",
            &layer_desc.digest,
            &RepositoryName::from("repo")
        ),
        "layer blob should be recorded as completed at repo"
    );
    assert!(
        !loaded.blob_known_at_repo(
            "target-reg",
            &config_desc.digest,
            &RepositoryName::from("other-repo")
        ),
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
/// (`max_concurrent` > 1). Results may arrive in any order, but shared blobs
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
        platforms: None,
        skip_existing: false,
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
/// With `max_concurrent` > 1, both may execute simultaneously. The test verifies
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
        targets: vec![target_entry("target", Arc::clone(&target_client))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
    };
    let mapping_b = ResolvedMapping {
        source_client,
        source_repo: "repo-b".into(),
        target_repo: "repo-b".into(),
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        !c.blob_known_at_repo(
            "target",
            &config_desc.digest,
            &RepositoryName::from("stale-repo")
        ),
        "stale cache entry for config at stale-repo should be invalidated"
    );
    assert!(
        !c.blob_known_at_repo(
            "target",
            &layer_desc.digest,
            &RepositoryName::from("stale-repo")
        ),
        "stale cache entry for layer at stale-repo should be invalidated"
    );
    assert!(
        c.blob_known_at_repo("target", &config_desc.digest, &RepositoryName::from("repo")),
        "config blob should be recorded as completed at repo after fallback push"
    );
    assert!(
        c.blob_known_at_repo("target", &layer_desc.digest, &RepositoryName::from("repo")),
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("latest")],
        platforms: None,
        skip_existing: false,
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
/// Verify image fails, `bytes_transferred` reflects only the first blob,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
        platforms: None,
        skip_existing: false,
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
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

// ---------------------------------------------------------------------------
// Batch blob checker tests
// ---------------------------------------------------------------------------

/// Batch checker reports all blobs exist -- per-blob HEAD is bypassed entirely.
///
/// The test has NO wiremock mock for blob HEAD endpoints. If the engine falls
/// through to per-blob HEAD (Step 3), wiremock returns 404, the engine would
/// attempt a pull+push for the blob, and the test would fail because no blob
/// pull/push endpoints are mocked either.
#[tokio::test]
async fn sync_batch_checker_all_blobs_exist_skips_head() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-batch";
    let layer_data = b"layer-batch";
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

    // Source: serve manifest only (no blob endpoints -- they shouldn't be needed).
    // Use different repo names to ensure the engine passes target_repo (not
    // source_repo) to the batch checker.
    mount_source_manifest(&source_server, "src/nginx", "v1", &manifest_bytes).await;

    // Target: manifest HEAD 404 (image needs sync), manifest PUT (for pushing).
    // Blob HEAD endpoints: expect(0) -- batch pre-population must prevent all
    // per-blob HEAD checks. This is the explicit negative assertion per CLAUDE.md.
    mount_manifest_head_not_found(&target_server, "tgt/nginx", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/tgt/nginx/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/tgt/nginx/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "tgt/nginx", "v1").await;

    // Batch checker: both blobs exist. Expected repo is target repo, not source.
    let existing = HashSet::from([config_desc.digest.clone(), layer_desc.digest.clone()]);
    let (checker, batch_call_count) = MockBatchChecker::new("tgt/nginx", existing);

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "src/nginx".into(),
        target_repo: "tgt/nginx".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: target_client,
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Exactly 1 batch call was made.
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be called exactly once"
    );
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Both blobs skipped via batch-populated cache (Step 1 cache hit).
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
}

/// Batch checker reports some blobs missing -- only missing blobs are transferred.
///
/// Config and layer2 exist (batch reports true), layer1 is missing (batch reports
/// false). Only layer1 should be pulled from source and pushed to target.
/// No blob HEAD endpoints are mocked -- the batch check handles all existence
/// decisions.
#[tokio::test]
async fn sync_batch_checker_partial_existence_transfers_missing() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-partial";
    let layer1_data = b"layer1-missing";
    let layer2_data = b"layer2-exists";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer1_desc = blob_descriptor(layer1_data, MediaType::OciLayerGzip);
    let layer2_desc = blob_descriptor(layer2_data, MediaType::OciLayerGzip);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer1_desc.clone(), layer2_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest and the missing blob (layer1).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    // Only layer1 should be pulled -- use expect(1) to verify.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer1_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(layer1_data.to_vec())
                .insert_header("content-length", layer1_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Target: manifest HEAD 404, blob push for layer1, manifest PUT.
    // NO blob HEAD mocked (batch check handles existence).
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;

    // Blob push endpoints for layer1.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo/blobs/uploads/upload-id"),
        )
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v2/repo/blobs/uploads/upload-id"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo/blobs/uploads/upload-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/upload-id"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    // Batch checker: config and layer2 exist, layer1 missing.
    let existing = HashSet::from([config_desc.digest.clone(), layer2_desc.digest.clone()]);
    let (checker, batch_call_count) = MockBatchChecker::new("repo", existing);

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: target_client,
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Exactly 1 batch call.
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be called exactly once"
    );
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // 2 blobs skipped (config + layer2), 1 transferred (layer1).
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 1);
    assert_eq!(
        report.images[0].bytes_transferred,
        layer1_data.len() as u64,
        "only layer1 bytes should be transferred"
    );
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 1);
    assert_eq!(report.stats.bytes_transferred, layer1_data.len() as u64);
    // wiremock expect(1) assertions on source pull and target push verify
    // exactly 1 blob was pulled and 1 was pushed (enforced on MockServer drop).
}

/// Without a batch checker, per-blob HEAD is used for existence checks.
///
/// Same image setup as the batch tests, but `batch_checker: None`. All 3 blobs
/// are checked via HEAD (returning 200 = exists) and skipped. This proves the
/// fallback path works and hasn't regressed.
#[tokio::test]
async fn sync_no_batch_checker_falls_back_to_per_blob_head() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-head";
    let layer1_data = b"layer1-head";
    let layer2_data = b"layer2-head";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer1_desc = blob_descriptor(layer1_data, MediaType::OciLayerGzip);
    let layer2_desc = blob_descriptor(layer2_data, MediaType::OciLayerGzip);
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer1_desc.clone(), layer2_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest only (no blobs needed -- all exist at target).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD 404, all 3 blobs return 200 on HEAD, manifest PUT.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;

    // Blob HEAD: use expect(1) per blob to verify exactly 1 HEAD per blob.
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer1_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer2_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_server)
        .await;

    mount_manifest_push(&target_server, "repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![target_entry("target", target_client)],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
    // All 3 blobs skipped via per-blob HEAD (Step 3).
    assert_eq!(report.images[0].blob_stats.skipped, 3);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 3);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
    // wiremock expect(1) per blob HEAD verifies exactly 3 HEAD requests were made
    // (enforced on MockServer drop).
}

/// Batch checker fails -- engine falls back to per-blob HEAD for all blobs.
///
/// Proves the bridge between batch failure and per-blob HEAD fallback works
/// end-to-end. The batch checker is called (and fails), then all blobs are
/// checked via individual HEAD requests.
#[tokio::test]
async fn sync_batch_checker_failure_falls_back_to_per_blob_head() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-fail";
    let layer_data = b"layer-fail";
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

    // Source: serve manifest only (no blob endpoints -- all exist at target).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD 404, blob HEADs return 200 (exist), manifest PUT.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;

    // Blob HEAD endpoints -- these MUST be called when batch fails.
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_server)
        .await;

    mount_manifest_push(&target_server, "repo", "v1").await;

    // Batch checker that always fails.
    let (checker, batch_call_count) = FailingBatchChecker::new("repo");

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: target_client,
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Batch checker was called (and failed).
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be attempted even though it fails"
    );
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Both blobs discovered via per-blob HEAD fallback (Step 3).
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
    // wiremock expect(1) per blob HEAD verifies the per-blob fallback path
    // was taken (enforced on MockServer drop).
}

/// Multi-target with independent batch checkers per target.
///
/// Target A: batch reports all blobs exist (both skipped).
/// Target B: batch reports config exists, layer missing (1 transferred).
/// Proves batch checkers are per-target and stats are tracked independently.
#[tokio::test]
async fn sync_batch_checker_multi_target_independent_checkers() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    let config_data = b"config-multi";
    let layer_data = b"layer-multi";
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

    // Source: manifest pulled exactly once (pull-once fan-out invariant).
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Layer blob served for target B's transfer.
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

    // Target A: batch says all exist -- no blob endpoints needed.
    mount_manifest_head_not_found(&target_a_server, "repo", "v1").await;
    mount_manifest_push(&target_a_server, "repo", "v1").await;

    // Target B: batch says config exists, layer missing -- need blob push.
    mount_manifest_head_not_found(&target_b_server, "repo", "v1").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202).insert_header("location", "/v2/repo/blobs/uploads/upload-b"),
        )
        .expect(1)
        .mount(&target_b_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v2/repo/blobs/uploads/upload-b"))
        .respond_with(
            ResponseTemplate::new(202).insert_header("location", "/v2/repo/blobs/uploads/upload-b"),
        )
        .mount(&target_b_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/upload-b"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&target_b_server)
        .await;
    mount_manifest_push(&target_b_server, "repo", "v1").await;

    // Batch checkers with different responses per target.
    let existing_a = HashSet::from([config_desc.digest.clone(), layer_desc.digest.clone()]);
    let (checker_a, count_a) = MockBatchChecker::new("repo", existing_a);

    let existing_b = HashSet::from([config_desc.digest.clone()]);
    let (checker_b, count_b) = MockBatchChecker::new("repo", existing_b);

    let source_client = mock_client(&source_server);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![
            TargetEntry {
                name: RegistryName::new("target-a"),
                client: mock_client(&target_a_server),
                batch_checker: Some(Rc::new(checker_a)),
            },
            TargetEntry {
                name: RegistryName::new("target-b"),
                client: mock_client(&target_b_server),
                batch_checker: Some(Rc::new(checker_b)),
            },
        ],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Each checker called exactly once.
    assert_eq!(
        count_a.load(Ordering::Relaxed),
        1,
        "target-a batch checker must be called once"
    );
    assert_eq!(
        count_b.load(Ordering::Relaxed),
        1,
        "target-b batch checker must be called once"
    );

    // 2 images (1 tag x 2 targets).
    assert_eq!(report.images.len(), 2);

    // Distinguish results by blob stats: target A has skipped=2/transferred=0,
    // target B has skipped=1/transferred=1.
    let all_skipped = report
        .images
        .iter()
        .find(|r| r.blob_stats.skipped == 2 && r.blob_stats.transferred == 0);
    let partial = report
        .images
        .iter()
        .find(|r| r.blob_stats.skipped == 1 && r.blob_stats.transferred == 1);

    let result_a = all_skipped.expect("target-a result (all skipped) not found");
    let result_b = partial.expect("target-b result (partial transfer) not found");

    assert!(matches!(result_a.status, ImageStatus::Synced));
    assert_eq!(result_a.bytes_transferred, 0);

    assert!(matches!(result_b.status, ImageStatus::Synced));
    assert_eq!(
        result_b.bytes_transferred,
        layer_data.len() as u64,
        "only layer bytes should be transferred to target-b"
    );

    // Aggregate stats across both targets.
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_skipped, 3); // 2 from A + 1 from B
    assert_eq!(report.stats.blobs_transferred, 1); // layer to B
    assert_eq!(report.stats.bytes_transferred, layer_data.len() as u64);
    // wiremock expect(1) on source blob GET and target-b blob POST/PUT verify
    // exactly 1 blob was pulled and pushed (enforced on MockServer drop).
}

/// Batch checker succeeds but reports zero blobs as existing -- all blobs
/// must be pulled from source and pushed to target.
///
/// Verifies the path where the batch API works correctly but nothing exists
/// at the target yet. Every blob falls through from cache miss (Step 1) to
/// HEAD (Step 3) to pull+push (Step 4). The batch call count is asserted
/// to prove the optimization was attempted.
#[tokio::test]
async fn sync_batch_checker_empty_result_transfers_all() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-empty-batch";
    let layer_data = b"layer-empty-batch";
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

    // Source: manifest + both blobs with expect(1) to verify pull-once.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
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

    // Target: manifest HEAD 404, blob HEAD expect(1) per blob (fallback from
    // empty batch result), blob push, manifest push.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    // Batch checker: empty set -- nothing exists at target.
    let (checker, batch_call_count) = MockBatchChecker::new("repo", HashSet::new());

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Batch was called (and returned empty).
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be called even when nothing exists"
    );
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // All blobs transferred (none existed).
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(
        report.images[0].bytes_transferred,
        (config_data.len() + layer_data.len()) as u64,
    );
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 0);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(
        report.stats.bytes_transferred,
        (config_data.len() + layer_data.len()) as u64,
    );
}

/// Mixed batch/no-batch multi-target: one ECR target with a batch checker,
/// one non-ECR target without. Both see the same image.
///
/// Target A: has batch checker reporting both blobs exist (both skipped).
/// Target B: no batch checker -- uses per-blob HEAD (both exist, both skipped).
/// Proves batch checkers are per-target and the absence of a checker on one
/// target does not affect the other.
#[tokio::test]
async fn sync_mixed_batch_and_no_batch_multi_target() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    let config_data = b"config-mixed";
    let layer_data = b"layer-mixed";
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

    // Target A (ECR with batch): manifest HEAD 404, explicit expect(0) on blob HEAD,
    // manifest PUT. Blob HEADs must NOT be called because batch handles it.
    mount_manifest_head_not_found(&target_a_server, "repo", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_a_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_a_server)
        .await;
    mount_manifest_push(&target_a_server, "repo", "v1").await;

    // Target B (no batch): manifest HEAD 404, per-blob HEAD expect(1), manifest PUT.
    mount_manifest_head_not_found(&target_b_server, "repo", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_b_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .expect(1)
        .mount(&target_b_server)
        .await;
    mount_manifest_push(&target_b_server, "repo", "v1").await;

    // Batch checker for target A only.
    let existing = HashSet::from([config_desc.digest.clone(), layer_desc.digest.clone()]);
    let (checker, batch_call_count) = MockBatchChecker::new("repo", existing);

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![
            TargetEntry {
                name: RegistryName::new("target-a"),
                client: mock_client(&target_a_server),
                batch_checker: Some(Rc::new(checker)),
            },
            target_entry("target-b", mock_client(&target_b_server)),
        ],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Batch checker called exactly once (for target A only).
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be called once for the ECR target"
    );

    // 2 images (1 tag x 2 targets), both synced with all blobs skipped.
    assert_eq!(report.images.len(), 2);
    for img in &report.images {
        assert!(matches!(img.status, ImageStatus::Synced));
        assert_eq!(img.blob_stats.skipped, 2);
        assert_eq!(img.blob_stats.transferred, 0);
        assert_eq!(img.bytes_transferred, 0);
    }

    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_skipped, 4); // 2 per target
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
    // wiremock expect(0) on target-a blob HEAD and expect(1) on target-b blob HEAD
    // verify that batch bypasses HEAD on A but not B (enforced on MockServer drop).
}

/// Multi-tag with batch checker: exercises `TargetEntry::Clone` with
/// `batch_checker: Some(...)` across tag iterations.
///
/// The engine clones `TargetEntry` per tag at `mapping.targets.clone()`.
/// With two tags sharing the same batch checker `Rc`, the cloned entry must
/// point to the same checker (shared call count). This test asserts
/// `call_count == 2` (one batch call per tag), proving the `Rc` was properly
/// cloned, not reconstructed.
#[tokio::test]
async fn sync_batch_checker_multi_tag_shares_rc() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-multitag";
    let layer_data = b"layer-multitag";
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

    // Source: serve manifest for both tags with expect(1) per tag.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Target: both tags need sync (HEAD 404), both can be pushed.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    // Blob HEAD expect(0): batch handles existence, HEAD must not be called.
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    // Batch checker: both blobs exist. Shared Rc across tags.
    let existing = HashSet::from([config_desc.digest.clone(), layer_desc.digest.clone()]);
    let (checker, batch_call_count) = MockBatchChecker::new("repo", existing);

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("v1"), TagPair::same("v2")],
        platforms: None,
        skip_existing: false,
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

    // Batch checker called twice (once per tag) via the cloned Rc.
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        2,
        "batch checker must be called once per tag, sharing the Rc"
    );

    // 2 images (2 tags x 1 target), both synced with all blobs skipped.
    assert_eq!(report.images.len(), 2);
    for img in &report.images {
        assert!(matches!(img.status, ImageStatus::Synced));
        assert_eq!(img.blob_stats.skipped, 2);
        assert_eq!(img.blob_stats.transferred, 0);
        assert_eq!(img.bytes_transferred, 0);
    }

    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_skipped, 4); // 2 per tag
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
}

/// Batch checker with an index manifest (multi-platform image).
///
/// Verifies the batch check works when blobs come from multiple child manifests
/// (the primary use case: Chainguard multi-arch → ECR). The batch checker reports
/// all 4 blobs (2 configs + 2 layers across amd64/arm64) as existing. No source
/// blob pulls or target blob HEADs should occur.
#[tokio::test]
async fn sync_batch_checker_index_manifest_all_exist() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build two child image manifests (amd64 and arm64).
    let amd64_config_data = b"amd64-config-batch";
    let amd64_layer_data = b"amd64-layer-batch";
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

    let arm64_config_data = b"arm64-config-batch";
    let arm64_layer_data = b"arm64-layer-batch";
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
                .set_body_bytes(index_bytes)
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
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
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{arm64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source: NO blob endpoints — batch reports all exist, no pulls needed.

    // Target: manifest HEAD 404, blob HEAD expect(0) for all 4 blobs.
    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", amd64_config_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", amd64_layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", arm64_config_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", arm64_layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;

    // Accept child manifest pushes (by digest) and index push (by tag).
    // Use expect(1) to verify exactly 3 manifest pushes occur.
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{amd64_digest}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{arm64_digest}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&target_server)
        .await;

    // Batch checker: all 4 blobs exist.
    let existing = HashSet::from([
        amd64_config_desc.digest.clone(),
        amd64_layer_desc.digest.clone(),
        arm64_config_desc.digest.clone(),
        arm64_layer_desc.digest.clone(),
    ]);
    let (checker, batch_call_count) = MockBatchChecker::new("repo", existing);

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("latest")],
        platforms: None,
        skip_existing: false,
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

    // Exactly 1 batch call for the index image.
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be called exactly once for index manifest"
    );
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // All 4 blobs skipped (2 per child manifest).
    assert_eq!(report.images[0].blob_stats.skipped, 4);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 4);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
    // wiremock expect(0) on all blob HEADs and expect(1) on source index manifest
    // verify the batch path was used exclusively (enforced on MockServer drop).
}

/// Batch checker with a pre-warmed cache: cache already knows some blobs exist.
///
/// The cache has blob A at the target repo (direct match, Step 1 cache hit).
/// The batch checker also reports blob A as existing (redundant). Blob B is
/// reported by batch as existing but not in the cache. This verifies:
/// 1. Cache entries from prior syncs still work alongside batch checking.
/// 2. Batch pre-population correctly adds entries for blobs the cache didn't know.
/// 3. No HEAD checks or transfers occur for any blob.
#[tokio::test]
async fn sync_batch_checker_with_prewarmed_cache() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-warm";
    let layer_data = b"layer-warm";
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

    // Source: serve manifest only (no blob endpoints needed).
    // expect(1) verifies the manifest is pulled exactly once.
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

    // Target: manifest HEAD 404, blob HEAD expect(0), manifest PUT expect(1).
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&target_server)
        .await;

    // Pre-warm cache: config blob already known at the target repo.
    // Layer blob is NOT in the cache — only batch reports it.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_exists("target", config_desc.digest.clone(), "repo".into());
    }

    // Batch checker: both blobs exist (config is redundant with cache).
    let existing = HashSet::from([config_desc.digest.clone(), layer_desc.digest.clone()]);
    let (checker, batch_call_count) = MockBatchChecker::new("repo", existing);

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![TargetEntry {
            name: RegistryName::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
        }],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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

    // Batch was called even though some blobs were in the cache.
    assert_eq!(
        batch_call_count.load(Ordering::Relaxed),
        1,
        "batch checker must be called even with pre-warmed cache"
    );
    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Both blobs skipped: config from cache (Step 1), layer from batch
    // pre-population (also Step 1 cache hit after batch populates it).
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
    // wiremock expect(0) on blob HEADs verifies no fallback path was used.
}

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

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("latest")],
        platforms: Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]),
        skip_existing: false,
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
// skip_existing tests
// ---------------------------------------------------------------------------

/// When `skip_existing` is true, a target HEAD returning any manifest (even
/// with a different digest) causes the image to be skipped.
#[tokio::test]
async fn sync_skip_existing_skips_without_digest_comparison() {
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

    // Source: serve manifest (will be pulled during discovery).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: HEAD returns 200 with a DIFFERENT digest -- normally would sync.
    let stale_digest = test_digest("d1ff");
    mount_manifest_head_matching(&target_server, "repo", "v1", &stale_digest).await;

    // No blob endpoints needed -- skip_existing should prevent any blob work.
    // If the engine incorrectly proceeds to sync, it will fail on missing
    // blob endpoints.

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: true,
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
        matches!(
            report.images[0].status,
            ImageStatus::Skipped {
                reason: SkipReason::SkipExisting,
            }
        ),
        "expected SkipExisting, got {:?}",
        report.images[0].status
    );
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    // Aggregate stats.
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.stats.blobs_transferred, 0);
}

/// When `skip_existing` is false (default), a target HEAD returning a different
/// digest triggers a full sync.
#[tokio::test]
async fn sync_skip_existing_false_syncs_on_different_digest() {
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
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target: HEAD returns 200 with a DIFFERENT digest -- should proceed to sync.
    let stale_digest = test_digest("d1ff");
    mount_manifest_head_matching(&target_server, "repo", "v1", &stale_digest).await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![target_entry("target", mock_client(&target_server))],
        tags: vec![TagPair::same("v1")],
        platforms: None,
        skip_existing: false,
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
        matches!(report.images[0].status, ImageStatus::Synced),
        "expected Synced, got {:?}",
        report.images[0].status
    );
    // Both blobs transferred.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (config_data.len() + layer_data.len()) as u64;
    assert_eq!(report.images[0].bytes_transferred, expected_bytes);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.bytes_transferred, expected_bytes);
}

// ---------------------------------------------------------------------------
// Multi-target independence tests
// ---------------------------------------------------------------------------

/// With two targets and `skip_existing = true`, each target is evaluated
/// independently:
/// - Target A has an existing manifest (different digest) → skipped
/// - Target B has no manifest → synced
///
/// Verifies that the source manifest is pulled exactly once (pull-once
/// fan-out invariant), that target A receives no blob or manifest pushes,
/// and that per-target and aggregate stats are correct.
#[tokio::test]
async fn sync_skip_existing_multi_target_independent() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let config_data = b"config-skip-multi";
    let layer_data = b"layer-skip-multi";
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

    // Source: manifest pulled exactly once (pull-once fan-out invariant).
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes)
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source blobs: only target B needs them; expect(1) each since only one
    // target actually transfers.
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

    // Target A: HEAD returns 200 with a different digest -- skip_existing
    // means the engine skips without comparing digests.  No blob or manifest
    // push endpoints are mounted; an unexpected request would fail the mock.
    let different_digest = test_digest("d1ff");
    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", different_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_a)
        .await;

    // Target B: HEAD returns 404 -- engine must sync.
    mount_manifest_head_not_found(&target_b, "repo", "latest").await;
    mount_blob_not_found(&target_b, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_b, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_b, "repo").await;
    mount_manifest_push(&target_b, "repo", "latest").await;

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        tags: vec![TagPair::same("latest")],
        platforms: None,
        skip_existing: true,
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

    // Exactly 2 image results (1 tag x 2 targets).
    assert_eq!(report.images.len(), 2);

    // Find results by status since target and source strings are identical.
    let result_a = report
        .images
        .iter()
        .find(|r| {
            matches!(
                r.status,
                ImageStatus::Skipped {
                    reason: SkipReason::SkipExisting,
                }
            )
        })
        .expect("target-a result (SkipExisting) not found");

    let result_b = report
        .images
        .iter()
        .find(|r| matches!(r.status, ImageStatus::Synced))
        .expect("target-b result (Synced) not found");

    // Target A: skipped, zero bytes, zero blob transfers.
    assert_eq!(result_a.bytes_transferred, 0);
    assert_eq!(result_a.blob_stats.transferred, 0);
    assert_eq!(result_a.blob_stats.skipped, 0);

    // Target B: synced, both blobs transferred.
    assert_eq!(result_b.blob_stats.transferred, 2);
    assert_eq!(result_b.blob_stats.skipped, 0);
    let expected_bytes = (config_data.len() + layer_data.len()) as u64;
    assert_eq!(result_b.bytes_transferred, expected_bytes);

    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.bytes_transferred, expected_bytes);
    // wiremock expect(N) assertions verify pull-once and no target-A pushes.
}

/// With two targets, an index manifest containing linux/amd64 and linux/arm64,
/// and a platform filter for linux/amd64 only:
/// - Source index is pulled exactly once
/// - Only the amd64 child manifest is pulled (arm64 filtered out)
/// - Both targets receive amd64 blobs and manifest pushes
/// - Neither target receives arm64 blobs or manifest pushes
///
/// Source blobs are pulled once per target (staging disabled), so each blob
/// GET has `.expect(2)`.
#[tokio::test]
async fn sync_platform_filter_multi_target() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    // --- Build two child image manifests: linux/amd64 and linux/arm64 ---

    let amd64_config_data = b"amd64-config-multi";
    let amd64_layer_data = b"amd64-layer-multi";
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

    let arm64_config_data = b"arm64-config-multi";
    let arm64_layer_data = b"arm64-layer-multi";
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
        ],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let index_bytes = serde_json::to_vec(&index).unwrap();

    // --- Source: serve index by tag, children by digest ---

    // Index pulled exactly once (pull-once fan-out invariant).
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

    // amd64 child: expect exactly 1 pull (platform matches; pulled once
    // during discovery and cached for both targets).
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

    // amd64 blobs: each pulled once per target (staging disabled → 2 pulls total).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", amd64_config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_config_data.to_vec())
                .insert_header("content-length", amd64_config_data.len().to_string()),
        )
        .expect(2)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", amd64_layer_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_layer_data.to_vec())
                .insert_header("content-length", amd64_layer_data.len().to_string()),
        )
        .expect(2)
        .mount(&source_server)
        .await;

    // arm64 blobs: expect 0 pulls (filtered out).
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

    // --- Both targets: no existing manifest, no blobs, accept pushes ---

    for target in [&target_a, &target_b] {
        // HEAD check for index tag.
        mount_manifest_head_not_found(target, "repo", "latest").await;
        // Blob checks and pushes for amd64 only.
        mount_blob_not_found(target, "repo", &amd64_config_desc.digest).await;
        mount_blob_not_found(target, "repo", &amd64_layer_desc.digest).await;
        mount_blob_push(target, "repo").await;
        // Accept amd64 child manifest push (by digest).
        mount_manifest_push(target, "repo", &amd64_digest.to_string()).await;
        // Accept filtered index push (by tag).
        mount_manifest_push(target, "repo", "latest").await;
        // arm64 manifest pushes must NOT arrive -- no mock mounted for them.
    }

    let mapping = ResolvedMapping {
        source_client: mock_client(&source_server),
        source_repo: "repo".into(),
        target_repo: "repo".into(),
        targets: vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        tags: vec![TagPair::same("latest")],
        platforms: Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]),
        skip_existing: false,
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

    // 2 image results (1 tag x 2 targets).
    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both targets must be Synced"
    );

    // Each target transfers 2 blobs (amd64 config + layer).
    let expected_blob_bytes = (amd64_config_data.len() + amd64_layer_data.len()) as u64;
    for result in &report.images {
        assert_eq!(
            result.blob_stats.transferred, 2,
            "each target must transfer exactly 2 amd64 blobs"
        );
        assert_eq!(result.blob_stats.skipped, 0);
        assert_eq!(result.bytes_transferred, expected_blob_bytes);
    }

    // Aggregate stats: 2 synced images, 4 blob transfers (2 per target).
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_transferred, 4);
    assert_eq!(report.stats.bytes_transferred, expected_blob_bytes * 2);
    // wiremock expect(N) assertions verify platform filtering and pull-once
    // on index and amd64 child manifests.
}
