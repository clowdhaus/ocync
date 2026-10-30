//! Error types for sync operations.

use std::sync::Arc;

use ocync_distribution::Digest;
use thiserror::Error;

/// Errors returned by sync operations.
#[derive(Debug, Error)]
pub enum Error {
    /// A glob pattern could not be compiled.
    #[error("invalid glob pattern '{pattern}': {reason}")]
    InvalidGlob {
        /// The pattern that failed to compile.
        pattern: String,
        /// Why the pattern is invalid.
        reason: String,
    },

    /// A version range could not be parsed.
    #[error("invalid version range '{range}': {reason}")]
    InvalidVersionRange {
        /// The range string that failed to parse.
        range: String,
        /// Why the range is invalid.
        reason: String,
    },

    /// `latest` was set without a `sort` order.
    #[error("`latest` requires `sort` to be set")]
    LatestWithoutSort,

    /// Fewer tags matched than the configured minimum.
    #[error("only {matched} tag(s) matched, minimum required is {minimum}")]
    BelowMinTags {
        /// How many tags actually matched.
        matched: usize,
        /// The configured minimum.
        minimum: usize,
    },

    /// A manifest pull or push failed during sync.
    #[error("manifest operation failed for {reference}: {source}")]
    Manifest {
        /// The manifest reference (tag or digest) involved.
        reference: String,
        /// The underlying distribution error.
        source: ocync_distribution::Error,
    },

    /// Engine-level manifest processing error (not from the registry), such
    /// as a platform filter matching nothing or an unsupported manifest
    /// structure. A deterministic engine decision: no HTTP status, never
    /// retryable.
    #[error("manifest {reference}: {reason}")]
    ManifestLogic {
        /// The manifest reference involved.
        reference: String,
        /// Why the manifest could not be processed.
        reason: String,
    },

    /// A blob transfer failed during sync.
    #[error("blob transfer failed for {digest}: {source}")]
    BlobTransfer {
        /// The digest of the blob that could not be transferred.
        digest: Digest,
        /// The underlying distribution error.
        source: ocync_distribution::Error,
    },

    /// Artifact sync required but no referrers found.
    #[error("no artifacts found for {reference} (require_artifacts is enabled)")]
    RequiredArtifactsMissing {
        /// The manifest reference that has no referrers.
        reference: String,
    },

    /// Artifact discovery or transfer failed.
    #[error("artifact sync failed for {reference}: {reason}")]
    ArtifactSync {
        /// The parent manifest reference.
        reference: String,
        /// Why the artifact sync failed.
        reason: String,
    },

    /// A failure recorded once and reported by every image that needed the
    /// same work, such as tags sharing one child manifest push.
    #[error(transparent)]
    Shared(Arc<Error>),
}

/// The error code ECR answers a push to an immutable tag that is already
/// set with.
const ECR_IMMUTABLE_TAG_CODE: &str = "ImageTagAlreadyExistsException";

/// OCI distribution-spec error codes that refuse a manifest for something
/// other than a blob the registry lacks.
const CODES_NOT_ABOUT_BLOBS: [&str; 9] = [
    "DENIED",
    "DIGEST_INVALID",
    "MANIFEST_UNKNOWN",
    "NAME_INVALID",
    "NAME_UNKNOWN",
    "SIZE_INVALID",
    "TAG_INVALID",
    "UNAUTHORIZED",
    "UNSUPPORTED",
];

/// Why a registry refused a manifest push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManifestRefusal {
    /// ECR refused it because the tag is immutable and already set.
    ImmutableTag,
    /// Refused as invalid (HTTP 400 or 404). A registry that lacks a
    /// referenced blob answers this way, though not all with the same error
    /// code, so only codes plainly about something else rule it out.
    Invalid,
}

impl Error {
    /// HTTP status code of the underlying distribution error, if any.
    pub fn status_code(&self) -> Option<http::StatusCode> {
        match self {
            Self::Manifest { source, .. } | Self::BlobTransfer { source, .. } => {
                source.status_code()
            }
            Self::Shared(err) => err.status_code(),
            _ => None,
        }
    }

