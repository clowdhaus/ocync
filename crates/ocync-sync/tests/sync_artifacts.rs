//! Artifact sync integration tests: disabled no-op, referrer transfer, require
//! enforcement, tag fallback, include/exclude filter, blob dedup, and transfer
//! failure.

mod helpers;

use std::rc::Rc;

use ocync_distribution::spec::{Descriptor, ImageIndex, ImageManifest, MediaType};
use ocync_sync::engine::{ResolvedArtifacts, ResolvedMapping, SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ErrorKind, ImageStatus};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

// ---------------------------------------------------------------------------
// Artifact sync tests
// ---------------------------------------------------------------------------

/// When `artifacts.enabled = false`, no referrers API requests are made.
///
/// Negative assertion: if the engine issued any referrers request, the mock
/// server would receive it and the request count would be non-zero.
#[tokio::test]
async fn artifact_sync_disabled_issues_no_referrers_requests() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"art-cfg";
    let layer_data = b"art-layer";
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

    mount_source_manifest(&source_server, "repo", "v1.0.0", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts {
            enabled: false,
            ..ResolvedArtifacts::default()
        }),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));

    // No referrers requests should have been made.
    let source_requests = source_server.received_requests().await.unwrap();
    let referrers_requests: Vec<_> = source_requests
        .iter()
        .filter(|r| r.url.path().contains("/referrers/"))
        .collect();
    assert_eq!(
        referrers_requests.len(),
        0,
        "artifacts disabled must issue zero referrers requests"
    );
}

/// When `artifacts.enabled = true` and the referrers API returns an artifact,
/// the engine pulls and pushes the artifact manifest and its blobs.
#[tokio::test]
async fn artifact_sync_transfers_referrer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // -- Parent image --
    let config_data = b"parent-config";
    let layer_data = b"parent-layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let parent_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (parent_bytes, parent_digest) = serialize_manifest(&parent_manifest);

    // -- Artifact (signature) --
    let sig_config_data = b"sig-config";
    let sig_layer_data = b"sig-payload";
    let sig_config_desc = blob_descriptor(sig_config_data, MediaType::OciConfig);
    let sig_layer_desc = blob_descriptor(sig_layer_data, MediaType::OciLayerGzip);
    let sig_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sig_config_desc.clone(),
        layers: vec![sig_layer_desc.clone()],
        subject: None,
        artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".to_string()),
        annotations: None,
    };
    let (sig_bytes, sig_digest) = serialize_manifest(&sig_manifest);

    // -- Referrers index --
    let referrers_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: vec![Descriptor {
            media_type: MediaType::OciManifest,
            digest: sig_digest.clone(),
            size: sig_bytes.len() as u64,
            platform: None,
            artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".to_string()),
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let referrers_body = serde_json::to_vec(&referrers_index).unwrap();

    // -- Source mocks --
    mount_source_manifest(&source_server, "repo", "v1.0.0", &parent_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Referrers API response.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{parent_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(referrers_body)
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact manifest pull.
    let sig_digest_str = sig_digest.to_string();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{sig_digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sig_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact blob pulls.
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

    // -- Target mocks --
    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;
    mount_manifest_push(&target_server, "repo", &sig_digest_str).await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "expected Synced, got {:?}",
        report.images[0].status,
    );

    // Verify artifact manifest was pushed to target.
    let target_requests = target_server.received_requests().await.unwrap();
    let artifact_pushes: Vec<_> = target_requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains(&sig_digest_str))
        .collect();
    assert_eq!(
        artifact_pushes.len(),
        1,
        "artifact manifest must be pushed to target"
    );
}

/// When `require_artifacts = true` and no referrers exist (confirmed via
/// successful 200 with empty index), the image sync must fail.
#[tokio::test]
async fn artifact_require_artifacts_fails_on_empty() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"req-cfg";
    let layer_data = b"req-layer";
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
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1.0.0", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Referrers API returns empty index (no artifacts).
    let empty_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: Vec::new(),
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{manifest_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(serde_json::to_vec(&empty_index).unwrap())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts {
            enabled: true,
            require_artifacts: true,
            ..ResolvedArtifacts::default()
        }),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    // Image should fail because require_artifacts is true and no referrers exist.
    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Failed { .. }),
        "expected Failed for require_artifacts with no referrers, got {:?}",
        report.images[0].status,
    );
}

