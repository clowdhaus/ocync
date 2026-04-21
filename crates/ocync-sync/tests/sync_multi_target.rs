//! Multi-target independence integration tests: target isolation, partial target
//! failure, and immutable tag handling in multi-target context.

mod helpers;

use ocync_distribution::spec::{MediaType, Platform, PlatformFilter};
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ErrorKind, ImageStatus, SkipReason};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

// ---------------------------------------------------------------------------
// Multi-target independence tests
// ---------------------------------------------------------------------------

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

    let amd64 = ManifestBuilder::new(b"amd64-config-multi")
        .layer(b"amd64-layer-multi")
        .build();
    let arm64 = ManifestBuilder::new(b"arm64-config-multi")
        .layer(b"arm64-layer-multi")
        .build();

    // --- Build index with platform-annotated descriptors ---

    let index = IndexBuilder::new()
        .manifest(
            &amd64,
            Some(Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
                os_version: None,
                os_features: None,
            }),
        )
        .manifest(
            &arm64,
            Some(Platform {
                os: "linux".into(),
                architecture: "arm64".into(),
                variant: None,
                os_version: None,
                os_features: None,
            }),
        )
        .build();

    // --- Source: serve index by tag, children by digest ---

    // Index pulled exactly once (pull-once fan-out invariant).
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(index.bytes.clone())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // amd64 child: expect exactly 1 pull (platform matches; pulled once
    // during discovery and cached for both targets).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", amd64.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // arm64 child: expect 0 pulls (filtered out).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", arm64.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // amd64 blobs: each pulled once per target (staging disabled -> 2 pulls total).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", amd64.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64.config_data.clone())
                .insert_header("content-length", amd64.config_data.len().to_string()),
        )
        .expect(2)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            amd64.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(amd64.layers_data[0].clone())
                .insert_header("content-length", amd64.layers_data[0].len().to_string()),
        )
        .expect(2)
        .mount(&source_server)
        .await;

    // arm64 blobs: expect 0 pulls (filtered out).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", arm64.config_desc.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64.config_data.clone())
                .insert_header("content-length", arm64.config_data.len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            arm64.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(arm64.layers_data[0].clone())
                .insert_header("content-length", arm64.layers_data[0].len().to_string()),
        )
        .expect(0)
        .mount(&source_server)
        .await;

    // --- Both targets: no existing manifest, no blobs, accept pushes ---

    for target in [&target_a, &target_b] {
        // HEAD check for index tag.
        mount_manifest_head_not_found(target, "repo", "latest").await;
        // Blob checks and pushes for amd64 only.
        mount_blob_not_found(target, "repo", &amd64.config_desc.digest).await;
        mount_blob_not_found(target, "repo", &amd64.layer_descs[0].digest).await;
        mount_blob_push(target, "repo").await;
        // Accept amd64 child manifest push (by digest).
        mount_manifest_push(target, "repo", &amd64.digest.to_string()).await;
        // Accept filtered index push (by tag).
        mount_manifest_push(target, "repo", "latest").await;
        // arm64 manifest pushes must NOT arrive -- no mock mounted for them.
    }

    let mut mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        vec![TagPair::same("latest")],
    );
    mapping.platforms = Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]);

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
    let expected_blob_bytes = (amd64.config_data.len() + amd64.layers_data[0].len()) as u64;
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

