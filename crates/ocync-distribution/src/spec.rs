//! OCI image spec types — manifests, descriptors, and platforms.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::Error;

/// OCI and Docker media type.
///
/// Known types have dedicated variants for type-safe matching.
/// Unknown or future types are represented by [`Other`](Self::Other).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MediaType {
    /// `application/vnd.oci.image.manifest.v1+json`
    OciManifest,
    /// `application/vnd.oci.image.index.v1+json`
    OciIndex,
    /// `application/vnd.oci.image.config.v1+json`
    OciConfig,
    /// `application/vnd.oci.image.layer.v1.tar+gzip`
    OciLayerGzip,
    /// `application/vnd.oci.image.layer.v1.tar+zstd`
    OciLayerZstd,
    /// `application/vnd.oci.image.layer.nondistributable.v1.tar+gzip`
    OciLayerNondistributableGzip,
    /// `application/vnd.docker.distribution.manifest.v2+json`
    DockerManifestV2,
    /// `application/vnd.docker.distribution.manifest.list.v2+json`
    DockerManifestList,
    /// `application/vnd.docker.container.image.v1+json`
    DockerConfig,
    /// `application/vnd.docker.image.rootfs.diff.tar.gzip`
    DockerLayerGzip,
    /// An unrecognized media type.
    Other(String),
}

impl MediaType {
    /// The wire-format MIME type string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::OciManifest => "application/vnd.oci.image.manifest.v1+json",
            Self::OciIndex => "application/vnd.oci.image.index.v1+json",
            Self::OciConfig => "application/vnd.oci.image.config.v1+json",
            Self::OciLayerGzip => "application/vnd.oci.image.layer.v1.tar+gzip",
            Self::OciLayerZstd => "application/vnd.oci.image.layer.v1.tar+zstd",
            Self::OciLayerNondistributableGzip => {
                "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
            }
            Self::DockerManifestV2 => "application/vnd.docker.distribution.manifest.v2+json",
            Self::DockerManifestList => "application/vnd.docker.distribution.manifest.list.v2+json",
            Self::DockerConfig => "application/vnd.docker.container.image.v1+json",
            Self::DockerLayerGzip => "application/vnd.docker.image.rootfs.diff.tar.gzip",
            Self::Other(s) => s,
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Look up a known media type from its wire-format string.
fn known_media_type(s: &str) -> Option<MediaType> {
    Some(match s {
        "application/vnd.oci.image.manifest.v1+json" => MediaType::OciManifest,
        "application/vnd.oci.image.index.v1+json" => MediaType::OciIndex,
        "application/vnd.oci.image.config.v1+json" => MediaType::OciConfig,
        "application/vnd.oci.image.layer.v1.tar+gzip" => MediaType::OciLayerGzip,
        "application/vnd.oci.image.layer.v1.tar+zstd" => MediaType::OciLayerZstd,
        "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => {
            MediaType::OciLayerNondistributableGzip
        }
        "application/vnd.docker.distribution.manifest.v2+json" => MediaType::DockerManifestV2,
        "application/vnd.docker.distribution.manifest.list.v2+json" => {
            MediaType::DockerManifestList
        }
        "application/vnd.docker.container.image.v1+json" => MediaType::DockerConfig,
        "application/vnd.docker.image.rootfs.diff.tar.gzip" => MediaType::DockerLayerGzip,
        _ => return None,
    })
}

impl From<&str> for MediaType {
    fn from(s: &str) -> Self {
        known_media_type(s).unwrap_or_else(|| Self::Other(s.to_owned()))
    }
}

impl From<String> for MediaType {
    fn from(s: String) -> Self {
        known_media_type(&s).unwrap_or(Self::Other(s))
    }
}

impl Serialize for MediaType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MediaType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(MediaType::from(s))
    }
}

/// OCI content descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    /// MIME type of the referenced content.
    pub media_type: MediaType,
    /// Content-addressable digest of the referenced content.
    pub digest: Digest,
    /// Size of the referenced content in bytes.
    pub size: u64,

    /// Target platform, used in image index entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,

    /// Artifact type when the descriptor references an artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,

    /// Arbitrary metadata as key-value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// OCI platform specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    /// CPU architecture (e.g. `amd64`, `arm64`).
    pub architecture: String,
    /// Operating system (e.g. `linux`, `windows`).
    pub os: String,

    /// CPU variant (e.g. `v8` for `arm64`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,

    /// OS version (e.g. `10.0.17763.1999` for Windows).
    #[serde(rename = "os.version", skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,

    /// Required OS features.
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
    /// Must be `2` for OCI image manifests.
    pub schema_version: u32,
    /// Media type of this manifest.
    pub media_type: Option<MediaType>,
    /// Image configuration descriptor.
    pub config: Descriptor,
    /// Ordered list of layer descriptors.
    pub layers: Vec<Descriptor>,

    /// Subject descriptor for referrers API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,

    /// Artifact type when this manifest represents an artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,

    /// Arbitrary metadata as key-value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// OCI image index (multi-platform manifest list).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageIndex {
    /// Must be `2` for OCI image indexes.
    pub schema_version: u32,
    /// Media type of this index.
    pub media_type: Option<MediaType>,
    /// List of platform-specific manifest descriptors.
    pub manifests: Vec<Descriptor>,

    /// Subject descriptor for referrers API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,

    /// Artifact type when this index represents an artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,

    /// Arbitrary metadata as key-value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// A parsed manifest — either a single image or a multi-platform index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestKind {
    /// A single-platform image manifest.
    Image(Box<ImageManifest>),
    /// A multi-platform image index.
    Index(Box<ImageIndex>),
}

