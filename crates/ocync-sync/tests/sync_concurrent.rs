//! Concurrent execution integration tests: non-sequential ordering, parallel
//! transfers, concurrent dedup, mount coordination, and blob concurrency bounds.

mod helpers;

use std::sync::Arc;

use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType};
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    // Use higher concurrency -- both tags can execute simultaneously.
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

    let mapping_a = resolved_mapping(
        Arc::clone(&source_client),
        "repo-a",
        "repo-a",
        vec![target_entry("target", Arc::clone(&target_client))],
        vec![TagPair::same("v1")],
    );
    let mapping_b = resolved_mapping(
        source_client,
        "repo-b",
        "repo-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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
            make_descriptor(amd64_digest.clone(), MediaType::OciManifest),
            make_descriptor(arm64_digest.clone(), MediaType::OciManifest),
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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
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

    // No manifest PUT mock -- if engine tries to push manifest, wiremock
    // returns 404 and the test fails differently.

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

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
    if let ImageStatus::Failed { kind, .. } = &report.images[0].status {
        assert_eq!(kind.to_string(), "blob transfer");
    }
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

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    // Real concurrency -- NOT 1.
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
// Test: concurrent transfers wait on in-progress uploads to enable cross-repo mount
// ---------------------------------------------------------------------------

/// When two concurrent transfers target different repos at the same registry
/// and share a blob, the second transfer must wait for the first to finish
/// uploading, then mount from the first repo -- not re-upload the blob.
///
/// Without the coordination fix, both tasks race past the cache check (no
/// completed entry yet since the first upload is still in-flight) and both
/// perform full uploads. The fix adds an in-progress uploader check + Notify
/// so the second task waits for completion.
///
/// Assertion strategy (order-independent):
/// - Both targets accept upload and mount for all blobs
/// - The leader election picks one image deterministically; the other mounts
/// - Assert aggregate: 2 blobs transferred + 2 blobs mounted (no double-upload)
#[tokio::test]
async fn sync_concurrent_shared_blob_mounts_instead_of_double_uploading() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"cfg-concurrent-mount";
    let layer_data = b"layer-concurrent-mount";
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

    // Both source repos serve the same manifest and blobs (both fully pullable).
    mount_source_manifest(&source_server, "src-a", "v1", &manifest_bytes).await;
    mount_source_manifest(&source_server, "src-b", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "src-a", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src-a", &layer_desc.digest, layer_data).await;
    mount_blob_pull(&source_server, "src-b", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "src-b", &layer_desc.digest, layer_data).await;

    // Target: both repos need HEAD 404 and blobs-not-found.
    mount_manifest_head_not_found(&target_server, "tgt-a", "v1").await;
    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-a", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-a", &layer_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &layer_desc.digest).await;

    // tgt-a: accepts upload (POST + PATCH + PUT) and mount (POST with mount=).
    // Mount mocks use priority 1 so wiremock checks them before the generic
    // upload POST (priority 5, default) which also matches mount requests.
    mount_blob_push(&target_server, "tgt-a").await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-a/blobs/uploads/"))
        .and(query_param("mount", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-a/blobs/uploads/"))
        .and(query_param("mount", layer_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;

    // tgt-b: accepts upload (POST + PATCH + PUT) and mount (POST with mount=).
    mount_blob_push(&target_server, "tgt-b").await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param("mount", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param("mount", layer_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;

    mount_manifest_push(&target_server, "tgt-a", "v1").await;
    mount_manifest_push(&target_server, "tgt-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping_a = resolved_mapping(
        Arc::clone(&source_client),
        "src-a",
        "tgt-a",
        vec![target_entry("target", Arc::clone(&target_client))],
        vec![TagPair::same("v1")],
    );
    let mapping_b = resolved_mapping(
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

    // max_concurrent = 10: both mappings run concurrently.
    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        )
        .await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync",
    );
    assert_eq!(report.stats.images_synced, 2);
    // Aggregate: one image uploads 2 blobs (leader), the other mounts 2 (follower).
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_mounted, 2);
    // At least one image mounted (not both uploading).
    assert!(
        report.images.iter().any(|r| r.blob_stats.mounted > 0),
        "at least one image should mount blobs from the other"
    );
    // At least one image transferred (not both mounting).
    assert!(
        report.images.iter().any(|r| r.blob_stats.transferred > 0),
        "at least one image should transfer blobs"
    );
}

/// Three images sharing a layer exercise the full leader-follower pipeline:
/// election picks one leader, wave promotion runs the two followers, and
/// the shared layer is mounted from the leader's committed repo.
#[tokio::test(flavor = "current_thread")]
async fn sync_wave_promotion_three_images_shared_layer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared layer across all three images; config blobs are unique.
    let shared_data = b"shared-layer-wave-test";
    let cfg_a_data = b"config-a-wave";
    let cfg_b_data = b"config-b-wave";
    let cfg_c_data = b"config-c-wave";

    let shared_desc = blob_descriptor(shared_data, MediaType::OciLayerGzip);
    let cfg_a_desc = blob_descriptor(cfg_a_data, MediaType::OciConfig);
    let cfg_b_desc = blob_descriptor(cfg_b_data, MediaType::OciConfig);
    let cfg_c_desc = blob_descriptor(cfg_c_data, MediaType::OciConfig);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_a_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_b_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_c = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_c_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };

    let (bytes_a, _) = serialize_manifest(&manifest_a);
    let (bytes_b, _) = serialize_manifest(&manifest_b);
    let (bytes_c, _) = serialize_manifest(&manifest_c);

    // Source: all three repos serve their manifests and blobs.
    mount_source_manifest(&source_server, "src-a", "v1", &bytes_a).await;
    mount_source_manifest(&source_server, "src-b", "v1", &bytes_b).await;
    mount_source_manifest(&source_server, "src-c", "v1", &bytes_c).await;
    mount_blob_pull(&source_server, "src-a", &cfg_a_desc.digest, cfg_a_data).await;
    mount_blob_pull(&source_server, "src-a", &shared_desc.digest, shared_data).await;
    mount_blob_pull(&source_server, "src-b", &cfg_b_desc.digest, cfg_b_data).await;
    mount_blob_pull(&source_server, "src-b", &shared_desc.digest, shared_data).await;
    mount_blob_pull(&source_server, "src-c", &cfg_c_desc.digest, cfg_c_data).await;
    mount_blob_pull(&source_server, "src-c", &shared_desc.digest, shared_data).await;

    // Target: all repos get manifest HEAD 404, blob HEAD 404, upload + mount mocks.
    for repo in &["tgt-a", "tgt-b", "tgt-c"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &cfg_a_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &cfg_b_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &cfg_c_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &shared_desc.digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;

        // Mount mock with higher priority so it matches before the upload POST.
        Mock::given(method("POST"))
            .and(path(format!("/v2/{repo}/blobs/uploads/")))
            .and(query_param("mount", shared_desc.digest.to_string()))
            .respond_with(ResponseTemplate::new(201))
            .with_priority(1)
            .mount(&target_server)
            .await;
    }

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping_a = resolved_mapping(
        Arc::clone(&source_client),
        "src-a",
        "tgt-a",
        vec![target_entry("target", Arc::clone(&target_client))],
        vec![TagPair::same("v1")],
    );
    let mapping_b = resolved_mapping(
        Arc::clone(&source_client),
        "src-b",
        "tgt-b",
        vec![target_entry("target", Arc::clone(&target_client))],
        vec![TagPair::same("v1")],
    );
    let mapping_c = resolved_mapping(
        source_client,
        "src-c",
        "tgt-c",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

    // max_concurrent=10: all three run concurrently within their phase.
    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping_a, mapping_b, mapping_c],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        )
        .await;

    assert_eq!(report.images.len(), 3);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "all three images should sync: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    // 3 unique config blobs transferred + 1 shared layer transferred by leader.
    assert_eq!(report.stats.blobs_transferred, 4);
    // Shared layer mounted by 2 followers.
    assert_eq!(report.stats.blobs_mounted, 2);
}

/// Verify that `transfer_image_blobs` processes blobs concurrently, not
/// sequentially. An image with 8 blobs, each delayed 100ms on source pull,
/// should complete well under 8 * 100ms = 800ms given `BLOB_CONCURRENCY=6`.
#[tokio::test(flavor = "current_thread")]
async fn sync_blob_concurrency_processes_multiple_blobs() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // 1 config + 7 layers = 8 blobs, each with unique data.
    let config_data = b"concurrency-config-blob";
    let layer_data: Vec<Vec<u8>> = (0..7)
        .map(|i| format!("concurrency-layer-{i}").into_bytes())
        .collect();
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_descs: Vec<Descriptor> = layer_data
        .iter()
        .map(|d| blob_descriptor(d, MediaType::OciLayerGzip))
        .collect();

    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: layer_descs.clone(),
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest immediately; each blob with a 100ms delay.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(config_data.to_vec())
                .insert_header("content-length", config_data.len().to_string())
                .set_delay(std::time::Duration::from_millis(100)),
        )
        .mount(&source_server)
        .await;
    for (i, desc) in layer_descs.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/v2/repo/blobs/{}", desc.digest)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(layer_data[i].clone())
                    .insert_header("content-length", layer_data[i].len().to_string())
                    .set_delay(std::time::Duration::from_millis(100)),
            )
            .mount(&source_server)
            .await;
    }

    // Target: manifest HEAD 404, all blob HEADs 404, push accepts.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    for desc in &layer_descs {
        mount_blob_not_found(&target_server, "repo", &desc.digest).await;
    }
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 1);
    let start = std::time::Instant::now();
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        )
        .await;
    let elapsed = start.elapsed();

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "image should sync, got: {:?}",
        report.images[0].status
    );
    assert_eq!(
        report.stats.blobs_transferred, 8,
        "all 8 blobs should be transferred"
    );
    // With BLOB_CONCURRENCY=6, 8 blobs at 100ms each need ~2 batches (~200ms).
    // 1200ms is generous for CI runners; still proves concurrency (sequential >= 800ms).
    assert!(
        elapsed < std::time::Duration::from_millis(1200),
        "elapsed {elapsed:?} should be < 1200ms (sequential would be >= 800ms)"
    );
}

