//! Basic sync integration tests: happy path, skips, source and target errors,
//! retries, cross-repo mount and multi-target isolation.

mod helpers;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ocync_distribution::RegistryClientBuilder;
use ocync_distribution::Tag;
use ocync_distribution::auth::{AuthProvider, Presents, Scope, Token};
use ocync_distribution::spec::{MediaType, PlatformFilter};
use ocync_sync::engine::TagPair;
use ocync_sync::{ImageStatus, RunOutcome, SkipReason, SyncReport};
use wiremock::matchers::{any, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// What the target's manifest HEAD answers before the sync.
#[derive(Clone, Copy, Debug)]
enum TargetHead {
    /// The tag does not exist.
    Missing,
    /// The tag exists at a digest other than the source's, so it is not skipped.
    OtherDigest,
}

#[tokio::test]
async fn sync_happy_path() {
    for target_head in [TargetHead::Missing, TargetHead::OtherDigest] {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parts = ManifestBuilder::new(b"config-data")
            .layer(b"layer-data")
            .build();

        parts
            .mount_source(&source_server, "library/nginx", "latest")
            .await;
        match target_head {
            TargetHead::Missing => {
                mount_manifest_head_not_found(&target_server, "mirror/nginx", "latest").await;
            }
            TargetHead::OtherDigest => {
                mount_manifest_head_matching(
                    &target_server,
                    "mirror/nginx",
                    "latest",
                    &make_digest("5ca1e"),
                )
                .await;
            }
        }
        parts
            .mount_target_blob_misses(&target_server, "mirror/nginx")
            .await;
        mount_blob_push(&target_server, "mirror/nginx").await;
        mount_manifest_push(&target_server, "mirror/nginx", "latest").await;

        let mapping = mapping_with_distinct_repos(
            &source_server,
            &target_server,
            "library/nginx",
            "mirror/nginx",
            vec![TagPair::same("latest")],
        );

        let report = run_sync(vec![mapping]).await;

        let bytes = parts.config_data.len() as u64 + parts.layers_data[0].len() as u64;
        assert_eq!(report.images.len(), 1, "{target_head:?}");
        assert_status!(report, 0, ImageStatus::Synced, "{target_head:?}");
        assert_eq!(
            report.images[0].bytes_transferred, bytes,
            "{target_head:?}: bytes_transferred must equal sum of blob sizes"
        );
        assert_eq!(
            report.images[0].blob_stats.transferred, 2,
            "{target_head:?}"
        );
        assert_eq!(report.images[0].blob_stats.skipped, 0, "{target_head:?}");
        assert_eq!(report.stats.images_synced, 1, "{target_head:?}");
        assert_eq!(report.stats.images_failed, 0, "{target_head:?}");
        assert_eq!(report.stats.blobs_transferred, 2, "{target_head:?}");
        assert_eq!(report.stats.blobs_skipped, 0, "{target_head:?}");
        assert_eq!(report.stats.bytes_transferred, bytes, "{target_head:?}");
        assert_eq!(report.outcome(), RunOutcome::Clean, "{target_head:?}");
    }
}

#[tokio::test]
async fn sync_skip_on_digest_match() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = make_digest("cc");
    let layer_digest = make_digest("11");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    mount_manifest_head_matching(&target_server, "repo", "v1", &manifest_digest).await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
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
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(report.outcome(), RunOutcome::Clean);
}

#[tokio::test]
async fn sync_blob_exists_at_target_skips_transfer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_digest = make_digest("c1");
    let layer_digest = make_digest("b1");
    let manifest = simple_image_manifest(&config_digest, &layer_digest);
    let (manifest_bytes, _) = serialize_manifest(&manifest);

    // The source blobs are not served: pulling one would 404 and fail the sync.
    mount_source_manifest(&source_server, "repo", "v1", &manifest_bytes).await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_exists(&target_server, "repo", &config_digest).await;
    mount_blob_exists(&target_server, "repo", &layer_digest).await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.stats.blobs_skipped, 2);
}

#[tokio::test]
async fn sync_manifest_pull_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&source_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Failed { .. });
    if let ImageStatus::Failed { kind, .. } = &report.images[0].status {
        assert_eq!(kind.to_string(), "manifest pull");
    }
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.outcome(), RunOutcome::AllFailed);
}

