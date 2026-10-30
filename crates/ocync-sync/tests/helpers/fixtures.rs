//! Mapping, client and digest fixtures for the sync integration tests.

#![allow(dead_code, unused_imports, unreachable_pub)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use ocync_distribution::spec::{
    Descriptor, ImageManifest, MediaType, RegistryAuthority, RepositoryName,
};
use ocync_distribution::{Digest, RegistryClientBuilder};
use ocync_sync::cache::{SnapshotKey, TransferStateCache};
use ocync_sync::engine::{RegistryAlias, ResolvedArtifacts, ResolvedMapping, TagPair, TargetEntry};
use ocync_sync::retry::RetryConfig;
use url::Url;
use wiremock::MockServer;

use super::digests::repo;

/// The base URL of `server`.
pub fn mock_url(server: &MockServer) -> Url {
    Url::parse(&server.uri()).unwrap()
}

/// A client for `server` with default settings.
pub fn mock_client(server: &MockServer) -> Arc<ocync_distribution::RegistryClient> {
    Arc::new(
        RegistryClientBuilder::new(mock_url(server))
            .build()
            .unwrap(),
    )
}

/// A client whose base URL has an ECR hostname but whose traffic is resolved
/// to `server`, to reach ECR-specific paths (mount short-circuit) locally.
pub fn ecr_mock_client(server: &MockServer) -> Arc<ocync_distribution::RegistryClient> {
    let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
    let mock_port = mock_url(server).port().unwrap();
    let base_url = Url::parse(&format!("http://{host}:{mock_port}")).unwrap();
    Arc::new(
        RegistryClientBuilder::new(base_url)
            .resolve(
                host,
                std::net::SocketAddr::from(([127, 0, 0, 1], mock_port)),
            )
            .build()
            .unwrap(),
    )
}

/// A non-zero image concurrency for `SyncEngine::new`, so call sites keep a
/// plain number.
pub fn concurrency(n: usize) -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(n).expect("test concurrency is non-zero")
}

/// Retry settings with millisecond backoff and two retries.
pub fn fast_retry() -> RetryConfig {
    RetryConfig {
        max_retries: 2,
        initial_backoff: std::time::Duration::from_millis(1),
        max_backoff: std::time::Duration::from_millis(10),
        backoff_multiplier: 2,
        // Zero skips the manifest-commit blob-visibility wait: mock batch
        // checkers do not reflect the engine's uploads. Tests of the wait
        // build their own `RetryConfig`.
        manifest_commit_wait: std::time::Duration::ZERO,
    }
}

/// A fresh in-memory transfer state cache.
pub fn empty_cache() -> Rc<RefCell<TransferStateCache>> {
    Rc::new(RefCell::new(TransferStateCache::new()))
}

/// A cache holding each of `digests` as completed at `repo_name` on `target`.
pub fn cache_with(
    target: &str,
    repo_name: &str,
    digests: &[&Digest],
) -> Rc<RefCell<TransferStateCache>> {
    let cache = empty_cache();
    for digest in digests {
        cache
            .borrow_mut()
            .set_blob_completed(target, (*digest).clone(), repo(repo_name));
    }
    cache
}

/// A [`SnapshotKey`] under the standard test authority.
pub fn snap_key(repo: &str, tag: &str) -> SnapshotKey {
    SnapshotKey::new(
        &RegistryAuthority::new("source.test.io:443"),
        &RepositoryName::new(repo).unwrap(),
        tag,
    )
}

/// A [`TargetEntry`] without a batch checker.
pub fn target_entry(name: &str, client: Arc<ocync_distribution::RegistryClient>) -> TargetEntry {
    TargetEntry {
        name: RegistryAlias::new(name),
        client,
        batch_checker: None,
        existing_tags: HashSet::new(),
    }
}

