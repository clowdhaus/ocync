//! Error types for OCI distribution operations.

use thiserror::Error;

use crate::Digest;

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

    /// A manifest was deserialized successfully but is semantically invalid.
    ///
    /// Currently raised when `schemaVersion` is not 2 (the only version
    /// defined by the OCI Image and Docker v2 specs).
    #[error("invalid manifest: {reason}")]
    InvalidManifest {
        /// Why the manifest is invalid.
        reason: String,
    },

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

    /// A URL could not be constructed or joined.
    #[error("URL construction failed for '{path}': {reason}")]
    UrlConstruction {
        /// The path or URL fragment that failed.
        path: String,
        /// Why construction failed.
        reason: String,
    },

    /// The blob upload protocol violated expectations.
    ///
    /// Raised during the POST/PUT/PATCH upload sequence when the registry
    /// returns a missing `Location` header or an unresolvable upload URL.
    /// For digest mismatches during upload verification, see
    /// [`DigestMismatch`](Self::DigestMismatch).
    #[error("upload protocol error: {reason}")]
    UploadProtocol {
        /// What went wrong during the upload sequence.
        reason: String,
    },

    /// The computed digest does not match the expected digest during blob upload.
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// The digest that was expected.
        expected: Digest,
        /// The digest that was actually computed.
        actual: Digest,
    },

    /// The registry response violated protocol expectations outside of uploads.
    ///
    /// Covers missing required headers on non-upload responses (e.g.
    /// `Docker-Content-Digest` on manifest HEAD), malformed `Link`-header
    /// pagination, and other spec violations in tag listing or manifest
    /// retrieval. For upload-specific protocol errors see
    /// [`UploadProtocol`](Self::UploadProtocol).
    #[error("registry protocol error: {reason}")]
    RegistryProtocol {
        /// What was unexpected in the registry response.
        reason: String,
    },

    /// An ECR-specific operation failed (region detection, SDK API call).
    #[error("ECR error: {reason}")]
    EcrApi {
        /// What went wrong with the ECR operation.
        reason: String,
    },

    /// An HTTP header value could not be constructed.
    #[error("invalid {header} header value: {reason}")]
    InvalidHeaderValue {
        /// Which header could not be constructed.
        header: String,
        /// Why the value is invalid.
        reason: String,
    },

    /// Credential configuration file I/O or parse failure.
    ///
    /// Covers Docker (`~/.docker/config.json`) and any future credential
    /// store formats (podman `auth.json`, etc.).
    #[error("credential config error: {reason}")]
    CredentialConfig {
        /// What went wrong reading or parsing the config.
        reason: String,
    },

    /// Filesystem I/O failed during a local operation.
    #[error("I/O error ({context}): {source}")]
    Io {
        /// What operation was attempted (e.g. "staging create", "staging read").
        context: &'static str,
        /// The underlying I/O error.
        source: std::io::Error,
    },
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

    /// Create a test digest with deterministic hex from a byte value.
    fn test_digest(n: u8) -> Digest {
        let hex = format!("{:0>64}", format!("{n:x}"));
        format!("sha256:{hex}").parse().unwrap()
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    // --- Display (pre-existing variants, guards against accidental message changes) ---

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

    // --- status_code ---

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
        let variants: Vec<Error> = vec![
            Error::InvalidReference {
                input: "bad".into(),
                reason: "reason".into(),
            },
            Error::InvalidDigest {
                digest: "bad".into(),
                reason: "reason".into(),
            },
            Error::InvalidPlatformFilter {
                input: "bad".into(),
                reason: "reason".into(),
            },
            Error::UnsupportedMediaType {
                media_type: "text/plain".into(),
            },
            Error::InvalidManifest {
                reason: "bad schema".into(),
            },
            Error::AuthFailed {
                registry: "example.com".into(),
                reason: "bad creds".into(),
            },
            Error::CredentialHelperFailed {
                helper: "docker-credential-ecr-login".into(),
                reason: "not found".into(),
            },
            Error::UrlConstruction {
                path: "/v2/".into(),
                reason: "no base".into(),
            },
            Error::UploadProtocol {
                reason: "missing header".into(),
            },
            Error::DigestMismatch {
                expected: test_digest(1),
                actual: test_digest(2),
            },
            Error::RegistryProtocol {
                reason: "bad response".into(),
            },
            Error::EcrApi {
                reason: "throttled".into(),
            },
            Error::InvalidHeaderValue {
                header: "Content-Type".into(),
                reason: "bad chars".into(),
            },
            Error::CredentialConfig {
                reason: "not found".into(),
            },
            Error::Io {
                context: "test",
                source: std::io::Error::new(std::io::ErrorKind::Other, "disk full"),
            },
        ];
        for err in &variants {
            assert_eq!(err.status_code(), None, "expected None for {err}, got Some");
        }
    }

    // --- is_not_found ---

    #[test]
    fn is_not_found_true_for_404() {
        let err = Error::RegistryError {
            status: http::StatusCode::NOT_FOUND,
            message: "missing".into(),
        };
        assert!(err.is_not_found());
    }

    #[test]
    fn is_not_found_false_for_other_status() {
        let err = Error::RegistryError {
            status: http::StatusCode::UNAUTHORIZED,
            message: "denied".into(),
        };
        assert!(!err.is_not_found());
    }

    #[test]
    fn is_not_found_false_for_non_registry() {
        let err = Error::RegistryProtocol {
            reason: "unrelated".into(),
        };
        assert!(!err.is_not_found());
    }

    // --- is_auth_error ---

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
    fn is_auth_error_false_for_server_error() {
        let err = Error::RegistryError {
            status: http::StatusCode::INTERNAL_SERVER_ERROR,
            message: "broke".into(),
        };
        assert!(!err.is_auth_error());
    }

    #[test]
    fn is_auth_error_false_for_non_auth_variants() {
        let variants: Vec<Error> = vec![
            Error::RegistryProtocol {
                reason: "bad".into(),
            },
            Error::EcrApi {
                reason: "throttled".into(),
            },
            Error::CredentialConfig {
                reason: "missing".into(),
            },
            Error::DigestMismatch {
                expected: test_digest(1),
                actual: test_digest(2),
            },
        ];
        for err in &variants {
            assert!(!err.is_auth_error(), "expected false for {err}");
        }
    }

    // --- Display (new variants) ---

    #[test]
    fn display_url_construction() {
        let err = Error::UrlConstruction {
            path: "/v2/".into(),
            reason: "relative URL without a base".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/v2/"), "should contain path: {msg}");
        assert!(
            msg.contains("relative URL without a base"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn display_upload_protocol() {
        let err = Error::UploadProtocol {
            reason: "missing Location header".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("missing Location header"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn display_registry_protocol() {
        let err = Error::RegistryProtocol {
            reason: "missing Docker-Content-Digest header".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("missing Docker-Content-Digest header"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn display_ecr_api() {
        let err = Error::EcrApi {
            reason: "BatchCheckLayerAvailability throttled".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("BatchCheckLayerAvailability throttled"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn display_invalid_header_value() {
        let err = Error::InvalidHeaderValue {
            header: "Authorization".into(),
            reason: "invalid byte in header value".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Authorization"),
            "should contain header name: {msg}"
        );
        assert!(
            msg.contains("invalid byte in header value"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn display_invalid_manifest() {
        let err = Error::InvalidManifest {
            reason: "unsupported schemaVersion 1, expected 2".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("schemaVersion"),
            "should contain reason: {msg}"
        );
    }

    #[test]
    fn display_credential_config() {
        let err = Error::CredentialConfig {
            reason: "failed to read config at ~/.docker/config.json: No such file".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("credential config error"),
            "should contain variant prefix: {msg}"
        );
        assert!(
            msg.contains("config.json"),
            "should contain path detail: {msg}"
        );
    }

    #[test]
    fn display_io() {
        let err = Error::Io {
            context: "staging write",
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("staging write"),
            "should contain context: {msg}"
        );
        assert!(msg.contains("access denied"), "should contain cause: {msg}");
    }

    // --- Structured fields ---

    #[test]
    fn digest_mismatch_preserves_typed_digests() {
        let expected = test_digest(1);
        let actual = test_digest(2);
        let err = Error::DigestMismatch {
            expected: expected.clone(),
            actual: actual.clone(),
        };
        match &err {
            Error::DigestMismatch {
                expected: e,
                actual: a,
            } => {
                assert_eq!(e, &expected);
                assert_eq!(a, &actual);
            }
            _ => panic!("wrong variant"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains(&expected.to_string()),
            "missing expected: {msg}"
        );
        assert!(msg.contains(&actual.to_string()), "missing actual: {msg}");
    }

    #[test]
    fn io_preserves_source_error_kind() {
        let err = Error::Io {
            context: "staging read",
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied"),
        };
        match &err {
            Error::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            _ => panic!("wrong variant"),
        }
    }
}