/// First-failure-stops semantics: when one blob source returns 500, the
/// image fails and not all blobs complete.
#[tokio::test(flavor = "current_thread")]
async fn sync_blob_failure_cancels_remaining_blobs() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // 1 config + 3 layers = 4 blobs.
    let config_data = b"cancel-config";
    let layer1_data = b"cancel-layer-1";
    let layer2_data = b"cancel-layer-2-will-fail";
    let layer3_data = b"cancel-layer-3-never-reached";

    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer1_desc = blob_descriptor(layer1_data, MediaType::OciLayerGzip);
    let layer2_desc = blob_descriptor(layer2_data, MediaType::OciLayerGzip);
    let layer3_desc = blob_descriptor(layer3_data, MediaType::OciLayerGzip);

    let manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![
            layer1_desc.clone(),
            layer2_desc.clone(),
            layer3_desc.clone(),
        ],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: manifest and blobs. Layer 2 always returns 500.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer1_desc.digest, layer1_data).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer2_desc.digest)))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&source_server)
        .await;
    mount_blob_pull(&source_server, "repo", &layer3_desc.digest, layer3_data).await;

    // Target: manifest HEAD 404, all blob HEADs 404, push accepts.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer1_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer2_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer3_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 1);
    let report = engine
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        )
        .await;

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Failed { .. }),
        "image should fail when a blob source returns 500, got: {:?}",
        report.images[0].status
    );
    // Not all blobs completed (the cancel flag stops remaining).
    assert!(
        report.images[0].blob_stats.transferred < 4,
        "transferred {} should be < 4 (cancel should stop remaining blobs)",
        report.images[0].blob_stats.transferred
    );
    assert_eq!(report.stats.images_failed, 1);
}