#[tokio::test]
async fn sync_blob_transfer_retries_on_source_500() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer-data").build();

    // The layer GET fails once with 500, then succeeds.
    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    let layer_path = format!("/v2/repo/blobs/{}", parts.layer_descs[0].digest);
    Mock::given(method("GET"))
        .and(path(layer_path.as_str()))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&source_server)
        .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.layer_descs[0].digest,
        &parts.layers_data[0],
    )
    .await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.blobs_transferred, 2);
}

/// Hands out `token-0`, then `token-1` after a refusal, and so on.
struct RotatingAuth(AtomicU32);

impl AuthProvider for RotatingAuth {
    fn name(&self) -> &'static str {
        "rotating"
    }

    fn presents(&self) -> Presents {
        Presents::Credentials
    }

    fn get_token(
        &self,
        _scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, ocync_distribution::Error>> + Send + '_>> {
        let token = Token::new(format!("token-{}", self.0.load(Ordering::SeqCst)));
        Box::pin(async move { Ok(token) })
    }

    fn refuse(&self, _refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// A blob upload the target answers with 401 drops the token and is sent
/// again under a new one, and the image syncs. Breaks if the refusal fails
/// the image, or the upload is resent with the refused token.
#[tokio::test]
async fn sync_retries_an_upload_whose_token_was_refused() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let parts = ManifestBuilder::new(b"config").layer(b"layer-data").build();
    parts.mount_source(&source_server, "repo", "v1").await;
    Mock::given(method("PUT"))
        .and(query_param(
            "digest",
            parts.layer_descs[0].digest.to_string(),
        ))
        .and(header("authorization", "Bearer token-0"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(1)
        .expect(1)
        .mount(&target_server)
        .await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let target = RegistryClientBuilder::new(mock_url(&target_server))
        .auth(RotatingAuth(AtomicU32::new(0)))
        .build()
        .unwrap();
    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![target_entry("target", Arc::new(target))],
        vec![TagPair::same("v1")],
    );
    let report = run_sync(vec![mapping]).await;

    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.blobs_transferred, 2);
}

#[tokio::test]
async fn sync_dedup_across_tags() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    parts.mount_source(&source_server, "repo", "v1").await;
    mount_source_manifest(&source_server, "repo", "v2", &parts.bytes).await;
    parts.mount_target(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    // Sequential, so the first tag's blobs are known before the second runs.
    let report = run_sync_sequential(vec![mapping]).await;

    assert_eq!(report.images.len(), 2);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_status!(report, 1, ImageStatus::Synced);
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

    // The manifest is pulled once and fanned out to both targets.
    source_manifest_mock("repo", "v1", &parts.bytes, MediaType::OciManifest)
        .expect(1)
        .mount(&source_server)
        .await;
    parts.mount_blob_pulls(&source_server, "repo").await;
    for target in [&target_a, &target_b] {
        parts.mount_target(target, "repo", "v1").await;
    }

    let mapping = mapping_to_targets(
        &source_server,
        &[("target-a", &target_a), ("target-b", &target_b)],
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_all_synced(&report, 2, "one image per target");
    assert_eq!(
        report.stats.blobs_transferred, 4,
        "2 blobs per target x 2 targets"
    );
    assert_eq!(report.outcome(), RunOutcome::Clean);
}

#[tokio::test]
async fn sync_retag() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();

    parts.mount_source(&source_server, "repo", "latest").await;

    parts.mount_target(&target_server, "repo", "stable").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::retag("latest", "stable")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
}

/// A pinned source whose digest a target already holds touches the source not
/// at all, with `head_first` off: no HEAD, no GET. Breaks if the engine HEADs
/// or pulls the source before checking the targets against the pin, or the
/// target-side skip is lost.
#[tokio::test]
async fn sync_pinned_source_skips_a_target_that_holds_the_digest() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&source_server)
        .await;
    mount_manifest_head_matching(&target_server, "repo", "stable", &parts.digest).await;
    manifest_push_mock("repo", "stable")
        .expect(0)
        .mount(&target_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::pinned(
            parts.digest.clone(),
            Tag::new("stable").unwrap(),
        )],
    );

    let report = run_sync(vec![mapping]).await;

    assert_status!(
        report,
        0,
        ImageStatus::Skipped {
            reason: SkipReason::DigestMatch,
        }
    );
}

