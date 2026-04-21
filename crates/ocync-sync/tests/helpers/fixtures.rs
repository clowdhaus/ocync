//! Factory functions for constructing test fixtures with sensible defaults.

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

/// Build a valid sha256 digest from a short suffix, zero-padded to 64 hex chars.
pub fn make_digest(suffix: &str) -> Digest {
    format!("sha256:{suffix:0>64}").parse().unwrap()
}

pub fn mock_url(server: &MockServer) -> Url {
    Url::parse(&server.uri()).unwrap()
}

pub fn mock_client(server: &MockServer) -> Arc<ocync_distribution::RegistryClient> {
    Arc::new(
        RegistryClientBuilder::new(mock_url(server))
            .build()
            .unwrap(),
    )
}

/// Build a `RegistryClient` whose `base_url` has an ECR hostname but all
/// traffic is redirected (via `RegistryClientBuilder::resolve`) to the mock
/// server's local port. Used to exercise ECR-specific code paths (mount
/// short-circuit) without a real ECR endpoint.
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

pub fn fast_retry() -> RetryConfig {
    RetryConfig {
        max_retries: 2,
        initial_backoff: std::time::Duration::from_millis(1),
        max_backoff: std::time::Duration::from_millis(10),
        backoff_multiplier: 2,
    }
}

pub fn empty_cache() -> Rc<RefCell<TransferStateCache>> {
    Rc::new(RefCell::new(TransferStateCache::new()))
}

/// Build a [`SnapshotKey`] for tests using the standard test authority.
pub fn snap_key(repo: &str, tag: &str) -> SnapshotKey {
    SnapshotKey::new(
        &RegistryAuthority::new("source.test.io:443"),
        &RepositoryName::new(repo).unwrap(),
        tag,
    )
}

/// Shorthand for a [`TargetEntry`] without a batch checker.
pub fn target_entry(name: &str, client: Arc<ocync_distribution::RegistryClient>) -> TargetEntry {
    TargetEntry {
        name: RegistryAlias::new(name),
        client,
        batch_checker: None,
        existing_tags: HashSet::new(),
    }
}

/// Construct a `ResolvedMapping` for tests with sensible defaults.
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
    }
}

/// Build a `ResolvedMapping` directly from mock servers (inlines `mock_client` + `target_entry`).
///
/// Convenience for the 60% of tests that have one source, one target, same repo name.
pub fn mapping_from_servers(
    source: &MockServer,
    target: &MockServer,
    repo: &str,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    resolved_mapping(
        mock_client(source),
        repo,
        repo,
        vec![target_entry("target", mock_client(target))],
        tags,
    )
}

/// Build a `ResolvedMapping` with distinct source/target repos.
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

/// Serialize an `ImageManifest` to JSON bytes and compute its digest.
pub fn serialize_manifest(manifest: &ImageManifest) -> (Vec<u8>, Digest) {
    let bytes = serde_json::to_vec(manifest).unwrap();
    let hash = ocync_distribution::sha256::Sha256::digest(&bytes);
    let digest = Digest::from_sha256(hash);
    (bytes, digest)
}

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

/// Compute the real SHA-256 digest for test blob data.
pub fn blob_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// Build a descriptor with the real digest and size of the given data.
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
