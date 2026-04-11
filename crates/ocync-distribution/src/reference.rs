//! OCI image reference parser (`registry/repository:tag@digest`).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::Error;

const DOCKER_HUB_REGISTRY: &str = "docker.io";
const DOCKER_HUB_OFFICIAL_REPO_PREFIX: &str = "library/";

/// An OCI image reference: `registry/repository:tag@digest`.
///
/// Handles Docker Hub shorthand (`nginx:latest` -> `docker.io/library/nginx:latest`),
/// registries with ports (`localhost:5000/repo:tag`), and ECR-style URLs.
/// Does NOT add an implicit `:latest` tag.
#[derive(Debug, Clone, Eq)]
pub struct Reference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<Digest>,
}

impl Reference {
    /// Build a reference from already-validated parts.
    pub fn from_parts(
        registry: impl Into<String>,
        repository: impl Into<String>,
        tag: Option<String>,
        digest: Option<Digest>,
    ) -> Self {
        Self {
            registry: registry.into(),
            repository: repository.into(),
            tag,
            digest,
        }
    }

    /// The registry hostname (e.g. `docker.io`, `ghcr.io`).
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The repository path (e.g. `library/nginx`, `clowdhaus/ocync`).
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// The tag, if present (e.g. `latest`, `v1.0`).
    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// The digest, if present.
    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }
}

/// Determine whether a hostname-like token is a registry host.
///
/// A token is considered a registry if it contains a dot, a colon (port),
/// or equals `localhost`.
fn is_registry(s: &str) -> bool {
    s.contains('.') || s.contains(':') || s == "localhost"
}

impl FromStr for Reference {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(Error::InvalidReference {
                input: s.into(),
                reason: "empty string".into(),
            });
        }

        // Split off @digest first.
        let (name_tag, digest) = if let Some(at) = s.rfind('@') {
            let digest_str = &s[at + 1..];
            let digest: Digest = digest_str.parse().map_err(|_| Error::InvalidReference {
                input: s.into(),
                reason: format!("invalid digest '{digest_str}'"),
            })?;
            (&s[..at], Some(digest))
        } else {
            (s, None)
        };

        // Split name_tag into name and tag.
        // We need to find the tag separator, which is the LAST colon that
        // appears after the last slash (to avoid confusing port colons).
        let (name, tag) = split_name_tag(name_tag);

        if name.is_empty() {
            return Err(Error::InvalidReference {
                input: s.into(),
                reason: "empty name".into(),
            });
        }

        // Determine registry vs repository.
        let (registry, repository) = if let Some(slash) = name.find('/') {
            let first = &name[..slash];
            if is_registry(first) {
                (first.to_owned(), name[slash + 1..].to_owned())
            } else {
                // e.g. "myuser/myrepo" — Docker Hub implied
                (DOCKER_HUB_REGISTRY.to_owned(), name.to_owned())
            }
        } else {
            // Single component — Docker Hub official image
            (
                DOCKER_HUB_REGISTRY.to_owned(),
                format!("{DOCKER_HUB_OFFICIAL_REPO_PREFIX}{name}"),
            )
        };

        if repository.is_empty() {
            return Err(Error::InvalidReference {
                input: s.into(),
                reason: "empty repository".into(),
            });
        }

        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }
}