/// When `require_artifacts = true` but the referrers API returns a non-404
/// error (e.g. 500), the image should NOT fail due to `require_artifacts`.
/// The transient error means we couldn't determine if artifacts exist.
#[tokio::test]
async fn artifact_require_artifacts_does_not_fire_on_api_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"err-cfg";
    let layer_data = b"err-layer";
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
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    mount_source_manifest(&source_server, "repo", "v1.0.0", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Referrers API returns 500 (transient error).
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{manifest_digest}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts {
            enabled: true,
            require_artifacts: true,
            ..ResolvedArtifacts::default()
        }),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    // Image should succeed (sync the image) because we couldn't confirm
    // whether artifacts exist -- require_artifacts only fires on positive
    // confirmation of zero referrers.
    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "transient referrers error should not fail the image, got {:?}",
        report.images[0].status,
    );
}

/// When the referrers API returns 404 but the tag fallback has an artifact,
/// the engine should discover and transfer it via the fallback path.
#[tokio::test]
async fn artifact_sync_tag_fallback_transfers_referrer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // -- Parent image --
    let config_data = b"fb-parent-config";
    let layer_data = b"fb-parent-layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let parent_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (parent_bytes, parent_digest) = serialize_manifest(&parent_manifest);

    // -- Signature artifact --
    let sig_config_data = b"fb-sig-config";
    let sig_layer_data = b"fb-sig-layer";
    let sig_config_desc = blob_descriptor(sig_config_data, MediaType::OciConfig);
    let sig_layer_desc = blob_descriptor(sig_layer_data, MediaType::OciLayerGzip);
    let sig_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sig_config_desc.clone(),
        layers: vec![sig_layer_desc.clone()],
        subject: Some(Descriptor {
            media_type: MediaType::OciManifest,
            digest: parent_digest.clone(),
            size: parent_bytes.len() as u64,
            platform: None,
            artifact_type: None,
            annotations: None,
        }),
        artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
        annotations: None,
    };
    let (sig_bytes, sig_digest) = serialize_manifest(&sig_manifest);

    // -- Source mocks --
    mount_source_manifest(&source_server, "repo", "v1.0.0", &parent_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Referrers API returns 404.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{parent_digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&source_server)
        .await;

    // Tag fallback: manifest at sha256-<hex> tag is an index with the artifact.
    let fallback_tag = parent_digest.tag_fallback();
    let referrers_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: vec![Descriptor {
            media_type: MediaType::OciManifest,
            digest: sig_digest.clone(),
            size: sig_bytes.len() as u64,
            platform: None,
            artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let referrers_body = serde_json::to_vec(&referrers_index).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{fallback_tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(referrers_body)
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact manifest pull.
    let sig_digest_str = sig_digest.to_string();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{sig_digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sig_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact blob pulls.
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

    // -- Target mocks --
    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;
    mount_manifest_push(&target_server, "repo", &sig_digest_str).await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "expected Synced via tag fallback, got {:?}",
        report.images[0].status,
    );

    // Verify artifact manifest was pushed.
    let target_requests = target_server.received_requests().await.unwrap();
    let artifact_pushes: Vec<_> = target_requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains(&sig_digest_str))
        .collect();
    assert_eq!(
        artifact_pushes.len(),
        1,
        "artifact manifest must be pushed via tag fallback path"
    );
}

/// When include filters are set, only matching artifacts should be transferred.
///
/// Setup: source has two artifacts (cosign signature + SBOM). Include filter
/// only allows cosign signatures. Assert only the cosign artifact is pushed.
#[tokio::test]
async fn artifact_sync_include_filter_skips_non_matching() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // -- Parent image --
    let config_data = b"filt-parent-config";
    let layer_data = b"filt-parent-layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let parent_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (parent_bytes, parent_digest) = serialize_manifest(&parent_manifest);

    // -- Cosign signature artifact --
    let sig_config_data = b"filt-sig-config";
    let sig_layer_data = b"filt-sig-layer";
    let sig_config_desc = blob_descriptor(sig_config_data, MediaType::OciConfig);
    let sig_layer_desc = blob_descriptor(sig_layer_data, MediaType::OciLayerGzip);
    let sig_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sig_config_desc.clone(),
        layers: vec![sig_layer_desc.clone()],
        subject: None,
        artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
        annotations: None,
    };
    let (sig_bytes, sig_digest) = serialize_manifest(&sig_manifest);

    // -- SBOM artifact (should NOT be transferred) --
    let sbom_config_data = b"filt-sbom-config";
    let sbom_layer_data = b"filt-sbom-layer";
    let sbom_config_desc = blob_descriptor(sbom_config_data, MediaType::OciConfig);
    let sbom_layer_desc = blob_descriptor(sbom_layer_data, MediaType::OciLayerGzip);
    let sbom_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sbom_config_desc.clone(),
        layers: vec![sbom_layer_desc.clone()],
        subject: None,
        artifact_type: Some("application/spdx+json".into()),
        annotations: None,
    };
    let (sbom_bytes, sbom_digest) = serialize_manifest(&sbom_manifest);

    // -- Source mocks --
    mount_source_manifest(&source_server, "repo", "v1.0.0", &parent_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    // Referrers API returns both artifacts.
    let referrers_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: vec![
            Descriptor {
                media_type: MediaType::OciManifest,
                digest: sig_digest.clone(),
                size: sig_bytes.len() as u64,
                platform: None,
                artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
                annotations: None,
            },
            Descriptor {
                media_type: MediaType::OciManifest,
                digest: sbom_digest.clone(),
                size: sbom_bytes.len() as u64,
                platform: None,
                artifact_type: Some("application/spdx+json".into()),
                annotations: None,
            },
        ],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{parent_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(serde_json::to_vec(&referrers_index).unwrap())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact manifest pulls (both available, but only sig should be requested).
    let sig_digest_str = sig_digest.to_string();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{sig_digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sig_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    let sbom_digest_str = sbom_digest.to_string();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{sbom_digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sbom_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact blob pulls (sig only).
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

    // -- Target mocks --
    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;
    mount_manifest_push(&target_server, "repo", &sig_digest_str).await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts {
            enabled: true,
            include: vec!["application/vnd.dev.cosign.artifact.sig.v1+json".into()],
            exclude: Vec::new(),
            require_artifacts: false,
        }),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    assert_eq!(report.images.len(), 1);
    assert!(matches!(report.images[0].status, ImageStatus::Synced));

    // Verify: cosign signature was pushed, SBOM was NOT.
    let target_requests = target_server.received_requests().await.unwrap();
    let sig_pushes: Vec<_> = target_requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains(&sig_digest_str))
        .collect();
    assert_eq!(sig_pushes.len(), 1, "cosign signature must be pushed");

    let sbom_pushes: Vec<_> = target_requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains(&sbom_digest_str))
        .collect();
    assert_eq!(
        sbom_pushes.len(),
        0,
        "SBOM must NOT be pushed when include filter excludes it"
    );
}

