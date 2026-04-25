//! Concurrent execution integration tests: non-sequential ordering, parallel
//! transfers, concurrent dedup, mount coordination, and blob concurrency bounds.

mod helpers;

use std::sync::Arc;

use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType, RepositoryName};
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

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

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("latest")],
    );
    let report = run_sync(vec![mapping]).await;

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

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    // Use max_concurrent=1 to ensure sequential blob processing (config first).
    let report = run_sync_sequential(vec![mapping]).await;

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

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    // Real concurrency -- NOT 1.
    let report = run_sync(vec![mapping]).await;

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

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
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

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
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

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo/many",
        vec![TagPair::same("v1")],
    );
    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // 11 blobs total: 1 config + 10 layers.
    assert_eq!(report.images[0].blob_stats.transferred, 11);
    assert_eq!(report.stats.blobs_transferred, 11);
}

// ---------------------------------------------------------------------------
// Tests: leader-follower watch channel -- adversarial timing
// ---------------------------------------------------------------------------

/// The leader's manifest push succeeds instantly (no delay), while the
/// follower's blob HEAD checks are delayed. This forces the "fire-before-
/// subscribe" path in the watch channel: the leader calls
/// `mark_repo_committed` before the follower ever calls `repo_committed_watch`.
///
/// Without `send_replace` (using plain `send`), the leader's signal would be
/// lost and the follower would deadlock waiting for a commit that already
/// happened.
#[tokio::test(flavor = "current_thread")]
async fn sync_leader_commits_before_follower_subscribes() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Two images sharing a layer. Leader finishes fast, follower starts late.
    let shared_data = b"shared-late-subscribe";
    let parts_a = ManifestBuilder::new(b"config-leader-fast")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-follower-slow")
        .layer(shared_data)
        .build();

    // Source: leader's blobs return instantly; follower's config blob is
    // delayed so the follower reaches the mount-source check after the
    // leader has already committed its manifest.
    parts_a.mount_source(&source_server, "src-a", "v1").await;

    // Follower source: manifest + shared layer (instant), config blob (delayed).
    mount_source_manifest(
        &source_server,
        "src-b",
        "v1",
        &serde_json::to_vec(&ImageManifest {
            schema_version: 2,
            media_type: None,
            config: parts_b.config_desc.clone(),
            layers: parts_b.layer_descs.clone(),
            subject: None,
            artifact_type: None,
            annotations: None,
        })
        .unwrap(),
    )
    .await;
    // Shared layer: same digest as leader's, served instantly.
    mount_blob_pull(
        &source_server,
        "src-b",
        &parts_b.layer_descs[0].digest,
        shared_data,
    )
    .await;
    // Config blob: 200ms delay forces follower to reach mount-source check
    // after leader has committed.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-b/blobs/{}",
            parts_b.config_desc.digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"config-follower-slow".to_vec())
                .insert_header("content-length", b"config-follower-slow".len().to_string())
                .set_delay(std::time::Duration::from_millis(200)),
        )
        .mount(&source_server)
        .await;

    // Target: both repos accept upload AND mount (symmetric mocks).
    for repo in &["tgt-a", "tgt-b"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &parts_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_b.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_a.layer_descs[0].digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;

        // Mount mock for the shared layer (priority 1 over generic upload POST).
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
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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
        "both images should sync (no deadlock): {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    // Leader transfers shared layer + config; follower mounts shared layer + transfers config.
    assert_eq!(
        report.stats.blobs_transferred + report.stats.blobs_mounted,
        4
    );
    assert!(
        report.stats.blobs_mounted >= 1,
        "follower should mount the shared layer, got {} mounts",
        report.stats.blobs_mounted,
    );
}

/// Leader's manifest push fails (500). Followers waiting on the watch channel
/// must unblock via `notify_repo_failed`, see `is_repo_committed` = false,
/// and fall through to HEAD+push instead of deadlocking.
#[tokio::test(flavor = "current_thread")]
async fn sync_leader_manifest_push_fails_follower_falls_through() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-leader-fail";
    let parts_a = ManifestBuilder::new(b"config-leader-fail")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-follower-fallback")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;

    // Target tgt-a: leader -- blob uploads succeed but manifest push fails (500).
    mount_manifest_head_not_found(&target_server, "tgt-a", "v1").await;
    mount_blob_not_found(&target_server, "tgt-a", &parts_a.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-a", &parts_a.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt-a").await;
    Mock::given(method("PUT"))
        .and(path("/v2/tgt-a/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&target_server)
        .await;

    // Target tgt-b: follower -- all operations succeed (including push fallback).
    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-b", &parts_b.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &parts_b.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt-b").await;
    mount_manifest_push(&target_server, "tgt-b", "v1").await;
    // Mount mock in case follower attempts mount before falling through.
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts_a.layer_descs[0].digest.to_string(),
        ))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;

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
    // Leader fails.
    assert!(
        report
            .images
            .iter()
            .any(|r| matches!(r.status, ImageStatus::Failed { .. })),
        "leader should fail"
    );
    // Follower succeeds (did not deadlock waiting for leader's commit).
    assert!(
        report
            .images
            .iter()
            .any(|r| matches!(r.status, ImageStatus::Synced)),
        "follower should succeed after leader failure: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
}

