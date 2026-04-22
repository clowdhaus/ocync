//! Discovery optimization, budget circuit breaker, and `head_first` integration
//! tests.
//!
//! Extracted from the `engine_integration` monolith. Covers:
//!
//! - **Discovery optimization** - source HEAD cache, target staleness detection,
//!   snapshot pruning, platform-filtered cache, multi-target discovery
//! - **Budget circuit breaker** - rate-limit pause/resume, threshold floor,
//!   refill, no-header, threshold verification, multi-source, tracing
//! - **`head_first`** - per-registry HEAD-before-GET optimization

mod helpers;

use std::sync::Arc;

use ocync_distribution::spec::{MediaType, Platform, PlatformFilter};
use ocync_sync::cache::{PlatformFilterKey, SourceSnapshot};
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ErrorKind, ImageStatus, SkipReason};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

// ---------------------------------------------------------------------------
// Discovery optimization tests
// ---------------------------------------------------------------------------

/// Cold cache: source HEAD succeeds but no cache entry exists, so full GET is
/// required. All four discovery counters must reflect exactly one cache miss.
#[tokio::test]
async fn discovery_cache_miss_first_run() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (returns digest for cache comparison).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (cache miss forces full pull).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Target HEAD: fires once (not synced yet, returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let cache = empty_cache();
    let report = run_sync_with_cache(vec![mapping], cache.clone()).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);

    // Verify cache was populated after successful sync.
    let c = cache.borrow();
    assert!(c.source_snapshot(&snap_key("src/repo", "v1")).is_some());
}

/// Warm cache: source HEAD matches cached snapshot and target HEAD returns the
/// same digest. The engine must skip with zero source GETs (no GET mounted).
#[tokio::test]
async fn discovery_cache_hit_skips_source_get() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let manifest_digest = make_digest("aabb");

    // Source HEAD: fires once (returns digest for cache comparison).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must NOT fire (cache hit skips the slow path).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;

    // Target HEAD: fires once (returns matching digest).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    // Pre-populate cache.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1"),
            SourceSnapshot {
                source_digest: manifest_digest.clone(),
                filtered_digest: manifest_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.stats.discovery_cache_hits, 1);
    assert_eq!(report.stats.discovery_cache_misses, 0);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
}

/// Source HEAD returns 500. The engine must fall through to a full GET pull and
/// record the HEAD failure in addition to the cache miss counter.
#[tokio::test]
async fn discovery_head_failure_falls_through() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, _manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (returns 500, triggers fallback to GET).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (fallback after HEAD failure).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target HEAD: fires once (returns 404, not synced yet).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let report = run_sync_with_cache(vec![mapping], empty_cache()).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_head_failures, 1);
    assert_eq!(report.stats.discovery_target_stale, 0);
}

/// Cache hit on source (HEAD matches snapshot), but target HEAD returns 404
/// (target was garbage-collected). The engine must fall through to a full pull
/// and record target staleness.
#[tokio::test]
async fn discovery_target_stale_triggers_full_pull() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (matches cached snapshot).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (target is stale, so full pull is needed).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target HEAD: fires twice -- once during discovery (cache validation finds
    // target stale) and once during execution (pre-push manifest check).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    // Pre-populate cache -- source matches but target won't.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1"),
            SourceSnapshot {
                source_digest: manifest_digest.clone(),
                filtered_digest: manifest_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 1);
}

/// Cache holds an old source digest. Source HEAD returns a NEW digest that does
/// not match the cached one. The engine must treat this as a cache miss (source
/// changed), not a HEAD failure, and perform a full pull.
#[tokio::test]
async fn discovery_source_changed_triggers_full_pull() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (returns NEW digest, mismatches cached old digest).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (cache miss due to digest change).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    // Pre-populate cache with an OLD source digest that won't match HEAD.
    let old_digest = make_digest("dead");
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1"),
            SourceSnapshot {
                source_digest: old_digest,
                filtered_digest: make_digest("beef"),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache.clone()).await;

    assert_eq!(report.stats.images_synced, 1);
    // HEAD succeeded but digest changed -- cache miss, not head failure.
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1
    );

    // Cache must be updated with the new digest.
    let c = cache.borrow();
    let snapshot = c
        .source_snapshot(&snap_key("src/repo", "v1"))
        .expect("cache should be populated after sync");
    assert_eq!(snapshot.source_digest, manifest_digest);
}

/// Source HEAD returns 404 (tag not found via HEAD, but GET succeeds). The
/// engine must fall through to full GET and count this as a HEAD failure.
#[tokio::test]
async fn discovery_head_404_falls_through() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, _manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (returns 404, triggers fallback to GET).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (fallback after HEAD 404).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let report = run_sync_with_cache(vec![mapping], empty_cache()).await;

    assert_eq!(report.stats.images_synced, 1);
    // 404 on HEAD means no usable digest -- counts as head failure.
    assert_eq!(report.stats.discovery_head_failures, 1);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1
    );
}

/// Source HEAD times out (delayed response exceeds discovery timeout). The
/// engine must fall through to full GET and count this as a HEAD failure.
#[tokio::test]
async fn discovery_head_timeout_falls_through() {
    use std::time::Duration;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (3-second delay exceeds the 1s discovery timeout).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (fallback after HEAD timeout).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    // Use a 1-second discovery HEAD timeout so the 3-second delay triggers timeout.
    let engine = SyncEngine::new(fast_retry(), 10).with_source_head_timeout(Duration::from_secs(1));
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.stats.images_synced, 1);
    // Timeout on HEAD counts as head failure.
    assert_eq!(report.stats.discovery_head_failures, 1);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1
    );
}

/// Bridge test: cache has a valid entry but source HEAD returns 500. The engine
/// must NOT use the cached `filtered_digest` -- it must fall through to the full
/// pull path because the HEAD failure prevents cache validation.
#[tokio::test]
async fn discovery_head_failure_ignores_valid_cache() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-data";
    let layer_data = b"layer-data";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source HEAD: fires once (returns 500, cache cannot be validated).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (fallback after HEAD failure, even with valid cache).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    // Pre-populate cache with a valid entry matching the real manifest digest.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1"),
            SourceSnapshot {
                source_digest: manifest_digest.clone(),
                filtered_digest: manifest_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache).await;

    // Image must sync via the GET path, proving the cache was NOT used.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.discovery_head_failures, 1);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1
    );
}

/// Source HEAD succeeds but source manifest GET returns 500 (all retries).
/// The image must fail and the cache must NOT be populated.
#[tokio::test]
async fn discovery_pull_failure_does_not_populate_cache() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let manifest_digest = make_digest("aabb");

    // Source HEAD: fires once (returns digest, cache miss triggers GET).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: returns 500 on all attempts (retries will hit this mock).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&source_server)
        .await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let cache = empty_cache();
    let report = run_sync_with_cache(vec![mapping], cache.clone()).await;

    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_status!(
        report,
        0,
        ImageStatus::Failed {
            kind: ErrorKind::ManifestPull,
            ..
        }
    );

    // Discovery counters: HEAD succeeded (cache miss, not head failure).
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1
    );

    // Cache must NOT have been populated on failure.
    let c = cache.borrow();
    assert!(
        c.source_snapshot(&snap_key("src/repo", "v1")).is_none(),
        "cache must not be populated after pull failure"
    );
}

