//! OCI image spec types - manifests, descriptors, and platforms.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

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

impl Platform {
    /// Returns `true` if this platform matches the given filter.
    ///
    /// Comparison is case-insensitive. When the filter specifies only `os/arch`,
    /// the platform's `variant` field is ignored - any variant (or none) will
    /// match.
    pub fn matches(&self, filter: &PlatformFilter) -> bool {
        if !self.os.eq_ignore_ascii_case(&filter.os)
            || !self.architecture.eq_ignore_ascii_case(&filter.arch)
        {
            return false;
        }

        match (&filter.variant, &self.variant) {
            (Some(fv), Some(pv)) => pv.eq_ignore_ascii_case(fv),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }
}

/// A platform filter such as `linux/amd64` or `linux/arm64/v8`, for matching
/// against [`Platform`] values.
#[derive(Debug, Clone)]
pub struct PlatformFilter {
    os: String,
    arch: String,
    variant: Option<String>,
}

impl FromStr for PlatformFilter {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.splitn(3, '/');
        let (Some(os), Some(arch)) = (parts.next(), parts.next()) else {
            return Err(Error::InvalidPlatformFilter {
                input: s.into(),
                reason: "expected 'os/arch' or 'os/arch/variant'".into(),
            });
        };

        if os.is_empty() || arch.is_empty() {
            return Err(Error::InvalidPlatformFilter {
                input: s.into(),
                reason: "os and arch must be non-empty".into(),
            });
        }

        let variant = parts.next().map(|v| {
            if v.is_empty() {
                Err(Error::InvalidPlatformFilter {
                    input: s.into(),
                    reason: "variant must be non-empty when specified".into(),
                })
            } else {
                Ok(v.to_owned())
            }
        });

        Ok(Self {
            os: os.to_owned(),
            arch: arch.to_owned(),
            variant: variant.transpose()?,
        })
    }
}

impl fmt::Display for PlatformFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)?;
        if let Some(ref v) = self.variant {
            write!(f, "/{v}")?;
        }
        Ok(())
    }
}

/// An OCI repository name (e.g. `library/nginx`, `mirror/nginx`).
///
/// Distinguishes repository names from tags, hostnames and other strings.
///
/// Deserialization goes through [`RepositoryName::new`] so a name read from a
/// cache file obeys the same character rule as one parsed from user input.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct RepositoryName(String);

impl TryFrom<String> for RepositoryName {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl RepositoryName {
    /// A repository name: non-empty and only `[a-zA-Z0-9._/-]`, as
    /// [`Reference`](crate::Reference) parsing accepts.
    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        match Self::check(&name) {
            Ok(()) => Ok(Self(name)),
            Err(reason) => Err(Error::InvalidReference {
                input: name,
                reason,
            }),
        }
    }

    /// [`Self::new`] for a name taken from `reference`, so the error names
    /// the whole reference the user wrote.
    pub(crate) fn within(name: String, reference: &str) -> Result<Self, Error> {
        Self::check(&name).map_err(|reason| Error::InvalidReference {
            input: reference.to_owned(),
            reason,
        })?;
        Ok(Self(name))
    }

    /// Why `name` is not a repository name, if it is not.
    fn check(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("repository name must be non-empty".into());
        }
        match name
            .chars()
            .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '/' | '-'))
        {
            Some(bad) => Err(format!(
                "repository name contains invalid character '{bad}'; \
                 allowed: [a-zA-Z0-9._/-]"
            )),
            None => Ok(()),
        }
    }

    /// Return the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for RepositoryName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RepositoryName {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// The longest tag the OCI distribution spec allows.
const TAG_MAX_LEN: usize = 128;

/// An OCI tag name (e.g. `latest`, `v1.0-rc.1`): 1 to 128 characters from
/// `[a-zA-Z0-9_.-]`.
///
/// Deserialization goes through [`Tag::new`] so a tag read from a config file
/// obeys the same rule as one parsed from a [`Reference`](crate::Reference).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Tag(String);

