//! Artifact sync integration tests: referrer transfer, `require_artifacts`,
//! tag fallback, filters, blob dedup and failure isolation.

mod helpers;

use std::rc::Rc;

use ocync_distribution::Digest;
use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType};
use ocync_sync::engine::{ResolvedArtifacts, ResolvedMapping, TagPair};
use ocync_sync::{ErrorKind, ImageStatus};
use wiremock::http::Method;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

const COSIGN_SIGNATURE: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";

/// A mapping of `repo:v1.0.0` between the servers with the given artifacts config.
fn artifact_mapping(
    source: &MockServer,
    target: &MockServer,
    artifacts: ResolvedArtifacts,
) -> ResolvedMapping {
    ResolvedMapping {
        artifacts_config: Rc::new(artifacts),
        ..mapping_from_servers(source, target, "repo", vec![TagPair::same("v1.0.0")])
    }
}

/// Mount a referrers API that answers `parent_digest` with `status`.
async fn mount_referrers_error(server: &MockServer, parent_digest: &Digest, status: u16) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{parent_digest}")))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// When `artifacts.enabled = false`, no referrers API requests are made.
#[tokio::test]
async fn artifact_sync_disabled_issues_no_referrers_requests() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"art-cfg").layer(b"art-layer").build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    parent.mount_target(&target_server, "repo", "v1.0.0").await;

    let mapping = artifact_mapping(
        &source_server,
        &target_server,
        ResolvedArtifacts {
            enabled: false,
            ..ResolvedArtifacts::default()
        },
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert!(
        !report.images[0].artifacts_skipped,
        "artifacts_skipped must be false when artifacts are disabled (not a transient skip)"
    );

    assert_eq!(
        count_requests(&source_server, None, PathMatch::Contains("/referrers/")).await,
        0,
        "artifacts disabled must issue zero referrers requests"
    );
}

/// With artifacts enabled and a referrer listed, the engine pulls and pushes
/// the artifact manifest and its blobs.
#[tokio::test]
async fn artifact_sync_transfers_referrer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"parent-config")
        .layer(b"parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    let sig = ArtifactBuilder::new(b"sig-config", b"sig-payload").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    sig.mount_source(&source_server, "repo").await;
    parent.mount_target(&target_server, "repo", "v1.0.0").await;
    sig.mount_target(&target_server, "repo").await;

    let mapping = artifact_mapping(&source_server, &target_server, ResolvedArtifacts::default());

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Contains(&sig.digest.to_string()),
        )
        .await,
        1,
        "artifact manifest must be pushed to target"
    );
}

/// When the referrers API answers 404, the engine finds the artifact through
/// the `sha256-<hex>` tag fallback and transfers it.
#[tokio::test]
async fn artifact_sync_tag_fallback_transfers_referrer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"fb-parent-config")
        .layer(b"fb-parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;

    // Signature artifact built by hand: it needs a `subject`.
    let sig_config_data = b"fb-sig-config";
    let sig_layer_data = b"fb-sig-layer";
    let sig_config_desc = blob_descriptor(sig_config_data, MediaType::OciConfig);
    let sig_layer_desc = blob_descriptor(sig_layer_data, MediaType::OciLayerGzip);
    let sig_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sig_config_desc.clone(),
        layers: vec![sig_layer_desc.clone()],
        subject: Some(blob_descriptor(&parent.bytes, MediaType::OciManifest)),
        artifact_type: Some(COSIGN_SIGNATURE.into()),
        annotations: None,
    };
    let (sig_bytes, sig_digest) = serialize_manifest(&sig_manifest);
    let sig_digest_str = sig_digest.to_string();

    mount_referrers_error(&source_server, &parent.digest, 404).await;
    let referrers = ReferrersIndexBuilder::new()
        .descriptor(Descriptor {
            artifact_type: Some(COSIGN_SIGNATURE.into()),
            ..blob_descriptor(&sig_bytes, MediaType::OciManifest)
        })
        .build();
    source_manifest_mock(
        "repo",
        &parent.digest.tag_fallback(),
        &referrers,
        MediaType::OciIndex,
    )
    .mount(&source_server)
    .await;
    mount_source_manifest(&source_server, "repo", &sig_digest_str, &sig_bytes).await;
    mount_blob_pull(
        &source_server,
        "repo",
        &sig_config_desc.digest,
        sig_config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "repo",
        &sig_layer_desc.digest,
        sig_layer_data,
    )
    .await;

    parent.mount_target(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &sig_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_layer_desc.digest).await;
    mount_manifest_push(&target_server, "repo", &sig_digest_str).await;

    let mapping = artifact_mapping(&source_server, &target_server, ResolvedArtifacts::default());

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Contains(&sig_digest_str),
        )
        .await,
        1,
        "artifact manifest must be pushed via tag fallback path"
    );
}