/// Source is a multi-arch index with only `linux/s390x`. Config requests
/// `linux/amd64`. The engine must fail with an actionable error mentioning
/// both the filter and available platforms.
#[tokio::test]
async fn discovery_zero_platform_match_returns_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build a child manifest for s390x (content doesn't matter, it shouldn't be pulled).
    let s390x_parts = ManifestBuilder::new(b"s390x-cfg")
        .layer(b"s390x-lyr")
        .build();
    let index_parts = IndexBuilder::new()
        .manifest(
            &s390x_parts,
            Some(Platform {
                architecture: "s390x".to_string(),
                os: "linux".to_string(),
                variant: None,
                os_version: None,
                os_features: None,
            }),
        )
        .build();
    let index_bytes = &index_parts.bytes;
    let index_digest = &index_parts.digest;

    // Source HEAD: fires once (returns index digest, cache miss).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", index_digest.to_string())
                .insert_header("content-type", MediaType::OciIndex.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (cache miss, pulls the index).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index_bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Request linux/amd64 but index only has linux/s390x.
    let mut mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );
    mapping.platforms = Some(vec!["linux/amd64".parse().unwrap()]);

    let report = run_sync_with_cache(vec![mapping], empty_cache()).await;

    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.stats.images_synced, 0);

    // Verify the failure is a ManifestPull with actionable error.
    assert_status!(
        report,
        0,
        ImageStatus::Failed {
            kind: ErrorKind::ManifestPull,
            ..
        }
    );
    if let ImageStatus::Failed { error, .. } = &report.images[0].status {
        assert!(
            error.contains("linux/amd64"),
            "error should mention the requested filter: {error}"
        );
        assert!(
            error.contains("linux/s390x"),
            "error should mention the available platform: {error}"
        );
    }

    // Discovery counters.
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1
    );
}

/// Multi-target fan-out with cache: source HEAD matches cache, Target A matches
/// `filtered_digest` (`DigestMatch` skip), Target B returns 404 (stale). The engine
/// must do a full source pull for Target B only. Discovery path is `TargetStale`.
#[tokio::test]
async fn discovery_mixed_fanout_one_match_one_stale() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    // Build a real image with blobs (using ManifestBuilder for correct sizes).
    let parts = ManifestBuilder::new(b"config-fanout")
        .layer(b"layer-fanout")
        .build();
    let config_data = parts.config_data.as_slice();
    let layer_data = parts.layers_data[0].as_slice();
    let config_desc = &parts.config_desc;
    let layer_desc = &parts.layer_descs[0];
    let manifest_bytes = &parts.bytes;
    let manifest_digest = &parts.digest;

    // Source HEAD: fires once (matches cached snapshot).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: fires once (target B is stale, requires full pull).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;

    // Target A HEAD: fires once (matches filtered_digest, DigestMatch skip).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_a_server)
        .await;

    // Target B HEAD: fires twice -- once during discovery (cache validation
    // finds target stale) and once during execution (pre-push manifest check).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&target_b_server)
        .await;
    mount_blob_not_found(&target_b_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_b_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_b_server, "tgt/repo").await;
    mount_manifest_push(&target_b_server, "tgt/repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_a_client = mock_client(&target_a_server);
    let target_b_client = mock_client(&target_b_server);

    let mapping = resolved_mapping(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![
            target_entry("target-a", target_a_client),
            target_entry("target-b", target_b_client),
        ],
        vec![TagPair::same("v1".to_owned())],
    );

    // Pre-populate cache so source HEAD matches.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1"),
            SourceSnapshot {
                source_digest: manifest_digest.clone(),
                filtered_digest: manifest_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache).await;

    // Two image results: one skipped (Target A), one synced (Target B).
    assert_eq!(report.images.len(), 2);
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 1);

    // Identify results by status.
    let skipped = report
        .images
        .iter()
        .find(|r| {
            matches!(
                r.status,
                ImageStatus::Skipped {
                    reason: SkipReason::DigestMatch,
                }
            )
        })
        .expect("Target A should be skipped with DigestMatch");
    assert_eq!(skipped.bytes_transferred, 0);
    assert_eq!(skipped.blob_stats.transferred, 0);

    let synced = report
        .images
        .iter()
        .find(|r| matches!(r.status, ImageStatus::Synced))
        .expect("Target B should be Synced");
    assert_eq!(synced.blob_stats.transferred, 2);
    let expected_bytes = (config_data.len() + layer_data.len()) as u64;
    assert_eq!(synced.bytes_transferred, expected_bytes);

    // Discovery counters: TargetStale path (cache matched source, but target B mismatched).
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_cache_misses, 1);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 1);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1,
        "sum invariant"
    );
}

/// Retag: source tag `v1.0` mapped to target tag `latest`. Cache is keyed on
/// source tag. Source HEAD uses `v1.0`, target HEAD uses `latest`. With cache
/// pre-populated, the engine should take the `CacheHit` path and skip.
#[tokio::test]
async fn discovery_retag_uses_correct_tags() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let manifest_digest = make_digest("aabb");

    // Source HEAD: fires once at /v2/src/repo/manifests/v1.0.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1.0"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must NOT fire (cache hit skips the slow path).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1.0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;

    // Target HEAD: fires once at /v2/tgt/repo/manifests/latest (retag).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::retag("v1.0".to_owned(), "latest".to_owned())],
    );

    // Pre-populate cache keyed on source tag "v1.0".
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1.0"),
            SourceSnapshot {
                source_digest: manifest_digest.clone(),
                filtered_digest: manifest_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache).await;

    // Image must be skipped (DigestMatch) via the cache hit path.
    assert_eq!(report.images.len(), 1);
    assert_status!(
        report,
        0,
        ImageStatus::Skipped {
            reason: SkipReason::DigestMatch,
        }
    );
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);

    // Discovery counters: cache hit path.
    assert_eq!(report.stats.discovery_cache_hits, 1);
    assert_eq!(report.stats.discovery_cache_misses, 0);
    assert_eq!(report.stats.discovery_head_failures, 0);
    assert_eq!(report.stats.discovery_target_stale, 0);
    // Sum invariant.
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        1,
        "sum invariant"
    );
}