/// When `require_artifacts = true` and referrers exist but are all excluded
/// by the include/exclude filter, the image should fail. This documents the
/// intentional semantic: `require_artifacts` means "require matching artifacts",
/// not "require any referrers at the source."
#[tokio::test]
async fn artifact_require_fires_when_all_filtered_out() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"req-filt-cfg";
    let layer_data = b"req-filt-layer";
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
    let (manifest_bytes, manifest_digest) = serialize_manifest(&manifest);

    // Source has only an SBOM referrer.
    let sbom_digest = make_digest("5b0e5b0e5b0e");
    let referrers_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: vec![Descriptor {
            media_type: MediaType::OciManifest,
            digest: sbom_digest,
            size: 100,
            platform: None,
            artifact_type: Some("application/spdx+json".into()),
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };

    mount_source_manifest(&source_server, "repo", "v1.0.0", &manifest_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{manifest_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(serde_json::to_vec(&referrers_index).unwrap())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    // require_artifacts + include only cosign (source only has SBOM).
    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts {
            enabled: true,
            include: vec!["application/vnd.dev.cosign.artifact.sig.v1+json".into()],
            exclude: Vec::new(),
            require_artifacts: true,
        }),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    // Should fail because no matching artifacts exist after filtering.
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

/// When artifact blobs already exist at the target (HEAD returns 200),
/// the engine should skip the push and not re-upload them.
#[tokio::test]
async fn artifact_blob_dedup_skips_existing() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // -- Parent image --
    let config_data = b"dedup-parent-config";
    let layer_data = b"dedup-parent-layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let parent_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (parent_bytes, parent_digest) = serialize_manifest(&parent_manifest);

    // -- Artifact --
    let sig_config_data = b"dedup-sig-config";
    let sig_layer_data = b"dedup-sig-layer";
    let sig_config_desc = blob_descriptor(sig_config_data, MediaType::OciConfig);
    let sig_layer_desc = blob_descriptor(sig_layer_data, MediaType::OciLayerGzip);
    let sig_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sig_config_desc.clone(),
        layers: vec![sig_layer_desc.clone()],
        subject: None,
        artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
        annotations: None,
    };
    let (sig_bytes, sig_digest) = serialize_manifest(&sig_manifest);

    // -- Source mocks --
    mount_source_manifest(&source_server, "repo", "v1.0.0", &parent_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    let referrers_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: vec![Descriptor {
            media_type: MediaType::OciManifest,
            digest: sig_digest.clone(),
            size: sig_bytes.len() as u64,
            platform: None,
            artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{parent_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(serde_json::to_vec(&referrers_index).unwrap())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    let sig_digest_str = sig_digest.to_string();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{sig_digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sig_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact blob pulls should NOT be needed since blobs exist at target.
    // (Not mounting them on source -- if engine tries to pull, it will 404.)

    // -- Target mocks --
    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    // Artifact blobs ALREADY EXIST at target (HEAD returns 200).
    mount_blob_exists(&target_server, "repo", &sig_config_desc.digest).await;
    mount_blob_exists(&target_server, "repo", &sig_layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;
    mount_manifest_push(&target_server, "repo", &sig_digest_str).await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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

    assert_eq!(report.images.len(), 1);
    assert!(
        matches!(report.images[0].status, ImageStatus::Synced),
        "expected Synced, got {:?}",
        report.images[0].status,
    );

    // Verify no blob upload requests for artifact blobs (no POST for upload initiation).
    // The parent blobs DO get uploaded, but artifact blobs should be skipped.
    // We verify by checking no GET was made for the artifact blobs from source.
    let source_requests = source_server.received_requests().await.unwrap();
    let sig_blob_pulls: Vec<_> = source_requests
        .iter()
        .filter(|r| {
            r.method.as_str() == "GET"
                && r.url.path().contains("/blobs/")
                && (r.url.path().contains(&sig_config_desc.digest.to_string())
                    || r.url.path().contains(&sig_layer_desc.digest.to_string()))
        })
        .collect();
    assert_eq!(
        sig_blob_pulls.len(),
        0,
        "artifact blobs already at target must not be pulled from source"
    );
}

/// When the target returns 500 on artifact manifest push, the image should
/// fail with `ErrorKind::ArtifactSync` (not `RequiredArtifactsMissing`).
#[tokio::test]
async fn artifact_transfer_failure_reports_artifact_sync_error() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // -- Parent image --
    let config_data = b"fail-parent-config";
    let layer_data = b"fail-parent-layer";
    let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
    let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
    let parent_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: config_desc.clone(),
        layers: vec![layer_desc.clone()],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let (parent_bytes, parent_digest) = serialize_manifest(&parent_manifest);

    // -- Artifact --
    let sig_config_data = b"fail-sig-config";
    let sig_layer_data = b"fail-sig-layer";
    let sig_config_desc = blob_descriptor(sig_config_data, MediaType::OciConfig);
    let sig_layer_desc = blob_descriptor(sig_layer_data, MediaType::OciLayerGzip);
    let sig_manifest = ImageManifest {
        schema_version: 2,
        media_type: None,
        config: sig_config_desc.clone(),
        layers: vec![sig_layer_desc.clone()],
        subject: None,
        artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
        annotations: None,
    };
    let (sig_bytes, sig_digest) = serialize_manifest(&sig_manifest);

    // -- Source mocks --
    mount_source_manifest(&source_server, "repo", "v1.0.0", &parent_bytes).await;
    mount_blob_pull(&source_server, "repo", &config_desc.digest, config_data).await;
    mount_blob_pull(&source_server, "repo", &layer_desc.digest, layer_data).await;

    let referrers_index = ImageIndex {
        schema_version: 2,
        media_type: Some(MediaType::OciIndex),
        manifests: vec![Descriptor {
            media_type: MediaType::OciManifest,
            digest: sig_digest.clone(),
            size: sig_bytes.len() as u64,
            platform: None,
            artifact_type: Some("application/vnd.dev.cosign.artifact.sig.v1+json".into()),
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{parent_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(serde_json::to_vec(&referrers_index).unwrap())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(&source_server)
        .await;

    let sig_digest_str = sig_digest.to_string();
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{sig_digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sig_bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(&source_server)
        .await;

    // Artifact blob pulls.
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

    // -- Target mocks --
    mount_manifest_head_not_found(&target_server, "repo", "v1.0.0").await;
    mount_blob_not_found(&target_server, "repo", &config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &layer_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &sig_layer_desc.digest).await;
    mount_blob_push(&target_server, "repo").await;
    // Parent manifest push succeeds.
    mount_manifest_push(&target_server, "repo", "v1.0.0").await;
    // Artifact manifest push FAILS with 500.
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{sig_digest_str}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&target_server)
        .await;

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        ..resolved_mapping(
            source_client,
            "repo",
            "repo",
            vec![target_entry("target", target_client)],
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