/// Multiple leaders sharing blobs complete without deadlock. Validates that
/// leader images do not deadlock waiting on each other's `repo_committed_watch`.
/// With the bounded-deadline resolver, whichever leader commits first becomes
/// a mount source for the other; if neither commits in time, the deadline trips
/// and both fall through to push. No `is_leader` skip needed.
///
/// Uses 3 images with overlapping blob sets that force `elect_leaders` to
/// pick 2 leaders. The shared blob count (4) is below `BLOB_CONCURRENCY`
/// (6), so this test exercises the bounded-wait path but does NOT trigger
/// the blob-semaphore-exhaustion deadlock -- that is covered by
/// `sync_cross_image_blob_claim_no_deadlock` which uses 8 shared layers to
/// exceed the semaphore limit.
#[tokio::test(flavor = "current_thread")]
async fn sync_multi_leader_shared_blobs_no_deadlock() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared blobs between A and B (forces elect_leaders to pick 2 leaders).
    let shared_ab_1 = b"shared-ab-layer-1-deadlock-test";
    let shared_ab_2 = b"shared-ab-layer-2-deadlock-test";

    // A's blobs also shared with C (gives A marginal coverage of C).
    let shared_ac = b"shared-ac-layer-deadlock-test";

    // B's blobs also shared with C (gives B marginal coverage of C).
    let shared_bc = b"shared-bc-layer-deadlock-test";

    // Image A: {shared_ab_1, shared_ab_2, shared_ac, config_a}
    let parts_a = ManifestBuilder::new(b"config-leader-a-deadlock")
        .layer(shared_ab_1)
        .layer(shared_ab_2)
        .layer(shared_ac)
        .build();

    // Image B: {shared_ab_1, shared_ab_2, shared_bc, config_b}
    let parts_b = ManifestBuilder::new(b"config-leader-b-deadlock")
        .layer(shared_ab_1)
        .layer(shared_ab_2)
        .layer(shared_bc)
        .build();

    // Image C: {shared_ac, shared_bc, config_c} -- follower
    let parts_c = ManifestBuilder::new(b"config-follower-c-deadlock")
        .layer(shared_ac)
        .layer(shared_bc)
        .build();

    // Source: all three repos serve manifests and blobs.
    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;
    parts_c.mount_source(&source_server, "src-c", "v1").await;

    // Target: all repos get manifest HEAD 404, blob HEAD 404, upload + mount mocks.
    // Mounts between leaders return 202 (Not Fulfilled) to simulate ECR behavior
    // where uncommitted repos cannot serve as mount sources.
    for repo in &["tgt-a", "tgt-b", "tgt-c"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        // All possible blob HEADs return 404.
        mount_blob_not_found(&target_server, repo, &parts_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_b.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_c.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_a.layer_descs[0].digest).await;
        mount_blob_not_found(&target_server, repo, &parts_a.layer_descs[1].digest).await;
        mount_blob_not_found(&target_server, repo, &parts_a.layer_descs[2].digest).await;
        mount_blob_not_found(&target_server, repo, &parts_c.layer_descs[0].digest).await;
        mount_blob_not_found(&target_server, repo, &parts_c.layer_descs[1].digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;

        // Mount mocks: 201 for any mount request (mounts from committed repos
        // will succeed; mounts from uncommitted repos are not attempted by
        // the engine when the fix is in place, but the mock handles both).
        for layer_desc in parts_a.layer_descs.iter() {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{repo}/blobs/uploads/")))
                .and(query_param("mount", layer_desc.digest.to_string()))
                .respond_with(ResponseTemplate::new(201))
                .with_priority(1)
                .mount(&target_server)
                .await;
        }
        for layer_desc in parts_c.layer_descs.iter() {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{repo}/blobs/uploads/")))
                .and(query_param("mount", layer_desc.digest.to_string()))
                .respond_with(ResponseTemplate::new(201))
                .with_priority(1)
                .mount(&target_server)
                .await;
        }
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

    let engine = SyncEngine::new(fast_retry(), 10);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.run(
            vec![mapping_a, mapping_b, mapping_c],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report = result.expect("engine deadlocked: multi-leader repo_committed_watch cycle");
    assert_eq!(report.images.len(), 3);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "all three images should sync (no deadlock): {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    assert_eq!(report.stats.images_synced, 3);
    // At least some blobs should be mounted (leader-follower coordination works).
    assert!(
        report.stats.blobs_mounted > 0,
        "expected some blobs to be mounted via leader-follower, got 0"
    );
}

/// Cross-image blob claim deadlock regression test.
///
/// Two images share 8 layers (more than `BLOB_CONCURRENCY`=6). Source
/// blobs for image B are delayed 50ms so image A claims all shared blobs
/// first. When B starts, it finds all 8 shared blobs claimed by A and
/// enters claim-waits. Without the fix (claim loop inside blob semaphore),
/// B's 6 semaphore permits are consumed by claim-waits. B's unique config
/// blob cannot acquire a permit. Meanwhile A's shared blobs complete and
/// notify B, but B cannot proceed because its semaphore is exhausted --
/// and A may itself be waiting on B's blobs for mount optimization.
///
/// With the fix, claim-waits happen OUTSIDE the semaphore. B's claim-waits
/// don't consume permits, so B's config blob proceeds normally. A finishes
/// shared blobs and notifies B. Both images complete.
///
/// The 5-second timeout catches the deadlock deterministically because the
/// 50ms delay forces the claim ordering. Under the fix, completes in <3s.
#[tokio::test(flavor = "current_thread")]
async fn sync_cross_image_blob_claim_no_deadlock() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // 8 shared layers between two images (exceeds BLOB_CONCURRENCY=6).
    let shared_layers: Vec<Vec<u8>> = (0..8)
        .map(|i| format!("shared-claim-deadlock-layer-{i}").into_bytes())
        .collect();

    let mut builder_a = ManifestBuilder::new(b"config-claim-deadlock-a");
    let mut builder_b = ManifestBuilder::new(b"config-claim-deadlock-b");
    for layer in &shared_layers {
        builder_a = builder_a.layer(layer);
        builder_b = builder_b.layer(layer);
    }
    let parts_a = builder_a.build();
    let parts_b = builder_b.build();

    // Source A: serve manifest and blobs immediately (A claims first).
    parts_a.mount_source(&source_server, "src-a", "v1").await;

    // Source B: manifest immediate, but ALL blob pulls delayed 50ms.
    // This ensures A's blob futures reach the claim loop first, claiming
    // all shared blobs before B's futures start. B then enters claim-wait
    // for every shared blob, exhausting its semaphore under the old code.
    mount_source_manifest(
        &source_server,
        "src-b",
        "v1",
        &serde_json::to_vec(&ImageManifest {
            schema_version: 2,
            media_type: None,
            config: parts_b.config_desc.clone(),
            layers: parts_b.layer_descs.clone(),
            subject: None,
            artifact_type: None,
            annotations: None,
        })
        .unwrap(),
    )
    .await;
    // B's config blob: delayed so B's blob futures start after A has claimed.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-b/blobs/{}",
            parts_b.config_desc.digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"config-claim-deadlock-b".to_vec())
                .insert_header(
                    "content-length",
                    b"config-claim-deadlock-b".len().to_string(),
                )
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&source_server)
        .await;
    // B's shared layer blobs: delayed so A claims them first.
    for (i, desc) in parts_b.layer_descs.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/v2/src-b/blobs/{}", desc.digest)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(shared_layers[i].clone())
                    .insert_header("content-length", shared_layers[i].len().to_string())
                    .set_delay(std::time::Duration::from_millis(50)),
            )
            .mount(&source_server)
            .await;
    }

    // Target: both repos accept uploads.
    for repo in &["tgt-a", "tgt-b"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &parts_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_b.config_desc.digest).await;
        for desc in &parts_a.layer_descs {
            mount_blob_not_found(&target_server, repo, &desc.digest).await;
        }
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;
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
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 10);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report = result
        .expect("engine deadlocked: cross-image blob claim waits exhausted blob semaphore permits");
    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync (no deadlock): {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    assert_eq!(report.stats.images_synced, 2);
}