impl TryFrom<String> for Tag {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl Tag {
    /// A tag name within the length and character limits above.
    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        match Self::check(&name) {
            Ok(()) => Ok(Self(name)),
            Err(reason) => Err(Error::InvalidReference {
                input: name,
                reason,
            }),
        }
    }

    /// [`Self::new`] for a tag taken from `reference`, so the error names the
    /// whole reference the user wrote.
    pub(crate) fn within(name: String, reference: &str) -> Result<Self, Error> {
        Self::check(&name).map_err(|reason| Error::InvalidReference {
            input: reference.to_owned(),
            reason,
        })?;
        Ok(Self(name))
    }

    /// Why `name` is not a tag, if it is not.
    fn check(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("tag must be non-empty".into());
        }
        if name.len() > TAG_MAX_LEN {
            return Err(format!(
                "tag exceeds {TAG_MAX_LEN} characters ({})",
                name.len()
            ));
        }
        match name
            .chars()
            .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-'))
        {
            Some(bad) => Err(format!(
                "tag contains invalid character '{bad}'; allowed: [a-zA-Z0-9_.-]"
            )),
            None => Ok(()),
        }
    }

    /// Return the tag as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Registry authority as `host:port` (e.g. `cgr.dev:443`, `localhost:5000`).
///
/// Used as a cache key to identify which registry a manifest was observed at.
/// Does not include the URL scheme - registries at the same host:port but
/// different schemes produce the same key, which is acceptable because no
/// production registry serves both HTTP and HTTPS on the same port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RegistryAuthority(String);

impl RegistryAuthority {
    /// Create a new registry authority.
    pub fn new(authority: impl Into<String>) -> Self {
        Self(authority.into())
    }

    /// Return the authority as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for RegistryAuthority {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
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

/// A parsed manifest - either a single image or a multi-platform index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestKind {
    /// A single-platform image manifest.
    Image(Box<ImageManifest>),
    /// A multi-platform image index.
    Index(Box<ImageIndex>),
}