/// Three tags discovered concurrently with mixed outcomes: tag-a is a cache
/// hit (pre-populated), tag-b is a cache miss (first run), and tag-c has a
/// source HEAD failure (500) that falls through to GET. Uses
/// `max_concurrent = 10` to exercise concurrent discovery.
#[tokio::test]
async fn discovery_concurrent_mixed_outcomes() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // --- Image A (for tag-a: cache hit path) ---
    let parts_a = ManifestBuilder::new(b"config-a").layer(b"layer-a").build();
    let manifest_a_digest = &parts_a.digest;

    // --- Image B (for tag-b: cache miss path) ---
    let parts_b = ManifestBuilder::new(b"config-b").layer(b"layer-b").build();
    let config_b = parts_b.config_data.as_slice();
    let config_b_desc = &parts_b.config_desc;
    let layer_b = parts_b.layers_data[0].as_slice();
    let layer_b_desc = &parts_b.layer_descs[0];
    let manifest_b_bytes = &parts_b.bytes;
    let manifest_b_digest = &parts_b.digest;

    // --- Image C (for tag-c: HEAD failure path) ---
    let parts_c = ManifestBuilder::new(b"config-c").layer(b"layer-c").build();
    let config_c = parts_c.config_data.as_slice();
    let config_c_desc = &parts_c.config_desc;
    let layer_c = parts_c.layers_data[0].as_slice();
    let layer_c_desc = &parts_c.layer_descs[0];
    let manifest_c_bytes = &parts_c.bytes;

    // --- Tag A: cache hit (source HEAD matches, target HEAD matches) ---
    // Source HEAD for tag-a: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/tag-a"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_a_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for tag-a: must NOT fire (cache hit).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/tag-a"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;
    // Target HEAD for tag-a: fires once (returns matching digest).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/tag-a"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_a_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    // --- Tag B: cache miss (no cache entry, full pull + push) ---
    // Source HEAD for tag-b: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/tag-b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_b_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for tag-b: fires once (cache miss).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/tag-b"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_b_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_b_desc.digest, config_b).await;
    mount_blob_pull(&source_server, "src/repo", &layer_b_desc.digest, layer_b).await;
    // Target HEAD for tag-b: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/tag-b"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_b_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_b_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "tag-b").await;

    // --- Tag C: HEAD failure (500 on HEAD, falls through to GET) ---
    // Source HEAD for tag-c: fires once (returns 500).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/tag-c"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for tag-c: fires once (fallback after HEAD failure).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/tag-c"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_c_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_c_desc.digest, config_c).await;
    mount_blob_pull(&source_server, "src/repo", &layer_c_desc.digest, layer_c).await;
    // Target HEAD for tag-c: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/tag-c"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_c_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_c_desc.digest).await;
    // blob_push already mounted for tgt/repo (shared across tags).
    mount_manifest_push(&target_server, "tgt/repo", "tag-c").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![
            TagPair::same("tag-a".to_owned()),
            TagPair::same("tag-b".to_owned()),
            TagPair::same("tag-c".to_owned()),
        ],
    );

    // Pre-populate cache for tag-a only.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "tag-a"),
            SourceSnapshot {
                source_digest: manifest_a_digest.clone(),
                filtered_digest: manifest_a_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let report = run_sync_with_cache(vec![mapping], cache).await;

    // Tag A: skipped (cache hit). Tag B: synced (cache miss). Tag C: synced (HEAD failure).
    assert_eq!(report.stats.images_skipped, 1, "tag-a should be skipped");
    assert_eq!(report.stats.images_synced, 2, "tag-b and tag-c should sync");
    assert_eq!(report.stats.discovery_cache_hits, 1, "tag-a = cache hit");
    assert_eq!(
        report.stats.discovery_cache_misses, 2,
        "tag-b + tag-c = cache misses"
    );
    assert_eq!(
        report.stats.discovery_head_failures, 1,
        "tag-c = HEAD failure"
    );
    assert_eq!(report.stats.discovery_target_stale, 0);
    assert_eq!(
        report.stats.discovery_cache_hits + report.stats.discovery_cache_misses,
        3,
        "sum invariant: 3 tags"
    );
}

/// Source changes between two engine runs sharing the same cache. Cycle 1
/// syncs digest D1 (cold cache). Between cycles the source image changes to
/// digest D2. Cycle 2 detects the mismatch (cache has D1, HEAD returns D2)
/// and performs a full pull of the new content.
#[tokio::test]
async fn discovery_source_change_across_cycles() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // --- Cycle 1 image (digest D1) ---
    let parts_1 = ManifestBuilder::new(b"config-cycle-1")
        .layer(b"layer-cycle-1")
        .build();
    let config_1 = parts_1.config_data.as_slice();
    let layer_1 = parts_1.layers_data[0].as_slice();
    let config_1_desc = &parts_1.config_desc;
    let layer_1_desc = &parts_1.layer_descs[0];
    let manifest_1_bytes = &parts_1.bytes;
    let manifest_1_digest = &parts_1.digest;

    // Cycle 1: source HEAD fires once, GET fires once, target HEAD fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_1_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_1_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_1_desc.digest, config_1).await;
    mount_blob_pull(&source_server, "src/repo", &layer_1_desc.digest, layer_1).await;
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_1_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_1_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let cache = empty_cache();
    let mapping_1 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let report_1 = run_sync_with_cache(vec![mapping_1], cache.clone()).await;

    // Cycle 1: cold cache, full sync.
    assert_eq!(report_1.stats.images_synced, 1);
    assert_eq!(report_1.stats.discovery_cache_misses, 1);
    assert_eq!(report_1.stats.discovery_cache_hits, 0);

    // Cache should now contain D1.
    {
        let c = cache.borrow();
        let snap = c
            .source_snapshot(&snap_key("src/repo", "v1"))
            .expect("cache populated after cycle 1");
        assert_eq!(&snap.source_digest, manifest_1_digest);
    }

    // --- Between cycles: source image changes to D2 ---
    source_server.reset().await;
    target_server.reset().await;

    let parts_2 = ManifestBuilder::new(b"config-cycle-2")
        .layer(b"layer-cycle-2")
        .build();
    let config_2 = parts_2.config_data.as_slice();
    let layer_2 = parts_2.layers_data[0].as_slice();
    let config_2_desc = &parts_2.config_desc;
    let layer_2_desc = &parts_2.layer_descs[0];
    let manifest_2_bytes = &parts_2.bytes;
    let manifest_2_digest = &parts_2.digest;

    // Cycle 2: source HEAD fires once (returns D2), GET fires once (cache miss).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_2_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_2_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "src/repo", &config_2_desc.digest, config_2).await;
    mount_blob_pull(&source_server, "src/repo", &layer_2_desc.digest, layer_2).await;
    // Target HEAD: fires once (returns old digest D1, sync is needed).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_1_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_2_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_2_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    // Build new mapping (consumed by run()).
    let mapping_2 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let report_2 = run_sync_with_cache(vec![mapping_2], cache.clone()).await;

    // Cycle 2: cache has D1, HEAD returns D2 -> cache miss, full pull.
    assert_eq!(report_2.stats.images_synced, 1);
    assert_eq!(report_2.stats.discovery_cache_misses, 1);
    assert_eq!(report_2.stats.discovery_cache_hits, 0);
    assert_eq!(report_2.stats.discovery_head_failures, 0);
    assert_eq!(report_2.stats.discovery_target_stale, 0);

    // Cache must be updated to D2.
    let c = cache.borrow();
    let snap = c
        .source_snapshot(&snap_key("src/repo", "v1"))
        .expect("cache updated after cycle 2");
    assert_eq!(&snap.source_digest, manifest_2_digest);
}

/// Two-cycle warm cache test: cycle 1 syncs from a cold cache, populating
/// it. Cycle 2 has the same source digest -- the cache hit path fires, no
/// source GET is issued, and the image is skipped. This is the core
/// proof that the warm cache works across engine runs.
#[tokio::test]
async fn discovery_two_cycle_cache_hit() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-warm")
        .layer(b"layer-warm")
        .build();
    let manifest_digest = &parts.digest;

    // --- Cycle 1: cold cache, full pull ---
    // Source HEAD: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET: fires once (cold cache).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;
    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &parts.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let cache = empty_cache();
    let mapping_1 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let report_1 = run_sync_with_cache(vec![mapping_1], cache.clone()).await;

    // Cycle 1: full sync, cache miss.
    assert_eq!(report_1.stats.images_synced, 1);
    assert_eq!(report_1.stats.discovery_cache_misses, 1);
    assert_eq!(report_1.stats.discovery_cache_hits, 0);

    // --- Between cycles: reset mocks, remount only what cycle 2 needs ---
    source_server.reset().await;
    target_server.reset().await;

    // Source HEAD: fires once (returns same digest, cache hit).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must NOT fire (warm cache hit skips the slow path).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;

    // Target HEAD: fires once (returns matching digest from cycle 1).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping_2 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );

    let report_2 = run_sync_with_cache(vec![mapping_2], cache).await;

    // Cycle 2: cache hit, image skipped, zero source GETs.
    assert_eq!(report_2.stats.images_skipped, 1);
    assert_eq!(report_2.stats.images_synced, 0);
    assert_eq!(report_2.stats.discovery_cache_hits, 1);
    assert_eq!(report_2.stats.discovery_cache_misses, 0);
    assert_eq!(report_2.stats.discovery_head_failures, 0);
    assert_eq!(report_2.stats.discovery_target_stale, 0);
}

