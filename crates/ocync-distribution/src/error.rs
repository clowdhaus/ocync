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

    /// A fetched digest did not match the expected value.
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// The digest that was expected.
        expected: String,
        /// The digest that was computed.
        actual: String,
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

    /// An auth provider requires a feature that was not compiled in.
    #[error("auth provider '{provider}' not compiled; rebuild with --features {feature}")]
    ProviderNotCompiled {
        /// The provider name (e.g. `"ecr"`).
        provider: &'static str,
        /// The Cargo feature required.
        feature: &'static str,
    },

    /// No credentials were found for the target registry.
    #[error("no credentials found for registry '{registry}'")]
    NoCredentials {
        /// The registry hostname.
        registry: String,
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

    /// The registry returned an unexpected HTTP status.
    #[error("registry returned {status}: {message}")]
    RegistryError {
        /// The HTTP status code.
        status: http::StatusCode,
        /// The response body.
        message: String,
    },

    /// The registry returned 401 Unauthorized.
    #[error("registry returned 401 Unauthorized for {registry}")]
    Unauthorized {
        /// The registry hostname.
        registry: String,
    },

    /// The registry returned 403 Forbidden.
    #[error("registry returned 403 Forbidden for {registry}/{repository}")]
    Forbidden {
        /// The registry hostname.
        registry: String,
        /// The repository that was denied.
        repository: String,
    },

    /// The requested resource was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Catch-all for errors without a dedicated variant.
    #[error("{0}")]
    Other(String),
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
    fn display_digest_mismatch() {
        let err = Error::DigestMismatch {
            expected: "sha256:aaa".into(),
            actual: "sha256:bbb".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("sha256:aaa"));
        assert!(msg.contains("sha256:bbb"));
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
}
