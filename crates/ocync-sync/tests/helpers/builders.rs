//! Builder types for constructing test manifests with sensible defaults.

#![allow(dead_code, unused_imports, unreachable_pub)]

use ocync_distribution::Digest;
use ocync_distribution::spec::{Descriptor, ImageIndex, ImageManifest, MediaType, Platform};

use super::fixtures::{blob_descriptor, blob_digest};

/// Builder for constructing `ImageManifest` with sensible test defaults.
///
/// Simplifies the common pattern of creating test manifests with real blob
/// data (matching digests and sizes). Call [`layer`](Self::layer) to add
/// layers, then [`build`](Self::build) to get the manifest plus all parts
/// needed for mock setup.
#[must_use = "builders do nothing until .build() is called"]
pub struct ManifestBuilder {
    config_data: Vec<u8>,
    layers_data: Vec<Vec<u8>>,
}

impl ManifestBuilder {
    /// Create a new builder with the given config blob data.
    pub fn new(config_data: &[u8]) -> Self {
        Self {
            config_data: config_data.to_vec(),
            layers_data: Vec::new(),
        }
    }

    /// Add a layer blob to the manifest.
    pub fn layer(mut self, data: &[u8]) -> Self {
        self.layers_data.push(data.to_vec());
        self
    }

    /// Build the manifest, returning all parts needed for mock setup.
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
        let hash = ocync_distribution::sha256::Sha256::digest(&bytes);
        let digest = Digest::from_sha256(hash);

        ManifestParts {
            manifest,
            bytes,
            digest,
            config_data: self.config_data,
            config_desc,
            layers_data: self.layers_data,
            layer_descs,
        }
    }
}

/// Output of [`ManifestBuilder::build`] with all parts needed for mock setup.
#[derive(Clone)]
pub struct ManifestParts {
    #[allow(dead_code)]
    pub manifest: ImageManifest,
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub config_data: Vec<u8>,
    pub config_desc: Descriptor,
    pub layers_data: Vec<Vec<u8>>,
    pub layer_descs: Vec<Descriptor>,
}

impl ManifestParts {
    /// Mount all source mocks (manifest GET + blob GETs) for this image.
    pub async fn mount_source(&self, server: &wiremock::MockServer, repo: &str, tag: &str) {
        super::mocks::mount_source_manifest(server, repo, tag, &self.bytes).await;
        super::mocks::mount_blob_pull(server, repo, &self.config_desc.digest, &self.config_data)
            .await;
        for (data, desc) in self.layers_data.iter().zip(self.layer_descs.iter()) {
            super::mocks::mount_blob_pull(server, repo, &desc.digest, data).await;
        }
    }

    /// Mount target mocks: HEAD 404, blob-not-found for all blobs, push endpoints.
    pub async fn mount_target(&self, server: &wiremock::MockServer, repo: &str, tag: &str) {
        super::mocks::mount_manifest_head_not_found(server, repo, tag).await;
        super::mocks::mount_blob_not_found(server, repo, &self.config_desc.digest).await;
        for desc in &self.layer_descs {
            super::mocks::mount_blob_not_found(server, repo, &desc.digest).await;
        }
        super::mocks::mount_blob_push(server, repo).await;
        super::mocks::mount_manifest_push(server, repo, tag).await;
    }
}

// ---------------------------------------------------------------------------
// ArtifactBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing OCI artifact manifests (signatures, SBOMs, etc).
///
/// Artifacts differ from images in having `artifact_type` set and being
/// referenced by digest (not tag). Each artifact has exactly one config blob
/// and one layer blob.
#[must_use = "builders do nothing until .build() is called"]
pub struct ArtifactBuilder {
    config_data: Vec<u8>,
    layer_data: Vec<u8>,
    artifact_type: String,
}

impl ArtifactBuilder {
    /// Create an artifact with the given config and layer blobs.
    /// Defaults to cosign signature artifact type.
    pub fn new(config_data: &[u8], layer_data: &[u8]) -> Self {
        Self {
            config_data: config_data.to_vec(),
            layer_data: layer_data.to_vec(),
            artifact_type: "application/vnd.dev.cosign.artifact.sig.v1+json".to_string(),
        }
    }

    /// Override the default artifact type.
    pub fn artifact_type(mut self, t: &str) -> Self {
        self.artifact_type = t.to_string();
        self
    }

    /// Build the artifact manifest and all parts needed for mock setup.
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
        let hash = ocync_distribution::sha256::Sha256::digest(&bytes);
        let digest = Digest::from_sha256(hash);