/// Platform filter change between cycles: cycle 1 syncs with `linux/amd64`,
/// populating the cache. Cycle 2 uses `linux/arm64` -- the `PlatformFilterKey`
/// mismatch must trigger a cache miss even though the source digest hasn't
/// changed, because the filtered manifest will differ.
#[tokio::test]
async fn discovery_platform_filter_change_triggers_cache_miss() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build a multi-arch index with two children.
    let amd64_parts = ManifestBuilder::new(b"config-amd64")
        .layer(b"layer-amd64")
        .build();
    let arm64_parts = ManifestBuilder::new(b"config-arm64")
        .layer(b"layer-arm64")
        .build();

    let amd64_platform = Platform {
        architecture: "amd64".into(),
        os: "linux".into(),
        variant: None,
        os_version: None,
        os_features: None,
    };
    let arm64_platform = Platform {
        architecture: "arm64".into(),
        os: "linux".into(),
        variant: None,
        os_version: None,
        os_features: None,
    };

    let index_parts = IndexBuilder::new()
        .manifest(&amd64_parts, Some(amd64_platform))
        .manifest(&arm64_parts, Some(arm64_platform))
        .build();
    let index_bytes = &index_parts.bytes;
    let index_digest = &index_parts.digest;
    let amd64_config = amd64_parts.config_data.as_slice();
    let amd64_layer = amd64_parts.layers_data[0].as_slice();
    let amd64_config_desc = &amd64_parts.config_desc;
    let amd64_layer_desc = &amd64_parts.layer_descs[0];
    let amd64_bytes = &amd64_parts.bytes;
    let amd64_digest = &amd64_parts.digest;
    let arm64_config = arm64_parts.config_data.as_slice();
    let arm64_layer = arm64_parts.layers_data[0].as_slice();
    let arm64_config_desc = &arm64_parts.config_desc;
    let arm64_layer_desc = &arm64_parts.layer_descs[0];
    let arm64_bytes = &arm64_parts.bytes;
    let arm64_digest = &arm64_parts.digest;

    // --- Cycle 1: sync with linux/amd64 filter ---
    // Source HEAD: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", index_digest.to_string())
                .insert_header("content-type", MediaType::OciIndex.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for the index: fires once.
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index_bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for the amd64 child manifest: fires once.
    Mock::given(method("GET"))
        .and(path(format!("/v2/src/repo/manifests/{amd64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &amd64_config_desc.digest,
        amd64_config,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &amd64_layer_desc.digest,
        amd64_layer,
    )
    .await;

    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &amd64_config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &amd64_layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;
    // Child manifest push.
    Mock::given(method("PUT"))
        .and(path(format!("/v2/tgt/repo/manifests/{amd64_digest}")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;

    let cache = empty_cache();
    let mut mapping_1 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );
    mapping_1.platforms = Some(vec!["linux/amd64".parse().unwrap()]);

    let report_1 = run_sync_with_cache(vec![mapping_1], cache.clone()).await;

    assert_eq!(report_1.stats.images_synced, 1);
    assert_eq!(report_1.stats.discovery_cache_misses, 1);

    // Cache should hold the amd64 platform filter key.
    {
        let c = cache.borrow();
        let snap = c
            .source_snapshot(&snap_key("src/repo", "v1"))
            .expect("cache populated after cycle 1");
        assert_eq!(&snap.source_digest, index_digest);
        assert_eq!(
            snap.platform_filter_key,
            PlatformFilterKey::from_filters(Some(&["linux/amd64".parse().unwrap()]))
        );
    }

    // --- Cycle 2: change platform filter to linux/arm64 ---
    source_server.reset().await;
    target_server.reset().await;

    // Source HEAD: fires once (same index digest, but platform key changed).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", index_digest.to_string())
                .insert_header("content-type", MediaType::OciIndex.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for the index: fires once (platform key mismatch triggers full pull).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index_bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for the arm64 child manifest: fires once.
    Mock::given(method("GET"))
        .and(path(format!("/v2/src/repo/manifests/{arm64_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &arm64_config_desc.digest,
        arm64_config,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &arm64_layer_desc.digest,
        arm64_layer,
    )
    .await;

    // Target HEAD: fires once (returns 404).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &arm64_config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &arm64_layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/tgt/repo/manifests/{arm64_digest}")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;

    let mut mapping_2 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![TagPair::same("v1".to_owned())],
    );
    mapping_2.platforms = Some(vec!["linux/arm64".parse().unwrap()]);

    let report_2 = run_sync_with_cache(vec![mapping_2], cache.clone()).await;

    // Platform filter changed → cache miss, full pull of arm64 child.
    assert_eq!(report_2.stats.images_synced, 1);
    assert_eq!(report_2.stats.discovery_cache_misses, 1);
    assert_eq!(report_2.stats.discovery_cache_hits, 0);
    assert_eq!(report_2.stats.discovery_head_failures, 0);
    assert_eq!(report_2.stats.discovery_target_stale, 0);

    // Cache should now hold the arm64 platform filter key.
    let c = cache.borrow();
    let snap = c
        .source_snapshot(&snap_key("src/repo", "v1"))
        .expect("cache updated after cycle 2");
    assert_eq!(
        snap.platform_filter_key,
        PlatformFilterKey::from_filters(Some(&["linux/arm64".parse().unwrap()]))
    );
}

/// Engine-level snapshot pruning: after a sync run, snapshot entries for tags
/// no longer in the mapping set must be removed. This prevents unbounded cache
/// growth when source tags are deleted.
#[tokio::test]
async fn discovery_snapshot_pruning_removes_deleted_tags() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"config-prune";
    let layer_data = b"layer-prune";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let manifest = simple_image_manifest(&config_desc.digest, &layer_desc.digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // --- Cycle 1: sync two tags (v1 and v2) ---
    // Source HEAD for v1: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for v1: fires once (cold cache).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Target HEAD for v1: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    // Source HEAD for v2: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for v2: fires once (cold cache).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Target HEAD for v2: fires once.
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v2"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "tgt/repo", "v2").await;

    mount_blob_pull(&source_server, "src/repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src/repo", &layer_desc.digest, layer_data).await;
    mount_blob_not_found(&target_server, "tgt/repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;

    let cache = empty_cache();
    let mapping_1 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        vec![
            TagPair::same("v1".to_owned()),
            TagPair::same("v2".to_owned()),
        ],
    );

    let report_1 = run_sync_with_cache(vec![mapping_1], cache.clone()).await;

    assert_eq!(report_1.stats.images_synced, 2);

    // Both tags should be in the snapshot cache.
    {
        let c = cache.borrow();
        assert!(c.source_snapshot(&snap_key("src/repo", "v1")).is_some());
        assert!(c.source_snapshot(&snap_key("src/repo", "v2")).is_some());
    }

    // --- Cycle 2: tag v2 was deleted from the source, only v1 remains ---
    source_server.reset().await;
    target_server.reset().await;

    // Only v1 in the mapping (v2 was removed).
    // Source HEAD for v1: fires once (cache hit).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    // Source GET for v1: must NOT fire (warm cache hit).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;
    // Target HEAD for v1: fires once (returns matching digest).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping_2 = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/repo",
        "tgt/repo",
        // v2 is gone from the mapping set.
        vec![TagPair::same("v1".to_owned())],
    );

    let report_2 = run_sync_with_cache(vec![mapping_2], cache.clone()).await;

    // v1 should be a cache hit (source + target match).
    assert_eq!(report_2.stats.images_skipped, 1);
    assert_eq!(report_2.stats.discovery_cache_hits, 1);

    // After the run, v2's snapshot must have been pruned.
    let c = cache.borrow();
    assert!(
        c.source_snapshot(&snap_key("src/repo", "v1")).is_some(),
        "v1 should still be in the snapshot cache"
    );
    assert!(
        c.source_snapshot(&snap_key("src/repo", "v2")).is_none(),
        "v2 should have been pruned from the snapshot cache"
    );
}

// ---------------------------------------------------------------------------
// Budget circuit breaker
// ---------------------------------------------------------------------------

/// Mount a source manifest GET mock that includes a `ratelimit-remaining`
/// header (Docker Hub format). Used by circuit breaker tests to simulate
/// rate-limited registries.
async fn mount_source_manifest_with_rate_limit(
    server: &MockServer,
    repo: &str,
    tag: &str,
    bytes: &[u8],
    remaining: u64,
) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("ratelimit-remaining", format!("{remaining};w=21600")),
        )
        .mount(server)
        .await;
}

/// Engine completes all images when source returns zero rate-limit budget.
///
/// With 15 tags and `ratelimit-remaining: 0`, the circuit breaker fires after
/// the first discovery completes (threshold = max(14/10, 1) = 1, remaining 0
/// < 1). The `must_resume` anti-stall path kicks in each time execution drains,
/// cycling discovery one-at-a-time until all images sync. Proves the breaker
/// does not deadlock the engine and that the header value was actually parsed
/// and stored on the source client.
#[tokio::test]
async fn budget_circuit_breaker_completes_under_zero_budget() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"cb-config")
        .layer(b"cb-layer")
        .build();
    let num_tags = 15;

    // Source: every manifest GET returns ratelimit-remaining: 0.
    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_source_manifest_with_rate_limit(&source_server, "library/app", &tag, &parts.bytes, 0)
            .await;
    }
    mount_blob_pull(
        &source_server,
        "library/app",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    for (data, desc) in parts.layers_data.iter().zip(parts.layer_descs.iter()) {
        mount_blob_pull(&source_server, "library/app", &desc.digest, data).await;
    }

    // Target: all tags need syncing.
    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_manifest_head_not_found(&target_server, "mirror/app", &tag).await;
        mount_manifest_push(&target_server, "mirror/app", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/app", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "mirror/app", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/app").await;

    let source_client = mock_client(&source_server);
    let source_client_ref = Arc::clone(&source_client);
    let target_client = mock_client(&target_server);

    let tags: Vec<TagPair> = (0..num_tags)
        .map(|i| TagPair::same(format!("v{i}")))
        .collect();

    let mapping = resolved_mapping(
        source_client,
        "library/app",
        "mirror/app",
        vec![target_entry("target-reg", target_client)],
        tags,
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(
        report.images.len(),
        num_tags,
        "all tags must produce a result"
    );
    assert_eq!(
        report.stats.images_synced, num_tags as u64,
        "all images must sync despite zero rate-limit budget"
    );
    // Every image transferred blobs (no spurious skips from breaker).
    for img in &report.images {
        assert!(
            matches!(img.status, ImageStatus::Synced),
            "image {} should be Synced, got {:?}",
            img.source,
            img.status,
        );
    }
    // The source client must have observed the zero budget from response
    // headers. This proves the header was parsed, stored in the AtomicU64,
    // and was available for the engine's threshold comparison.
    assert_eq!(
        source_client_ref.rate_limit_remaining(),
        Some(0),
        "source client must reflect the zero budget from response headers"
    );
}

/// Circuit breaker fires even with fewer than 10 discovery items.
///
/// Regression test for the threshold dead zone: without the `.max(1)` floor,
/// `remaining_discovery / 10` yields 0 for fewer than 10 items, making the
/// breaker impossible to trigger. With the floor, threshold = 1 and
/// `ratelimit-remaining: 0` triggers the pause.
#[tokio::test]
async fn budget_circuit_breaker_threshold_floor_small_sync() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"small-config")
        .layer(b"small-layer")
        .build();
    let num_tags = 5;

    for i in 0..num_tags {
        let tag = format!("t{i}");
        mount_source_manifest_with_rate_limit(
            &source_server,
            "library/small",
            &tag,
            &parts.bytes,
            0,
        )
        .await;
    }
    mount_blob_pull(
        &source_server,
        "library/small",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    for (data, desc) in parts.layers_data.iter().zip(parts.layer_descs.iter()) {
        mount_blob_pull(&source_server, "library/small", &desc.digest, data).await;
    }

    for i in 0..num_tags {
        let tag = format!("t{i}");
        mount_manifest_head_not_found(&target_server, "mirror/small", &tag).await;
        mount_manifest_push(&target_server, "mirror/small", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/small", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "mirror/small", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/small").await;

    let tags: Vec<TagPair> = (0..num_tags)
        .map(|i| TagPair::same(format!("t{i}")))
        .collect();

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "library/small",
        "mirror/small",
        tags,
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), num_tags);
    assert_eq!(report.stats.images_synced, num_tags as u64);
}