/// A pinned source with a platform filter fails that image before any source
/// request and pushes nothing, since filtering would push another digest under
/// the pinned name. Breaks if the guard is missing (the filtered index would
/// be pushed) or runs after the pull.
#[tokio::test]
async fn sync_pinned_source_with_platforms_fails_before_any_pull() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let child = ManifestBuilder::new(b"config").layer(b"layer").build();
    let index = IndexBuilder::new()
        .manifest(&child, Some(linux("amd64")))
        .build();
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&source_server)
        .await;
    mount_manifest_head_not_found(&target_server, "repo", "stable").await;
    manifest_push_mock("repo", "stable")
        .expect(0)
        .mount(&target_server)
        .await;

    let mut mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::pinned(
            index.digest.clone(),
            Tag::new("stable").unwrap(),
        )],
    );
    mapping.platforms = Some(vec!["linux/amd64".parse::<PlatformFilter>().unwrap()]);

    let report = run_sync(vec![mapping]).await;

    let ImageStatus::Failed { error, .. } = &report.images[0].status else {
        panic!("expected a failure, got {:?}", report.images[0].status);
    };
    assert!(error.contains("cannot be platform-filtered"), "{error}");
}

/// A source serving other bytes under a pinned digest fails that image with
/// both digests named and pushes nothing for it, while a sibling image in the
/// same run syncs. Breaks if the pin is not verified, the failure cancels the
/// run, or the mismatched manifest is pushed.
#[tokio::test]
async fn sync_pinned_digest_mismatch_fails_only_that_image() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let pinned = ManifestBuilder::new(b"pinned-config")
        .layer(b"pinned")
        .build();
    let served = ManifestBuilder::new(b"served-config")
        .layer(b"served")
        .build();
    let sibling = ManifestBuilder::new(b"sibling-config")
        .layer(b"sibling")
        .build();

    mount_source_manifest(
        &source_server,
        "repo",
        &pinned.digest.to_string(),
        &served.bytes,
    )
    .await;
    mount_manifest_head_not_found(&target_server, "repo", "bad").await;
    manifest_push_mock("repo", "bad")
        .expect(0)
        .mount(&target_server)
        .await;
    sibling
        .mount_source(&source_server, "repo", &sibling.digest.to_string())
        .await;
    sibling.mount_target(&target_server, "repo", "good").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![
            TagPair::pinned(pinned.digest.clone(), Tag::new("bad").unwrap()),
            TagPair::pinned(sibling.digest.clone(), Tag::new("good").unwrap()),
        ],
    );

    let report = run_sync(vec![mapping]).await;

    let status_of = |target: &str| {
        &report
            .images
            .iter()
            .find(|i| i.target.ends_with(&format!(":{target}")))
            .unwrap_or_else(|| panic!("no result for {target}: {:?}", report.images))
            .status
    };
    let ImageStatus::Failed {
        kind,
        error,
        retries,
        ..
    } = status_of("bad")
    else {
        panic!("expected the mismatch to fail, got {:?}", status_of("bad"));
    };
    assert_eq!(kind.to_string(), "manifest pull");
    assert!(
        error.contains(&pinned.digest.to_string()) && error.contains(&served.digest.to_string()),
        "error should name both digests: {error}"
    );
    assert_eq!(*retries, 0, "a digest mismatch is not retried");
    assert!(matches!(status_of("good"), ImageStatus::Synced));
    assert_eq!(report.outcome(), RunOutcome::Partial);
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

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &config_digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_digest).await;
    // 403 on upload initiation is not retryable.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Failed { .. });
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.outcome(), RunOutcome::AllFailed);
}

#[tokio::test]
async fn sync_index_manifest_multi_platform() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let amd64_parts = ManifestBuilder::new(b"amd64-config")
        .layer(b"amd64-layer")
        .build();
    let arm64_parts = ManifestBuilder::new(b"arm64-config")
        .layer(b"arm64-layer")
        .build();
    let index_parts = IndexBuilder::new()
        .manifest(&amd64_parts, Some(linux("amd64")))
        .manifest(&arm64_parts, Some(linux("arm64")))
        .build();

    index_parts
        .mount_source(&source_server, "repo", "latest")
        .await;
    index_parts
        .mount_target(&target_server, "repo", "latest")
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("latest")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // Two configs and two layers.
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
async fn sync_with_nothing_to_do_reports_clean() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let cases: [(&str, Vec<_>); 2] = [
        ("no mappings", vec![]),
        (
            "a mapping with no tags",
            vec![mapping_from_servers(
                &source_server,
                &target_server,
                "repo",
                vec![],
            )],
        ),
    ];
    for (name, mappings) in cases {
        let report = run_sync(mappings).await;

        assert!(report.images.is_empty(), "{name}");
        assert_eq!(report.stats.images_synced, 0, "{name}");
        assert_eq!(report.outcome(), RunOutcome::Clean, "{name}");
    }
}