/// With an include filter, only matching artifacts are transferred: the
/// cosign signature is pushed and the SBOM is not.
#[tokio::test]
async fn artifact_sync_include_filter_skips_non_matching() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"filt-parent-config")
        .layer(b"filt-parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    let sig = ArtifactBuilder::new(b"filt-sig-config", b"filt-sig-layer").build();
    let sbom = ArtifactBuilder::new(b"filt-sbom-config", b"filt-sbom-layer")
        .artifact_type("application/spdx+json")
        .build();
    let referrers = ReferrersIndexBuilder::new()
        .artifact(&sig)
        .artifact(&sbom)
        .build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    // Both are pullable, so only the filter keeps the SBOM out.
    sig.mount_source(&source_server, "repo").await;
    sbom.mount_source(&source_server, "repo").await;
    parent.mount_target(&target_server, "repo", "v1.0.0").await;
    sig.mount_target(&target_server, "repo").await;

    let mapping = artifact_mapping(
        &source_server,
        &target_server,
        ResolvedArtifacts {
            enabled: true,
            include: vec![COSIGN_SIGNATURE.into()],
            exclude: Vec::new(),
            require_artifacts: false,
        },
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Contains(&sig.digest.to_string()),
        )
        .await,
        1,
        "cosign signature must be pushed"
    );
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Contains(&sbom.digest.to_string()),
        )
        .await,
        0,
        "SBOM must NOT be pushed when include filter excludes it"
    );
}