impl ManifestKind {
    /// Deserialize a manifest from JSON bytes, using the media type to discriminate.
    pub fn from_json(media_type: &MediaType, bytes: &[u8]) -> Result<Self, Error> {
        match media_type {
            MediaType::OciManifest | MediaType::DockerManifestV2 => {
                let m: ImageManifest = serde_json::from_slice(bytes)?;
                Ok(Self::Image(Box::new(m)))
            }
            MediaType::OciIndex | MediaType::DockerManifestList => {
                let m: ImageIndex = serde_json::from_slice(bytes)?;
                Ok(Self::Index(Box::new(m)))
            }
            _ => Err(Error::UnsupportedMediaType {
                media_type: media_type.to_string(),
            }),
        }
    }

    /// Return the digests of all content referenced by this manifest.
    ///
    /// For an image manifest this includes the config and all layers.
    /// For an index this includes the digests of the child manifests.
    pub fn referenced_digests(&self) -> Vec<&Digest> {
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

    const TEST_DIGEST: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn test_descriptor() -> serde_json::Value {
        serde_json::json!({
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": TEST_DIGEST,
            "size": 1234
        })
    }

    fn test_layer_descriptor() -> serde_json::Value {
        serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": TEST_DIGEST,
            "size": 5678
        })
    }

    // -- MediaType tests --

    #[test]
    fn media_type_from_known_string() {
        assert_eq!(
            MediaType::from("application/vnd.oci.image.manifest.v1+json"),
            MediaType::OciManifest
        );
        assert_eq!(
            MediaType::from("application/vnd.docker.distribution.manifest.v2+json"),
            MediaType::DockerManifestV2
        );
    }

    #[test]
    fn media_type_from_unknown_string() {
        let mt = MediaType::from("text/plain");
        assert_eq!(mt, MediaType::Other("text/plain".to_owned()));
        assert_eq!(mt.as_str(), "text/plain");
    }

    #[test]
    fn media_type_display() {
        assert_eq!(
            MediaType::OciManifest.to_string(),
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(
            MediaType::Other("custom/type".into()).to_string(),
            "custom/type"
        );
    }

    #[test]
    fn media_type_serde_roundtrip() {
        let mt = MediaType::OciManifest;
        let json = serde_json::to_string(&mt).unwrap();
        assert_eq!(json, r#""application/vnd.oci.image.manifest.v1+json""#);
        let parsed: MediaType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mt);
    }

    #[test]
    fn media_type_serde_unknown() {
        let json = r#""application/x-custom""#;
        let mt: MediaType = serde_json::from_str(json).unwrap();
        assert_eq!(mt, MediaType::Other("application/x-custom".into()));
    }

    // -- Descriptor tests --

    #[test]
    fn deserialize_descriptor() {
        let d: Descriptor = serde_json::from_value(test_descriptor()).unwrap();
        assert_eq!(d.media_type, MediaType::OciConfig);
        assert_eq!(d.digest.to_string(), TEST_DIGEST);
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
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let m: ImageManifest = serde_json::from_value(json).unwrap();
        assert_eq!(m.schema_version, 2);
        assert_eq!(m.config.media_type, MediaType::OciConfig);
        assert_eq!(m.layers.len(), 1);
    }

    #[test]
    fn deserialize_image_index() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": TEST_DIGEST,
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
        let m = ManifestKind::from_json(&MediaType::OciManifest, &bytes).unwrap();
        assert!(matches!(m, ManifestKind::Image(_)));
    }

    #[test]
    fn manifest_from_json_docker_v2() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = ManifestKind::from_json(&MediaType::DockerManifestV2, &bytes).unwrap();
        assert!(matches!(m, ManifestKind::Image(_)));
    }

    #[test]
    fn manifest_from_json_index() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": TEST_DIGEST,
                "size": 1000
            }]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = ManifestKind::from_json(&MediaType::OciIndex, &bytes).unwrap();
        assert!(matches!(m, ManifestKind::Index(_)));
    }

    #[test]
    fn manifest_from_json_unsupported() {
        let r = ManifestKind::from_json(&MediaType::Other("text/plain".into()), b"{}");
        assert!(r.is_err());
    }

    #[test]
    fn referenced_digests_image() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = ManifestKind::from_json(&MediaType::OciManifest, &bytes).unwrap();
        let digests = m.referenced_digests();
        assert_eq!(digests.len(), 2); // config + 1 layer
    }

    #[test]
    fn referenced_digests_index() {
        let json = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": TEST_DIGEST,
                    "size": 1000
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": TEST_DIGEST,
                    "size": 2000
                }
            ]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = ManifestKind::from_json(&MediaType::OciIndex, &bytes).unwrap();
        let digests = m.referenced_digests();
        assert_eq!(digests.len(), 2);
    }

    #[test]
    fn descriptor_with_artifact_type() {
        let json = serde_json::json!({
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": TEST_DIGEST,
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
    fn media_type_all_known_variants_roundtrip() {
        let known = [
            MediaType::OciManifest,
            MediaType::OciIndex,
            MediaType::OciConfig,
            MediaType::OciLayerGzip,
            MediaType::OciLayerZstd,
            MediaType::OciLayerNondistributableGzip,
            MediaType::DockerManifestV2,
            MediaType::DockerManifestList,
            MediaType::DockerConfig,
            MediaType::DockerLayerGzip,
        ];
        for mt in &known {
            let s = mt.as_str();
            let parsed = MediaType::from(s);
            assert_eq!(&parsed, mt, "roundtrip failed for {s}");
        }
    }
}
