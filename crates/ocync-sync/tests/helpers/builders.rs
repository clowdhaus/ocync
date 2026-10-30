//! Builders for test manifests with real digests, plus the `*Parts` they return.

#![allow(dead_code, unused_imports, unreachable_pub)]

use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::{Descriptor, ImageIndex, ImageManifest, MediaType, Platform};
use wiremock::MockServer;

use super::fixtures::{blob_descriptor, blob_digest};
use super::mocks::*;

/// Builds an `ImageManifest` over real blob data (matching digests and sizes).
#[must_use = "builders do nothing until .build() is called"]
pub struct ManifestBuilder {
    config_data: Vec<u8>,
    layers_data: Vec<Vec<u8>>,
}

impl ManifestBuilder {
    /// Start a manifest whose config blob is `config_data`.
    pub fn new(config_data: &[u8]) -> Self {
        Self {
            config_data: config_data.to_vec(),
            layers_data: Vec::new(),
        }
    }

    /// Append a layer blob.
    pub fn layer(mut self, data: &[u8]) -> Self {
        self.layers_data.push(data.to_vec());
        self
    }

    /// Serialize the manifest and keep the blobs it describes.
    pub fn build(self) -> ManifestParts {
        let config_desc = blob_descriptor(&self.config_data, MediaType::OciConfig);
        let layer_descs: Vec<Descriptor> = self
            .layers_data
            .iter()
            .map(|d| blob_descriptor(d, MediaType::OciLayerGzip))
            .collect();

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: config_desc.clone(),
            layers: layer_descs.clone(),
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = blob_digest(&bytes);

        ManifestParts {
            bytes,
            digest,
            config_data: self.config_data,
            config_desc,
            layers_data: self.layers_data,
            layer_descs,
        }
    }
}

/// Output of [`ManifestBuilder::build`].
#[derive(Clone)]
pub struct ManifestParts {
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub config_data: Vec<u8>,
    pub config_desc: Descriptor,
    pub layers_data: Vec<Vec<u8>>,
    pub layer_descs: Vec<Descriptor>,
}

impl ManifestParts {
    /// The config and then every layer, as digest and data.
    fn blobs(&self) -> impl Iterator<Item = (&Digest, &[u8])> {
        std::iter::once((&self.config_desc.digest, self.config_data.as_slice())).chain(
            self.layer_descs
                .iter()
                .map(|d| &d.digest)
                .zip(self.layers_data.iter().map(Vec::as_slice)),
        )
    }

    /// Digests of the config and every layer.
    pub fn blob_digests(&self) -> impl Iterator<Item = Digest> + '_ {
        self.blobs().map(|(digest, _)| digest.clone())
    }

    /// Mount the source blob GETs (config and layers).
    pub async fn mount_blob_pulls(&self, server: &MockServer, repo: &str) {
        for (digest, data) in self.blobs() {
            mount_blob_pull(server, repo, digest, data).await;
        }
    }

    /// Mount source mocks: manifest GET on `reference` plus every blob GET.
    pub async fn mount_source(&self, server: &MockServer, repo: &str, reference: &str) {
        mount_source_manifest(server, repo, reference, &self.bytes).await;
        self.mount_blob_pulls(server, repo).await;
    }

    /// As [`mount_source`](Self::mount_source), but every blob GET answers
    /// only after `delay`; the manifest answers at once.
    pub async fn mount_source_delayed(
        &self,
        server: &MockServer,
        repo: &str,
        reference: &str,
        delay: Duration,
    ) {
        mount_source_manifest(server, repo, reference, &self.bytes).await;
        for (digest, data) in self.blobs() {
            mount_blob_pull_delayed(server, repo, digest, data, delay).await;
        }
    }

    /// As [`mount_source`](Self::mount_source), but the manifest and each blob
    /// must be requested exactly `pulls` times (`0` asserts it is never touched).
    /// `row` names the table row in a failed expectation.
    pub async fn mount_source_expecting(
        &self,
        server: &MockServer,
        repo: &str,
        reference: &str,
        pulls: u64,
        row: &str,
    ) {
        source_manifest_mock(repo, reference, &self.bytes, MediaType::OciManifest)
            .expect(pulls)
            .named(format!("{row}: manifest GET {reference}"))
            .mount(server)
            .await;
        self.mount_blob_pulls_expecting(server, repo, pulls, row)
            .await;
    }

    /// As [`mount_blob_pulls`](Self::mount_blob_pulls), but each blob must be
    /// requested exactly `pulls` times. `row` names the table row in a failed
    /// expectation.
    pub async fn mount_blob_pulls_expecting(
        &self,
        server: &MockServer,
        repo: &str,
        pulls: u64,
        row: &str,
    ) {
        for (digest, data) in self.blobs() {
            blob_pull_mock(repo, digest, data)
                .expect(pulls)
                .named(format!("{row}: blob GET {digest}"))
                .mount(server)
                .await;
        }
    }

    /// Mount blob HEAD 404s for the config and every layer.
    pub async fn mount_target_blob_misses(&self, server: &MockServer, repo: &str) {
        for (digest, _) in self.blobs() {
            mount_blob_not_found(server, repo, digest).await;
        }
    }

    /// Mount target mocks: manifest HEAD 404, blob HEAD 404s, push endpoints.
    pub async fn mount_target(&self, server: &MockServer, repo: &str, reference: &str) {
        mount_manifest_head_not_found(server, repo, reference).await;
        self.mount_target_blob_misses(server, repo).await;
        mount_blob_push(server, repo).await;
        mount_manifest_push(server, repo, reference).await;
    }

    /// As [`mount_target`](Self::mount_target), and every cross-repo mount of
    /// one of its blobs answers `mount_status`.
    pub async fn mount_target_answering_mounts(
        &self,
        server: &MockServer,
        repo: &str,
        reference: &str,
        mount_status: u16,
    ) {
        self.mount_target(server, repo, reference).await;
        for digest in self.blob_digests() {
            blob_mount_mock(repo, &digest, mount_status)
                .mount(server)
                .await;
        }
    }
}