/// Discovery resumes when rate-limit budget refills above threshold.
///
/// Uses sequenced mocks (wiremock `up_to_n_times`) so the budget transition
/// is deterministic regardless of `FuturesUnordered` polling order: the
/// first 3 manifest GETs (whichever tags they are) return
/// `ratelimit-remaining: 0`; all subsequent GETs return
/// `ratelimit-remaining: 100`. The breaker fires after the first low-budget
/// completion, then the anti-stall path serializes until a high-budget
/// response arrives, at which point discovery resumes normally.
#[tokio::test]
async fn budget_circuit_breaker_resumes_on_budget_refill() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"refill-config")
        .layer(b"refill-layer")
        .build();
    let num_tags: usize = 12;

    // Sequenced source manifest mocks. Wiremock matches same-priority
    // mocks by insertion order (first registered wins). The low-budget
    // mock is registered first with up_to_n_times(3), so the first 3
    // manifest GETs hit it. Once exhausted, the high-budget fallback
    // handles the remaining 9.
    Mock::given(method("GET"))
        .and(path_regex(r"/v2/library/refill/manifests/v\d+"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("ratelimit-remaining", "0;w=21600"),
        )
        .up_to_n_times(3)
        .expect(3)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/v2/library/refill/manifests/v\d+"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("ratelimit-remaining", "100;w=21600"),
        )
        .expect((num_tags - 3) as u64..)
        .mount(&source_server)
        .await;

    mount_blob_pull(
        &source_server,
        "library/refill",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    for (data, desc) in parts.layers_data.iter().zip(parts.layer_descs.iter()) {
        mount_blob_pull(&source_server, "library/refill", &desc.digest, data).await;
    }

    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_manifest_head_not_found(&target_server, "mirror/refill", &tag).await;
        mount_manifest_push(&target_server, "mirror/refill", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/refill", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "mirror/refill", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/refill").await;

    let tags: Vec<TagPair> = (0..num_tags)
        .map(|i| TagPair::same(format!("v{i}")))
        .collect();

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "library/refill",
        "mirror/refill",
        tags,
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), num_tags);
    assert_eq!(
        report.stats.images_synced, num_tags as u64,
        "all images must sync after budget refill"
    );
}

