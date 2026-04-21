//! Transfer state cache integration tests: progressive cache population, cross-repo mount,
//! monolithic upload, lazy invalidation, cache persistence round-trip, warm cache skip,
//! and batch blob checker pre-population with fallback on error.

mod helpers;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ocync_distribution::spec::{ImageIndex, ImageManifest, MediaType, RepositoryName};
use ocync_distribution::{BatchBlobChecker, Digest};
use ocync_sync::ImageStatus;
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{RegistryAlias, SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

// ---------------------------------------------------------------------------
// Local mock implementations for batch blob checking
// ---------------------------------------------------------------------------

/// Mock batch checker that returns a pre-configured set of existing digests.
///
/// Returns a pre-configured set of existing digests, filtered by input.
/// Verifies the caller passes the expected repository name (per mock
/// contract fidelity -- a bug where the engine passes the wrong repo
/// would be invisible without this check).
struct MockBatchChecker {
    /// Expected repository name -- panics if the caller passes a different repo.
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

// ---------------------------------------------------------------------------
// Tests: progressive cache population, cross-repo mount, monolithic upload,
// lazy invalidation, and cache persistence round-trip.
// ---------------------------------------------------------------------------

/// Two images sharing one base layer and one unique layer each.
///
/// After the first image syncs, the base layer is recorded as completed at
/// `(target, repo)`. When the second image processes the base layer, the
/// engine hits `blob_known_at_repo` -> skips the HEAD check entirely.
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

    // v1 blobs: all missing at target -- base, config_a, layer_a each need HEAD + push.
    mount_blob_not_found(&target_server, "repo", &base_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &config_a_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_a_desc.digest).await;

    // v2 blobs: base was already completed by v1 -- no HEAD issued.
    // config_b and layer_b are new so HEAD + push needed.
    mount_blob_not_found(&target_server, "repo", &config_b_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_b_desc.digest).await;

    // The push endpoint accepts any upload (3 pushes for v1: base + config_a + layer_a;
    // 2 pushes for v2: config_b + layer_b; base is skipped entirely for v2).
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
/// source in the cache and issues a cross-repo mount POST (201 -> Mounted).
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
        c.set_blob_completed(
            target,
            config_desc.digest.clone(),
            RepositoryName::new("repo-a").unwrap(),
        );
        c.set_blob_completed(
            target,
            layer_desc.digest.clone(),
            RepositoryName::new("repo-a").unwrap(),
        );
    }

    let mapping = resolved_mapping(
        source_client,
        "repo-b",
        "repo-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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

/// When the target is ECR and `BLOB_MOUNTING` is disabled (the default),
/// mount POSTs return 202 (not fulfilled). The engine issues the POST,
/// gets 202, falls through to HEAD + push. Mount attempts are not
/// short-circuited -- the 202 fallback is cheap and enables the mount
/// optimization when `BLOB_MOUNTING` is enabled.
#[tokio::test]
async fn sync_warm_cache_ecr_target_mount_not_fulfilled() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"ecr-config-data";
    let layer_data = b"ecr-layer-data";
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

    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo-b", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-b", &layer_desc.digest, layer_data).await;

    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;

    // Mount POST returns 202 (not fulfilled) -- engine falls through to push.
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param("from", "repo-a"))
        .respond_with(
            ResponseTemplate::new(202)
                .append_header("Location", "/v2/repo-b/blobs/uploads/fallback-uuid"),
        )
        .expect(2)
        .mount(&target_server)
        .await;

    mount_blob_push(&target_server, "repo-b").await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = ecr_mock_client(&target_server);

    let cache = empty_cache();
    let target_name = "ecr-target";
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed(
            target_name,
            config_desc.digest.clone(),
            RepositoryName::new("repo-a").unwrap(),
        );
        c.set_blob_completed(
            target_name,
            layer_desc.digest.clone(),
            RepositoryName::new("repo-a").unwrap(),
        );
    }

    let mapping = resolved_mapping(
        source_client,
        "repo-b",
        "repo-b",
        vec![target_entry(target_name, target_client)],
        vec![TagPair::same("v1")],
    );

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
    // Mount attempted but not fulfilled (202) -- blobs transferred instead.
    assert_eq!(report.images[0].blob_stats.mounted, 0);
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.stats.blobs_mounted, 0);
    assert_eq!(report.stats.blobs_transferred, 2);
}