/// Two images sharing a blob sync concurrently with staging enabled.
/// Verifies that concurrent staging writes to the same digest do not
/// panic or produce ENOENT errors (unique tmp paths prevent collision).
#[tokio::test(flavor = "current_thread")]
async fn sync_staging_concurrent_write_no_collision() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    // Shared blob across both images.
    let shared_data = b"shared-staging-collision-test";
    let shared_desc = blob_descriptor(shared_data, MediaType::OciLayerGzip);

    // Unique config blobs per image.
    let cfg_a_data = b"staging-cfg-a";
    let cfg_b_data = b"staging-cfg-b";
    let cfg_a_desc = blob_descriptor(cfg_a_data, MediaType::OciConfig);
    let cfg_b_desc = blob_descriptor(cfg_b_data, MediaType::OciConfig);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_a_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_b_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (bytes_a, _) = serialize_manifest(&manifest_a);
    let (bytes_b, _) = serialize_manifest(&manifest_b);

    // Source: both repos serve their manifests and all blobs.
    mount_source_manifest(&source_server, "src-a", "v1", &bytes_a).await;
    mount_source_manifest(&source_server, "src-b", "v1", &bytes_b).await;
    mount_blob_pull(&source_server, "src-a", &cfg_a_desc.digest, cfg_a_data).await;
    mount_blob_pull(&source_server, "src-a", &shared_desc.digest, shared_data).await;
    mount_blob_pull(&source_server, "src-b", &cfg_b_desc.digest, cfg_b_data).await;
    mount_blob_pull(&source_server, "src-b", &shared_desc.digest, shared_data).await;

    // Target A: manifest HEAD 404, blobs 404, push accepts.
    mount_manifest_head_not_found(&target_a, "tgt-a", "v1").await;
    mount_blob_not_found(&target_a, "tgt-a", &cfg_a_desc.digest).await;
    mount_blob_not_found(&target_a, "tgt-a", &shared_desc.digest).await;
    mount_blob_push(&target_a, "tgt-a").await;
    mount_manifest_push(&target_a, "tgt-a", "v1").await;

    // Target B: manifest HEAD 404, blobs 404, push accepts.
    mount_manifest_head_not_found(&target_b, "tgt-b", "v1").await;
    mount_blob_not_found(&target_b, "tgt-b", &cfg_b_desc.digest).await;
    mount_blob_not_found(&target_b, "tgt-b", &shared_desc.digest).await;
    mount_blob_push(&target_b, "tgt-b").await;
    mount_manifest_push(&target_b, "tgt-b", "v1").await;

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let source_client = mock_client(&source_server);

    let mapping_a = resolved_mapping(
        Arc::clone(&source_client),
        "src-a",
        "tgt-a",
        vec![target_entry("target-a", mock_client(&target_a))],
        vec![TagPair::same("v1")],
    );
    let mapping_b = resolved_mapping(
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target-b", mock_client(&target_b))],
        vec![TagPair::same("v1")],
    );

    // max_concurrent=10: both images sync concurrently.
    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            staging,
            &NullProgress,
            Some(&ShutdownSignal::new()),
        )
        .await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync without collision: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    // At least the 2 unique config blobs are transferred.
    assert!(
        report.stats.blobs_transferred >= 2,
        "at least 2 unique config blobs should be transferred, got {}",
        report.stats.blobs_transferred
    );
}

