//! Sync engine — two-phase orchestrator for image transfers.

use std::sync::Arc;

use ocync_distribution::spec::ImageManifest;
use ocync_distribution::{Digest, RegistryClient};

/// A fully resolved mapping ready for the sync engine.
///
/// All config resolution (registry lookup, tag filtering) is done
/// before constructing this type. The engine operates purely on
/// resolved values.
#[derive(Debug)]
pub struct ResolvedMapping {
    /// Client for the source registry.
    pub source_client: Arc<RegistryClient>,
    /// Repository path at the source (e.g. `library/nginx`).
    pub source_repo: String,
    /// Repository path at the target (e.g. `mirror/nginx`).
    pub target_repo: String,
    /// Target registries to sync to.
    pub targets: Vec<TargetEntry>,
    /// Tags to sync (already filtered).
    pub tags: Vec<String>,
}

/// A single target registry entry.
#[derive(Debug)]
pub struct TargetEntry {
    /// Human-readable name from config (e.g. `us-ecr`).
    pub name: String,
    /// Client for this target registry.
    pub client: Arc<RegistryClient>,
}

/// Collect all blob digests from an image manifest (config + layers).
fn collect_image_blobs(manifest: &ImageManifest) -> Vec<Digest> {
    let mut digests = Vec::with_capacity(1 + manifest.layers.len());
    digests.push(manifest.config.digest.clone());
    for layer in &manifest.layers {
        digests.push(layer.digest.clone());
    }
    digests
}

#[cfg(test)]
mod tests {
    use ocync_distribution::spec::{Descriptor, MediaType};

    use super::*;

    /// Build a valid sha256 digest from a short suffix, zero-padded to 64 hex chars.
    fn test_digest(suffix: &str) -> Digest {
        format!("sha256:{suffix:0>64}").parse().unwrap()
    }

    /// Build a minimal descriptor with the given digest.
    fn test_descriptor(digest: Digest, media_type: MediaType) -> Descriptor {
        Descriptor {
            media_type,
            digest,
            size: 0,
            platform: None,
            artifact_type: None,
            annotations: None,
        }
    }

    #[test]
    fn collect_blobs_config_and_layers() {
        let config_digest = test_digest("c0");
        let layer1_digest = test_digest("a1");
        let layer2_digest = test_digest("b2");

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: test_descriptor(config_digest.clone(), MediaType::OciConfig),
            layers: vec![
                test_descriptor(layer1_digest.clone(), MediaType::OciLayerGzip),
                test_descriptor(layer2_digest.clone(), MediaType::OciLayerGzip),
            ],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0], config_digest);
        assert_eq!(blobs[1], layer1_digest);
        assert_eq!(blobs[2], layer2_digest);
    }

    #[test]
    fn collect_blobs_no_layers() {
        let config_digest = test_digest("cc");

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: test_descriptor(config_digest.clone(), MediaType::OciConfig),
            layers: vec![],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0], config_digest);
    }
}