/// `require_artifacts` means "require matching artifacts": referrers that exist
/// but are all excluded by the filter still fail the image.
#[tokio::test]
async fn artifact_require_fires_when_all_filtered_out() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"req-filt-cfg")
        .layer(b"req-filt-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    let referrers = ReferrersIndexBuilder::new()
        .descriptor(Descriptor {
            artifact_type: Some("application/spdx+json".into()),
            ..make_descriptor(make_digest("5b0e5b0e5b0e"), MediaType::OciManifest)
        })
        .build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    parent.mount_target(&target_server, "repo", "v1.0.0").await;

    // The source has only an SBOM; the filter admits only cosign.
    let mapping = artifact_mapping(
        &source_server,
        &target_server,
        ResolvedArtifacts {
            enabled: true,
            include: vec![COSIGN_SIGNATURE.into()],
            exclude: Vec::new(),
            require_artifacts: true,
        },
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    match &report.images[0].status {
        ImageStatus::Failed { kind, .. } => {
            assert!(
                matches!(kind, ErrorKind::RequiredArtifactsMissing),
                "expected RequiredArtifactsMissing, got {kind:?}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Artifact blobs that already exist at the target are not pulled from the source.
#[tokio::test]
async fn artifact_blob_dedup_skips_existing() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"dedup-parent-config")
        .layer(b"dedup-parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    let sig = ArtifactBuilder::new(b"dedup-sig-config", b"dedup-sig-layer").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    // The artifact's blobs are deliberately not served: a pull would 404.
    let sig_digest_str = sig.digest.to_string();
    mount_source_manifest(&source_server, "repo", &sig_digest_str, &sig.bytes).await;

    parent.mount_target(&target_server, "repo", "v1.0.0").await;
    mount_blob_exists(&target_server, "repo", &sig.config_desc.digest).await;
    mount_blob_exists(&target_server, "repo", &sig.layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", &sig_digest_str).await;

    let mapping = artifact_mapping(&source_server, &target_server, ResolvedArtifacts::default());

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        count_requests(
            &source_server,
            Some(&Method::GET),
            PathMatch::Contains(&sig.config_desc.digest.to_string())
        )
        .await
            + count_requests(
                &source_server,
                Some(&Method::GET),
                PathMatch::Contains(&sig.layer_desc.digest.to_string())
            )
            .await,
        0,
        "artifact blobs already at target must not be pulled from source"
    );
}

/// A referrers API that answers 500 leaves artifacts unconfirmed, so the image
/// still syncs and is flagged `artifacts_skipped`, even with `require_artifacts`
/// (which fires only on confirmed absence).
#[tokio::test]
async fn artifact_transient_error_sets_artifacts_skipped() {
    // (row, require_artifacts)
    let cases = [("require_artifacts", true), ("artifacts optional", false)];
    for (name, require_artifacts) in cases {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parent = ManifestBuilder::new(b"err-cfg").layer(b"err-layer").build();
        parent.mount_source(&source_server, "repo", "v1.0.0").await;
        mount_referrers_error(&source_server, &parent.digest, 500).await;
        parent.mount_target(&target_server, "repo", "v1.0.0").await;

        let mapping = artifact_mapping(
            &source_server,
            &target_server,
            ResolvedArtifacts {
                require_artifacts,
                ..ResolvedArtifacts::default()
            },
        );

        let report = run_sync(vec![mapping]).await;

        assert_eq!(report.images.len(), 1, "{name}");
        assert_status!(report, 0, ImageStatus::Synced, "{name}");
        assert!(report.images[0].artifacts_skipped, "{name}");
        assert_eq!(report.stats.artifacts_skipped, 1, "{name}");
    }
}

/// A referrers API that answers 200 with an empty index confirms there are no
/// artifacts: `require_artifacts` fails the image, otherwise it syncs, and in
/// neither case is `artifacts_skipped` set (only a transient error sets it).
#[tokio::test]
async fn artifact_empty_referrers_index_is_confirmed_absence() {
    // (row, require_artifacts): the image fails exactly when artifacts are required.
    let cases = [("require_artifacts", true), ("artifacts optional", false)];
    for (name, require_artifacts) in cases {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parent = ManifestBuilder::new(b"req-cfg").layer(b"req-layer").build();
        parent.mount_source(&source_server, "repo", "v1.0.0").await;
        let referrers = ReferrersIndexBuilder::new().build();
        mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
        parent.mount_target(&target_server, "repo", "v1.0.0").await;

        let mapping = artifact_mapping(
            &source_server,
            &target_server,
            ResolvedArtifacts {
                require_artifacts,
                ..ResolvedArtifacts::default()
            },
        );

        let report = run_sync(vec![mapping]).await;

        assert_eq!(report.images.len(), 1, "{name}");
        if require_artifacts {
            assert_status!(
                report,
                0,
                ImageStatus::Failed {
                    kind: ErrorKind::RequiredArtifactsMissing,
                    ..
                },
                "{name}"
            );
        } else {
            assert_status!(report, 0, ImageStatus::Synced, "{name}");
        }
        assert!(!report.images[0].artifacts_skipped, "{name}");
        assert_eq!(report.stats.artifacts_skipped, 0, "{name}");
    }
}

/// A 500 on the artifact manifest push fails the image with
/// `ErrorKind::ArtifactSync`, not `RequiredArtifactsMissing`.
#[tokio::test]
async fn artifact_transfer_failure_reports_artifact_sync_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"fail-parent-config")
        .layer(b"fail-parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    let sig = ArtifactBuilder::new(b"fail-sig-config", b"fail-sig-layer").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    sig.mount_source(&source_server, "repo").await;

    parent.mount_target(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &sig.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig.layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{}", sig.digest)))
        .respond_with(ResponseTemplate::new(500))
        .mount(&target_server)
        .await;

    let mapping = artifact_mapping(&source_server, &target_server, ResolvedArtifacts::default());

    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    match &report.images[0].status {
        ImageStatus::Failed { kind, error, .. } => {
            assert!(
                matches!(kind, ErrorKind::ArtifactSync),
                "expected ArtifactSync, got {kind:?}"
            );
            assert!(
                error.contains("manifest push failed"),
                "error should mention manifest push, got: {error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// An earlier run's cache lists an artifact's blobs at the target, but the
/// registry has lost them and refuses the artifact manifest. As for an image,
/// the run copies the blobs again and pushes the manifest once more, so the
/// image syncs in the same run.
#[tokio::test]
async fn artifact_refused_for_a_lost_blob_is_repaired_in_the_same_run() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let parent = ManifestBuilder::new(b"lost-parent-config")
        .layer(b"lost-parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    let sig = ArtifactBuilder::new(b"lost-sig-config", b"lost-sig-layer").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    sig.mount_source(&source_server, "repo").await;
    let sig_manifest = format!("/v2/repo/manifests/{}", sig.digest);
    Mock::given(method("PUT"))
        .and(path(sig_manifest.as_str()))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"errors":[{"code":"MANIFEST_BLOB_UNKNOWN","message":"blob unknown to registry"}]}"#,
        ))
        .up_to_n_times(1)
        .mount(&target_server)
        .await;
    parent.mount_target(&target_server, "repo", "v1.0.0").await;
    sig.mount_target(&target_server, "repo").await;

    let sig_blobs = [&sig.config_desc.digest, &sig.layer_desc.digest];
    let cache = cache_with("target", "repo", &sig_blobs);
    let mapping = artifact_mapping(&source_server, &target_server, ResolvedArtifacts::default());
    let report = run_sync_with_cache(vec![mapping], Rc::clone(&cache)).await;

    assert_status!(report, 0, ImageStatus::Synced);
    let requests = target_server.received_requests().await.unwrap();
    let pushes_of = |p: &str| {
        requests
            .iter()
            .filter(|r| r.method == Method::PUT && r.url.path() == p)
            .count()
    };
    assert_eq!(
        pushes_of(&sig_manifest),
        2,
        "refused once, then pushed again"
    );
    let sig_heads = requests
        .iter()
        .filter(|r| {
            r.method == Method::HEAD
                && sig_blobs
                    .iter()
                    .any(|d| r.url.path() == format!("/v2/repo/blobs/{d}"))
        })
        .count();
    assert_eq!(sig_heads, 2, "each dropped blob is checked again once");
}

/// With two targets and a transient referrers error, both report
/// `artifacts_skipped`. The second target reads the failure from the referrers
/// cache, so this fails if the cache swallows the skip signal.
#[tokio::test]
async fn artifact_transient_error_propagates_to_all_targets() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parent = ManifestBuilder::new(b"mt-cfg").layer(b"mt-layer").build();
    parent.mount_source(&source_server, "repo", "v1.0.0").await;
    mount_referrers_error(&source_server, &parent.digest, 500).await;
    for target in [&target_a, &target_b] {
        parent.mount_target(target, "repo", "v1.0.0").await;
    }

    let mapping = mapping_to_targets(
        &source_server,
        &[("target-a", &target_a), ("target-b", &target_b)],
        "repo",
        vec![TagPair::same("v1.0.0")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_all_synced(&report, 2, "both targets should sync successfully");
    assert!(
        report.images.iter().all(|r| r.artifacts_skipped),
        "both targets must report artifacts_skipped (cache propagation)"
    );
    assert_eq!(
        report.stats.artifacts_skipped, 2,
        "aggregate count must reflect both targets"
    );
}

/// Four tags of one signed image synced concurrently into one repo: the
/// signature is pulled and pushed once, not once per tag.
#[tokio::test]
async fn artifact_shared_by_tags_syncs_once() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"parent-config")
        .layer(b"parent-layer")
        .build();
    let sig = ArtifactBuilder::new(b"sig-config", b"sig-payload").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();
    for tag in SHARED_TAGS {
        parent.mount_source(&source_server, "repo", tag).await;
        parent.mount_target(&target_server, "repo", tag).await;
    }
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    sig.mount_source(&source_server, "repo").await;
    sig.mount_target(&target_server, "repo").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        same_tags(&SHARED_TAGS),
    );
    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.stats.images_synced, 4);
    let sig_path = format!("/v2/repo/manifests/{}", sig.digest);
    assert_eq!(
        count_requests(
            &source_server,
            Some(&Method::GET),
            PathMatch::Exact(&sig_path)
        )
        .await,
        1,
        "signature pulled per tag"
    );
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Exact(&sig_path)
        )
        .await,
        1,
        "signature pushed per tag"
    );
}