/// A `ResolvedMapping` with no platform filter or immutable glob and the
/// default artifacts config (enabled, no type filters).
pub fn resolved_mapping(
    source_client: Arc<ocync_distribution::RegistryClient>,
    source_repo: &str,
    target_repo: &str,
    targets: Vec<TargetEntry>,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    ResolvedMapping {
        source_authority: RegistryAuthority::new("source.test.io:443"),
        source_client,
        source_repo: RepositoryName::new(source_repo).unwrap(),
        target_repo: RepositoryName::new(target_repo).unwrap(),
        targets,
        tags,
        platforms: None,
        head_first: false,
        immutable_glob: None,
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        candidate_count: None,
        filter_report: None,
    }
}

/// One source, one target named `target`, the same repo name on both.
pub fn mapping_from_servers(
    source: &MockServer,
    target: &MockServer,
    repo: &str,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    mapping_with_distinct_repos(source, target, repo, repo, tags)
}

/// As [`mapping_from_servers`] with different source and target repos.
pub fn mapping_with_distinct_repos(
    source: &MockServer,
    target: &MockServer,
    source_repo: &str,
    target_repo: &str,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    resolved_mapping(
        mock_client(source),
        source_repo,
        target_repo,
        vec![target_entry("target", mock_client(target))],
        tags,
    )
}

/// Fan one source out to several named targets, the same repo name everywhere.
pub fn mapping_to_targets(
    source: &MockServer,
    targets: &[(&str, &MockServer)],
    repo: &str,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    resolved_mapping(
        mock_client(source),
        repo,
        repo,
        targets
            .iter()
            .map(|(name, server)| target_entry(name, mock_client(server)))
            .collect(),
        tags,
    )
}

/// Tag pairs that keep each name in `tags` unchanged.
pub fn same_tags(tags: &[&str]) -> Vec<TagPair> {
    tags.iter().map(|t| TagPair::same(*t)).collect()
}

/// One `v1` mapping per `(source_repo, target_repo)` pair, all sharing one
/// source client and one target client (alias `target`).
pub fn v1_mappings<S: AsRef<str>>(
    source: &MockServer,
    target: &MockServer,
    repos: &[(S, S)],
) -> Vec<ResolvedMapping> {
    let source_client = mock_client(source);
    let target_client = mock_client(target);
    repos
        .iter()
        .map(|(src, tgt)| {
            resolved_mapping(
                Arc::clone(&source_client),
                src.as_ref(),
                tgt.as_ref(),
                vec![target_entry("target", Arc::clone(&target_client))],
                same_tags(&["v1"]),
            )
        })
        .collect()
}

/// An `ImageManifest` as JSON bytes and its digest.
pub fn serialize_manifest(manifest: &ImageManifest) -> (Vec<u8>, Digest) {
    let bytes = serde_json::to_vec(manifest).unwrap();
    let digest = blob_digest(&bytes);
    (bytes, digest)
}

/// A descriptor with a fixed size of 100, for fake-digest tests.
pub fn make_descriptor(digest: Digest, media_type: MediaType) -> Descriptor {
    Descriptor {
        media_type,
        digest,
        size: 100,
        platform: None,
        artifact_type: None,
        annotations: None,
    }
}

/// A one-layer manifest over fake digests, for HEAD-check and discovery tests.
pub fn simple_image_manifest(config_digest: &Digest, layer_digest: &Digest) -> ImageManifest {
    ImageManifest {
        schema_version: 2,
        media_type: None,
        config: make_descriptor(config_digest.clone(), MediaType::OciConfig),
        layers: vec![make_descriptor(
            layer_digest.clone(),
            MediaType::OciLayerGzip,
        )],
        subject: None,
        artifact_type: None,
        annotations: None,
    }
}

/// The real SHA-256 digest of `data`.
pub fn blob_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// A descriptor with the real digest and size of `data`.
pub fn blob_descriptor(data: &[u8], media_type: MediaType) -> Descriptor {
    Descriptor {
        media_type,
        digest: blob_digest(data),
        size: data.len() as u64,
        platform: None,
        artifact_type: None,
        annotations: None,
    }
}