/// Verify the breaker's threshold was met on every discovery cycle.
///
/// With 15 tags and zero budget, the breaker condition
/// (`remaining < max(remaining_discovery / 10, 1)`) holds after every discovery
/// completion. This test checks that the source client's `rate_limit_remaining`
/// is `Some(0)` after the run AND that all 15 manifest GETs actually happened
/// (via wiremock `.expect()`). Together these prove: (a) the breaker condition
/// was reachable on every cycle, and (b) the engine still completed all work.
///
/// The breaker's runtime effect (serializing discovery via the anti-stall path)
/// is not externally observable without tracing capture, but the condition
/// being met + all images syncing proves the anti-stall logic is exercised.
#[tokio::test]
async fn budget_circuit_breaker_threshold_met_every_cycle() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"cycle-config")
        .layer(b"cycle-layer")
        .build();
    let num_tags: usize = 15;

    // Use a single regex mock with `.expect(num_tags)` to verify all
    // manifest GETs happened (wiremock panics on drop if count mismatches).
    Mock::given(method("GET"))
        .and(path_regex(r"/v2/library/cycle/manifests/v\d+"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("ratelimit-remaining", "0;w=21600"),
        )
        .expect(num_tags as u64)
        .mount(&source_server)
        .await;

    mount_blob_pull(
        &source_server,
        "library/cycle",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    for (data, desc) in parts.layers_data.iter().zip(parts.layer_descs.iter()) {
        mount_blob_pull(&source_server, "library/cycle", &desc.digest, data).await;
    }

    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_manifest_head_not_found(&target_server, "mirror/cycle", &tag).await;
        mount_manifest_push(&target_server, "mirror/cycle", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/cycle", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "mirror/cycle", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/cycle").await;

    let source_client = mock_client(&source_server);
    let source_client_ref = Arc::clone(&source_client);
    let target_client = mock_client(&target_server);

    let tags: Vec<TagPair> = (0..num_tags)
        .map(|i| TagPair::same(format!("v{i}")))
        .collect();

    let mapping = resolved_mapping(
        source_client,
        "library/cycle",
        "mirror/cycle",
        vec![target_entry("target-reg", target_client)],
        tags,
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.stats.images_synced, num_tags as u64);
    // Source client must show zero budget -- proves the AtomicU64 was written
    // by send_with_aimd and readable from outside the engine.
    assert_eq!(source_client_ref.rate_limit_remaining(), Some(0));
    // The `.expect(15)` on the wiremock mock verifies all 15 manifest GETs
    // happened (wiremock panics on drop if the count doesn't match).
}

/// Registries without rate-limit headers are unaffected by the circuit breaker.
///
/// Verifies that the breaker does not interfere with normal operation when no
/// `ratelimit-remaining` header is present (e.g., ECR, GAR, Chainguard).
#[tokio::test]
async fn budget_circuit_breaker_no_header_unaffected() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"no-rl-config")
        .layer(b"no-rl-layer")
        .build();
    let num_tags = 10;

    // Source: standard manifests without rate-limit headers.
    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_source_manifest(&source_server, "library/ecr", &tag, &parts.bytes).await;
    }
    mount_blob_pull(
        &source_server,
        "library/ecr",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    for (data, desc) in parts.layers_data.iter().zip(parts.layer_descs.iter()) {
        mount_blob_pull(&source_server, "library/ecr", &desc.digest, data).await;
    }

    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_manifest_head_not_found(&target_server, "mirror/ecr", &tag).await;
        mount_manifest_push(&target_server, "mirror/ecr", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/ecr", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "mirror/ecr", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/ecr").await;

    let source_client = mock_client(&source_server);
    let source_client_ref = Arc::clone(&source_client);
    let target_client = mock_client(&target_server);

    let tags: Vec<TagPair> = (0..num_tags)
        .map(|i| TagPair::same(format!("v{i}")))
        .collect();

    let mapping = resolved_mapping(
        source_client,
        "library/ecr",
        "mirror/ecr",
        vec![target_entry("target-reg", target_client)],
        tags,
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), num_tags);
    assert_eq!(
        report.stats.images_synced, num_tags as u64,
        "registries without rate-limit headers must sync normally"
    );
    // Without rate-limit headers, the client should never have stored a value.
    assert_eq!(
        source_client_ref.rate_limit_remaining(),
        None,
        "source client must remain None when no rate-limit header is present"
    );
}