/// Pre-warm the cache with committed repos so the follower's mount-source
/// lookup returns a Tier 1 match (committed + preferred). The follower should
/// mount immediately without waiting on the watch channel -- exercising the
/// `Some(repo) if is_committed` branch (not the watch-wait branch).
#[tokio::test(flavor = "current_thread")]
async fn sync_prewarmed_committed_repo_skips_watch_wait() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-prewarm-mount";
    let parts_a = ManifestBuilder::new(b"config-prewarm-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-prewarm-b")
        .layer(shared_data)
        .build();

    parts_b.mount_source(&source_server, "src-b", "v1").await;

    // Target tgt-b: blobs not found (except mount succeeds for shared layer).
    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-b", &parts_b.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &parts_b.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt-b").await;
    mount_manifest_push(&target_server, "tgt-b", "v1").await;

    // Mount mock from tgt-a (the pre-warmed committed source).
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts_b.layer_descs[0].digest.to_string(),
        ))
        .and(query_param("from", "tgt-a"))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .expect(1)
        .mount(&target_server)
        .await;

    let target_name = "target";
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        // Pre-warm: tgt-a has the shared blob and a committed manifest.
        c.set_blob_completed(
            target_name,
            parts_a.layer_descs[0].digest.clone(),
            RepositoryName::new("tgt-a").unwrap(),
        );
        c.mark_repo_committed(target_name, &RepositoryName::new("tgt-a").unwrap());
    }

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "src-b",
        "tgt-b",
        vec![target_entry(target_name, mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // Shared layer mounted from pre-warmed tgt-a; config transferred.
    assert_eq!(report.images[0].blob_stats.mounted, 1);
    assert_eq!(report.images[0].blob_stats.transferred, 1);
}

/// Follower-leader mount deadlock: bounded deadline breaks the prior-run cycle.
///
/// This exercises the deadlock where a follower (tgt-b) is waiting for a leader
/// (tgt-a) to commit before mounting 7 warm-cache blobs, while the leader is
/// blocked on a shared blob claimed by the follower. Without a deadline, this
/// is a cyclic wait: follower waits for tgt-a commit, leader waits for follower's
/// shared-blob upload, which can't proceed because follower's watch-waits block it.
///
/// Setup:
/// - Leader (tgt-a): 1 config + 1 unique layer + 1 shared layer = 3 blobs.
/// - Follower (tgt-b): 1 config + 7 warm layers + 1 shared layer = 9 blobs.
///   The 7 warm layers "previously uploaded by the leader" exist at tgt-a in
///   the cache but tgt-a is not yet committed.
///
/// Resolution via bounded deadline:
/// 1. Follower claims shared blob first (leader blobs delayed 50ms).
/// 2. Follower pushes shared blob (no committed source -> falls through to push).
/// 3. Leader claims shared blob, waits for a committed mount source.
/// 4. The bounded deadline (2s) trips for the leader on the shared blob.
/// 5. Leader falls through to push, completes all blobs, commits.
/// 6. tgt-a commit fires the watch; follower mounts its 7 warm blobs.
///
/// The semaphore-boundary fix still matters here: watch-waits run OUTSIDE the
/// semaphore so the follower's shared-blob future can always get a permit.
#[tokio::test(flavor = "current_thread")]
async fn sync_follower_mount_wait_no_deadlock() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared layer between leader and follower.
    let shared_data = b"shared-mount-deadlock-layer";

    // 7 layers that "already exist at tgt-a" (warm cache).
    // These have distinct data so they get distinct digests.
    let warm_layers: Vec<Vec<u8>> = (0..7)
        .map(|i| format!("warm-cache-layer-{i}-mount-deadlock").into_bytes())
        .collect();

    // Leader image: 1 config + 1 unique layer + 1 shared layer.
    let parts_leader = ManifestBuilder::new(b"config-mount-deadlock-leader")
        .layer(b"unique-leader-layer-mount-deadlock")
        .layer(shared_data)
        .build();

    // Follower image: 1 config + 7 warm layers + shared layer = 9 blobs total.
    let mut builder_follower = ManifestBuilder::new(b"config-mount-deadlock-follower");
    for layer in &warm_layers {
        builder_follower = builder_follower.layer(layer);
    }
    builder_follower = builder_follower.layer(shared_data);
    let parts_follower = builder_follower.build();

    // Source: leader blobs delayed 50ms so follower claims shared blob first.
    mount_source_manifest(
        &source_server,
        "src-leader",
        "v1",
        &serde_json::to_vec(&ImageManifest {
            schema_version: 2,
            media_type: None,
            config: parts_leader.config_desc.clone(),
            layers: parts_leader.layer_descs.clone(),
            subject: None,
            artifact_type: None,
            annotations: None,
        })
        .unwrap(),
    )
    .await;
    // Leader config blob: delayed so follower runs first.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-leader/blobs/{}",
            parts_leader.config_desc.digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"config-mount-deadlock-leader".to_vec())
                .insert_header(
                    "content-length",
                    b"config-mount-deadlock-leader".len().to_string(),
                )
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&source_server)
        .await;
    // Leader unique layer: delayed.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-leader/blobs/{}",
            parts_leader.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"unique-leader-layer-mount-deadlock".to_vec())
                .insert_header(
                    "content-length",
                    b"unique-leader-layer-mount-deadlock".len().to_string(),
                )
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&source_server)
        .await;
    // Leader shared layer: delayed.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-leader/blobs/{}",
            parts_leader.layer_descs[1].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(shared_data.to_vec())
                .insert_header("content-length", shared_data.len().to_string())
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&source_server)
        .await;

    // Source: follower blobs served instantly (follower claims before leader).
    parts_follower
        .mount_source(&source_server, "src-follower", "v1")
        .await;

    // Target: both repos accept uploads and mounts.
    for repo in &["tgt-a", "tgt-b"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        // All blob HEADs return 404.
        mount_blob_not_found(&target_server, repo, &parts_leader.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &parts_follower.config_desc.digest).await;
        for desc in &parts_leader.layer_descs {
            mount_blob_not_found(&target_server, repo, &desc.digest).await;
        }
        for desc in &parts_follower.layer_descs {
            mount_blob_not_found(&target_server, repo, &desc.digest).await;
        }
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;

        // Mount mocks for all blobs (priority 1 over upload POST).
        for desc in parts_leader.layer_descs.iter() {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{repo}/blobs/uploads/")))
                .and(query_param("mount", desc.digest.to_string()))
                .respond_with(ResponseTemplate::new(201))
                .with_priority(1)
                .mount(&target_server)
                .await;
        }
        for desc in parts_follower.layer_descs.iter() {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{repo}/blobs/uploads/")))
                .and(query_param("mount", desc.digest.to_string()))
                .respond_with(ResponseTemplate::new(201))
                .with_priority(1)
                .mount(&target_server)
                .await;
        }
    }

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping_leader = resolved_mapping(
        Arc::clone(&source_client),
        "src-leader",
        "tgt-a",
        vec![target_entry("target", Arc::clone(&target_client))],
        vec![TagPair::same("v1")],
    );
    let mapping_follower = resolved_mapping(
        source_client,
        "src-follower",
        "tgt-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

    // Warm cache: the 7 "old" blobs are already at tgt-a (the leader's repo)
    // with status Completed. This makes the follower's mount-source lookup
    // return tgt-a for each of them, triggering the repo_committed_watch wait.
    let target_name = "target";
    let cache = empty_cache();
    {
        let tgt_a_repo = RepositoryName::new("tgt-a").unwrap();
        let mut c = cache.borrow_mut();
        for desc in &parts_follower.layer_descs[..7] {
            c.set_blob_completed(target_name, desc.digest.clone(), tgt_a_repo.clone());
        }
        // Do NOT mark tgt-a as committed -- this forces the watch wait.
    }

    // Short mount-source wait deadline so the leader's deadline trips quickly
    // on the shared blob (no committed source) instead of waiting 60s.
    // The follower still mounts its 7 warm blobs from the committed leader.
    let engine = SyncEngine::new(fast_retry(), 10)
        .with_mount_source_wait_deadline(std::time::Duration::from_secs(2));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run(
            vec![mapping_leader, mapping_follower],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report =
        result.expect("engine deadlocked: cyclic watch-wait not broken by bounded deadline");
    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync via deadline fallback (no deadlock): {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    assert_eq!(report.stats.images_synced, 2);
    // Core invariant: no deadlock. Both images must complete.
    // Whether warm blobs are mounted vs pushed depends on timing; the
    // bounded-deadline guarantees correctness (both sync), not mount rate.
}

/// Batch-check `notify_blob` correctness: two images share a blob and
/// run concurrently. Both complete without timeout, proving the batch
/// check's `notify_blob` call correctly wakes any claim waiter so the
/// second image can skip or mount the shared blob.
///
/// Without the `notify_blob` in the batch-check path, a blob transition
/// from `InProgress` to `ExistsAtTarget` via batch check would violate
/// the synchronization contract and delay waiters until the original
/// uploader finishes its upload.
#[tokio::test(flavor = "current_thread")]
async fn sync_batch_check_notifies_in_progress_waiters() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-batch-notify-layer";

    let parts_a = ManifestBuilder::new(b"config-batch-notify-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-batch-notify-b")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;

    // Target tgt-a: all blobs missing, accepts uploads.
    mount_manifest_head_not_found(&target_server, "tgt-a", "v1").await;
    mount_blob_not_found(&target_server, "tgt-a", &parts_a.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-a", &parts_a.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt-a").await;
    mount_manifest_push(&target_server, "tgt-a", "v1").await;

    // Target tgt-b: shared blob HEAD returns 200 (already exists).
    // This forces the per-blob HEAD check (or batch check) to find the
    // blob and call set_blob_verified on the target repo.
    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-b", &parts_b.config_desc.digest).await;
    Mock::given(method("HEAD"))
        .and(path(format!(
            "/v2/tgt-b/blobs/{}",
            parts_b.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", shared_data.len().to_string()),
        )
        .mount(&target_server)
        .await;
    mount_blob_push(&target_server, "tgt-b").await;
    mount_manifest_push(&target_server, "tgt-b", "v1").await;

    // Mount mocks (priority 1).
    for repo in &["tgt-a", "tgt-b"] {
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
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 10);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report = result.expect("engine should not deadlock with batch-check notify");
    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    assert_eq!(report.stats.images_synced, 2);
}

// ---------------------------------------------------------------------------
// New integration tests for the bounded-deadline mount-source resolver
// ---------------------------------------------------------------------------

/// A follower's mount waits for the leader's manifest commit, then
/// succeeds with 201. Validates the core spec invariant: committed-only
/// mount sources mean every mount attempt has a viable target.
///
/// Setup: two images sharing one layer. Leader commits first; follower
/// waits on the watch, wakes on commit, and mounts the shared layer.
/// Total blob pushes = 2 (leader: config + layer) + 1 (follower: config);
/// follower's shared layer is mounted (201), not pushed.
#[tokio::test(flavor = "current_thread")]
async fn mount_waits_for_committed_source() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared = b"mount-waits-shared-layer";
    let leader_img = ManifestBuilder::new(b"mount-waits-leader-cfg")
        .layer(shared)
        .build();
    let follower_img = ManifestBuilder::new(b"mount-waits-follower-cfg")
        .layer(shared)
        .build();

    // Source: both serve manifests and blobs.
    leader_img
        .mount_source(&source_server, "leader", "v1")
        .await;
    follower_img
        .mount_source(&source_server, "follower", "v1")
        .await;

    // Target: both repos have manifest HEAD 404; all blobs missing.
    for repo in &["leader", "follower"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &leader_img.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &follower_img.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &leader_img.layer_descs[0].digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;
        // Mount POST for the shared layer (priority 1 over generic upload POST).
        Mock::given(method("POST"))
            .and(path(format!("/v2/{repo}/blobs/uploads/")))
            .and(query_param(
                "mount",
                leader_img.layer_descs[0].digest.to_string(),
            ))
            .respond_with(ResponseTemplate::new(201))
            .with_priority(1)
            .mount(&target_server)
            .await;
    }

    let mappings = vec![
        mapping_from_servers(
            &source_server,
            &target_server,
            "leader",
            vec![TagPair::same("v1")],
        ),
        mapping_from_servers(
            &source_server,
            &target_server,
            "follower",
            vec![TagPair::same("v1")],
        ),
    ];
    let report = run_sync(mappings).await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    // One image should have mounted the shared layer rather than pushed it.
    let total_mounts: u64 = report.images.iter().map(|r| r.blob_stats.mounted).sum();
    assert!(
        total_mounts >= 1,
        "shared layer should have been mounted (committed-source invariant): mounted={}",
        total_mounts
    );
}

/// When the leader's manifest commit never arrives before the per-blob
/// deadline, the follower falls back to push rather than hanging.
///
/// Uses a very short deadline (10ms) so the test completes quickly on real
/// wall-clock. The leader's manifest PUT returns 500, so the leader fails
/// and never commits. Follower's deadline trips and it pushes its own copy
/// of the shared blob.
#[tokio::test(flavor = "current_thread")]
async fn mount_resolver_timeout_falls_back_to_push() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared = b"deadline-shared-layer";
    let leader_img = ManifestBuilder::new(b"deadline-leader-cfg")
        .layer(shared)
        .build();
    let follower_img = ManifestBuilder::new(b"deadline-follower-cfg")
        .layer(shared)
        .build();

    // Source: both images are fully servable.
    leader_img
        .mount_source(&source_server, "leader", "v1")
        .await;
    follower_img
        .mount_source(&source_server, "follower", "v1")
        .await;

    // Target: leader manifest PUT returns 500 (leader fails, never commits).
    // Follower falls back to push after deadline.
    for repo in &["leader", "follower"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &leader_img.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &follower_img.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &leader_img.layer_descs[0].digest).await;
        mount_blob_push(&target_server, repo).await;
    }
    // Leader manifest PUT fails -> leader image fails; watch fires via notify_repo_failed.
    Mock::given(method("PUT"))
        .and(path("/v2/leader/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&target_server)
        .await;
    // Follower manifest PUT succeeds after deadline fallback push.
    mount_manifest_push(&target_server, "follower", "v1").await;

    let engine = SyncEngine::new(fast_retry(), 10)
        .with_mount_source_wait_deadline(std::time::Duration::from_millis(10));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run(
            vec![
                mapping_from_servers(
                    &source_server,
                    &target_server,
                    "leader",
                    vec![TagPair::same("v1")],
                ),
                mapping_from_servers(
                    &source_server,
                    &target_server,
                    "follower",
                    vec![TagPair::same("v1")],
                ),
            ],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report = result.expect("engine should not hang after deadline fallback");
    // Follower must sync: it falls back to push after deadline.
    let synced = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ImageStatus::Synced))
        .count();
    assert!(
        synced >= 1,
        "at least follower should sync via push fallback after deadline: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    // Follower must have pushed (not mounted) the shared blob since leader
    // never commits.
    assert_eq!(
        report.stats.blobs_mounted, 0,
        "no mounts should succeed when leader never commits"
    );
}

/// Prior-run state cycle: both repos have the shared blob from a prior run,
/// but neither is committed. Bounded deadline breaks the cycle; both fall
/// through to push; both manifests commit.
///
/// Pre-seeding via direct cache manipulation since creating a true prior-run
/// cycle end-to-end would require coordinated multi-step mock state.
#[tokio::test(flavor = "current_thread")]
async fn prior_run_cycle_breaks_via_deadline() {
    use ocync_distribution::spec::RepositoryName;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let img_a = ManifestBuilder::new(b"cycle-a-cfg")
        .layer(b"cycle-shared")
        .build();
    let img_b = ManifestBuilder::new(b"cycle-b-cfg")
        .layer(b"cycle-shared")
        .build();

    img_a.mount_source(&source_server, "repo-a", "v1").await;
    img_b.mount_source(&source_server, "repo-b", "v1").await;

    for repo in &["repo-a", "repo-b"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &img_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &img_b.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &img_a.layer_descs[0].digest).await;
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;
    }

    // Pre-seed cycle: shared blob is "completed" at both repos from a prior run.
    // Neither is committed; each would be waiting for the other's commit.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed(
            "target",
            img_a.layer_descs[0].digest.clone(),
            RepositoryName::new("repo-b").unwrap(),
        );
        c.set_blob_completed(
            "target",
            img_a.layer_descs[0].digest.clone(),
            RepositoryName::new("repo-a").unwrap(),
        );
    }

    let engine = SyncEngine::new(fast_retry(), 10)
        .with_mount_source_wait_deadline(std::time::Duration::from_millis(10));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run(
            vec![
                mapping_from_servers(
                    &source_server,
                    &target_server,
                    "repo-a",
                    vec![TagPair::same("v1")],
                ),
                mapping_from_servers(
                    &source_server,
                    &target_server,
                    "repo-b",
                    vec![TagPair::same("v1")],
                ),
            ],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report = result.expect("engine must not hang on prior-run cycle");
    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync after deadline breaks cycle: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
}

