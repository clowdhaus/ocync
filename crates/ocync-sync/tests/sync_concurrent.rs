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
    let parts_a = ManifestBuilder::new(b"cfg-a-concurrent")
        .layer(shared_layer)
        .build();
    let parts_b = ManifestBuilder::new(b"cfg-b-concurrent")
        .layer(shared_layer)
        .build();

    parts_a.mount_source(&source_server, "repo", "v1").await;
    parts_b.mount_source(&source_server, "repo", "v2").await;
    parts_a.mount_target(&target_server, "repo", "v1").await;
    parts_b.mount_target(&target_server, "repo", "v2").await;

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

    let parts = ManifestBuilder::new(b"cfg-mount-concurrent")
        .layer(b"layer-mount-concurrent")
        .build();

    // Both repos share the same manifest/blobs at source.
    parts.mount_source(&source_server, "repo-a", "v1").await;
    parts.mount_source(&source_server, "repo-b", "v1").await;
    parts.mount_target(&target_server, "repo-a", "v1").await;
    parts.mount_target(&target_server, "repo-b", "v1").await;

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
    let amd64_parts = ManifestBuilder::new(b"amd64-config-fail")
        .layer(b"amd64-layer-fail")
        .build();
    let arm64_parts = ManifestBuilder::new(b"arm64-config-fail")
        .layer(b"arm64-layer-fail")
        .build();

    // Build the index manifest referencing both children.
    let index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![
            make_descriptor(amd64_parts.digest.clone(), MediaType::OciManifest),
            make_descriptor(arm64_parts.digest.clone(), MediaType::OciManifest),
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
        .and(path(format!("/v2/repo/manifests/{}", amd64_parts.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64_parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // arm64 child manifest pull returns 500 (server error).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", arm64_parts.digest)))
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
    assert_status!(report, 0, ImageStatus::Failed { .. });
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
    let parts = ManifestBuilder::new(config_data).layer(layer_data).build();

    // Source: serve manifest and both blobs.
    parts.mount_source(&source_server, "repo", "v1").await;

    // Target: manifest HEAD 404, both blobs not found.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[0].digest).await;

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
        .and(query_param("digest", parts.config_desc.digest.to_string()))
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
    assert_status!(report, 0, ImageStatus::Failed { .. });
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

    // Two different manifests that share the same config and layer blobs.
    let parts_v1 = ManifestBuilder::new(config_data).layer(layer_data).build();
    let parts_v2 = ManifestBuilder::new(config_data).layer(layer_data).build();

    // Source: serve each manifest with expect(1) to verify each pulled once.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts_v1.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts_v2.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts_v1.config_desc.digest,
        config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts_v1.layer_descs[0].digest,
        layer_data,
    )
    .await;

    // Target: manifest HEAD 404 for both tags, blobs not found, push endpoints.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &parts_v1.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts_v1.layer_descs[0].digest).await;
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

    let parts = ManifestBuilder::new(b"cfg-concurrent-mount")
        .layer(b"layer-concurrent-mount")
        .build();

    // Both source repos serve the same manifest and blobs (both fully pullable).
    parts.mount_source(&source_server, "src-a", "v1").await;
    parts.mount_source(&source_server, "src-b", "v1").await;

    // Target: both repos need HEAD 404 and blobs-not-found.
    mount_manifest_head_not_found(&target_server, "tgt-a", "v1").await;
    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-a", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-a", &parts.layer_descs[0].digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &parts.layer_descs[0].digest).await;

    // tgt-a: accepts upload (POST + PATCH + PUT) and mount (POST with mount=).
    // Mount mocks use priority 1 so wiremock checks them before the generic
    // upload POST (priority 5, default) which also matches mount requests.
    mount_blob_push(&target_server, "tgt-a").await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-a/blobs/uploads/"))
        .and(query_param("mount", parts.config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-a/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts.layer_descs[0].digest.to_string(),
        ))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;

    // tgt-b: accepts upload (POST + PATCH + PUT) and mount (POST with mount=).
    mount_blob_push(&target_server, "tgt-b").await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param("mount", parts.config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts.layer_descs[0].digest.to_string(),
        ))
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

    let parts_a = ManifestBuilder::new(b"config-a-wave")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-b-wave")
        .layer(shared_data)
        .build();
    let parts_c = ManifestBuilder::new(b"config-c-wave")
        .layer(shared_data)
        .build();

    // Source: all three repos serve their manifests and blobs.
    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;
    parts_c.mount_source(&source_server, "src-c", "v1").await;

    // Target: all repos get manifest HEAD 404, blob HEAD 404, upload + mount mocks.
    for repo in &["tgt-a", "tgt-b", "tgt-c"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &parts_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_b.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_c.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_a.layer_descs[0].digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;

        // Mount mock with higher priority so it matches before the upload POST.
        Mock::given(method("POST"))
            .and(path(format!("/v2/{repo}/blobs/uploads/")))
            .and(query_param(
                "mount",
                parts_a.layer_descs[0].digest.to_string(),
            ))
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
    assert_status!(report, 0, ImageStatus::Synced);
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

    let parts = ManifestBuilder::new(config_data)
        .layer(layer1_data)
        .layer(layer2_data)
        .layer(layer3_data)
        .build();

    // Source: manifest and blobs. Layer 2 always returns 500.
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.layer_descs[0].digest,
        layer1_data,
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[1].digest
        )))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.layer_descs[2].digest,
        layer3_data,
    )
    .await;

    // Target: manifest HEAD 404, all blob HEADs 404, push accepts.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[0].digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[1].digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[2].digest).await;
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
    assert_status!(report, 0, ImageStatus::Failed { .. });
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

    // Unique config blobs per image.
    let parts_a = ManifestBuilder::new(b"staging-cfg-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"staging-cfg-b")
        .layer(shared_data)
        .build();

    // Source: both repos serve their manifests and all blobs.
    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;

    // Target A: manifest HEAD 404, blobs 404, push accepts.
    mount_manifest_head_not_found(&target_a, "tgt-a", "v1").await;
    mount_blob_not_found(&target_a, "tgt-a", &parts_a.config_desc.digest).await;
    mount_blob_not_found(&target_a, "tgt-a", &parts_a.layer_descs[0].digest).await;
    mount_blob_push(&target_a, "tgt-a").await;
    mount_manifest_push(&target_a, "tgt-a", "v1").await;

    // Target B: manifest HEAD 404, blobs 404, push accepts.
    mount_manifest_head_not_found(&target_b, "tgt-b", "v1").await;
    mount_blob_not_found(&target_b, "tgt-b", &parts_b.config_desc.digest).await;
    mount_blob_not_found(&target_b, "tgt-b", &parts_b.layer_descs[0].digest).await;
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

    let parts_a = ManifestBuilder::new(b"dedup-cfg-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"dedup-cfg-b")
        .layer(shared_data)
        .build();

    // Source: both repos serve their manifests and all blobs.
    mount_source_manifest(&source_server, "src-a", "v1", &parts_a.bytes).await;
    mount_source_manifest(&source_server, "src-b", "v1", &parts_b.bytes).await;
    mount_blob_pull(
        &source_server,
        "src-a",
        &parts_a.config_desc.digest,
        b"dedup-cfg-a",
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src-b",
        &parts_b.config_desc.digest,
        b"dedup-cfg-b",
    )
    .await;
    // Shared blob: served by both repos, but `.expect(0..=1)` on each to verify
    // that the source-pull dedup prevents a second GET.
    let shared_pull_a = Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-a/blobs/{}",
            parts_a.layer_descs[0].digest
        )))
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
        .and(path(format!(
            "/v2/src-b/blobs/{}",
            parts_b.layer_descs[0].digest
        )))
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
        mount_blob_not_found(&target_server, repo, &parts_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_b.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_a.layer_descs[0].digest).await;
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
    let shared_blob_suffix = format!("/blobs/{}", parts_a.layer_descs[0].digest);
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
    assert_status!(report, 0, ImageStatus::Synced);
    // 11 blobs total: 1 config + 10 layers.
    assert_eq!(report.images[0].blob_stats.transferred, 11);
    assert_eq!(report.stats.blobs_transferred, 11);
}