/// Builds an OCI artifact manifest (signature, SBOM): one config blob, one
/// layer blob, `artifact_type` set, referenced by digest rather than tag.
#[must_use = "builders do nothing until .build() is called"]
pub struct ArtifactBuilder {
    config_data: Vec<u8>,
    layer_data: Vec<u8>,
    artifact_type: String,
}

impl ArtifactBuilder {
    /// An artifact of the cosign signature type over the given blobs.
    pub fn new(config_data: &[u8], layer_data: &[u8]) -> Self {
        Self {
            config_data: config_data.to_vec(),
            layer_data: layer_data.to_vec(),
            artifact_type: "application/vnd.dev.cosign.artifact.sig.v1+json".to_string(),
        }
    }

    /// Replace the artifact type.
    pub fn artifact_type(mut self, t: &str) -> Self {
        self.artifact_type = t.to_string();
        self
    }

    /// Serialize the artifact manifest and keep its blobs.
    pub fn build(self) -> ArtifactParts {
        let config_desc = blob_descriptor(&self.config_data, MediaType::OciConfig);
        let layer_desc = blob_descriptor(&self.layer_data, MediaType::OciLayerGzip);

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: config_desc.clone(),
            layers: vec![layer_desc.clone()],
            subject: None,
            artifact_type: Some(self.artifact_type.clone()),
            annotations: None,
        };

        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = blob_digest(&bytes);

        ArtifactParts {
            bytes,
            digest,
            config_data: self.config_data,
            config_desc,
            layer_data: self.layer_data,
            layer_desc,
            artifact_type: self.artifact_type,
        }
    }
}

/// Output of [`ArtifactBuilder::build`].
#[derive(Clone)]
pub struct ArtifactParts {
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub config_data: Vec<u8>,
    pub config_desc: Descriptor,
    pub layer_data: Vec<u8>,
    pub layer_desc: Descriptor,
    pub artifact_type: String,
}

impl ArtifactParts {
    /// Mount source mocks: manifest GET by digest plus blob GETs.
    pub async fn mount_source(&self, server: &MockServer, repo: &str) {
        mount_source_manifest(server, repo, &self.digest.to_string(), &self.bytes).await;
        mount_blob_pull(server, repo, &self.config_desc.digest, &self.config_data).await;
        mount_blob_pull(server, repo, &self.layer_desc.digest, &self.layer_data).await;
    }

    /// Mount target mocks: blob HEAD 404s, blob push, manifest push by digest.
    pub async fn mount_target(&self, server: &MockServer, repo: &str) {
        mount_blob_not_found(server, repo, &self.config_desc.digest).await;
        mount_blob_not_found(server, repo, &self.layer_desc.digest).await;
        mount_blob_push(server, repo).await;
        mount_manifest_push(server, repo, &self.digest.to_string()).await;
    }

    /// This artifact as a descriptor in a referrers index.
    pub fn referrers_descriptor(&self) -> Descriptor {
        Descriptor {
            media_type: MediaType::OciManifest,
            digest: self.digest.clone(),
            size: self.bytes.len() as u64,
            platform: None,
            artifact_type: Some(self.artifact_type.clone()),
            annotations: None,
        }
    }
}

/// Builds an OCI image index (multi-arch) over child manifests.
#[must_use = "builders do nothing until .build() is called"]
#[derive(Default)]
pub struct IndexBuilder {
    children: Vec<(ManifestParts, Option<Platform>)>,
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a child manifest with an optional platform annotation.
    pub fn manifest(mut self, parts: &ManifestParts, platform: Option<Platform>) -> Self {
        self.children.push((parts.clone(), platform));
        self
    }

