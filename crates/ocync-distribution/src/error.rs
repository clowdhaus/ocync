//! Error types for OCI distribution operations.

use thiserror::Error;

/// Errors returned by OCI distribution operations.
#[derive(Debug, Error)]
pub enum Error {
    /// The image reference string could not be parsed.
    #[error("invalid image reference '{input}': {reason}")]
    InvalidReference {
        /// The raw reference string that failed to parse.
        input: String,
        /// Why the reference is invalid.
        reason: String,
    },

    /// The digest string could not be parsed.
    #[error("invalid digest '{digest}': {reason}")]
    InvalidDigest {
        /// The raw digest string that failed to parse.
        digest: String,
        /// Why the digest is invalid.
        reason: String,
    },

    /// A platform filter string could not be parsed.
    #[error("invalid platform filter '{input}': {reason}")]
    InvalidPlatformFilter {
        /// The raw filter string that failed to parse.
        input: String,
        /// Why the filter is invalid.
        reason: String,
    },

    /// The manifest media type is not recognized.
    #[error("unsupported manifest media type: {media_type}")]
    UnsupportedMediaType {
        /// The unrecognized media type string.
        media_type: String,
    },

    /// A manifest could not be deserialized from JSON.
    #[error("manifest deserialization failed: {0}")]
    ManifestParse(#[from] serde_json::Error),

    /// Authentication with the registry failed.
    #[error("authentication failed for registry '{registry}': {reason}")]
    AuthFailed {
        /// The registry that rejected authentication.
        registry: String,
        /// Why authentication failed.
        reason: String,
    },

    /// An external credential helper process failed.
    #[error("credential helper '{helper}' failed: {reason}")]
    CredentialHelperFailed {
        /// The credential helper command.
        helper: String,
        /// Why the helper failed.
        reason: String,
    },

    /// An HTTP request to the registry failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The registry returned a non-success HTTP status.
    #[error("registry returned {status}: {message}")]
    RegistryError {
        /// The HTTP status code.
        status: http::StatusCode,
        /// Human-readable context (typically registry/repository and response body).
        message: String,
    },

    /// Catch-all for errors without a dedicated variant.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Extract the HTTP status code from this error, if applicable.
    pub fn status_code(&self) -> Option<http::StatusCode> {
        match self {
            Self::RegistryError { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Whether this error represents a 404 Not Found response.
    pub fn is_not_found(&self) -> bool {
        self.status_code() == Some(http::StatusCode::NOT_FOUND)
    }

    /// Whether this error represents an authentication or authorization failure.
    ///
    /// Returns `true` for [`AuthFailed`](Self::AuthFailed) errors and registry
    /// responses with HTTP 401 Unauthorized or 403 Forbidden status codes.
    pub fn is_auth_error(&self) -> bool {
        match self {
            Self::AuthFailed { .. } => true,
            Self::RegistryError { status, .. } => {
                *status == http::StatusCode::UNAUTHORIZED || *status == http::StatusCode::FORBIDDEN
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_reference() {
        let err = Error::InvalidReference {
            input: "!!!".into(),
            reason: "bad chars".into(),
        };
        assert!(err.to_string().contains("!!!"));
        assert!(err.to_string().contains("bad chars"));
    }

    #[test]
    fn display_invalid_digest() {
        let err = Error::InvalidDigest {
            digest: "bad".into(),
            reason: "missing colon".into(),
        };
        assert!(err.to_string().contains("bad"));
        assert!(err.to_string().contains("missing colon"));
    }

    #[test]
    fn display_unsupported_media_type() {
        let err = Error::UnsupportedMediaType {
            media_type: "text/plain".into(),
        };
        assert!(err.to_string().contains("text/plain"));
    }

    #[test]
    fn display_other() {
        let err = Error::Other("something broke".into());
        assert_eq!(err.to_string(), "something broke");
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn status_code_from_registry_error() {
        let err = Error::RegistryError {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            message: "rate limited".into(),
        };
        assert_eq!(err.status_code(), Some(http::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn status_code_none_for_non_registry_errors() {
        let err = Error::Other("something".into());
        assert_eq!(err.status_code(), None);

        let err = Error::InvalidReference {
            input: "bad".into(),
            reason: "reason".into(),
        };
        assert_eq!(err.status_code(), None);

        let err = Error::AuthFailed {
            registry: "example.com".into(),
            reason: "bad creds".into(),
        };
        assert_eq!(err.status_code(), None);
    }

    #[test]
    fn is_not_found() {
        let err = Error::RegistryError {
            status: http::StatusCode::NOT_FOUND,
            message: "missing".into(),
        };
        assert!(err.is_not_found());

        let err = Error::RegistryError {
            status: http::StatusCode::UNAUTHORIZED,
            message: "denied".into(),
        };
        assert!(!err.is_not_found());

        let err = Error::Other("unrelated".into());
        assert!(!err.is_not_found());
    }

    #[test]
    fn is_auth_error_auth_failed() {
        let err = Error::AuthFailed {
            registry: "example.com".into(),
            reason: "bad creds".into(),
        };
        assert!(err.is_auth_error());
    }

    #[test]
    fn is_auth_error_unauthorized() {
        let err = Error::RegistryError {
            status: http::StatusCode::UNAUTHORIZED,
            message: "denied".into(),
        };
        assert!(err.is_auth_error());
    }

    #[test]
    fn is_auth_error_forbidden() {
        let err = Error::RegistryError {
            status: http::StatusCode::FORBIDDEN,
            message: "no access".into(),
        };
        assert!(err.is_auth_error());
    }

    #[test]
    fn is_auth_error_server_error() {
        let err = Error::RegistryError {
            status: http::StatusCode::INTERNAL_SERVER_ERROR,
            message: "broke".into(),
        };
        assert!(!err.is_auth_error());
    }

    #[test]
    fn is_auth_error_other() {
        let err = Error::Other("something".into());
        assert!(!err.is_auth_error());
    }
}
