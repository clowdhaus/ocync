//! OCI image spec types — manifests, descriptors, and platforms.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::DistributionError;

/// OCI and Docker media type constants.
pub mod media_types {
    // OCI image spec
    pub const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    pub const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    pub const OCI_IMAGE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
    pub const OCI_IMAGE_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    pub const OCI_IMAGE_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
    pub const OCI_IMAGE_LAYER_NONDISTRIBUTABLE_GZIP: &str =
        "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip";

    // Docker v2
    pub const DOCKER_MANIFEST_V2: &str = "application/vnd.docker.distribution.manifest.v2+json";
    pub const DOCKER_MANIFEST_LIST: &str =
        "application/vnd.docker.distribution.manifest.list.v2+json";
    pub const DOCKER_IMAGE_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
    pub const DOCKER_IMAGE_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
}

/// OCI content descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: Digest,
    pub size: i64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// OCI platform specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    pub architecture: String,
    pub os: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,

    #[serde(rename = "os.version", skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,

    #[serde(rename = "os.features", skip_serializing_if = "Option::is_none")]
    pub os_features: Option<Vec<String>>,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(ref v) = self.variant {
            write!(f, "/{v}")?;
        }
        Ok(())
    }
}

/// OCI image manifest (single-platform).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageManifest {
    pub schema_version: u32,
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// OCI image index (multi-platform manifest list).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageIndex {
    pub schema_version: u32,
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// A parsed manifest — either a single image or a multi-platform index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manifest {
    Image(Box<ImageManifest>),
    Index(Box<ImageIndex>),
}

impl Manifest {
    /// Deserialize a manifest from JSON bytes, using the media type to discriminate.
    pub fn from_json(media_type: &str, bytes: &[u8]) -> Result<Self, DistributionError> {
        match media_type {
            media_types::OCI_IMAGE_MANIFEST | media_types::DOCKER_MANIFEST_V2 => {
                let m: ImageManifest = serde_json::from_slice(bytes)?;
                Ok(Self::Image(Box::new(m)))
            }
            media_types::OCI_IMAGE_INDEX | media_types::DOCKER_MANIFEST_LIST => {
                let m: ImageIndex = serde_json::from_slice(bytes)?;
                Ok(Self::Index(Box::new(m)))
            }
            _ => Err(DistributionError::UnsupportedMediaType {
                media_type: media_type.to_owned(),
            }),
        }
    }

    /// Return the digests of all blobs referenced by this manifest.
    ///
    /// For an image manifest this includes the config and all layers.
    /// For an index this includes the digests of the child manifests.
    pub fn blob_digests(&self) -> Vec<&Digest> {
        match self {
            Self::Image(m) => {
                let mut digests = vec![&m.config.digest];
                digests.extend(m.layers.iter().map(|l| &l.digest));
                digests
            }
            Self::Index(m) => m.manifests.iter().map(|d| &d.digest).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn test_descriptor() -> serde_json::Value {
        serde_json::json!({
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": SHA,
            "size": 1234
        })
    }

    fn test_layer_descriptor() -> serde_json::Value {
        serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": SHA,
            "size": 5678
        })
    }

    #[test]
    fn deserialize_descriptor() {
        let d: Descriptor = serde_json::from_value(test_descriptor()).unwrap();
        assert_eq!(d.media_type, media_types::OCI_IMAGE_CONFIG);
        assert_eq!(d.digest.to_string(), SHA);
        assert_eq!(d.size, 1234);
        assert!(d.platform.is_none());
        assert!(d.artifact_type.is_none());
        assert!(d.annotations.is_none());
    }

    #[test]
    fn deserialize_platform() {
        let json = serde_json::json!({
            "architecture": "amd64",
            "os": "linux"
        });
        let p: Platform = serde_json::from_value(json).unwrap();
        assert_eq!(p.architecture, "amd64");
        assert_eq!(p.os, "linux");
        assert!(p.variant.is_none());
    }

    #[test]
    fn platform_display() {
        let p = Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: None,
            os_version: None,
            os_features: None,
        };
        assert_eq!(p.to_string(), "linux/amd64");

        let p2 = Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: Some("v8".into()),
            os_version: None,
            os_features: None,
        };
        assert_eq!(p2.to_string(), "linux/arm64/v8");
    }

    #[test]
    fn deserialize_image_manifest() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::OCI_IMAGE_MANIFEST,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let m: ImageManifest = serde_json::from_value(json).unwrap();
        assert_eq!(m.schema_version, 2);
        assert_eq!(m.config.media_type, media_types::OCI_IMAGE_CONFIG);
        assert_eq!(m.layers.len(), 1);
    }

    #[test]
    fn deserialize_image_index() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::OCI_IMAGE_INDEX,
            "manifests": [{
                "mediaType": media_types::OCI_IMAGE_MANIFEST,
                "digest": SHA,
                "size": 1000,
                "platform": {
                    "architecture": "amd64",
                    "os": "linux"
                }
            }]
        });
        let idx: ImageIndex = serde_json::from_value(json).unwrap();
        assert_eq!(idx.schema_version, 2);
        assert_eq!(idx.manifests.len(), 1);
        assert!(idx.manifests[0].platform.is_some());
    }

    #[test]
    fn manifest_from_json_oci_image() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = Manifest::from_json(media_types::OCI_IMAGE_MANIFEST, &bytes).unwrap();
        assert!(matches!(m, Manifest::Image(_)));
    }

    #[test]
    fn manifest_from_json_docker_v2() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = Manifest::from_json(media_types::DOCKER_MANIFEST_V2, &bytes).unwrap();
        assert!(matches!(m, Manifest::Image(_)));
    }

    #[test]
    fn manifest_from_json_index() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": media_types::OCI_IMAGE_MANIFEST,
                "digest": SHA,
                "size": 1000
            }]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = Manifest::from_json(media_types::OCI_IMAGE_INDEX, &bytes).unwrap();
        assert!(matches!(m, Manifest::Index(_)));
    }

    #[test]
    fn manifest_from_json_unsupported() {
        let r = Manifest::from_json("text/plain", b"{}");
        assert!(r.is_err());
    }

    #[test]
    fn blob_digests_image() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = Manifest::from_json(media_types::OCI_IMAGE_MANIFEST, &bytes).unwrap();
        let digests = m.blob_digests();
        assert_eq!(digests.len(), 2); // config + 1 layer
    }

    #[test]
    fn blob_digests_index() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [
                {
                    "mediaType": media_types::OCI_IMAGE_MANIFEST,
                    "digest": SHA,
                    "size": 1000
                },
                {
                    "mediaType": media_types::OCI_IMAGE_MANIFEST,
                    "digest": SHA,
                    "size": 2000
                }
            ]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = Manifest::from_json(media_types::OCI_IMAGE_INDEX, &bytes).unwrap();
        let digests = m.blob_digests();
        assert_eq!(digests.len(), 2);
    }

    #[test]
    fn descriptor_with_artifact_type() {
        let json = serde_json::json!({
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": SHA,
            "size": 100,
            "artifactType": "application/vnd.example+type"
        });
        let d: Descriptor = serde_json::from_value(json).unwrap();
        assert_eq!(
            d.artifact_type.as_deref(),
            Some("application/vnd.example+type")
        );
    }

    #[test]
    fn media_type_constants() {
        assert!(media_types::OCI_IMAGE_MANIFEST.contains("oci"));
        assert!(media_types::DOCKER_MANIFEST_V2.contains("docker"));
        assert!(media_types::OCI_IMAGE_INDEX.contains("index"));
        assert!(media_types::DOCKER_MANIFEST_LIST.contains("list"));
    }
}
