//! Basic sync integration tests: happy path, skip on digest match, source errors,
//! multi-layer images, cross-repo mount, and multi-target scenarios.

mod helpers;

use ocync_distribution::spec::MediaType;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageStatus, SkipReason};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

#[tokio::test]
async fn sync_happy_path() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-data")
        .layer(b"layer-data")
        .build();

    parts
        .mount_source(&source_server, "library/nginx", "latest")
        .await;
    parts
        .mount_target(&target_server, "mirror/nginx", "latest")
        .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping(
        source_client,
        "library/nginx",
        "mirror/nginx",
        vec![target_entry("target-reg", target_client)],
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
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        report.images[0].bytes_transferred,
        parts.config_data.len() as u64 + parts.layers_data[0].len() as u64,
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

    let config_digest = make_digest("cc");
    let layer_digest = make_digest("11");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD returns matching digest -> should skip.
    mount_manifest_head_matching(&target_server, "repo", "v1", &manifest_digest).await;

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
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_blob_exists_at_target_skips_transfer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = make_digest("c1");
    let layer_digest = make_digest("b1");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // Source: serve manifest (blobs NOT served -- they shouldn't be pulled).
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD 404, but both blobs already exist.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_exists(&target_server, "repo", &config_digest).await;
    mount_blob_exists(&target_server, "repo", &layer_digest).await;
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
    assert_status!(report, 0, ImageStatus::Synced);
    // No bytes transferred -- blobs existed at target.
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
    assert_status!(report, 0, ImageStatus::Failed { .. });
    if let ImageStatus::Failed { kind, .. } = &report.images[0].status {
        assert_eq!(kind.to_string(), "manifest pull");
    }
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn sync_blob_transfer_retries_on_source_500() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer-data").build();

    // Source: manifest and config blob served normally by mount_source,
    // but we need custom mocks for the layer (500 then success).
    // Mount manifest + config only.
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;

    // Layer blob: first attempt 500, second attempt succeeds.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(parts.layers_data[0].clone()))
        .mount(&source_server)
        .await;

    // Target: standard setup.
    parts.mount_target(&target_server, "repo", "v1").await;

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
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.blobs_transferred, 2);
}

#[tokio::test]
async fn sync_dedup_across_tags() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: both tags serve the same manifest.
    parts.mount_source(&source_server, "repo", "v1").await;
    mount_source_manifest(&source_server, "repo", "v2", &parts.bytes).await;

    // Target: manifest HEAD 404 for both, blobs not found initially.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "repo", &desc.digest).await;
    }
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

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
    assert_status!(report, 0, ImageStatus::Synced);
    assert_status!(report, 1, ImageStatus::Synced);

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

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: serve manifest with expect(1) to verify pull-once fan-out.
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;

    // Both targets: no manifest, no blobs, accept pushes.
    for target in [&target_a, &target_b] {
        parts.mount_target(target, "repo", "v1").await;
    }

    let source_client = mock_client(&source_server);

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
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

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: serve manifest at source tag.
    parts.mount_source(&source_server, "repo", "latest").await;

    // Target: HEAD for target tag, blobs missing, push at target tag.
    parts.mount_target(&target_server, "repo", "stable").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping(
        source_client,
        "repo",
        "repo",
        vec![target_entry("target", target_client)],
        vec![TagPair::retag("latest", "stable")],
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

    let config_digest = make_digest("c5");
    let layer_digest = make_digest("b5");
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
    assert_status!(report, 0, ImageStatus::Failed { .. });
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn sync_index_manifest_multi_platform() {
    use ocync_distribution::spec::Platform;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Build two child image manifests (simulating amd64 and arm64).
    let amd64_parts = ManifestBuilder::new(b"amd64-config")
        .layer(b"amd64-layer")
        .build();
    let arm64_parts = ManifestBuilder::new(b"arm64-config")
        .layer(b"arm64-layer")
        .build();

    let amd64_platform = Platform {
        architecture: "amd64".to_string(),
        os: "linux".to_string(),
        variant: None,
        os_version: None,
        os_features: None,
    };
    let arm64_platform = Platform {
        architecture: "arm64".to_string(),
        os: "linux".to_string(),
        variant: None,
        os_version: None,
        os_features: None,
    };

    let index_parts = IndexBuilder::new()
        .manifest(&amd64_parts, Some(amd64_platform))
        .manifest(&arm64_parts, Some(arm64_platform))
        .build();

    index_parts
        .mount_source(&source_server, "repo", "latest")
        .await;
    index_parts
        .mount_target(&target_server, "repo", "latest")
        .await;

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
    assert_status!(report, 0, ImageStatus::Synced);
    // 4 blobs: 2 configs + 2 layers across two platforms.
    assert_eq!(report.images[0].blob_stats.transferred, 4);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (amd64_parts.config_data.len()
        + amd64_parts.layers_data[0].len()
        + arm64_parts.config_data.len()
        + arm64_parts.layers_data[0].len()) as u64;
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

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: serve manifest and blobs.
    parts.mount_source(&source_server, "repo", "v1").await;

    // Target: manifest HEAD returns a DIFFERENT digest -> should proceed.
    let stale_digest = make_digest("5ca1e");
    mount_manifest_head_matching(&target_server, "repo", "v1", &stale_digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
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
    // Per-image stats.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (parts.config_data.len() + parts.layers_data[0].len()) as u64;
    assert_eq!(report.images[0].bytes_transferred, expected_bytes);
    // Aggregate stats.
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.bytes_transferred, expected_bytes);
}

#[tokio::test]
async fn sync_empty_tags_produces_no_images() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", mock_client(&target_server))],
        vec![],
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

    assert!(report.images.is_empty());
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn sync_manifest_push_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: serve everything normally.
    parts.mount_source(&source_server, "repo", "v1").await;

    // Target: blobs succeed, but manifest PUT fails with 403.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "repo", &desc.digest).await;
    }
    mount_blob_push(&target_server, "repo").await;

    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

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
    assert_status!(report, 0, ImageStatus::Failed { .. });
    if let ImageStatus::Failed { kind, error, .. } = &report.images[0].status {
        assert_eq!(kind.to_string(), "manifest push");
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

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: manifest and config succeed normally.
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;

    // Layer blob: always returns 429 (retryable) -- should exhaust retries.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&source_server)
        .await;

    parts.mount_target(&target_server, "repo", "v1").await;

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
    assert_status!(report, 0, ImageStatus::Failed { .. });
    if let ImageStatus::Failed { retries, .. } = &report.images[0].status {
        assert_eq!(*retries, 2); // max_retries from fast_retry()
    }
    assert_eq!(report.stats.images_failed, 1);
}