#[tokio::test]
async fn sync_manifest_push_failure() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();
    parts.mount_source(&source_server, "repo", "v1").await;

    // Blobs upload, the manifest PUT is refused with 403.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    parts.mount_target_blob_misses(&target_server, "repo").await;
    mount_blob_push(&target_server, "repo").await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

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

    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &parts.config_desc.digest,
        &parts.config_data,
    )
    .await;
    // The layer always answers 429 (retryable), so retries run out.
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[0].digest
        )))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&source_server)
        .await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Failed { .. });
    if let ImageStatus::Failed { retries, .. } = &report.images[0].status {
        assert_eq!(*retries, 2, "max_retries from fast_retry()");
    }
    assert_eq!(report.stats.images_failed, 1);
}

/// Syncs `repo-a` and `repo-b` through one source client and one target
/// client, one image at a time, so the first image's blobs are known mount
/// sources for the second.
async fn sync_two_repos_one_at_a_time(source: &MockServer, target: &MockServer) -> SyncReport {
    let mappings = v1_mappings(
        source,
        target,
        &[("repo-a", "repo-a"), ("repo-b", "repo-b")],
    );
    run_sync_sequential(mappings).await
}

#[tokio::test]
async fn sync_cross_repo_mount_success() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();
    for repo in ["repo-a", "repo-b"] {
        parts.mount_source(&source_server, repo, "v1").await;
        parts
            .mount_target_answering_mounts(&target_server, repo, "v1", 201)
            .await;
    }

    let report = sync_two_repos_one_at_a_time(&source_server, &target_server).await;

    assert_all_synced(&report, 2, "both images should sync");
    // One image uploads its 2 blobs, the other mounts them (order-independent).
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_mounted, 2);
    assert!(
        report.images.iter().any(|r| r.blob_stats.mounted > 0),
        "at least one image should mount blobs from the other"
    );
    assert!(
        report.images.iter().any(|r| r.blob_stats.transferred > 0),
        "at least one image should transfer blobs"
    );
}

/// A refused mount falls back to pull and push: neither image ends with a
/// mounted blob, and the follower makes exactly one mount attempt per blob
/// before it transfers them.
#[tokio::test]
async fn sync_cross_repo_mount_falls_back_to_pull_push() {
    // (row, status answering the mount attempts).
    let cases = [("202 not fulfilled", 202), ("500 error", 500)];
    for (name, mount_status) in cases {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parts = ManifestBuilder::new(b"config").layer(b"layer").build();
        // Either repo may run second and so be the one that tries to mount, so
        // both refuse.
        for repo in ["repo-a", "repo-b"] {
            parts.mount_source(&source_server, repo, "v1").await;
            parts
                .mount_target_answering_mounts(&target_server, repo, "v1", mount_status)
                .await;
        }

        let report = sync_two_repos_one_at_a_time(&source_server, &target_server).await;

        assert_all_synced(&report, 2, name);
        assert_eq!(report.stats.blobs_mounted, 0, "{name}");
        // The leader pushes its 2 blobs and the follower, refused, pushes its own 2.
        assert_eq!(report.stats.blobs_transferred, 4, "{name}");
        assert_eq!(
            count_mount_posts(&target_server).await,
            2,
            "{name}: one mount attempt per blob"
        );
    }
}

#[tokio::test]
async fn sync_multi_target_partial_blob_failure_isolates_targets() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();
    parts.mount_source(&source_server, "repo", "v1").await;
    parts.mount_target(&target_a, "repo", "v1").await;

    // Target B refuses upload initiation with a non-retryable 403.
    mount_manifest_head_not_found(&target_b, "repo", "v1").await;
    parts.mount_target_blob_misses(&target_b, "repo").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_b)
        .await;

    let mapping = mapping_to_targets(
        &source_server,
        &[("target-a", &target_a), ("target-b", &target_b)],
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 2);
    // Results arrive in any order, so find them by status.
    let synced: Vec<_> = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ImageStatus::Synced))
        .collect();
    let failed = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ImageStatus::Failed { .. }))
        .count();
    assert_eq!(synced.len(), 1, "one target should succeed");
    assert_eq!(failed, 1, "one target should fail");
    assert_eq!(synced[0].blob_stats.transferred, 2);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_failed, 1);
}
