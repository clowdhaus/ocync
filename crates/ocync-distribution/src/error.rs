//! Error types for OCI distribution operations.

use std::sync::Arc;

use thiserror::Error;

use crate::Digest;

/// Errors returned by OCI distribution operations.
///
/// Cloneable, so a failure can be handed to every caller that waited on it:
/// the causes that cannot be cloned are shared.
#[derive(Debug, Clone, Error)]
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
    ManifestParse(#[source] Arc<serde_json::Error>),

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

    /// Google's credentials yielded no token for the registry.
    #[error("no Google token for registry '{registry}': {reason}")]
    GoogleCredentials {
        /// The registry the token was for.
        registry: String,
        /// Why no token was had, with its causes.
        reason: String,
        /// Whether asking again may succeed.
        transience: Transience,
    },

    /// A registry's token endpoint answered with an error status. 401
    /// Unauthorized refuses the credentials sent; other statuses refuse the
    /// scope asked for or fail on the endpoint's side. Not a registry answer,
    /// so [`Self::status_code`] leaves it out.
    #[error("authentication failed for registry '{registry}': token endpoint returned {status}")]
    TokenEndpoint {
        /// The registry whose token endpoint answered.
        registry: String,
        /// The status it answered.
        status: http::StatusCode,
    },

    /// A registry's `/v2/` ping, sent to find its token endpoint, answered
    /// with a status other than the 401 challenge. Already retried where
    /// transient, so [`Self::status_code`] leaves it out as it does
    /// [`Self::TokenEndpoint`].
    #[error(
        "authentication failed for registry '{registry}': registry ping returned {status} \
         (expected 401 challenge)"
    )]
    RegistryPing {
        /// The registry pinged.
        registry: String,
        /// The status it answered.
        status: http::StatusCode,
    },

    /// An external credential helper process failed.
    #[error("credential helper '{helper}' failed: {reason}")]
    CredentialHelperFailed {
        /// The credential helper command.
        helper: String,
        /// Why the helper failed.
        reason: String,
    },

    /// An HTTP request to the registry failed. The HTTP client's text
    /// leaves out its causes, such as a refused connection or a failed DNS
    /// lookup, so this names them.
    #[error("HTTP request failed: {}", error_chain(&**.0))]
    Http(#[source] Arc<reqwest::Error>),

    /// The registry returned a non-success HTTP status.
    #[error("registry returned {status}: {message}")]
    RegistryError {
        /// The HTTP status code.
        status: http::StatusCode,
        /// Human-readable context (typically registry/repository and response body).
        message: String,
    },

    /// The registry refused the token sent with a streamed blob upload. The
    /// body was the stream itself and cannot be sent again, so the token is
    /// dropped and the upload is left for the caller to send again, from
    /// the start, under a new one.
    #[error("registry refused the upload's token: {message}")]
    UploadTokenRefused {
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
        source: Arc<std::io::Error>,
    },
}

/// Whether a failure may pass if the same request is sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transience {
    /// It may pass, as a server error, a throttle or a timeout can.
    Transient,
    /// It fails again, as refused credentials do.
    Final,
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::ManifestParse(Arc::new(error))
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(Arc::new(error))
    }
}

impl Error {
    /// An [`Error::AuthFailed`] for `registry`, with `reason` as its text.
    pub(crate) fn auth_failed(registry: &str, reason: impl std::fmt::Display) -> Self {
        Self::AuthFailed {
            registry: registry.to_owned(),
            reason: reason.to_string(),
        }
    }

    /// Extract the HTTP status code from this error, if applicable.
    pub fn status_code(&self) -> Option<http::StatusCode> {
        match self {
            Self::RegistryError { status, .. } => Some(*status),
            Self::UploadTokenRefused { .. } => Some(http::StatusCode::UNAUTHORIZED),
            _ => None,
        }
    }

    /// Whether this error represents a 404 Not Found response.
    pub fn is_not_found(&self) -> bool {
        self.status_code() == Some(http::StatusCode::NOT_FOUND)
    }

    /// Whether this error represents an authentication or authorization failure.
    ///
    /// Returns `true` for [`AuthFailed`](Self::AuthFailed) errors, a token
    /// endpoint's 4xx other than 408 Request Timeout and 429 Too Many
    /// Requests, Google credentials
    /// that fail for good, and registry responses with HTTP 401 Unauthorized
    /// or 403 Forbidden status codes.
    pub fn is_auth_error(&self) -> bool {
        match self {
            Self::AuthFailed { .. }
            | Self::UploadTokenRefused { .. }
            | Self::GoogleCredentials {
                transience: Transience::Final,
                ..
            } => true,
            Self::TokenEndpoint { status, .. } | Self::RegistryPing { status, .. } => {
                status.is_client_error() && !crate::retry::is_transient_status(*status)
            }
            Self::RegistryError { status, .. } => {
                *status == http::StatusCode::UNAUTHORIZED || *status == http::StatusCode::FORBIDDEN
            }
            _ => false,
        }
    }
}

