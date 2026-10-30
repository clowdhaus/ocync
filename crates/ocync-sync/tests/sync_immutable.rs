//! Immutable tag skip integration tests: a tag matching the immutable glob that
//! already exists at every target is skipped with no API calls.

mod helpers;

use ocync_sync::engine::{ResolvedMapping, TagPair, TargetEntry};
use ocync_sync::filter::build_glob_set;
use ocync_sync::{ImageStatus, SkipReason};
use wiremock::MockServer;

use helpers::*;

/// A mapping whose immutable glob is the standard semver pattern.
fn immutable_mapping(
    source: &MockServer,
    source_repo: &str,
    target_repo: &str,
    targets: Vec<TargetEntry>,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    ResolvedMapping {
        immutable_glob: Some(build_glob_set(&["v[0-9]*.[0-9]*.[0-9]*".into()]).unwrap()),
        ..resolved_mapping(mock_client(source), source_repo, target_repo, targets, tags)
    }
}

/// A target that already holds `existing_tags`.
fn target_with_tags(name: &str, server: &MockServer, existing_tags: &[&str]) -> TargetEntry {
    TargetEntry {
        existing_tags: existing_tags.iter().map(|t| t.to_string()).collect(),
        ..target_entry(name, mock_client(server))
    }
}

/// A tag that matches the glob and exists at the target is skipped with no
/// source or target request at all.
#[tokio::test]
async fn immutable_tag_skip_when_present_at_target() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // No mocks are mounted: any request would 404, and the request counts
    // below would catch it.
    let mapping = immutable_mapping(
        &source_server,
        "library/nginx",
        "mirror/nginx",
        vec![target_with_tags(
            "target-reg",
            &target_server,
            &["v1.2.3", "v1.0.0", "v2.0.0"],
        )],
        vec![TagPair::same("v1.2.3"), TagPair::same("v1.0.0")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 2);
    for (i, image) in report.images.iter().enumerate() {
        assert_status!(
            report,
            i,
            ImageStatus::Skipped {
                reason: SkipReason::ImmutableTag,
            }
        );
        assert_eq!(image.bytes_transferred, 0);
    }
    assert_eq!(report.stats.images_skipped, 2);
    assert_eq!(report.stats.immutable_tag_skips, 2);

    let source_requests = source_server.received_requests().await.unwrap();
    let target_requests = target_server.received_requests().await.unwrap();
    assert_eq!(
        source_requests.len(),
        0,
        "immutable skip must issue zero source requests"
    );
    assert_eq!(
        target_requests.len(),
        0,
        "immutable skip must issue zero target requests"
    );
}

/// A tag that matches the glob but is absent from the target's `existing_tags`
/// falls through to normal discovery and syncs.
#[tokio::test]
async fn immutable_tag_not_skipped_when_absent_from_target() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"immutable-config")
        .layer(b"immutable-layer")
        .build();
    parts
        .mount_source(&source_server, "library/nginx", "v3.0.0")
        .await;
    parts
        .mount_target(&target_server, "mirror/nginx", "v3.0.0")
        .await;

    let mapping = immutable_mapping(
        &source_server,
        "library/nginx",
        "mirror/nginx",
        vec![target_with_tags(
            "target-reg",
            &target_server,
            &["v1.0.0", "v2.0.0"],
        )],
        vec![TagPair::same("v3.0.0")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.immutable_tag_skips, 0);
}

/// A tag that does not match the glob takes the HEAD and digest compare path
/// even when it exists at the target.
#[tokio::test]
async fn non_matching_tag_falls_through_to_head_check() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = make_digest("cc");
    let layer_digest = make_digest("11");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);
    mount_source_manifest(&source_server, "repo", "latest", &manifest_bytes).await;
    mount_manifest_head_matching(&target_server, "repo", "latest", &manifest_digest).await;

    let mapping = immutable_mapping(
        &source_server,
        "repo",
        "repo",
        vec![target_with_tags(
            "target",
            &target_server,
            &["latest", "v1.0.0"],
        )],
        vec![TagPair::same("latest")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(
        report,
        0,
        ImageStatus::Skipped {
            reason: SkipReason::DigestMatch,
        }
    );
    assert_eq!(
        report.stats.immutable_tag_skips, 0,
        "non-matching tags must not count as immutable skips",
    );
}

/// The tag must exist at ALL targets for the skip to fire: with it at target A
/// but not B, both targets sync.
#[tokio::test]
async fn immutable_tag_not_skipped_when_missing_from_one_target() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"mt-config")
        .layer(b"mt-layer")
        .build();
    parts.mount_source(&source_server, "repo", "v1.0.0").await;
    parts.mount_target(&target_a_server, "repo", "v1.0.0").await;
    parts.mount_target(&target_b_server, "repo", "v1.0.0").await;

    let mapping = immutable_mapping(
        &source_server,
        "repo",
        "repo",
        vec![
            target_with_tags("target-a", &target_a_server, &["v1.0.0"]),
            target_entry("target-b", mock_client(&target_b_server)),
        ],
        vec![TagPair::same("v1.0.0")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_all_synced(&report, 2, "one image per target");
    assert_eq!(
        report.stats.immutable_tag_skips, 0,
        "must not skip when any target is missing the tag",
    );
}
