//! Multi-target integration tests: platform filtering with pull-once fan-out,
//! and ECR immutable-tag handling on manifest push.

mod helpers;

use ocync_distribution::spec::{MediaType, PlatformFilter};
use ocync_sync::engine::TagPair;
use ocync_sync::{ErrorKind, ImageStatus, RunOutcome, SkipReason, SyncReport};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// Two targets, an index of linux/amd64 and linux/arm64, and a filter for
/// linux/amd64 only:
/// - the index and the amd64 child manifest are pulled once, and arm64 never
/// - each amd64 blob is pulled once per target (staging disabled)
/// - both targets receive amd64 blobs and manifest pushes, neither arm64
#[tokio::test]
async fn sync_platform_filter_multi_target() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let amd64 = ManifestBuilder::new(b"amd64-config-multi")
        .layer(b"amd64-layer-multi")
        .build();
    let arm64 = ManifestBuilder::new(b"arm64-config-multi")
        .layer(b"arm64-layer-multi")
        .build();
    let index = IndexBuilder::new()
        .manifest(&amd64, Some(linux("amd64")))
        .manifest(&arm64, Some(linux("arm64")))
        .build();

    // The amd64 manifest is pulled once and cached for both targets; its
    // blobs are pulled once per target. arm64 is never touched.
    source_manifest_mock("repo", "latest", &index.bytes, MediaType::OciIndex)
        .expect(1)
        .mount(&source_server)
        .await;
    source_manifest_mock(
        "repo",
        &amd64.digest.to_string(),
        &amd64.bytes,
        MediaType::OciManifest,
    )
    .expect(1)
    .mount(&source_server)
    .await;
    amd64
        .mount_blob_pulls_expecting(&source_server, "repo", 2)
        .await;
    arm64
        .mount_source_expecting(&source_server, "repo", &arm64.digest.to_string(), 0)
        .await;

    // Only amd64 is pushed. An arm64 push has no mock, so it would 404 and fail the sync.
    for target in [&target_a, &target_b] {
        amd64
            .mount_target(target, "repo", &amd64.digest.to_string())
            .await;
        mount_manifest_head_not_found(target, "repo", "latest").await;
        mount_manifest_push(target, "repo", "latest").await;
    }

    let mut mapping = mapping_to_targets(
        &source_server,
        &[("target-a", &target_a), ("target-b", &target_b)],
        "repo",
        vec![TagPair::same("latest")],
    );
    mapping.platforms = Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]);

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .all(|r| matches!(r.status, ImageStatus::Synced)),
        "both targets must be Synced"
    );
    let expected_blob_bytes = (amd64.config_data.len() + amd64.layers_data[0].len()) as u64;
    for result in &report.images {
        assert_eq!(
            result.blob_stats.transferred, 2,
            "each target must transfer exactly 2 amd64 blobs"
        );
        assert_eq!(result.blob_stats.skipped, 0);
        assert_eq!(result.bytes_transferred, expected_blob_bytes);
    }
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_transferred, 4);
    assert_eq!(report.stats.bytes_transferred, expected_blob_bytes * 2);
}

/// Syncs `src/nginx:v1.0` to `tgt/nginx` when the target answers the manifest
/// PUT with a 400 carrying `put_body`. Every source pull and target call is
/// counted, so the blobs must be transferred before the one push attempt (400
/// is not retried).
async fn sync_with_manifest_put_400(put_body: &str) -> (SyncReport, ManifestParts) {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-400")
        .layer(b"layer-400")
        .build();
    parts
        .mount_source_expecting(&source_server, "src/nginx", "v1.0", 1)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/v2/tgt/nginx/manifests/v1.0"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&target_server)
        .await;
    for digest in parts.blob_digests() {
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/tgt/nginx/blobs/{digest}")))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&target_server)
            .await;
    }
    // Small blobs upload monolithically (POST then PUT). No PATCH is mounted,
    // so one would 404 and fail the sync.
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
    Mock::given(method("PUT"))
        .and(path("/v2/tgt/nginx/manifests/v1.0"))
        .respond_with(ResponseTemplate::new(400).set_body_string(put_body))
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "src/nginx",
        "tgt/nginx",
        vec![TagPair::same("v1.0")],
    );
    (run_sync(vec![mapping]).await, parts)
}

/// ECR's `ImageTagAlreadyExistsException` on the manifest PUT yields
/// `Skipped { ImmutableTag }`, not `Failed`, and the blob work already done
/// is reported.
#[tokio::test]
async fn sync_immutable_tag_skips_instead_of_failing() {
    let (report, parts) = sync_with_manifest_put_400(
        r#"{"errors":[{"code":"TAG_INVALID","message":"ImageTagAlreadyExistsException: The image tag 'v1.0' already exists"}]}"#,
    )
    .await;

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
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(
        report.images[0].bytes_transferred,
        parts.config_data.len() as u64 + parts.layers_data[0].len() as u64
    );
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.stats.images_failed, 0);
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.outcome(), RunOutcome::Clean);
}

/// Any other 400 on the manifest PUT still fails the image: only the specific
/// ECR exception takes the skip path.
#[tokio::test]
async fn sync_non_immutable_400_still_fails() {
    let (report, _) = sync_with_manifest_put_400(
        r#"{"errors":[{"code":"MANIFEST_INVALID","message":"manifest invalid"}]}"#,
    )
    .await;

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