/// All blobs use streaming PUT (POST + PUT) with no PATCH. The mock expects
/// exactly 1 POST and 1 PUT per blob, and 0 PATCH requests.
#[tokio::test]
async fn sync_small_blob_uses_monolithic_upload() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Small blobs -- streaming PUT handles all sizes.
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

    // Monolithic upload: POST initiates, PUT finalizes -- no PATCH.
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

    // No PATCH mock registered -- any PATCH would cause a wiremock 404 and fail the test.

    mount_manifest_push(&target_server, "repo", "v1").await;

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
    // path (POST -> 202, then PUT -> 201; no PATCH).
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
        c.set_blob_completed(
            "target",
            config_desc.digest.clone(),
            RepositoryName::new("other-repo").unwrap(),
        );
        c.set_blob_completed(
            "target",
            layer_desc.digest.clone(),
            RepositoryName::new("other-repo").unwrap(),
        );
    }

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target-reg", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

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
            &RepositoryName::new("repo").unwrap()
        ),
        "config blob should be recorded as completed at repo"
    );
    assert!(
        loaded.blob_known_at_repo(
            "target-reg",
            &layer_desc.digest,
            &RepositoryName::new("repo").unwrap()
        ),
        "layer blob should be recorded as completed at repo"
    );
    assert!(
        !loaded.blob_known_at_repo(
            "target-reg",
            &config_desc.digest,
            &RepositoryName::new("other-repo").unwrap()
        ),
        "blob should not appear at an unrelated repo"
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
        c.set_blob_completed(
            "target",
            config_desc.digest.clone(),
            RepositoryName::new("stale-repo").unwrap(),
        );
        c.set_blob_completed(
            "target",
            layer_desc.digest.clone(),
            RepositoryName::new("stale-repo").unwrap(),
        );
    }

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
            &RepositoryName::new("stale-repo").unwrap()
        ),
        "stale cache entry for config at stale-repo should be invalidated"
    );
    assert!(
        !c.blob_known_at_repo(
            "target",
            &layer_desc.digest,
            &RepositoryName::new("stale-repo").unwrap()
        ),
        "stale cache entry for layer at stale-repo should be invalidated"
    );
    assert!(
        c.blob_known_at_repo(
            "target",
            &config_desc.digest,
            &RepositoryName::new("repo").unwrap()
        ),
        "config blob should be recorded as completed at repo after fallback push"
    );
    assert!(
        c.blob_known_at_repo(
            "target",
            &layer_desc.digest,
            &RepositoryName::new("repo").unwrap()
        ),
        "layer blob should be recorded as completed at repo after fallback push"
    );
}

// ---------------------------------------------------------------------------
// Tests: warm cache skips blob entirely (no HEAD check issued)
// ---------------------------------------------------------------------------

/// A pre-warmed cache records a blob as completed at `(target, repo)`.
/// Verify the engine skips it entirely -- no HEAD check, no pull, no push.
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
    // No source blob pulls mounted -- they shouldn't be needed.

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    // No blob HEAD mocks -- any HEAD attempt would cause wiremock to return 404
    // which would trigger a push, which would also fail (no push mock).
    // The test succeeds only if the cache skip prevents all blob operations.
    mount_manifest_push(&target_server, "repo", "v1").await;

    // Pre-warm cache: both blobs known at (target, repo).
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed(
            "target",
            config_desc.digest.clone(),
            RepositoryName::new("repo").unwrap(),
        );
        c.set_blob_completed(
            "target",
            layer_desc.digest.clone(),
            RepositoryName::new("repo").unwrap(),
        );
    }

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
            cache,
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Both blobs skipped via cache -- no transfers, no HEAD checks.
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

    let mapping = resolved_mapping(
        source_client,
        "src/nginx",
        "tgt/nginx",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: target_client,
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
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

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: target_client,
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
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

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![target_entry("target", target_client)],
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

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: target_client,
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
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

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![
            TargetEntry {
                name: RegistryAlias::new("target-a"),
                client: mock_client(&target_a_server),
                batch_checker: Some(Rc::new(checker_a)),
                existing_tags: HashSet::new(),
            },
            TargetEntry {
                name: RegistryAlias::new("target-b"),
                client: mock_client(&target_b_server),
                batch_checker: Some(Rc::new(checker_b)),
                existing_tags: HashSet::new(),
            },
        ],
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

    // Target: manifest HEAD 404, blob HEAD expect(0) because batch-check
    // already confirmed absent (all digests in batch_checked set skip HEAD).
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&target_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&target_server)
        .await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    // Batch checker: empty set -- nothing exists at target.
    let (checker, batch_call_count) = MockBatchChecker::new("repo", HashSet::new());

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            TargetEntry {
                name: RegistryAlias::new("target-a"),
                client: mock_client(&target_a_server),
                batch_checker: Some(Rc::new(checker)),
                existing_tags: HashSet::new(),
            },
            target_entry("target-b", mock_client(&target_b_server)),
        ],
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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
        vec![TagPair::same("v1"), TagPair::same("v2")],
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
/// (the primary use case: Chainguard multi-arch -> ECR). The batch checker reports
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
            make_descriptor(amd64_digest.clone(), MediaType::OciManifest),
            make_descriptor(arm64_digest.clone(), MediaType::OciManifest),
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

    // Source: NO blob endpoints -- batch reports all exist, no pulls needed.

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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
        vec![TagPair::same("latest")],
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
    // Layer blob is NOT in the cache -- only batch reports it.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_exists(
            "target",
            config_desc.digest.clone(),
            RepositoryName::new("repo").unwrap(),
        );
    }

    // Batch checker: both blobs exist (config is redundant with cache).
    let existing = HashSet::from([config_desc.digest.clone(), layer_desc.digest.clone()]);
    let (checker, batch_call_count) = MockBatchChecker::new("repo", existing);

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![TargetEntry {
            name: RegistryAlias::new("target"),
            client: mock_client(&target_server),
            batch_checker: Some(Rc::new(checker)),
            existing_tags: HashSet::new(),
        }],
        vec![TagPair::same("v1")],
    );

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