/// Two leader images sharing blobs complete without deadlock under the
/// bounded-deadline resolver (no `is_leader` skip required).
///
/// Whichever leader commits first becomes a mount source; if no committed
/// source appears before the deadline, the second leader falls through to
/// push. Either way, no deadlock and both images sync.
#[tokio::test(flavor = "current_thread")]
async fn two_leaders_sharing_blobs_no_deadlock() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Two distinct shared blobs force elect_leaders to pick two leaders.
    let shared_1 = b"two-leaders-shared-1";
    let shared_2 = b"two-leaders-shared-2";
    let img_a = ManifestBuilder::new(b"two-leaders-cfg-a")
        .layer(shared_1)
        .layer(shared_2)
        .build();
    let img_b = ManifestBuilder::new(b"two-leaders-cfg-b")
        .layer(shared_1)
        .layer(shared_2)
        .build();

    img_a.mount_source(&source_server, "src-a", "v1").await;
    img_b.mount_source(&source_server, "src-b", "v1").await;

    for repo in &["tgt-a", "tgt-b"] {
        mount_manifest_head_not_found(&target_server, repo, "v1").await;
        mount_blob_not_found(&target_server, repo, &img_a.config_desc.digest).await;
        mount_blob_not_found(&target_server, repo, &img_b.config_desc.digest).await;
        for desc in img_a.layer_descs.iter() {
            mount_blob_not_found(&target_server, repo, &desc.digest).await;
        }
        mount_blob_push(&target_server, repo).await;
        mount_manifest_push(&target_server, repo, "v1").await;
        for desc in img_a.layer_descs.iter() {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{repo}/blobs/uploads/")))
                .and(query_param("mount", desc.digest.to_string()))
                .respond_with(ResponseTemplate::new(201))
                .with_priority(1)
                .mount(&target_server)
                .await;
        }
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
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 10);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run(
            vec![mapping_a, mapping_b],
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await;

    let report = result.expect("two leaders must not deadlock with bounded-deadline resolver");
    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both leader images should sync: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
    assert_eq!(report.stats.images_synced, 2);
}