/// Split a name (without digest) into (name, optional tag).
///
/// The tag separator is the last colon that appears after the last slash.
/// This avoids treating a port number as a tag.
fn split_name_tag(name_tag: &str) -> (&str, Option<String>) {
    let after_slash = name_tag.rfind('/').map_or(0, |i| i + 1);
    let suffix = &name_tag[after_slash..];
    if let Some(colon) = suffix.rfind(':') {
        let tag_start = after_slash + colon + 1;
        let tag = &name_tag[tag_start..];
        if tag.is_empty() {
            (name_tag, None)
        } else {
            (&name_tag[..after_slash + colon], Some(tag.to_owned()))
        }
    } else {
        (name_tag, None)
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(ref tag) = self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(ref digest) = self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

impl PartialEq for Reference {
    fn eq(&self, other: &Self) -> bool {
        self.registry == other.registry
            && self.repository == other.repository
            && self.tag == other.tag
            && self.digest == other.digest
    }
}

impl Hash for Reference {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.registry.hash(state);
        self.repository.hash(state);
        self.tag.hash(state);
        self.digest.hash(state);
    }
}

impl Serialize for Reference {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Reference {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DIGEST: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn full_with_tag() {
        let r: Reference = "ghcr.io/clowdhaus/ocync:v1.0".parse().unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "clowdhaus/ocync");
        assert_eq!(r.tag(), Some("v1.0"));
        assert!(r.digest().is_none());
    }

    #[test]
    fn full_with_digest() {
        let input = format!("ghcr.io/clowdhaus/ocync@{TEST_DIGEST}");
        let r: Reference = input.parse().unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "clowdhaus/ocync");
        assert!(r.tag().is_none());
        assert_eq!(r.digest().unwrap().to_string(), TEST_DIGEST);
    }

    #[test]
    fn tag_and_digest() {
        let input = format!("ghcr.io/clowdhaus/ocync:v1.0@{TEST_DIGEST}");
        let r: Reference = input.parse().unwrap();
        assert_eq!(r.tag(), Some("v1.0"));
        assert_eq!(r.digest().unwrap().to_string(), TEST_DIGEST);
    }

    #[test]
    fn docker_hub_shorthand_library() {
        let r: Reference = "nginx".parse().unwrap();
        assert_eq!(r.registry(), "docker.io");
        assert_eq!(r.repository(), "library/nginx");
        assert!(r.tag().is_none());
    }

    #[test]
    fn docker_hub_user_repo() {
        let r: Reference = "myuser/myrepo".parse().unwrap();
        assert_eq!(r.registry(), "docker.io");
        assert_eq!(r.repository(), "myuser/myrepo");
    }

    #[test]
    fn docker_hub_with_tag() {
        let r: Reference = "nginx:latest".parse().unwrap();
        assert_eq!(r.registry(), "docker.io");
        assert_eq!(r.repository(), "library/nginx");
        assert_eq!(r.tag(), Some("latest"));
    }

    #[test]
    fn with_port() {
        let r: Reference = "localhost:5000/myrepo:v1".parse().unwrap();
        assert_eq!(r.registry(), "localhost:5000");
        assert_eq!(r.repository(), "myrepo");
        assert_eq!(r.tag(), Some("v1"));
    }

    #[test]
    fn ecr() {
        let r: Reference = "123456789012.dkr.ecr.us-east-1.amazonaws.com/my-app:latest"
            .parse()
            .unwrap();
        assert_eq!(r.registry(), "123456789012.dkr.ecr.us-east-1.amazonaws.com");
        assert_eq!(r.repository(), "my-app");
        assert_eq!(r.tag(), Some("latest"));
    }

    #[test]
    fn no_tag_no_digest() {
        let r: Reference = "ghcr.io/clowdhaus/ocync".parse().unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "clowdhaus/ocync");
        assert!(r.tag().is_none());
        assert!(r.digest().is_none());
    }

    #[test]
    fn empty_string_error() {
        let r = "".parse::<Reference>();
        assert!(r.is_err());
    }

    #[test]
    fn display_roundtrip() {
        let input = "ghcr.io/clowdhaus/ocync:v1.0";
        let r: Reference = input.parse().unwrap();
        assert_eq!(r.to_string(), input);
    }

    #[test]
    fn display_roundtrip_with_digest() {
        let input = format!("ghcr.io/clowdhaus/ocync@{TEST_DIGEST}");
        let r: Reference = input.parse().unwrap();
        assert_eq!(r.to_string(), input);
    }

    #[test]
    fn from_parts() {
        let d: Digest = TEST_DIGEST.parse().unwrap();
        let r = Reference::from_parts("ghcr.io", "clowdhaus/ocync", Some("v1.0".into()), Some(d));
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "clowdhaus/ocync");
        assert_eq!(r.tag(), Some("v1.0"));
        assert!(r.digest().is_some());
    }

    #[test]
    fn serde_roundtrip() {
        let r: Reference = "ghcr.io/clowdhaus/ocync:v1.0".parse().unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let r2: Reference = serde_json::from_str(&json).unwrap();
        assert_eq!(r, r2);
    }
}