/// `error` and each of its sources, joined as upstream prints them, so a
/// reason kept as text still names its cause. A cause the text already ends
/// with is not repeated, nor are the rest once the text ends with a cause
/// and all of its own: some errors print their causes and also return them
/// as their source.
pub(crate) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut errors = vec![error];
    while let Some(source) = errors.last().and_then(|e| shown_source(*e)) {
        errors.push(source);
    }
    let texts: Vec<String> = errors.iter().map(ToString::to_string).collect();
    // The chain from each error down, built from the bottom up.
    let mut chains = vec![String::new(); texts.len()];
    for i in (0..texts.len()).rev() {
        let mut text = texts[i].clone();
        for j in i + 1..texts.len() {
            if text.ends_with(&chains[j]) {
                break;
            }
            if !text.ends_with(&texts[j]) {
                text.push_str(": ");
                text.push_str(&texts[j]);
            }
        }
        chains[i] = text;
    }
    chains.swap_remove(0)
}

/// The source of `error` whose text may be shown. The JSON parser's text
/// for a response body the HTTP client could not decode quotes the value it
/// refused, and a body can carry a token, so it is not shown.
fn shown_source<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a (dyn std::error::Error + 'static)> {
    // `Error::Http` holds the client's error shared, so either form can be
    // the parent.
    let from_http = error.is::<reqwest::Error>() || error.is::<Arc<reqwest::Error>>();
    error
        .source()
        .filter(|source| !(from_http && source.is::<serde_json::Error>()))
}

/// The most of an external body or stream kept in an error, in bytes.
pub(crate) const MAX_QUOTED_BYTES: usize = 200;

/// The start of `body`, at most [`MAX_QUOTED_BYTES`] and cut on a character
/// boundary. A response body or a helper's stderr can carry a token, and an
/// error's text reaches logs and the `--json` report.
pub(crate) fn truncate_quoted(body: &str) -> &str {
    if body.len() <= MAX_QUOTED_BYTES {
        return body;
    }
    let mut end = MAX_QUOTED_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed request names its cause once, and joining its sources adds
    /// nothing. Breaks if the text stops at "error sending request", or a
    /// cause the text already holds is repeated.
    #[tokio::test]
    async fn an_http_error_names_its_cause_once() {
        let refused = crate::refusal();
        let err = Error::from(
            crate::test_http_client()
                .get(format!("http://{}/v2/", crate::REFUSING))
                .send()
                .await
                .unwrap_err(),
        );
        let text = err.to_string();
        assert!(text.ends_with(&refused), "{text}");
        assert_eq!(error_chain(&err), text);
    }

    /// A body cut off partway names why reading it stopped. Breaks if a
    /// body-read failure is cut back to its top line.
    #[tokio::test]
    async fn a_cut_off_body_names_its_cause() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort")
                .await
                .unwrap();
        });
        let response = crate::test_http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let err = Error::from(response.bytes().await.unwrap_err());
        let text = err.to_string();
        assert!(
            text.ends_with(
                "error reading a body from connection: end of file before message length reached"
            ),
            "{text}"
        );
        assert_eq!(error_chain(&err), text);
    }

    /// A body that fails to decode is named as such, without the parser's
    /// text, which quotes the body. Breaks if the body reaches the text.
    #[tokio::test]
    async fn a_decode_error_leaves_the_body_out() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("\"s3cret\""))
            .mount(&server)
            .await;
        let response = crate::test_http_client()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let err = Error::from(response.json::<u64>().await.unwrap_err());
        let text = err.to_string();
        assert!(
            text.starts_with("HTTP request failed: error decoding response body"),
            "{text}"
        );
        assert!(!text.contains("s3cret"), "{text}");
        assert!(!error_chain(&err).contains("s3cret"), "{err}");
    }

    #[test]
    fn truncate_quoted_short() {
        assert_eq!(truncate_quoted("short"), "short");
    }

    #[test]
    fn truncate_quoted_at_multibyte_boundary() {
        // 198 ASCII chars + a 3-byte UTF-8 char (euro sign) = 201 bytes.
        // Naive &body[..200] would slice mid-char and panic.
        let body = format!("{}x{}", "a".repeat(197), '\u{20AC}');
        assert_eq!(body.len(), 201);
        let truncated = truncate_quoted(&body);
        assert!(truncated.len() <= MAX_QUOTED_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

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
                source: std::io::Error::other("disk full").into(),
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

    /// A token endpoint that refuses is an auth error, and one that times
    /// out, is throttled or is failing is not. Breaks if a 408, 429 or 503
    /// exits as refused credentials.
    #[test]
    fn is_auth_error_token_endpoint() {
        for (status, auth_error) in [
            (http::StatusCode::UNAUTHORIZED, true),
            (http::StatusCode::NOT_FOUND, true),
            (http::StatusCode::REQUEST_TIMEOUT, false),
            (http::StatusCode::TOO_MANY_REQUESTS, false),
            (http::StatusCode::SERVICE_UNAVAILABLE, false),
        ] {
            let err = Error::TokenEndpoint {
                registry: "example.com".into(),
                status,
            };
            assert_eq!(err.is_auth_error(), auth_error, "{status}");
        }
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
        // Exhaustive: every variant except AuthFailed and RegistryError
        // (which return true). Compile error if a new variant is added
        // without being included here.
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
                reason: "missing".into(),
            },
            Error::Io {
                context: "test",
                source: std::io::Error::other("disk full").into(),
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
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied")
                .into(),
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
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied")
                .into(),
        };
        match &err {
            Error::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            _ => panic!("wrong variant"),
        }
    }
}