        ArtifactParts {
            manifest,
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
    pub manifest: ImageManifest,
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub config_data: Vec<u8>,
    pub config_desc: Descriptor,
    pub layer_data: Vec<u8>,
    pub layer_desc: Descriptor,
    pub artifact_type: String,
}

impl ArtifactParts {
    /// Mount source mocks: manifest GET by digest + blob GETs.
    pub async fn mount_source(&self, server: &wiremock::MockServer, repo: &str) {
        let digest_str = self.digest.to_string();
        super::mocks::mount_source_manifest(server, repo, &digest_str, &self.bytes).await;
        super::mocks::mount_blob_pull(server, repo, &self.config_desc.digest, &self.config_data)
            .await;
        super::mocks::mount_blob_pull(server, repo, &self.layer_desc.digest, &self.layer_data)
            .await;
    }

    /// Mount target mocks: blob HEAD 404s + blob push + manifest push by digest.
    pub async fn mount_target(&self, server: &wiremock::MockServer, repo: &str) {
        super::mocks::mount_blob_not_found(server, repo, &self.config_desc.digest).await;
        super::mocks::mount_blob_not_found(server, repo, &self.layer_desc.digest).await;
        super::mocks::mount_blob_push(server, repo).await;
        let digest_str = self.digest.to_string();
        super::mocks::mount_manifest_push(server, repo, &digest_str).await;
    }

    /// Build a referrers index descriptor for this artifact.
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

// ---------------------------------------------------------------------------
// IndexBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing OCI image index (multi-arch) manifests.
#[must_use = "builders do nothing until .build() is called"]
#[derive(Default)]
pub struct IndexBuilder {
    children: Vec<(ManifestParts, Option<Platform>)>,
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Add a child manifest with an optional platform annotation.
    pub fn manifest(mut self, parts: &ManifestParts, platform: Option<Platform>) -> Self {
        self.children.push((parts.clone(), platform));
        self
    }

    /// Build the index, returning all parts needed for mock setup.
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
        let hash = ocync_distribution::sha256::Sha256::digest(&bytes);
        let digest = Digest::from_sha256(hash);

        let children: Vec<ManifestParts> = self.children.into_iter().map(|(p, _)| p).collect();

        IndexParts {
            index,
            bytes,
            digest,
            children,
        }
    }
}

/// Output of [`IndexBuilder::build`].
#[derive(Clone)]
pub struct IndexParts {
    pub index: ImageIndex,
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub children: Vec<ManifestParts>,
}

impl IndexParts {
    /// Mount source mocks: index GET by tag + child manifest GETs by digest + all blob GETs.
    pub async fn mount_source(&self, server: &wiremock::MockServer, repo: &str, tag: &str) {
        // Index manifest by tag.
        super::mocks::mount_source_manifest_with_content_type(
            server,
            repo,
            tag,
            &self.bytes,
            MediaType::OciIndex,
        )
        .await;

        // Each child manifest by digest + its blobs.
        for child in &self.children {
            let digest_str = child.digest.to_string();
            super::mocks::mount_source_manifest(server, repo, &digest_str, &child.bytes).await;
            super::mocks::mount_blob_pull(
                server,
                repo,
                &child.config_desc.digest,
                &child.config_data,
            )
            .await;
            for (data, desc) in child.layers_data.iter().zip(child.layer_descs.iter()) {
                super::mocks::mount_blob_pull(server, repo, &desc.digest, data).await;
            }
        }
    }

    /// Mount target mocks: index HEAD 404 + all child blob HEAD 404s + push endpoints.
    pub async fn mount_target(&self, server: &wiremock::MockServer, repo: &str, tag: &str) {
        super::mocks::mount_manifest_head_not_found(server, repo, tag).await;
        for child in &self.children {
            super::mocks::mount_blob_not_found(server, repo, &child.config_desc.digest).await;
            for desc in &child.layer_descs {
                super::mocks::mount_blob_not_found(server, repo, &desc.digest).await;
            }
        }
        super::mocks::mount_blob_push(server, repo).await;
        super::mocks::mount_manifest_push(server, repo, tag).await;
        // Push each child manifest by digest.
        for child in &self.children {
            let digest_str = child.digest.to_string();
            super::mocks::mount_manifest_push(server, repo, &digest_str).await;
        }
    }
}

// ---------------------------------------------------------------------------
// ReferrersIndexBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing OCI referrers index responses.
#[must_use = "builders do nothing until .build() is called"]
#[derive(Default)]
pub struct ReferrersIndexBuilder {
    descriptors: Vec<Descriptor>,
}

impl ReferrersIndexBuilder {
    pub fn new() -> Self {
        Self {
            descriptors: Vec::new(),
        }
    }

    /// Add a raw descriptor to the referrers index.
    pub fn descriptor(mut self, desc: Descriptor) -> Self {
        self.descriptors.push(desc);
        self
    }

    /// Add an artifact's referrers descriptor (convenience over `.descriptor()`).
    pub fn artifact(self, parts: &ArtifactParts) -> Self {
        self.descriptor(parts.referrers_descriptor())
    }

    /// Build the referrers index as serialized bytes.
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