#[tokio::test]
async fn sync_cross_repo_mount_success() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: two repos with the same blobs (both fully pullable).
    parts.mount_source(&source_server, "repo-a", "v1").await;
    parts.mount_source(&source_server, "repo-b", "v1").await;

    // Target for repo-a: no manifest, no blobs, accepts upload and mount.
    // Mount mocks use priority 1 so wiremock checks them before the generic
    // upload POST (priority 5, default) which also matches mount requests.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "repo-a", &desc.digest).await;
    }
    mount_blob_push(&target_server, "repo-a").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-a/blobs/uploads/"))
        .and(query_param("mount", parts.config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-a/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts.layer_descs[0].digest.to_string(),
        ))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: no manifest, no blobs, accepts upload and mount.
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "repo-b", &desc.digest).await;
    }
    mount_blob_push(&target_server, "repo-b").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param("mount", parts.config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts.layer_descs[0].digest.to_string(),
        ))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // First mapping: repo-a syncs normally (pull+push).
    let mapping_a = resolved_mapping(
        source_client.clone(),
        "repo-a",
        "repo-a",
        vec![target_entry("target", target_client.clone())],
        vec![TagPair::same("v1")],
    );

    // Second mapping: repo-b should mount from repo-a.
    let mapping_b = resolved_mapping(
        source_client,
        "repo-b",
        "repo-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both images should sync",
    );
    // Aggregate: one image uploads 2 blobs, the other mounts 2 (order-independent).
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

#[tokio::test]
async fn sync_cross_repo_mount_fallback_to_pull_push() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: two repos.
    parts.mount_source(&source_server, "repo-a", "v1").await;
    parts.mount_source(&source_server, "repo-b", "v1").await;

    // Target for repo-a: normal sync.
    parts.mount_target(&target_server, "repo-a", "v1").await;

    // Target for repo-b: mount returns 202 Accepted (fallback).
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "repo-b", &desc.digest).await;
    }
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

    let mapping_a = resolved_mapping(
        source_client.clone(),
        "repo-a",
        "repo-a",
        vec![target_entry("target", target_client.clone())],
        vec![TagPair::same("v1")],
    );

    let mapping_b = resolved_mapping(
        source_client,
        "repo-b",
        "repo-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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
    assert_status!(report, 0, ImageStatus::Synced);
    assert_status!(report, 1, ImageStatus::Synced);
    // Second mapping: mount fallback -> pull+push, so transferred not mounted.
    assert_eq!(report.images[1].blob_stats.mounted, 0);
    assert_eq!(report.images[1].blob_stats.transferred, 2);
}

#[tokio::test]
async fn sync_cross_repo_mount_failure_falls_back() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: two repos.
    parts.mount_source(&source_server, "repo-a", "v1").await;
    parts.mount_source(&source_server, "repo-b", "v1").await;

    // Target for repo-a: normal sync.
    parts.mount_target(&target_server, "repo-a", "v1").await;

    // Target for repo-b: mount returns 500 (error), falls back to pull+push.
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_server, "repo-b", &desc.digest).await;
    }
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

    let mapping_a = resolved_mapping(
        source_client.clone(),
        "repo-a",
        "repo-a",
        vec![target_entry("target", target_client.clone())],
        vec![TagPair::same("v1")],
    );

    let mapping_b = resolved_mapping(
        source_client,
        "repo-b",
        "repo-b",
        vec![target_entry("target", target_client)],
        vec![TagPair::same("v1")],
    );

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
    assert_status!(report, 0, ImageStatus::Synced);
    assert_status!(report, 1, ImageStatus::Synced);
    // Mount failed -> fell back to pull+push.
    assert_eq!(report.images[1].blob_stats.mounted, 0);
    assert_eq!(report.images[1].blob_stats.transferred, 2);
}

#[tokio::test]
async fn sync_multi_target_partial_blob_failure_isolates_targets() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    // Source: serve manifest and blobs.
    parts.mount_source(&source_server, "repo", "v1").await;

    // Target A: everything succeeds.
    parts.mount_target(&target_a, "repo", "v1").await;

    // Target B: blob push initiation returns 403 (non-retryable).
    mount_manifest_head_not_found(&target_b, "repo", "v1").await;
    mount_blob_not_found(&target_b, "repo", &parts.config_desc.digest).await;
    for desc in &parts.layer_descs {
        mount_blob_not_found(&target_b, "repo", &desc.digest).await;
    }
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_b)
        .await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
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
