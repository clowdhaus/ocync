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

    /// Engine-level manifest processing error (not from the registry).
    ///
    /// Covers failures like platform filter matching, unsupported manifest
    /// structures, and index rewriting -- cases where the engine itself
    /// rejects or cannot process a manifest, rather than a registry error.
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

impl Error {
    /// Extract the HTTP status code from the underlying distribution error, if any.
    pub fn status_code(&self) -> Option<http::StatusCode> {
        match self {
            Self::Manifest { source, .. } | Self::BlobTransfer { source, .. } => {
                source.status_code()
            }
            Self::Shared(err) => err.status_code(),
            _ => None,
        }
    }

    /// Whether the registry refused a manifest for what it references:
    /// HTTP 400 or 404, which a registry answers when it lacks a referenced
    /// blob, whatever error code it names. Throttling, access and server
    /// errors are not, nor is ECR's immutable-tag refusal.
    pub(crate) fn is_reference_refusal(&self) -> bool {
        matches!(
            self,
            Self::Manifest {
                source: ocync_distribution::Error::RegistryError { status, .. },
                ..
            } if matches!(*status, http::StatusCode::BAD_REQUEST | http::StatusCode::NOT_FOUND)
        ) && !self.is_immutable_tag()
    }

    /// Whether ECR refused a manifest because its tag is immutable and
    /// already set.
    pub(crate) fn is_immutable_tag(&self) -> bool {
        matches!(
            self,
            Self::Manifest {
                source: ocync_distribution::Error::RegistryError { status, message },
                ..
            } if *status == http::StatusCode::BAD_REQUEST
                && message.contains("ImageTagAlreadyExistsException")
        )
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
        const IMMUTABLE: &str = "ImageTagAlreadyExistsException: tag already exists";
        // (status, message, reference refusal, immutable tag)
        let cases = [
            (
                StatusCode::BAD_REQUEST,
                "MANIFEST_BLOB_UNKNOWN",
                true,
                false,
            ),
            (StatusCode::BAD_REQUEST, "MANIFEST_INVALID", true, false),
            (StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", true, false),
            (StatusCode::BAD_REQUEST, IMMUTABLE, false, true),
            (StatusCode::FORBIDDEN, "DENIED", false, false),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "TOOMANYREQUESTS",
                false,
                false,
            ),
            (StatusCode::INTERNAL_SERVER_ERROR, IMMUTABLE, false, false),
        ];
        for (status, message, refusal, immutable) in cases {
            let err = manifest_error(status, message);
            assert_eq!(err.is_reference_refusal(), refusal, "{status} {message}");
            assert_eq!(err.is_immutable_tag(), immutable, "{status} {message}");
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
                message: "ImageTagAlreadyExistsException".into(),
            },
        };
        assert!(!err.is_reference_refusal());
        assert!(!err.is_immutable_tag());
    }

    #[test]
    fn display_invalid_glob() {
        let err = Error::InvalidGlob {
            pattern: "[bad".into(),
            reason: "unclosed bracket".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("[bad"));
        assert!(msg.contains("unclosed bracket"));
    }

    #[test]
    fn display_invalid_version_range() {
        let err = Error::InvalidVersionRange {
            range: ">= oops".into(),
            reason: "unexpected character".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains(">= oops"));
        assert!(msg.contains("unexpected character"));
    }

    #[test]
    fn display_latest_without_sort() {
        let err = Error::LatestWithoutSort;
        assert!(err.to_string().contains("latest"));
        assert!(err.to_string().contains("sort"));
    }

    #[test]
    fn display_below_min_tags() {
        let err = Error::BelowMinTags {
            matched: 2,
            minimum: 5,
        };
        let msg = err.to_string();
        assert!(msg.contains('2'));
        assert!(msg.contains('5'));
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn display_manifest_error() {
        let err = Error::Manifest {
            reference: "latest".into(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::NOT_FOUND,
                message: "not found".into(),
            },
        };
        let msg = err.to_string();
        assert!(msg.contains("latest"));
        assert!(msg.contains("manifest"));
    }

    #[test]
    fn display_blob_transfer_error() {
        let digest: Digest =
            "sha256:def0000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();
        let err = Error::BlobTransfer {
            digest: digest.clone(),
            source: ocync_distribution::Error::Io {
                context: "staging read",
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ),
            },
        };
        let msg = err.to_string();
        assert!(msg.contains("blob transfer"));
        assert!(msg.contains(&digest.to_string()));
    }

    #[test]
    fn display_manifest_logic() {
        let err = Error::ManifestLogic {
            reference: "latest".into(),
            reason: "platform filter matched no manifests in index".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("latest"), "should contain reference: {msg}");
        assert!(
            msg.contains("platform filter"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn manifest_logic_status_code_is_none() {
        let err = Error::ManifestLogic {
            reference: "latest".into(),
            reason: "no match".into(),
        };
        // ManifestLogic is a deterministic engine decision (not a registry
        // error), so it has no HTTP status and is never retryable.
        assert_eq!(err.status_code(), None);
    }

    #[test]
    fn manifest_logic_is_not_required_artifacts_missing() {
        let err = Error::ManifestLogic {
            reference: "v1".into(),
            reason: "nested index manifests are not supported".into(),
        };
        assert!(!err.is_required_artifacts_missing());
    }

    #[test]
    fn status_code_from_manifest_error() {
        let err = Error::Manifest {
            reference: "v1".into(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::TOO_MANY_REQUESTS,
                message: "rate limited".into(),
            },
        };
        assert_eq!(err.status_code(), Some(http::StatusCode::TOO_MANY_REQUESTS));
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
    fn status_code_none_for_filter_errors() {
        let err = Error::LatestWithoutSort;
        assert_eq!(err.status_code(), None);
    }

    #[test]
    fn display_required_artifacts_missing() {
        let err = Error::RequiredArtifactsMissing {
            reference: "sha256:abc123".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("sha256:abc123"));
        assert!(msg.contains("require_artifacts"));
    }

    #[test]
    fn display_artifact_sync_error() {
        let err = Error::ArtifactSync {
            reference: "sha256:def456".into(),
            reason: "manifest pull failed".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("sha256:def456"));
        assert!(msg.contains("manifest pull failed"));
    }

    #[test]
    fn is_required_artifacts_missing() {
        let err = Error::RequiredArtifactsMissing {
            reference: "sha256:abc".into(),
        };
        assert!(err.is_required_artifacts_missing());

        let err = Error::LatestWithoutSort;
        assert!(
            !err.is_required_artifacts_missing(),
            "non-artifact error should not match"
        );
    }

    #[test]
    fn status_code_none_for_artifact_errors() {
        let err = Error::RequiredArtifactsMissing {
            reference: "sha256:abc".into(),
        };
        assert_eq!(err.status_code(), None);

        let err = Error::ArtifactSync {
            reference: "sha256:def".into(),
            reason: "failed".into(),
        };
        assert_eq!(err.status_code(), None);
    }
}