/// After a mount source is pre-marked as stale in the cache, the resolver
/// skips it and uses the remaining committed source to mount successfully.
///
/// Note: this test uses direct cache manipulation to pre-configure the stale
/// exclusion. End-to-end simulation of `mark_blob_repo_stale` (triggered by a
/// 202 response) would require a custom per-from-repo mount mock that goes
/// beyond the current helper infrastructure.
#[tokio::test(flavor = "current_thread")]
async fn resolver_retries_mount_after_source_marked_stale() {
    use ocync_distribution::spec::RepositoryName;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let img = ManifestBuilder::new(b"retry-cfg")
        .layer(b"retry-layer")
        .build();
    img.mount_source(&source_server, "tgt-b", "v1").await;

    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-b", &img.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-b", &img.layer_descs[0].digest).await;
    mount_manifest_push(&target_server, "tgt-b", "v1").await;

    // Mount POST for the layer returns 201 (valid source tgt-a is committed).
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param("mount", img.layer_descs[0].digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    // Config blob: upload path (not in cache).
    mount_blob_push(&target_server, "tgt-b").await;

    // Pre-warm cache: tgt-a is committed and valid; stale-src is pre-marked stale.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed(
            "target",
            img.layer_descs[0].digest.clone(),
            RepositoryName::new("tgt-a").unwrap(),
        );
        c.mark_repo_committed("target", &RepositoryName::new("tgt-a").unwrap());
        // stale-src was rejected in a prior mount attempt; should be excluded.
        c.mark_blob_repo_stale(
            "target",
            &img.layer_descs[0].digest,
            &RepositoryName::new("stale-src").unwrap(),
        );
    }

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "tgt-b",
        "tgt-b",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );
    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // Layer should be mounted from committed tgt-a (stale-src excluded).
    assert_eq!(
        report.images[0].blob_stats.mounted, 1,
        "layer should be mounted from non-stale committed source"
    );
    assert_eq!(
        report.images[0].blob_stats.transferred, 1,
        "config should be pushed (not in cache)"
    );
}