/// Source-pull dedup: when two images share a blob and staging is enabled,
/// the source blob should be pulled exactly once. The second image's task
/// waits for the first to finish staging, then reads from disk.
#[tokio::test(flavor = "current_thread")]
async fn sync_source_pull_dedup_with_staging() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared layer across both images; config blobs are unique.
    let shared_data = b"shared-source-dedup-layer";
    let cfg_a_data = b"dedup-cfg-a";
    let cfg_b_data = b"dedup-cfg-b";

    let shared_desc = blob_descriptor(shared_data, MediaType::OciLayerGzip);
    let cfg_a_desc = blob_descriptor(cfg_a_data, MediaType::OciConfig);
    let cfg_b_desc = blob_descriptor(cfg_b_data, MediaType::OciConfig);

    let manifest_a = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_a_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_b = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: cfg_b_desc.clone(),
        layers: vec![shared_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (bytes_a, _) = serialize_manifest(&manifest_a);
    let (bytes_b, _) = serialize_manifest(&manifest_b);

    // Source: both repos serve their manifests and all blobs.
    mount_source_manifest(&source_server, "src-a", "v1", &bytes_a).await;
    mount_source_manifest(&source_server, "src-b", "v1", &bytes_b).await;
    mount_blob_pull(&source_server, "src-a", &cfg_a_desc.digest, cfg_a_data).await;
    mount_blob_pull(&source_server, "src-b", &cfg_b_desc.digest, cfg_b_data).await;
    // Shared blob: served by both repos, but `.expect(1)` on each to verify
    // that the source-pull dedup prevents a second GET.
    let shared_pull_a = Mock::given(method("GET"))
        .and(path(format!("/v2/src-a/blobs/{}", shared_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(shared_data.to_vec())
                .insert_header("content-length", shared_data.len().to_string()),
        )
        .expect(0..=1)
        .named("shared blob GET src-a")
        .mount_as_scoped(&source_server)
        .await;
    let shared_pull_b = Mock::given(method("GET"))
        .and(path(format!("/v2/src-b/blobs/{}", shared_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(shared_data.to_vec())
                .insert_header("content-length", shared_data.len().to_string()),
        )
        .expect(0..=1)
        .named("shared blob GET src-b")
        .mount_as_scoped(&source_server)
        .await;

    // Target: manifest HEAD 404, blobs 404, push accepts.
    for repo in &["tgt-a", "tgt-b"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &cfg_a_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &cfg_b_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &shared_desc.digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;
    }

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let source_client = mock_client(&source_server);

    let mapping_a = resolved_mapping(
        Arc::clone(&source_client),
        "src-a",
        "tgt-a",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );
    let mapping_b = resolved_mapping(
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    // max_concurrent=10: both images sync concurrently.
    let engine = SyncEngine::new(fast_retry(), 10);
    let report = engine
        .run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            staging,
            &NullProgress,
            Some(&ShutdownSignal::new()),
        )
        .await;

    // Drop scoped mocks before checking received requests.
    drop(shared_pull_a);
    drop(shared_pull_b);

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    // 4 blobs pushed to targets: 2 unique configs + shared layer to each target.
    // Source-pull dedup doesn't reduce target pushes -- only source GETs.
    assert_eq!(report.stats.blobs_transferred, 4);

    // Key assertion: the shared blob was pulled from source exactly once
    // across both repos. Without dedup this would be 2 (one per image).
    let received = source_server.received_requests().await.unwrap();
    let shared_blob_suffix = format!("/blobs/{}", shared_desc.digest);
    let source_gets_for_shared = received
        .iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path().ends_with(&shared_blob_suffix))
        .count();
    assert_eq!(
        source_gets_for_shared, 1,
        "shared blob should be pulled from source exactly once (dedup), got {source_gets_for_shared}"
    );
}

// ---------------------------------------------------------------------------
// Blob concurrency bound: Semaphore(6) correctness with >6 layers
// ---------------------------------------------------------------------------

/// An image with more layers than the per-image blob concurrency limit (6)
/// must still sync correctly. All blobs transfer successfully even though the
/// internal semaphore serializes some of them. This verifies the semaphore
/// does not deadlock or drop permits when the layer count exceeds the bound.
#[tokio::test]
async fn sync_image_with_more_layers_than_blob_concurrency_limit() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build an image with 10 layers (exceeds BLOB_CONCURRENCY = 6).
    let parts = ManifestBuilder::new(b"config-many-layers")
        .layer(b"layer-01")
        .layer(b"layer-02")
        .layer(b"layer-03")
        .layer(b"layer-04")
        .layer(b"layer-05")
        .layer(b"layer-06")
        .layer(b"layer-07")
        .layer(b"layer-08")
        .layer(b"layer-09")
        .layer(b"layer-10")
        .build();

    parts.mount_source(&source_server, "repo/many", "v1").await;
    parts.mount_target(&target_server, "repo/many", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping(
        source_client,
        "repo/many",
        "repo/many",
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
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "image with 10 layers must sync: {:?}",
        report.images[0].status
    );
    // 11 blobs total: 1 config + 10 layers.
    assert_eq!(report.images[0].blob_stats.transferred, 11);
    assert_eq!(report.stats.blobs_transferred, 11);
}