/// Multi-source: one rate-limited source pauses ALL discovery (all-or-nothing).
///
/// Two source registries: source A returns `ratelimit-remaining: 0` (rate-limited),
/// source B returns no rate-limit header (unlimited, e.g. ECR). The all-or-nothing
/// pause means source B's discovery is paused too, but the anti-stall path ensures
/// all images eventually sync. Proves the multi-source dedup works and that the
/// breaker's `any`-trigger / `all`-resume asymmetry does not deadlock.
#[tokio::test]
async fn budget_circuit_breaker_multi_source_all_or_nothing() {
    let source_a_server = MockServer::start().await; // rate-limited (Docker Hub)
    let source_b_server = MockServer::start().await; // unlimited (ECR-like)
    let target_server = MockServer::start().await;

    let parts_a = ManifestBuilder::new(b"ms-config-a")
        .layer(b"ms-layer-a")
        .build();
    let parts_b = ManifestBuilder::new(b"ms-config-b")
        .layer(b"ms-layer-b")
        .build();

    let tags_per_source = 8;

    // Source A: rate-limited.
    for i in 0..tags_per_source {
        let tag = format!("v{i}");
        mount_source_manifest_with_rate_limit(
            &source_a_server,
            "library/alpha",
            &tag,
            &parts_a.bytes,
            0,
        )
        .await;
    }
    mount_blob_pull(
        &source_a_server,
        "library/alpha",
        &parts_a.config_desc.digest,
        &parts_a.config_data,
    )
    .await;
    for (data, desc) in parts_a.layers_data.iter().zip(parts_a.layer_descs.iter()) {
        mount_blob_pull(&source_a_server, "library/alpha", &desc.digest, data).await;
    }

    // Source B: no rate-limit headers.
    for i in 0..tags_per_source {
        let tag = format!("v{i}");
        mount_source_manifest(&source_b_server, "library/beta", &tag, &parts_b.bytes).await;
    }
    mount_blob_pull(
        &source_b_server,
        "library/beta",
        &parts_b.config_desc.digest,
        &parts_b.config_data,
    )
    .await;
    for (data, desc) in parts_b.layers_data.iter().zip(parts_b.layer_descs.iter()) {
        mount_blob_pull(&source_b_server, "library/beta", &desc.digest, data).await;
    }

    // Target: both repos.
    for i in 0..tags_per_source {
        let tag = format!("v{i}");
        mount_manifest_head_not_found(&target_server, "mirror/alpha", &tag).await;
        mount_manifest_push(&target_server, "mirror/alpha", &tag).await;
        mount_manifest_head_not_found(&target_server, "mirror/beta", &tag).await;
        mount_manifest_push(&target_server, "mirror/beta", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/alpha", &parts_a.config_desc.digest).await;
    for desc in &parts_a.layer_descs {
        mount_blob_not_found(&target_server, "mirror/alpha", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/alpha").await;
    mount_blob_not_found(&target_server, "mirror/beta", &parts_b.config_desc.digest).await;
    for desc in &parts_b.layer_descs {
        mount_blob_not_found(&target_server, "mirror/beta", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/beta").await;

    let source_a_client = mock_client(&source_a_server);
    let source_a_ref = Arc::clone(&source_a_client);
    let source_b_client = mock_client(&source_b_server);
    let source_b_ref = Arc::clone(&source_b_client);
    let target_client = mock_client(&target_server);

    let tags: Vec<TagPair> = (0..tags_per_source)
        .map(|i| TagPair::same(format!("v{i}")))
        .collect();

    let mapping_a = resolved_mapping(
        source_a_client,
        "library/alpha",
        "mirror/alpha",
        vec![target_entry("target-reg", Arc::clone(&target_client))],
        tags.clone(),
    );
    let mapping_b = resolved_mapping(
        source_b_client,
        "library/beta",
        "mirror/beta",
        vec![target_entry("target-reg", target_client)],
        tags,
    );

    let report = run_sync(vec![mapping_a, mapping_b]).await;

    let total = tags_per_source * 2;
    assert_eq!(
        report.images.len(),
        total,
        "all images from both sources must produce a result"
    );
    assert_eq!(
        report.stats.images_synced, total as u64,
        "all images must sync despite one source being rate-limited"
    );
    for img in &report.images {
        assert!(
            matches!(img.status, ImageStatus::Synced),
            "image {} should be Synced, got {:?}",
            img.source,
            img.status,
        );
    }
    // Source A: rate-limited, must reflect zero budget.
    assert_eq!(
        source_a_ref.rate_limit_remaining(),
        Some(0),
        "rate-limited source must reflect zero budget"
    );
    // Source B: no headers, must remain None.
    assert_eq!(
        source_b_ref.rate_limit_remaining(),
        None,
        "unlimited source must remain None"
    );
}

/// Tracing capture: verify the circuit breaker warn! message is emitted.
///
/// Uses `tracing-subscriber` with an in-memory layer to capture log output.
/// Proves the breaker actually fires (not just that the engine completes),
/// making the "breaker fired" assertion explicit rather than inferred from
/// side effects.
#[tokio::test]
async fn budget_circuit_breaker_emits_tracing_warn() {
    use std::sync::Mutex;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    /// Minimal tracing layer that captures formatted warn-level events.
    struct CaptureLayer {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut visitor = MessageVisitor(String::new());
                event.record(&mut visitor);
                self.messages.lock().unwrap().push(visitor.0);
            }
        }
    }

    struct MessageVisitor(String);

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let layer = CaptureLayer {
        messages: Arc::clone(&captured),
    };
    let subscriber = tracing_subscriber::registry().with(layer);

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"trace-config")
        .layer(b"trace-layer")
        .build();
    let num_tags: usize = 10;

    // The first tag responds instantly (sets rate_limit_remaining=0 on the
    // client). Remaining tags are delayed 200ms so they are guaranteed to be
    // in-flight when the budget check fires after the first completion. The
    // delay must be long enough to survive CI load on slow runners (50ms was
    // insufficient on x86 ubuntu CI).
    for i in 0..num_tags {
        let tag = format!("v{i}");
        let delay = if i == 0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_millis(200)
        };
        Mock::given(method("GET"))
            .and(path(format!("/v2/library/traced/manifests/{tag}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(parts.bytes.clone())
                    .insert_header("content-type", MediaType::OciManifest.as_str())
                    .insert_header("ratelimit-remaining", "0;w=21600")
                    .set_delay(delay),
            )
            .mount(&source_server)
            .await;
    }
    mount_blob_pull(
        &source_server,
        "library/traced",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    for (data, desc) in parts.layers_data.iter().zip(parts.layer_descs.iter()) {
        mount_blob_pull(&source_server, "library/traced", &desc.digest, data).await;
    }

    for i in 0..num_tags {
        let tag = format!("v{i}");
        mount_manifest_head_not_found(&target_server, "mirror/traced", &tag).await;
        mount_manifest_push(&target_server, "mirror/traced", &tag).await;
    }
    mount_blob_not_found(&target_server, "mirror/traced", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "mirror/traced", &desc.digest).await;
    }
    mount_blob_push(&target_server, "mirror/traced").await;

    let tags: Vec<TagPair> = (0..num_tags)
        .map(|i| TagPair::same(format!("v{i}")))
        .collect();

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "library/traced",
        "mirror/traced",
        tags,
    );

    let engine = SyncEngine::new(fast_retry(), 50);

    // Run the engine under our custom subscriber.
    let _guard = tracing::subscriber::set_default(subscriber);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;
    drop(_guard);

    assert_eq!(report.stats.images_synced, num_tags as u64);

    let messages = captured.lock().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("rate-limit budget low, pausing discovery")),
        "expected warn message about pausing discovery, got: {messages:?}"
    );
}

// ---------------------------------------------------------------------------
// head_first tests
// ---------------------------------------------------------------------------

/// Construct a `ResolvedMapping` with `head_first` enabled and optional platform filters.
fn resolved_mapping_head_first(
    source_client: Arc<ocync_distribution::RegistryClient>,
    source_repo: &str,
    target_repo: &str,
    targets: Vec<TargetEntry>,
    tags: Vec<TagPair>,
    platforms: Option<Vec<PlatformFilter>>,
) -> ResolvedMapping {
    ResolvedMapping {
        platforms,
        head_first: true,
        ..resolved_mapping(source_client, source_repo, target_repo, targets, tags)
    }
}

/// `head_first`: when all targets match the source HEAD digest on cold cache,
/// the full source GET is skipped entirely.
#[tokio::test]
async fn head_first_all_targets_match_skips_source_get() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let manifest_digest = make_digest("f001");

    // Source HEAD: fires once (discovery optimization).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must NOT fire (head_first skips the full pull).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;

    // Target HEAD: fires once (head_first checks target).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![target_entry("target-reg", target_client)],
        vec![TagPair::same("v1")],
        None,
    );

    // Empty cache -- no discovery cache entry.
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

    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.stats.discovery_head_first_skips, 1);
    // head_first hits are independent of cache hits/misses -- no cache entry
    // existed, but no full pull was needed either.
    assert_eq!(report.stats.discovery_cache_hits, 0);
    assert_eq!(report.stats.discovery_cache_misses, 0);
}

/// `head_first`: when target HEAD returns a different digest, fall through
/// to the full source GET and complete the sync.
#[tokio::test]
async fn head_first_mismatch_falls_through_to_get() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"hf-config")
        .layer(b"hf-layer")
        .build();
    let manifest_bytes = &parts.bytes;
    let manifest_digest = &parts.digest;

    // Source HEAD: returns the real digest (exactly once -- reused by head_first).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must fire (head_first detects mismatch, full pull needed).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source blobs.
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;

    // Target HEAD for head_first: returns 404 (not found = mismatch).
    // After the full pull, the engine does another target HEAD in
    // full_pull_and_build_tasks which also returns 404.
    mount_target_fresh(
        &target_server,
        "tgt/repo",
        "v1",
        &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
    )
    .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![target_entry("target-reg", target_client)],
        vec![TagPair::same("v1")],
        None,
    );

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

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_skipped, 0);
    // head_first did not skip (mismatch) so it falls through to CacheMiss.
    assert_eq!(report.stats.discovery_head_first_skips, 0);
    assert_eq!(report.stats.discovery_cache_misses, 1);
}

