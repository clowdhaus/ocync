//! OCI image reference parser (`registry/repository:tag@digest`).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::Error;
use crate::spec::RepositoryName;

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

/// Whether the first path component names a registry host: it contains a
/// dot or a port colon, or is `localhost`.
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

        let (name, tag) = split_name_tag(name_tag);

        if name.is_empty() {
            return Err(Error::InvalidReference {
                input: s.into(),
                reason: "empty name".into(),
            });
        }

        let (registry, repository) = if let Some(slash) = name.find('/') {
            let first = &name[..slash];
            if is_registry(first) {
                (first.to_owned(), name[slash + 1..].to_owned())
            } else {
                // e.g. "myuser/myrepo" - Docker Hub implied
                (DOCKER_HUB_REGISTRY.to_owned(), name.to_owned())
            }
        } else {
            // Single component - Docker Hub official image
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

        // The character rule lives in `RepositoryName::new`; only the error's
        // `input` is widened to the full reference string.
        RepositoryName::new(&repository).map_err(|e| match e {
            Error::InvalidReference { reason, .. } => Error::InvalidReference {
                input: s.into(),
                reason,
            },
            other => other,
        })?;

        // Validate tag: must be 1-128 chars and match [a-zA-Z0-9_.-].
        if let Some(ref t) = tag {
            if t.len() > 128 {
                return Err(Error::InvalidReference {
                    input: s.into(),
                    reason: format!("tag exceeds 128 characters ({})", t.len()),
                });
            }
            if let Some(bad) = t
                .chars()
                .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-'))
            {
                return Err(Error::InvalidReference {
                    input: s.into(),
                    reason: format!(
                        "tag contains invalid character '{bad}'; \
                         allowed: [a-zA-Z0-9_.-]"
                    ),
                });
            }
        }

        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }
}

/// Split a name (without digest) into (name, optional tag) at the last
/// colon after the last slash, so a port is never read as a tag.
fn split_name_tag(name_tag: &str) -> (&str, Option<String>) {
    let after_slash = name_tag.rfind('/').map_or(0, |i| i + 1);
    let suffix = &name_tag[after_slash..];
    if let Some(colon) = suffix.rfind(':') {
        let tag_start = after_slash + colon + 1;
        let tag = &name_tag[tag_start..];
        if tag.is_empty() {
            // Trailing colon with no tag (e.g. "ghcr.io/repo:") -- strip the colon.
            (&name_tag[..after_slash + colon], None)
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

    /// A reference splits into registry, repository and tag, with Docker
    /// Hub and `library/` filled in for short names. Breaks if a port, an
    /// IP, a nested path, a trailing colon or a 128-character tag is read
    /// wrongly.
    #[test]
    fn parses_registry_repository_and_tag() {
        let long_tag = "a".repeat(128);
        let long = format!("ghcr.io/repo:{long_tag}");
        let cases = [
            (
                "ghcr.io/clowdhaus/ocync:v1.0",
                "ghcr.io",
                "clowdhaus/ocync",
                Some("v1.0"),
            ),
            (
                "ghcr.io/clowdhaus/ocync",
                "ghcr.io",
                "clowdhaus/ocync",
                None,
            ),
            ("nginx", "docker.io", "library/nginx", None),
            ("nginx:latest", "docker.io", "library/nginx", Some("latest")),
            ("myuser/myrepo", "docker.io", "myuser/myrepo", None),
            (
                "myuser/myrepo:latest",
                "docker.io",
                "myuser/myrepo",
                Some("latest"),
            ),
            (
                "localhost:5000/myrepo:v1",
                "localhost:5000",
                "myrepo",
                Some("v1"),
            ),
            ("localhost/repo:tag", "localhost", "repo", Some("tag")),
            (
                "192.168.1.1:5000/repo:tag",
                "192.168.1.1:5000",
                "repo",
                Some("tag"),
            ),
            (
                "123456789012.dkr.ecr.us-east-1.amazonaws.com/my-app:latest",
                "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                "my-app",
                Some("latest"),
            ),
            (
                "ghcr.io/a/b/c/d/e/f:tag",
                "ghcr.io",
                "a/b/c/d/e/f",
                Some("tag"),
            ),
            ("a.io/b:c", "a.io", "b", Some("c")),
            (
                "ghcr.io/repo:v1.0-beta.1_rc",
                "ghcr.io",
                "repo",
                Some("v1.0-beta.1_rc"),
            ),
            ("ghcr.io/repo:", "ghcr.io", "repo", None),
            (
                "ghcr.io/My-Org/my_repo.v2:tag",
                "ghcr.io",
                "My-Org/my_repo.v2",
                Some("tag"),
            ),
            (long.as_str(), "ghcr.io", "repo", Some(long_tag.as_str())),
        ];
        for (input, registry, repository, tag) in cases {
            let r: Reference = input.parse().unwrap();
            assert_eq!(r.registry(), registry, "{input}");
            assert_eq!(r.repository(), repository, "{input}");
            assert_eq!(r.tag(), tag, "{input}");
            assert!(r.digest().is_none(), "{input}");
        }
    }

    /// A malformed reference is refused, naming what is wrong. Breaks if a
    /// character that would change a request URL, or an overlong tag, is
    /// accepted.
    #[test]
    fn rejects_malformed_references() {
        let long = format!("ghcr.io/repo:{}", "a".repeat(129));
        let cases = [
            ("", "empty string"),
            (
                "ghcr.io/repo?evil:tag",
                "repository name contains invalid character '?'",
            ),
            (
                "ghcr.io/repo#fragment:tag",
                "repository name contains invalid character '#'",
            ),
            (
                "ghcr.io/repo name:tag",
                "repository name contains invalid character ' '",
            ),
            (
                "ghcr.io/repo\x01name:tag",
                "repository name contains invalid character '\x01'",
            ),
            (
                "ghcr.io/repo:v1.0+build",
                "tag contains invalid character '+'",
            ),
            (long.as_str(), "tag exceeds 128 characters"),
        ];
        for (input, needle) in cases {
            let err = input.parse::<Reference>().unwrap_err().to_string();
            assert!(err.contains(needle), "{input:?}: {err}");
        }
    }
}
