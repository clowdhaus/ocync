//! Basic sync integration tests: happy path, skip on digest match, source errors,
//! multi-layer images, cross-repo mount, and multi-target scenarios.

mod helpers;

use ocync_distribution::spec::{ImageManifest, MediaType};
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

    let config_digest = make_digest("cc");
    let layer_digest = make_digest("11");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    // Target: manifest HEAD returns matching digest → should skip.
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
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
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
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
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
    let stale_digest = make_digest("5ca1e");
    mount_manifest_head_matching(&target_server, "repo", "v1", &stale_digest).await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
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
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    // Per-image stats.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    let expected_bytes = (config_data.len() + layer_data.len()) as u64;
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
    assert!(matches!(
        report.images[0].status,
        ImageStatus::Failed { .. }
    ));
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

    // Layer blob: always returns 429 (retryable) -- should exhaust retries.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", layer_desc.digest)))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;

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

    // Source: two repos with the same blobs (both fully pullable).
    mount_source_manifest(&source_server, "repo-a", "v1", &manifest_a_bytes).await;
    mount_source_manifest(&source_server, "repo-b", "v1", &manifest_b_bytes).await;
    mount_blob_pull(&source_server, "repo-a", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-a", &layer_desc.digest, layer_data).await;
    mount_blob_pull(&source_server, "repo-b", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo-b", &layer_desc.digest, layer_data).await;

    // Target for repo-a: no manifest, no blobs, accepts upload and mount.
    // Mount mocks use priority 1 so wiremock checks them before the generic
    // upload POST (priority 5, default) which also matches mount requests.
    mount_manifest_head_not_found(&target_server, "repo-a", "v1").await;
    mount_blob_not_found(&target_server, "repo-a", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-a", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo-a").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-a/blobs/uploads/"))
        .and(query_param("mount", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-a/blobs/uploads/"))
        .and(query_param("mount", layer_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-a", "v1").await;

    // Target for repo-b: no manifest, no blobs, accepts upload and mount.
    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    mount_blob_not_found(&target_server, "repo-b", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo-b", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo-b").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param("mount", config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param("mount", layer_desc.digest.to_string()))
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
// ---------------------------------------------------------------------------
// ManifestBuilder demonstration: happy path rewritten
// ---------------------------------------------------------------------------

/// Same test as `sync_happy_path` but using `ManifestBuilder` to demonstrate
/// the reduced boilerplate.
#[tokio::test]
async fn sync_happy_path_manifest_builder() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-data-mb")
        .layer(b"layer-data-mb")
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
    assert!(matches!(report.images[0].status, ImageStatus::Synced));
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.stats.images_synced, 1);
}

/// Multi-layer manifest builder: verifies 3 layers all transfer correctly.
#[tokio::test]
async fn sync_three_layers_manifest_builder() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"cfg-3layer")
        .layer(b"base-layer")
        .layer(b"app-layer")
        .layer(b"runtime-layer")
        .build();

    parts.mount_source(&source_server, "app/svc", "v2").await;
    parts.mount_target(&target_server, "mirror/svc", "v2").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = resolved_mapping(
        source_client,
        "app/svc",
        "mirror/svc",
        vec![target_entry("ecr", target_client)],
        vec![TagPair::same("v2")],
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
    // 1 config + 3 layers = 4 blobs.
    assert_eq!(report.images[0].blob_stats.transferred, 4);
}
