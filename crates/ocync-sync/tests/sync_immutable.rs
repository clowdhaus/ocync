//! Immutable tag skip optimization integration tests: zero-API-call skip for
//! tags matching an immutable glob that already exist at all targets.

mod helpers;

use std::collections::HashSet;

use ocync_distribution::spec::{ImageManifest, MediaType};
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair, TargetEntry};
use ocync_sync::filter::build_glob_set;
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageStatus, SkipReason};
use wiremock::MockServer;

use helpers::*;

// ---------------------------------------------------------------------------
// Immutable tags skip optimization
// ---------------------------------------------------------------------------

/// Helper: build a `GlobSet` for the standard semver immutable pattern.
fn immutable_semver_glob() -> globset::GlobSet {
    build_glob_set(&["v[0-9]*.[0-9]*.[0-9]*".into()]).unwrap()
}

/// When a tag matches the `immutable_glob` pattern AND exists in the target's
/// `existing_tags`, the engine must skip with zero API calls - no HEAD, no pull.
///
/// Negative assertion: if the optimization were NOT taken, the source server
/// would receive at least one HEAD request and the test would fail.
#[tokio::test]
async fn immutable_tag_skip_when_present_at_target() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Do NOT mount any mocks - if the engine issues any request, the mock
    // server returns 500 which would cause the test to produce a failure
    // status instead of a skip.

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let target_tags: HashSet<String> = ["v1.2.3", "v1.0.0", "v2.0.0"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let targets = vec![TargetEntry {
        existing_tags: target_tags,
        ..target_entry("target-reg", target_client)
    }];
    let tags = vec![TagPair::same("v1.2.3"), TagPair::same("v1.0.0")];

    let mapping = ResolvedMapping {
        immutable_glob: Some(immutable_semver_glob()),
        ..resolved_mapping(
            source_client,
            "library/nginx",
            "mirror/nginx",
            targets,
            tags,
        )
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

    // Both tags should be skipped as immutable.
    assert_eq!(report.images.len(), 2);
    for image in &report.images {
        assert!(
            matches!(
                image.status,
                ImageStatus::Skipped {
                    reason: SkipReason::ImmutableTag,
                }
            ),
            "expected ImmutableTag skip, got {:?}",
            image.status,
        );
        assert_eq!(image.bytes_transferred, 0);
    }
    assert_eq!(report.stats.images_skipped, 2);
    assert_eq!(report.stats.immutable_tag_skips, 2);

    // Negative assertion: zero requests issued to either server.
    let source_requests = source_server.received_requests().await.unwrap();
    let target_requests = target_server.received_requests().await.unwrap();
    assert_eq!(
        source_requests.len(),
        0,
        "immutable skip must issue zero source requests, got {}",
        source_requests.len(),
    );
    assert_eq!(
        target_requests.len(),
        0,
        "immutable skip must issue zero target requests, got {}",
        target_requests.len(),
    );
}

/// When a tag matches the `immutable_glob` pattern but does NOT exist in the
/// target's `existing_tags`, the engine must fall through to normal discovery (not
/// skip). This tests a new tag that needs to be synced for the first time.
#[tokio::test]
async fn immutable_tag_not_skipped_when_absent_from_target() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"immutable-config";
    let layer_data = b"immutable-layer";
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

    // Source: serve the manifest and blobs.
    mount_source_manifest(&source_server, "library/nginx", "v3.0.0", &manifest_bytes).await;
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
    mount_manifest_head_not_found(&target_server, "mirror/nginx", "v3.0.0").await;
    mount_blob_not_found(&target_server, "mirror/nginx", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "mirror/nginx", &layer_desc.digest).await;
    mount_blob_push(&target_server, "mirror/nginx").await;
    mount_manifest_push(&target_server, "mirror/nginx", "v3.0.0").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // v3.0.0 is NOT in the target's existing_tags - it's a new tag.
    let target_tags: HashSet<String> = ["v1.0.0", "v2.0.0"].iter().map(|s| s.to_string()).collect();

    let targets = vec![TargetEntry {
        existing_tags: target_tags,
        ..target_entry("target-reg", target_client)
    }];

    let mapping = ResolvedMapping {
        immutable_glob: Some(immutable_semver_glob()),
        ..resolved_mapping(
            source_client,
            "library/nginx",
            "mirror/nginx",
            targets,
            vec![TagPair::same("v3.0.0")],
        )
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

    // Tag must be synced, not skipped.
    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "new immutable tag should sync, got {:?}",
        report.images[0].status,
    );
    assert_eq!(report.stats.immutable_tag_skips, 0);
}

