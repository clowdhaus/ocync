//! Error types for sync operations.

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

    /// A semver version range could not be parsed.
    #[error("invalid semver range '{range}': {reason}")]
    InvalidSemverRange {
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

    /// A blob transfer failed during sync.
    #[error("blob transfer failed for {digest}: {source}")]
    BlobTransfer {
        /// The digest of the blob that could not be transferred.
        digest: String,
        /// The underlying distribution error.
        source: ocync_distribution::Error,
    },
}

impl Error {
    /// Extract the HTTP status code from the underlying distribution error, if any.
    pub fn status_code(&self) -> Option<http::StatusCode> {
        match self {
            Self::Manifest { source, .. } | Self::BlobTransfer { source, .. } => {
                source.status_code()
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn display_invalid_semver_range() {
        let err = Error::InvalidSemverRange {
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
    fn display_blob_push_error() {
        let err = Error::BlobTransfer {
            digest: "sha256:def".into(),
            source: ocync_distribution::Error::Other("timeout".into()),
        };
        let msg = err.to_string();
        assert!(msg.contains("blob transfer"));
        assert!(msg.contains("sha256:def"));
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
    fn status_code_none_for_filter_errors() {
        let err = Error::LatestWithoutSort;
        assert_eq!(err.status_code(), None);
    }
}
