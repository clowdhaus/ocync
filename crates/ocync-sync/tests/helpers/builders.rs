//! Builder types for constructing test manifests with sensible defaults.

#![allow(dead_code, unused_imports, unreachable_pub)]

use ocync_distribution::Digest;
use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType};

use super::fixtures::{blob_descriptor, blob_digest};

/// Builder for constructing `ImageManifest` with sensible test defaults.
///
/// Simplifies the common pattern of creating test manifests with real blob
/// data (matching digests and sizes). Call [`layer`](Self::layer) to add
/// layers, then [`build`](Self::build) to get the manifest plus all parts
/// needed for mock setup.
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