/// Tags that do NOT match the `immutable_glob` pattern fall through to the
/// normal HEAD + digest compare path, even when the tag exists at the target.
#[tokio::test]
async fn non_matching_tag_falls_through_to_head_check() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = make_digest("cc");
    let layer_digest = make_digest("11");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source: serve manifest for HEAD.
    mount_source_manifest(&source_server, "repo", "latest", &manifest_bytes).await;

    // Target: manifest HEAD returns matching digest -> should skip via digest match.
    mount_manifest_head_matching(&target_server, "repo", "latest", &manifest_digest).await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // "latest" exists at target but does NOT match the pattern.
    let target_tags: HashSet<String> = ["latest", "v1.0.0"].iter().map(|s| s.to_string()).collect();

    let targets = vec![TargetEntry {
        existing_tags: target_tags,
        ..target_entry("target", target_client)
    }];

    let mapping = ResolvedMapping {
        immutable_glob: Some(immutable_semver_glob()),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            targets,
            vec![TagPair::same("latest")],
        )
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

    // "latest" does not match the immutable pattern, so it falls through to
    // HEAD + digest compare. The digest matches, so it should be skipped
    // with DigestMatch reason, NOT ImmutableTag.
    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(
            report.images[0].status,
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            }
        ),
        "non-matching tag should use digest compare, got {:?}",
        report.images[0].status,
    );
    assert_eq!(
        report.stats.immutable_tag_skips, 0,
        "non-matching tags must not count as immutable skips",
    );
}

/// Multi-target: tag must exist in ALL targets' `existing_tags` for the skip to
/// fire. When target A has the tag but target B does not, the engine must fall
/// through to normal discovery for both targets (not skip one and sync the other).
#[tokio::test]
async fn immutable_tag_not_skipped_when_missing_from_one_target() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    let config_data = b"mt-config";
    let layer_data = b"mt-layer";
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
    mount_source_manifest(&source_server, "repo", "v1.0.0", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Target A: manifest HEAD 404, push endpoints (needs sync).
    mount_manifest_head_not_found(&target_a_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_a_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_a_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_a_server, "repo").await;
    mount_manifest_push(&target_a_server, "repo", "v1.0.0").await;

    // Target B: manifest HEAD 404, push endpoints (needs sync).
    mount_manifest_head_not_found(&target_b_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_b_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_b_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_b_server, "repo").await;
    mount_manifest_push(&target_b_server, "repo", "v1.0.0").await;

    let source_client = mock_client(&source_server);

    // Target A has the tag, target B does not.
    let tags_a: HashSet<String> = ["v1.0.0"].iter().map(|s| s.to_string()).collect();

    let targets = vec![
        TargetEntry {
            existing_tags: tags_a,
            ..target_entry("target-a", mock_client(&target_a_server))
        },
        target_entry("target-b", mock_client(&target_b_server)),
    ];

    let mapping = ResolvedMapping {
        immutable_glob: Some(immutable_semver_glob()),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            targets,
            vec![TagPair::same("v1.0.0")],
        )
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

    // Both targets must be synced (not skipped) since target B is missing the tag.
    assert_eq!(report.images.len(), 2);
    for image in &report.images {
        assert!(
            matches!(image.status, ImageStatus::Synced),
            "expected Synced when one target is missing the tag, got {:?}",
            image.status,
        );
    }
    assert_eq!(
        report.stats.immutable_tag_skips, 0,
        "must not skip when any target is missing the tag",
    );
}