/// ECR immutable tag: manifest push returns HTTP 400 with
/// `ImageTagAlreadyExistsException` -> engine produces `Skipped { ImmutableTag }`,
/// NOT `Failed`. Blobs are transferred before the manifest push, so the image
/// result should show the blob work that was done.
#[tokio::test]
async fn sync_immutable_tag_skips_instead_of_failing() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-immutable")
        .layer(b"layer-immutable")
        .build();

    // Source: serve manifest and blobs normally.
    Mock::given(method("GET"))
        .and(path("/v2/src/nginx/manifests/v1.0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src/nginx/blobs/{}",
            parts.config_desc.digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.config_data.clone())
                .insert_header("content-length", parts.config_data.len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src/nginx/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(parts.layers_data[0].clone())
                .insert_header("content-length", parts.layers_data[0].len().to_string()),
        )
        .expect(1)
        .mount(&source_server)
        .await;

    // Target: manifest HEAD 404, blob HEADs 404, blob push, manifest PUT -> 400.
    // All target endpoints use inline mocks with expect(N) to verify the engine
    // transferred blobs before attempting the manifest push.
    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/nginx/manifests/v1.0"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;

    // Blob HEAD checks -- one per blob.
    Mock::given(method("HEAD"))
        .and(path(format!(
            "/v2/tgt/nginx/blobs/{}",
            parts.config_desc.digest
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path(format!(
            "/v2/tgt/nginx/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;

    // Monolithic blob push: POST initiate + PUT finalize -- no PATCH for small blobs.
    Mock::given(method("POST"))
        .and(path("/v2/tgt/nginx/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/tgt/nginx/blobs/uploads/mono-id"),
        )
        .expect(2)
        .mount(&target_server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/v2/tgt/nginx/blobs/uploads/mono-id"))
        .respond_with(ResponseTemplate::new(201))
        .expect(2)
        .mount(&target_server)
        .await;

    // No PATCH registered -- any PATCH would cause a wiremock 404 and fail the test.

    // Manifest PUT returns ECR immutable tag error (HTTP 400).
    Mock::given(method("PUT"))
        .and(path("/v2/tgt/nginx/manifests/v1.0"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                r#"{"errors":[{"code":"TAG_INVALID","message":"ImageTagAlreadyExistsException: The image tag 'v1.0' already exists"}]}"#,
            ),
        )
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "src/nginx",
        "tgt/nginx",
        vec![target_entry("ecr-target", mock_client(&target_server))],
        vec![TagPair::same("v1.0")],
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

    // Per-image: skipped with ImmutableTag reason, NOT failed.
    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(
            report.images[0].status,
            ImageStatus::Skipped {
                reason: SkipReason::ImmutableTag,
            }
        ),
        "expected Skipped/ImmutableTag, got: {:?}",
        report.images[0].status
    );

    // Blobs were transferred before the manifest push was attempted.
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(
        report.images[0].bytes_transferred,
        parts.config_data.len() as u64 + parts.layers_data[0].len() as u64
    );

    // Aggregate stats: counted as skipped, NOT failed.
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.stats.images_failed, 0);
    assert_eq!(report.stats.blobs_transferred, 2);

    // Exit code: 0 (skipped is success, not failure).
    assert_eq!(report.exit_code(), 0);
    // wiremock expect(N) verifies: 1 source manifest pull, 1 target manifest HEAD,
    // 1 config blob pull, 1 layer blob pull, 1 manifest PUT (rejected).
}

/// Non-immutable 400 errors on manifest push still produce `Failed`, not `Skipped`.
/// This is the negative assertion -- ensures only the specific ECR exception triggers
/// the skip path.
#[tokio::test]
async fn sync_non_immutable_400_still_fails() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-400")
        .layer(b"layer-400")
        .build();

    // Source: expect(1) on manifest pull to verify pull-once.
    Mock::given(method("GET"))
        .and(path("/v2/src/app/manifests/latest"))
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
        "src/app",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src/app",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;

    mount_manifest_head_not_found(&target_server, "tgt/app", "latest").await;
    mount_blob_not_found(&target_server, "tgt/app", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "tgt/app", &parts.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "tgt/app").await;

    // Manifest PUT returns 400 but NOT ImageTagAlreadyExistsException.
    // 400 is not retryable, so expect exactly 1 attempt.
    Mock::given(method("PUT"))
        .and(path("/v2/tgt/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"errors":[{"code":"MANIFEST_INVALID","message":"manifest invalid"}]}"#,
        ))
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "src/app",
        "tgt/app",
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

    // Must be Failed with ManifestPush kind, NOT Skipped.
    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(
            report.images[0].status,
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                ..
            }
        ),
        "expected Failed/ManifestPush, got: {:?}",
        report.images[0].status
    );
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.stats.images_skipped, 0);
}