/// `head_first`: when the source HEAD itself fails, fall through gracefully
/// to the full source GET (HEAD failure path) without attempting target HEADs.
#[tokio::test]
async fn head_first_source_head_failure_falls_through() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"hf-cfg-fail")
        .layer(b"hf-lyr-fail")
        .build();
    let manifest_bytes = &parts.bytes;

    // Source HEAD: returns 500 (failure).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must fire (HEAD failed, full pull path).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;

    // Target HEAD (from full_pull_and_build_tasks).
    mount_target_fresh(
        &target_server,
        "tgt/repo",
        "v1",
        &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
    )
    .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![target_entry("target-reg", target_client)],
        vec![TagPair::same("v1")],
        None,
    );

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

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_skipped, 0);
    // head_first was enabled but source HEAD failed, so it's a HEAD failure.
    // HeadFailure increments both discovery_head_failures and discovery_cache_misses.
    assert_eq!(report.stats.discovery_head_first_skips, 0);
    assert_eq!(report.stats.discovery_head_failures, 1);
    assert_eq!(report.stats.discovery_cache_misses, 1);
}

/// `head_first`: with two targets, one matching and one mismatching, the
/// matching target is skipped while the mismatching target receives the
/// full sync. Route is `CacheMiss` (not `HeadFirstSkip`) because a GET was
/// still required.
#[tokio::test]
async fn head_first_partial_target_match_syncs_only_mismatched() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"hf-partial-cfg")
        .layer(b"hf-partial-lyr")
        .build();
    let manifest_bytes = &parts.bytes;
    let manifest_digest = &parts.digest;

    // Source HEAD: returns the real digest (exactly once).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must fire (partial mismatch requires full pull).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;

    // Target A: HEAD returns matching digest (already in sync).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .mount(&target_a)
        .await;

    // Target A: must NOT receive any blob pushes or manifest pushes.
    // (If the engine incorrectly sends to target A, wiremock will report
    // unmatched requests and the test runner will surface them.)

    // Target B: HEAD returns 404 (mismatch, needs sync). Receives the full push.
    mount_target_fresh(
        &target_b,
        "tgt/repo",
        "v1",
        &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
    )
    .await;

    let source_client = mock_client(&source_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        vec![TagPair::same("v1")],
        None,
    );

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

    // Image was synced (partial match means we still did work).
    assert_eq!(report.stats.images_synced, 1);
    // Not a HeadFirstSkip because not ALL targets matched.
    assert_eq!(report.stats.discovery_head_first_skips, 0);
    // Fell through to CacheMiss route.
    assert_eq!(report.stats.discovery_cache_misses, 1);
}

/// `head_first`: when platform filtering is active, `head_first` is bypassed
/// even if enabled. The target holds a filtered index digest that differs from
/// the unfiltered source HEAD digest, so comparison would always fail.
#[tokio::test]
async fn head_first_bypassed_with_platform_filter() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build a multi-platform index with one child manifest.
    let child_parts = ManifestBuilder::new(b"hf-platform-cfg")
        .layer(b"hf-platform-lyr")
        .build();
    let child_manifest_digest = &child_parts.digest;

    let index_parts = IndexBuilder::new()
        .manifest(
            &child_parts,
            Some(Platform {
                architecture: "amd64".to_string(),
                os: "linux".to_string(),
                os_version: None,
                os_features: None,
                variant: None,
            }),
        )
        .build();
    let index_bytes = &index_parts.bytes;
    let index_digest = &index_parts.digest;

    // Source HEAD: returns the index digest.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", index_digest.to_string())
                .insert_header("content-type", MediaType::OciIndex.as_str())
                .insert_header("content-length", index_bytes.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET for index: must fire (head_first bypassed, full pull path).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index_bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET for child manifest.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src/repo/manifests/{}",
            child_manifest_digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(child_parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    mount_blob_pull(
        &source_server,
        "src/repo",
        &child_parts.config_desc.digest,
        &child_parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &child_parts.layer_descs[0].digest,
        &child_parts.layers_data[0],
    )
    .await;

    // Target: no existing manifests, accept pushes.
    mount_manifest_head_not_found(&target_server, "tgt/repo", "v1").await;
    Mock::given(method("HEAD"))
        .and(path(format!(
            "/v2/tgt/repo/manifests/{}",
            child_manifest_digest
        )))
        .respond_with(ResponseTemplate::new(404))
        .mount(&target_server)
        .await;
    mount_blob_not_found(&target_server, "tgt/repo", &child_parts.config_desc.digest).await;
    mount_blob_not_found(
        &target_server,
        "tgt/repo",
        &child_parts.layer_descs[0].digest,
    )
    .await;
    mount_blob_push(&target_server, "tgt/repo").await;
    // Accept push for both child manifest (by digest) and index (by tag).
    Mock::given(method("PUT"))
        .and(path(format!(
            "/v2/tgt/repo/manifests/{}",
            child_manifest_digest
        )))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![target_entry("target-reg", target_client)],
        vec![TagPair::same("v1")],
        Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]),
    );

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

    // Image synced normally (head_first bypassed due to platform filter).
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_skipped, 0);
    // head_first was bypassed, so no hits.
    assert_eq!(report.stats.discovery_head_first_skips, 0);
    // Took the CacheMiss route (full pull path).
    assert_eq!(report.stats.discovery_cache_misses, 1);
}

/// `head_first`: when a target HEAD request returns a server error, the
/// target is treated as mismatched (degraded to sync) rather than failing
/// the entire discovery.
#[tokio::test]
async fn head_first_target_head_error_degrades_to_sync() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"hf-terr-cfg")
        .layer(b"hf-terr-lyr")
        .build();
    let manifest_bytes = &parts.bytes;
    let manifest_digest = &parts.digest;

    // Source HEAD: returns the real digest.
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must fire (target HEAD failed, treated as mismatch).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;

    // Target HEAD for head_first: returns 500 (server error).
    // The head_first mock is mounted first; full_pull_and_build_tasks also
    // issues a target HEAD which hits the same path (both return 500, the
    // engine treats both as "needs push").
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&target_server)
        .await;

    mount_blob_not_found(&target_server, "tgt/repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/repo", &parts.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt/repo").await;
    mount_manifest_push(&target_server, "tgt/repo", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![target_entry("target-reg", target_client)],
        vec![TagPair::same("v1")],
        None,
    );

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

    // Image synced successfully despite target HEAD error.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_skipped, 0);
    assert_eq!(report.stats.images_failed, 0);
    // Not a HeadFirstSkip because the target HEAD errored (treated as mismatch).
    assert_eq!(report.stats.discovery_head_first_skips, 0);
    assert_eq!(report.stats.discovery_cache_misses, 1);
}

/// `head_first`: when the discovery cache is warm (source snapshot exists),
/// the standard cache-hit path fires and `head_first` has no effect --
/// no target HEADs are issued for the `head_first` check.
#[tokio::test]
async fn head_first_warm_cache_uses_standard_cache_hit() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let manifest_digest = make_digest("f0a1");

    // Source HEAD: fires once (discovery optimization confirms cache match).
    Mock::given(method("HEAD"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Source GET: must NOT fire (cache hit skips full pull).
    Mock::given(method("GET"))
        .and(path("/v2/src/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&source_server)
        .await;

    // Target HEAD by tag: fires once (standard cache-hit target check).
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping_head_first(
        source_client,
        "src/repo",
        "tgt/repo",
        vec![target_entry("target-reg", target_client)],
        vec![TagPair::same("v1")],
        None,
    );

    // Pre-populate discovery cache so the standard cache-hit path fires.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_source_snapshot(
            snap_key("src/repo", "v1"),
            SourceSnapshot {
                source_digest: manifest_digest.clone(),
                filtered_digest: manifest_digest.clone(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
    }

    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    // Standard cache hit, NOT head_first.
    assert_eq!(report.stats.discovery_cache_hits, 1);
    assert_eq!(report.stats.discovery_head_first_skips, 0);
    assert_eq!(report.stats.discovery_cache_misses, 0);
}