    /// Why the registry refused a manifest push, when the engine acts on
    /// the reason. Throttling, access and server errors are `None`.
    pub(crate) fn manifest_refusal(&self) -> Option<ManifestRefusal> {
        let Self::Manifest {
            source: ocync_distribution::Error::RegistryError { status, message },
            ..
        } = self
        else {
            return None;
        };
        match *status {
            http::StatusCode::BAD_REQUEST if message.contains(ECR_IMMUTABLE_TAG_CODE) => {
                Some(ManifestRefusal::ImmutableTag)
            }
            http::StatusCode::BAD_REQUEST | http::StatusCode::NOT_FOUND => {
                let mut codes = crate::retry::oci_error_codes(message).peekable();
                let about_something_else = codes.peek().is_some()
                    && codes.all(|code| CODES_NOT_ABOUT_BLOBS.contains(&code.as_str()));
                (!about_something_else).then_some(ManifestRefusal::Invalid)
            }
            _ => None,
        }
    }

    /// Whether a blob copy failed, rather than a manifest operation.
    pub(crate) fn is_blob_transfer(&self) -> bool {
        match self {
            Self::BlobTransfer { .. } => true,
            Self::Shared(err) => err.is_blob_transfer(),
            _ => false,
        }
    }

    /// Whether this error represents a required-artifacts-missing condition.
    pub fn is_required_artifacts_missing(&self) -> bool {
        matches!(self, Self::RequiredArtifactsMissing { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_error(status: http::StatusCode, message: &str) -> Error {
        Error::Manifest {
            reference: "v1.0".into(),
            source: ocync_distribution::Error::RegistryError {
                status,
                message: message.into(),
            },
        }
    }

    #[test]
    fn classifies_manifest_refusals() {
        use http::StatusCode;
        let immutable = format!("{ECR_IMMUTABLE_TAG_CODE}: tag already exists");
        let cases = [
            (
                StatusCode::BAD_REQUEST,
                "MANIFEST_BLOB_UNKNOWN",
                Some(ManifestRefusal::Invalid),
            ),
            (
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                Some(ManifestRefusal::Invalid),
            ),
            (
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                Some(ManifestRefusal::Invalid),
            ),
            (
                StatusCode::BAD_REQUEST,
                &immutable,
                Some(ManifestRefusal::ImmutableTag),
            ),
            (
                StatusCode::NOT_FOUND,
                r#"{"errors":[{"code":"BLOB_UNKNOWN"},{"code":"NAME_UNKNOWN"}]}"#,
                Some(ManifestRefusal::Invalid),
            ),
            (
                StatusCode::NOT_FOUND,
                r#"{"errors":[{"code":"NAME_UNKNOWN","message":"repository name not known"}]}"#,
                None,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"errors":[{"code":"TAG_INVALID"},{"code":"UNSUPPORTED"}]}"#,
                None,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"errors":[{"code":"DENIED"}]}"#,
                None,
            ),
            (StatusCode::FORBIDDEN, "DENIED", None),
            (StatusCode::TOO_MANY_REQUESTS, "TOOMANYREQUESTS", None),
            (StatusCode::INTERNAL_SERVER_ERROR, &immutable, None),
        ];
        for (status, message, refusal) in cases {
            assert_eq!(
                manifest_error(status, message).manifest_refusal(),
                refusal,
                "{status} {message}"
            );
        }
    }

    #[test]
    fn blob_error_is_not_a_manifest_refusal() {
        let err = Error::BlobTransfer {
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::BAD_REQUEST,
                message: ECR_IMMUTABLE_TAG_CODE.into(),
            },
        };
        assert_eq!(err.manifest_refusal(), None);
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn display_names_every_field() {
        let registry_404 = ocync_distribution::Error::RegistryError {
            status: http::StatusCode::NOT_FOUND,
            message: "not found".into(),
        };
        let permission_denied = ocync_distribution::Error::Io {
            context: "staging read",
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied")
                .into(),
        };
        // (name, error, fragments the message must contain)
        let cases: [(&str, Error, &[&str]); 9] = [
            (
                "invalid glob",
                Error::InvalidGlob {
                    pattern: "[bad".into(),
                    reason: "unclosed bracket".into(),
                },
                &["[bad", "unclosed bracket"],
            ),
            (
                "invalid version range",
                Error::InvalidVersionRange {
                    range: ">= oops".into(),
                    reason: "unexpected character".into(),
                },
                &[">= oops", "unexpected character"],
            ),
            (
                "latest without sort",
                Error::LatestWithoutSort,
                &["latest", "sort"],
            ),
            (
                "below min tags",
                Error::BelowMinTags {
                    matched: 2,
                    minimum: 5,
                },
                &["2", "5"],
            ),
            (
                "manifest",
                Error::Manifest {
                    reference: "latest".into(),
                    source: registry_404,
                },
                &["latest", "manifest"],
            ),
            (
                "blob transfer",
                Error::BlobTransfer {
                    digest:
                        "sha256:def0000000000000000000000000000000000000000000000000000000000000"
                            .parse()
                            .unwrap(),
                    source: permission_denied,
                },
                &[
                    "blob transfer",
                    "sha256:def0000000000000000000000000000000000000000000000000000000000000",
                ],
            ),
            (
                "manifest logic",
                Error::ManifestLogic {
                    reference: "latest".into(),
                    reason: "platform filter matched no manifests in index".into(),
                },
                &["latest", "platform filter"],
            ),
            (
                "required artifacts missing",
                Error::RequiredArtifactsMissing {
                    reference: "sha256:abc123".into(),
                },
                &["sha256:abc123", "require_artifacts"],
            ),
            (
                "artifact sync",
                Error::ArtifactSync {
                    reference: "sha256:def456".into(),
                    reason: "manifest pull failed".into(),
                },
                &["sha256:def456", "manifest pull failed"],
            ),
        ];
        for (name, err, fragments) in cases {
            let msg = err.to_string();
            for fragment in fragments {
                assert!(
                    msg.contains(fragment),
                    "{name}: {fragment:?} not in {msg:?}"
                );
            }
        }
    }

    #[test]
    fn status_code_by_variant() {
        let cases = [
            (
                "manifest error carries the registry status",
                Error::Manifest {
                    reference: "v1".into(),
                    source: ocync_distribution::Error::RegistryError {
                        status: http::StatusCode::TOO_MANY_REQUESTS,
                        message: "rate limited".into(),
                    },
                },
                Some(http::StatusCode::TOO_MANY_REQUESTS),
            ),
            (
                "manifest logic is an engine decision, not a registry answer",
                Error::ManifestLogic {
                    reference: "latest".into(),
                    reason: "no match".into(),
                },
                None,
            ),
            ("filter error", Error::LatestWithoutSort, None),
            (
                "required artifacts missing",
                Error::RequiredArtifactsMissing {
                    reference: "sha256:abc".into(),
                },
                None,
            ),
            (
                "artifact sync",
                Error::ArtifactSync {
                    reference: "sha256:def".into(),
                    reason: "failed".into(),
                },
                None,
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(err.status_code(), want, "{name}");
        }
    }

    #[test]
    fn shared_error_keeps_message_and_status_code() {
        let err = Error::Shared(Arc::new(Error::Manifest {
            reference: "sha256:abc".into(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::BAD_REQUEST,
                message: "denied".into(),
            },
        }));
        assert_eq!(err.status_code(), Some(http::StatusCode::BAD_REQUEST));
        assert_eq!(
            err.to_string(),
            "manifest operation failed for sha256:abc: registry returned 400 Bad Request: denied"
        );
    }

    #[test]
    fn is_required_artifacts_missing_only_for_that_variant() {
        let cases = [
            (
                "required artifacts missing",
                Error::RequiredArtifactsMissing {
                    reference: "sha256:abc".into(),
                },
                true,
            ),
            ("non-artifact error", Error::LatestWithoutSort, false),
            (
                "manifest logic",
                Error::ManifestLogic {
                    reference: "v1".into(),
                    reason: "nested index manifests are not supported".into(),
                },
                false,
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(err.is_required_artifacts_missing(), want, "{name}");
        }
    }
}