/// Two mappings from different sources sync one signed image into one
/// target repo. The signature cannot be pulled from one source; the other
/// source's mapping still syncs it, and the broken source's mapping fails.
#[tokio::test]
async fn artifact_failure_from_one_source_does_not_fail_another() {
    let good_source = MockServer::start().await;
    let broken_source = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"parent-config")
        .layer(b"parent-layer")
        .build();
    let sig = ArtifactBuilder::new(b"sig-config", b"sig-payload").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();
    for (source, tag) in [(&good_source, "good"), (&broken_source, "broken")] {
        parent.mount_source(source, "repo", tag).await;
        parent.mount_target(&target_server, "repo", tag).await;
        mount_referrers(source, "repo", &parent.digest, &referrers).await;
    }
    // Only the good source serves the signature itself.
    sig.mount_source(&good_source, "repo").await;
    sig.mount_target(&target_server, "repo").await;

    let mapping = |source: &MockServer, tag: &str| {
        mapping_from_servers(source, &target_server, "repo", same_tags(&[tag]))
    };
    let report = run_sync(vec![
        mapping(&broken_source, "broken"),
        mapping(&good_source, "good"),
    ])
    .await;

    let status = |tag: &str| {
        &report
            .images
            .iter()
            .find(|i| i.target.ends_with(&format!(":{tag}")))
            .expect("every tag is reported")
            .status
    };
    assert!(
        matches!(status("good"), ImageStatus::Synced),
        "the good source's mapping got the broken source's failure: {:?}",
        status("good")
    );
    assert!(
        matches!(status("broken"), ImageStatus::Failed { .. }),
        "the broken source's mapping reused the good source's copy: {:?}",
        status("broken")
    );
}