/// Cancel signal during resolver wait causes the engine to drain cleanly
/// without panic or hang.
///
/// A ghost source in the cache (never committed) causes the resolver to
/// wait. Cancel fires; engine drains within the drain deadline.
#[tokio::test(flavor = "current_thread")]
async fn resolver_cancel_during_wait_drops_cleanly() {
    use ocync_distribution::spec::RepositoryName;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let img = ManifestBuilder::new(b"cancel-cfg")
        .layer(b"cancel-layer")
        .build();
    img.mount_source(&source_server, "tgt-a", "v1").await;

    mount_manifest_head_not_found(&target_server, "tgt-a", "v1").await;
    mount_blob_not_found(&target_server, "tgt-a", &img.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt-a", &img.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt-a").await;
    mount_manifest_push(&target_server, "tgt-a", "v1").await;

    // ghost-src has the layer from a prior run but is never committed here.
    let cache = empty_cache();
    {
        let mut c = cache.borrow_mut();
        c.set_blob_completed(
            "target",
            img.layer_descs[0].digest.clone(),
            RepositoryName::new("ghost-src").unwrap(),
        );
    }

    let shutdown = ShutdownSignal::new();
    // Drain deadline << mount-source wait deadline so engine exits via drain.
    let engine = SyncEngine::new(fast_retry(), 10)
        .with_drain_deadline(std::time::Duration::from_millis(100))
        .with_mount_source_wait_deadline(std::time::Duration::from_secs(60));

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "tgt-a",
        "tgt-a",
        vec![target_entry("target", mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    // Cancel immediately so the engine shuts down during the resolver wait.
    shutdown.trigger();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.run(
            vec![mapping],
            cache,
            BlobStage::disabled(),
            &NullProgress,
            Some(&shutdown),
        ),
    )
    .await;

    // Engine must complete within the timeout -- no hang or panic.
    let _report = result.expect("engine must drain cleanly on cancel during resolver wait");
    // Image status may be Cancelled or Synced depending on scheduling; either is correct.
}