impl ManifestKind {
    /// Deserialize a manifest from JSON bytes, using the media type to discriminate.
    ///
    /// Validates that `schemaVersion` is `2` as required by the OCI spec.
    #[expect(
        clippy::disallowed_methods,
        reason = "a manifest holds no credential, and the refused value helps fix it"
    )]
    pub fn from_json(media_type: &MediaType, bytes: &[u8]) -> Result<Self, Error> {
        match media_type {
            MediaType::OciManifest | MediaType::DockerManifestV2 => {
                let m: ImageManifest = serde_json::from_slice(bytes)?;
                if m.schema_version != 2 {
                    return Err(Error::InvalidManifest {
                        reason: format!(
                            "unsupported schemaVersion {}, expected 2",
                            m.schema_version
                        ),
                    });
                }
                Ok(Self::Image(Box::new(m)))
            }
            MediaType::OciIndex | MediaType::DockerManifestList => {
                let m: ImageIndex = serde_json::from_slice(bytes)?;
                if m.schema_version != 2 {
                    return Err(Error::InvalidManifest {
                        reason: format!(
                            "unsupported schemaVersion {}, expected 2",
                            m.schema_version
                        ),
                    });
                }
                Ok(Self::Index(Box::new(m)))
            }
            _ => Err(Error::UnsupportedMediaType {
                media_type: media_type.to_string(),
            }),
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

    // - MediaType tests --

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

    // - Descriptor tests --

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

    /// The media type picks how a manifest is read: an image for the OCI
    /// and Docker v2 manifest types, an index for the OCI index and Docker
    /// manifest list, and any other type is refused. Breaks if a type is
    /// read as the wrong kind or an unknown type is accepted.
    #[test]
    fn the_media_type_picks_the_manifest_kind() {
        let image = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        }))
        .unwrap();
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": TEST_DIGEST,
                "size": 1000
            }]
        }))
        .unwrap();
        for media_type in [MediaType::OciManifest, MediaType::DockerManifestV2] {
            let m = ManifestKind::from_json(&media_type, &image).unwrap();
            assert!(matches!(m, ManifestKind::Image(_)), "{media_type:?}");
        }
        for media_type in [MediaType::OciIndex, MediaType::DockerManifestList] {
            let m = ManifestKind::from_json(&media_type, &index).unwrap();
            assert!(matches!(m, ManifestKind::Index(_)), "{media_type:?}");
        }
        let other = MediaType::Other("text/plain".into());
        let err = ManifestKind::from_json(&other, &image).unwrap_err();
        assert!(matches!(err, Error::UnsupportedMediaType { .. }), "{err:?}");
    }

    // - Platform::matches tests --

    fn linux_amd64() -> Platform {
        Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: None,
            os_version: None,
            os_features: None,
        }
    }

    fn linux_arm64_v8() -> Platform {
        Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: Some("v8".into()),
            os_version: None,
            os_features: None,
        }
    }

    /// Helper to parse a filter string, panicking on invalid input.
    fn filter(s: &str) -> PlatformFilter {
        s.parse().unwrap()
    }

    #[test]
    fn platform_matches_os_arch() {
        assert!(linux_amd64().matches(&filter("linux/amd64")));
    }

    #[test]
    fn platform_matches_os_arch_variant() {
        assert!(linux_arm64_v8().matches(&filter("linux/arm64/v8")));
    }

    #[test]
    fn platform_matches_case_insensitive() {
        assert!(linux_amd64().matches(&filter("Linux/AMD64")));
        assert!(linux_arm64_v8().matches(&filter("LINUX/ARM64/V8")));
    }

    #[test]
    fn platform_matches_variant_mismatch_returns_false() {
        // filter specifies v7 but platform has v8
        let p = Platform {
            architecture: "arm".into(),
            os: "linux".into(),
            variant: Some("v8".into()),
            os_version: None,
            os_features: None,
        };
        assert!(!p.matches(&filter("linux/arm/v7")));
    }

    #[test]
    fn platform_matches_filter_variant_no_platform_variant_returns_false() {
        // filter requires a specific variant but platform has none
        assert!(!linux_amd64().matches(&filter("linux/amd64/v1")));
    }

    #[test]
    fn platform_matches_os_arch_ignores_platform_variant() {
        // os/arch filter matches regardless of what variant the platform has
        assert!(linux_arm64_v8().matches(&filter("linux/arm64")));
    }

    #[test]
    fn platform_matches_wrong_os_returns_false() {
        assert!(!linux_amd64().matches(&filter("windows/amd64")));
    }

    #[test]
    fn platform_matches_wrong_arch_returns_false() {
        assert!(!linux_amd64().matches(&filter("linux/arm64")));
    }

    // --- PlatformFilter parsing tests ---

    /// A platform filter is `os/arch` or `os/arch/variant`, each part
    /// non-empty, and prints back as written. Breaks if a missing or empty
    /// part is accepted, or a refusal does not say which part is wrong.
    #[test]
    fn platform_filters_parse_or_name_the_missing_part() {
        for input in ["linux/amd64", "linux/arm64/v8"] {
            let f: PlatformFilter = input.parse().unwrap();
            assert_eq!(f.to_string(), input);
        }
        let refused: [(&str, &[&str]); 5] = [
            ("", &["os/arch"]),
            ("linux", &["os/arch"]),
            ("/amd64", &["non-empty"]),
            ("linux/", &["non-empty"]),
            ("linux/amd64/", &["variant", "non-empty"]),
        ];
        for (given, needles) in refused {
            let err = given.parse::<PlatformFilter>().unwrap_err();
            let Error::InvalidPlatformFilter { input, reason } = &err else {
                panic!("{given:?}: expected InvalidPlatformFilter, got {err:?}");
            };
            assert_eq!(input, given);
            for needle in needles {
                assert!(reason.contains(needle), "{given:?}: {reason}");
            }
        }
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

    #[test]
    fn manifest_from_json_rejects_schema_version_1() {
        let json = serde_json::json!({
            "schemaVersion": 1,
            "config": test_descriptor(),
            "layers": [test_layer_descriptor()]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let err = ManifestKind::from_json(&MediaType::OciManifest, &bytes).unwrap_err();
        assert!(
            matches!(err, Error::InvalidManifest { .. }),
            "expected ManifestValidation, got: {err}"
        );
    }

    #[test]
    fn index_from_json_rejects_schema_version_1() {
        let json = serde_json::json!({
            "schemaVersion": 1,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": TEST_DIGEST,
                "size": 1000
            }]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let err = ManifestKind::from_json(&MediaType::OciIndex, &bytes).unwrap_err();
        assert!(
            matches!(err, Error::InvalidManifest { .. }),
            "expected ManifestValidation, got: {err}"
        );
    }

    // --- RepositoryName validation tests ---

    #[test]
    fn repository_name_valid() {
        assert!(RepositoryName::new("library/nginx").is_ok());
        assert!(RepositoryName::new("My-Org/my_repo.v2").is_ok());
    }

    #[test]
    fn repository_name_rejects_invalid_chars() {
        let err = RepositoryName::new("repo?evil").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid character"), "error: {msg}");
        assert!(msg.contains('?'), "error: {msg}");

        assert!(RepositoryName::new("repo name").is_err());
        assert!(RepositoryName::new("repo#frag").is_err());
    }

    #[test]
    fn repository_name_serde_round_trips_as_plain_string() {
        let name = RepositoryName::new("library/nginx").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""library/nginx""#);
        let back: RepositoryName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
    }

    #[test]
    fn repository_name_deserialize_rejects_invalid() {
        assert!(serde_json::from_str::<RepositoryName>(r#""bad name""#).is_err());
    }

    #[test]
    fn repository_name_rejects_empty() {
        let err = RepositoryName::new("").unwrap_err();
        assert!(err.to_string().contains("non-empty"), "error: {err}");
    }

    /// A tag takes 1 to 128 characters of `[a-zA-Z0-9_.-]` and nothing else.
    /// Breaks if the length bound moves, an empty tag or a character that
    /// would change a request URL is accepted, or an error loses its reason.
    #[test]
    fn tag_accepts_only_the_oci_rule() {
        let at_limit = "a".repeat(128);
        let over_limit = "a".repeat(129);
        let cases = [
            ("latest", None),
            ("v1.0-rc.1_x", None),
            (at_limit.as_str(), None),
            ("", Some("tag must be non-empty")),
            (
                over_limit.as_str(),
                Some("tag exceeds 128 characters (129)"),
            ),
            ("v1+build", Some("tag contains invalid character '+'")),
            ("a b", Some("tag contains invalid character ' '")),
            ("a/b", Some("tag contains invalid character '/'")),
            ("a:b", Some("tag contains invalid character ':'")),
            ("a@b", Some("tag contains invalid character '@'")),
        ];
        for (input, want_err) in cases {
            match (Tag::new(input), want_err) {
                (Ok(tag), None) => assert_eq!(tag.as_str(), input),
                (Err(err), Some(needle)) => {
                    assert!(err.to_string().contains(needle), "{input:?}: {err}")
                }
                (got, want) => panic!("{input:?}: got {got:?}, wanted error {want:?}"),
            }
        }
    }

    /// A tag serializes as a plain string and deserializing runs the rule.
    /// Breaks if deserialization skips `Tag::new`.
    #[test]
    fn tag_serde_round_trips_and_rejects_invalid() {
        let tag = Tag::new("15.19").unwrap();
        let json = serde_json::to_string(&tag).unwrap();
        assert_eq!(json, r#""15.19""#);
        assert_eq!(serde_json::from_str::<Tag>(&json).unwrap(), tag);
        assert!(serde_json::from_str::<Tag>(r#""bad tag""#).is_err());
    }
}