/// Two signatures of one image share their config blob. The blob is
/// uploaded to the target repo once, not once per artifact.
#[tokio::test]
async fn artifacts_sharing_a_blob_upload_it_once() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parent = ManifestBuilder::new(b"parent-config")
        .layer(b"parent-layer")
        .build();
    parent.mount_source(&source_server, "repo", "v1").await;
    parent.mount_target(&target_server, "repo", "v1").await;
    let first = ArtifactBuilder::new(b"shared-config", b"payload-one").build();
    let second = ArtifactBuilder::new(b"shared-config", b"payload-two").build();
    assert_eq!(first.config_desc.digest, second.config_desc.digest);
    let referrers = ReferrersIndexBuilder::new()
        .artifact(&first)
        .artifact(&second)
        .build();
    mount_referrers(&source_server, "repo", &parent.digest, &referrers).await;
    for artifact in [&first, &second] {
        artifact.mount_source(&source_server, "repo").await;
        artifact.mount_target(&target_server, "repo").await;
    }

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let report = run_sync(vec![mapping]).await;

    assert_status!(report, 0, ImageStatus::Synced);
    let uploads = count_blob_uploads(
        &target_server,
        PathMatch::Contains("/blobs/uploads/"),
        &first.config_desc.digest,
    )
    .await;
    assert_eq!(
        uploads, 1,
        "a blob shared by two artifacts is uploaded per artifact"
    );
}