    /// Serialize the index and keep its children.
    pub fn build(self) -> IndexParts {
        let manifests: Vec<Descriptor> = self
            .children
            .iter()
            .map(|(parts, platform)| Descriptor {
                media_type: MediaType::OciManifest,
                digest: parts.digest.clone(),
                size: parts.bytes.len() as u64,
                platform: platform.clone(),
                artifact_type: None,
                annotations: None,
            })
            .collect();

        let index = ImageIndex {
            schema_version: 2,
            media_type: None,
            manifests,
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let bytes = serde_json::to_vec(&index).unwrap();
        let digest = blob_digest(&bytes);

        let children: Vec<ManifestParts> = self.children.into_iter().map(|(p, _)| p).collect();

        IndexParts {
            bytes,
            digest,
            children,
        }
    }
}

/// Output of [`IndexBuilder::build`].
#[derive(Clone)]
pub struct IndexParts {
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub children: Vec<ManifestParts>,
}

impl IndexParts {
    /// Mount source mocks: index GET by tag, then each child manifest by
    /// digest with its blobs.
    pub async fn mount_source(&self, server: &MockServer, repo: &str, tag: &str) {
        source_manifest_mock(repo, tag, &self.bytes, MediaType::OciIndex)
            .mount(server)
            .await;
        for child in &self.children {
            child
                .mount_source(server, repo, &child.digest.to_string())
                .await;
        }
    }

    /// As [`mount_source`](Self::mount_source), but the index must be pulled
    /// exactly once and child `i` (manifest and blobs) `child_pulls[i]` times.
    /// `row` names the table row in a failed expectation.
    pub async fn mount_source_expecting(
        &self,
        server: &MockServer,
        repo: &str,
        tag: &str,
        child_pulls: &[u64],
        row: &str,
    ) {
        assert_eq!(
            child_pulls.len(),
            self.children.len(),
            "{row}: one pull count per child"
        );
        source_manifest_mock(repo, tag, &self.bytes, MediaType::OciIndex)
            .expect(1)
            .named(format!("{row}: index GET {tag}"))
            .mount(server)
            .await;
        for (child, pulls) in self.children.iter().zip(child_pulls) {
            child
                .mount_source_expecting(server, repo, &child.digest.to_string(), *pulls, row)
                .await;
        }
    }

    /// Mount target mocks: index HEAD 404, every child's blob HEAD 404s, push
    /// endpoints for the index tag and each child digest.
    pub async fn mount_target(&self, server: &MockServer, repo: &str, tag: &str) {
        mount_manifest_head_not_found(server, repo, tag).await;
        for child in &self.children {
            child.mount_target_blob_misses(server, repo).await;
        }
        mount_blob_push(server, repo).await;
        mount_manifest_push(server, repo, tag).await;
        for child in &self.children {
            mount_manifest_push(server, repo, &child.digest.to_string()).await;
        }
    }
}

/// Builds an OCI referrers index response.
#[must_use = "builders do nothing until .build() is called"]
#[derive(Default)]
pub struct ReferrersIndexBuilder {
    descriptors: Vec<Descriptor>,
}

impl ReferrersIndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a raw descriptor.
    pub fn descriptor(mut self, desc: Descriptor) -> Self {
        self.descriptors.push(desc);
        self
    }

    /// Add an artifact's descriptor.
    pub fn artifact(self, parts: &ArtifactParts) -> Self {
        self.descriptor(parts.referrers_descriptor())
    }

    /// Serialize the index.
    pub fn build(self) -> Vec<u8> {
        let index = ImageIndex {
            schema_version: 2,
            media_type: Some(MediaType::OciIndex),
            manifests: self.descriptors,
            subject: None,
            artifact_type: None,
            annotations: None,
        };
        serde_json::to_vec(&index).unwrap()
    }
}

/// An `os/arch` platform with no variant.
pub fn platform(os: &str, arch: &str) -> Platform {
    Platform {
        architecture: arch.to_string(),
        os: os.to_string(),
        variant: None,
        os_version: None,
        os_features: None,
    }
}

/// A `linux/<arch>` platform.
pub fn linux(arch: &str) -> Platform {
    platform("linux", arch)
}

/// Tags that all point at one manifest, as alpine's floating tags do.
pub const SHARED_TAGS: [&str; 4] = ["latest", "3", "3.24", "3.24.3"];

/// Mount a two-child (amd64, arm64) index under every tag in
/// [`SHARED_TAGS`] at both servers, returning the two children.
pub async fn mount_shared_index(
    source: &MockServer,
    target: &MockServer,
) -> (ManifestParts, ManifestParts) {
    let amd64 = ManifestBuilder::new(b"cfg-amd64")
        .layer(b"layer-amd64")
        .build();
    let arm64 = ManifestBuilder::new(b"cfg-arm64")
        .layer(b"layer-arm64")
        .build();
    let index = IndexBuilder::new()
        .manifest(&amd64, Some(linux("amd64")))
        .manifest(&arm64, Some(linux("arm64")))
        .build();
    for tag in SHARED_TAGS {
        index.mount_source(source, "repo", tag).await;
        index.mount_target(target, "repo", tag).await;
    }
    (amd64, arm64)
}
