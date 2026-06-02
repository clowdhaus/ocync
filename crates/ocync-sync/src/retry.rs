//! Retry configuration and backoff logic for transient failures.

use std::time::Duration;

use http::StatusCode;

/// Retry configuration with exponential backoff.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_retries: u32,
    /// Initial backoff delay before the first retry.
    pub initial_backoff: Duration,
    /// Upper bound on backoff delay.
    pub max_backoff: Duration,
    /// Multiplier applied to backoff on each successive attempt.
    pub backoff_multiplier: u32,
    /// Maximum time the engine waits for the target's blob-availability
    /// view to converge before issuing a manifest `PUT`, when a
    /// [`BatchBlobChecker`](ocync_distribution::ecr::BatchBlobChecker)
    /// is configured (production: ECR targets).
    ///
    /// Production default: 30s -- larger than typical ECR consistency
    /// windows but bounded so a stuck view does not hold the engine
    /// open. Set to `Duration::ZERO` to disable the wait entirely; the
    /// standard retry path will still catch any `BLOB_UPLOAD_UNKNOWN`.
    pub manifest_commit_wait: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            backoff_multiplier: 2,
            manifest_commit_wait: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    /// Compute the backoff duration for the given attempt (0-indexed).
    ///
    /// Uses exponential backoff capped at `max_backoff`, then applies
    /// multiplicative jitter in \[0.75, 1.25) to decorrelate concurrent
    /// retries. All arithmetic saturates instead of overflowing.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let multiplier = self.backoff_multiplier.saturating_pow(attempt);
        let backoff = self.initial_backoff.saturating_mul(multiplier);
        let capped = backoff.min(self.max_backoff);
        jitter(capped)
    }
}

/// Determine whether a request should be retried based on HTTP status code.
///
/// Retries on 408 (Request Timeout), 429 (Too Many Requests), and all 5xx
/// server errors. This matches the behavior of crane and regsync.
pub fn should_retry(status: StatusCode, current_attempt: u32, max_retries: u32) -> bool {
    if current_attempt >= max_retries {
        return false;
    }
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// ECR (and possibly other registries) can return 404 with the OCI error
/// code `BLOB_UPLOAD_UNKNOWN` when a manifest push references blobs
/// whose PUT-201 came back but haven't been promoted to the
/// manifest-validation index yet. The OCI distribution spec describes
/// `BLOB_UPLOAD_UNKNOWN` as a state that "may be returned" for upload
/// sessions in flux, leaving room for transient interpretation.
///
/// Returns `true` only when the error is a `RegistryError` with status
/// 404 whose body is structured-OCI-error JSON and contains an
/// `errors[].code == "BLOB_UPLOAD_UNKNOWN"` entry. A free-text mention
/// of the string in some other shape (or a body that fails to parse) is
/// NOT classified as retryable -- substring matching would otherwise
/// false-positive on bodies that reference the code in prose.
///
/// # Interaction with the manifest-commit blob-visibility wait
///
/// For ECR targets, [`crate::engine::push_manifests`] now gates the
/// manifest `PUT` on
/// [`BatchBlobChecker::wait_for_blobs_available`](ocync_distribution::ecr::BatchBlobChecker::wait_for_blobs_available),
/// which polls the authoritative `BatchCheckLayerAvailability` API
/// until every referenced layer is visible (deadline:
/// [`RetryConfig::manifest_commit_wait`]). This eliminates the
/// `BLOB_UPLOAD_UNKNOWN` race for the common case. Retrying on this
/// error remains as a defence-in-depth fallback for the wait-deadline
/// case (a stuck consistency view that exceeds
/// `manifest_commit_wait`) and for non-ECR targets that don't have a
/// batch checker configured.
pub fn is_blob_upload_unknown(error: &ocync_distribution::Error) -> bool {
    let ocync_distribution::Error::RegistryError { status, message } = error else {
        return false;
    };
    if *status != StatusCode::NOT_FOUND {
        return false;
    }
    let Ok(body) = serde_json::from_str::<OciErrorBody>(message) else {
        return false;
    };
    body.errors.iter().any(|e| e.code == "BLOB_UPLOAD_UNKNOWN")
}

/// OCI distribution-spec error response body shape.
#[derive(serde::Deserialize)]
struct OciErrorBody {
    errors: Vec<OciError>,
}

#[derive(serde::Deserialize)]
struct OciError {
    code: String,
}

/// Determine whether a transport-level (non-HTTP) error should be retried.
///
/// Returns `true` for connection failures, request timeouts, mid-stream
/// send failures (connection reset during body upload), and response body
/// errors (premature EOF, truncated body, decode failures from corrupted
/// transport) surfaced by `reqwest`. These are transient by nature and
/// safe to retry idempotent OCI operations on. Deterministic decode
/// failures (e.g. malformed registry responses) are bounded by
/// `max_retries`.
///
/// # Predicate set
///
/// `reqwest::Error::is_request()` covers errors raised during request
/// dispatch (including connect failures and timeouts on the async hyper
/// path - both are wrapped as `Kind::Request` by reqwest). `is_body()`
/// and `is_decode()` cover the response-body and decoder phases, which
/// are NOT classified as `Kind::Request`. Together these three predicates
/// cover the transient-network surface without overlap.
///
/// The `should_retry_transport_*` tests below verify that connection
/// failures and timeouts both classify as retryable through `is_request()`
/// alone, pinning the equivalence.
///
/// # Known limitation
///
/// Only inspects `ocync_distribution::Error::Http(reqwest::Error)`. Transport
/// errors that arrive wrapped in other variants are NOT retried:
/// - `Error::Io { .. }` -- local filesystem errors, never transient (typed `io::Error`)
/// - `Error::RegistryError { .. }` -- HTTP-level; retried via `should_retry()` instead
/// - `Error::RegistryProtocol { .. }` -- deterministic spec violation, never transient
///
/// If operators observe unretrieved transient errors, the debug log below will
/// show which variant was encountered so the match can be extended.
pub fn should_retry_transport(error: &ocync_distribution::Error) -> bool {
    if let ocync_distribution::Error::Http(reqwest_err) = error {
        reqwest_err.is_request() || reqwest_err.is_body() || reqwest_err.is_decode()
    } else {
        tracing::debug!(
            error = %error,
            "non-Http error variant not inspectable for transport retry"
        );
        false
    }
}

/// Apply multiplicative jitter to a backoff duration.
///
/// Scales the base duration by a random factor in \[0.75, 1.25) to
/// decorrelate concurrent retries. Uses `fastrand`'s thread-local PRNG
/// (auto-seeded from OS entropy on first use) so each call is a single
/// `f64` draw with no per-call syscall.
fn jitter(base: Duration) -> Duration {
    let factor = 0.75 + fastrand::f64() * 0.5;
    base.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.initial_backoff, Duration::from_secs(1));
        assert_eq!(cfg.max_backoff, Duration::from_secs(300));
        assert_eq!(cfg.backoff_multiplier, 2);
        assert_eq!(cfg.manifest_commit_wait, Duration::from_secs(30));
    }

    /// Helper: assert a duration falls within the jitter range [base*0.75, base*1.25].
    fn assert_in_jitter_range(actual: Duration, base: Duration) {
        let lo = base.mul_f64(0.75);
        let hi = base.mul_f64(1.25);
        assert!(
            actual >= lo && actual <= hi,
            "expected {actual:?} in [{lo:?}, {hi:?}] (base={base:?})"
        );
    }

    #[test]
    fn backoff_exponential() {
        let cfg = RetryConfig::default();
        assert_in_jitter_range(cfg.backoff_for(0), Duration::from_secs(1));
        assert_in_jitter_range(cfg.backoff_for(1), Duration::from_secs(2));
        assert_in_jitter_range(cfg.backoff_for(2), Duration::from_secs(4));
        assert_in_jitter_range(cfg.backoff_for(3), Duration::from_secs(8));
    }

    #[test]
    fn backoff_caps_at_max() {
        let cfg = RetryConfig {
            max_backoff: Duration::from_secs(5),
            ..RetryConfig::default()
        };
        // 2^10 = 1024 seconds, but capped at 5, then jitter applied
        assert_in_jitter_range(cfg.backoff_for(10), Duration::from_secs(5));
    }

    #[test]
    fn backoff_with_multiplier_one_is_constant() {
        let cfg = RetryConfig {
            backoff_multiplier: 1,
            ..RetryConfig::default()
        };
        assert_in_jitter_range(cfg.backoff_for(0), Duration::from_secs(1));
        assert_in_jitter_range(cfg.backoff_for(5), Duration::from_secs(1));
        assert_in_jitter_range(cfg.backoff_for(100), Duration::from_secs(1));
    }

    #[test]
    fn backoff_saturates_on_overflow() {
        let cfg = RetryConfig {
            backoff_multiplier: 3,
            max_backoff: Duration::from_secs(300),
            ..RetryConfig::default()
        };
        // 3^30 overflows u32 - saturating_pow caps at u32::MAX,
        // saturating_mul caps Duration, then min caps at max_backoff
        let result = cfg.backoff_for(30);
        assert_in_jitter_range(result, Duration::from_secs(300));
    }

    #[test]
    fn backoff_with_multiplier_zero_gives_zero_after_first() {
        let cfg = RetryConfig {
            backoff_multiplier: 0,
            ..RetryConfig::default()
        };
        // 0^0 = 1, so attempt 0 gives initial_backoff * 1
        assert_in_jitter_range(cfg.backoff_for(0), Duration::from_secs(1));
        // 0^n = 0 for n > 0, so all subsequent attempts give zero backoff
        // jitter of zero is still zero
        assert_eq!(cfg.backoff_for(1), Duration::ZERO);
        assert_eq!(cfg.backoff_for(5), Duration::ZERO);
    }

    #[test]
    fn jitter_stays_in_range() {
        // Run jitter many times and verify bounds. RandomState seeds
        // differ per call, so we get coverage of the range.
        let base = Duration::from_secs(10);
        for _ in 0..100 {
            let j = jitter(base);
            assert!(
                j >= base.mul_f64(0.75) && j <= base.mul_f64(1.25),
                "jitter {j:?} out of range for base {base:?}"
            );
        }
    }

    #[test]
    fn jitter_of_zero_is_zero() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn should_retry_on_408() {
        assert!(should_retry(StatusCode::REQUEST_TIMEOUT, 0, 3));
        assert!(!should_retry(StatusCode::REQUEST_TIMEOUT, 3, 3));
    }

    #[test]
    fn should_retry_on_429() {
        assert!(should_retry(StatusCode::TOO_MANY_REQUESTS, 0, 3));
        assert!(should_retry(StatusCode::TOO_MANY_REQUESTS, 2, 3));
        assert!(!should_retry(StatusCode::TOO_MANY_REQUESTS, 3, 3));
    }

    #[test]
    fn should_retry_on_5xx() {
        assert!(should_retry(StatusCode::INTERNAL_SERVER_ERROR, 0, 3));
        assert!(should_retry(StatusCode::BAD_GATEWAY, 0, 3));
        assert!(should_retry(StatusCode::SERVICE_UNAVAILABLE, 1, 3));
    }

    #[test]
    fn should_not_retry_on_4xx() {
        assert!(!should_retry(StatusCode::BAD_REQUEST, 0, 3));
        assert!(!should_retry(StatusCode::UNAUTHORIZED, 0, 3));
        assert!(!should_retry(StatusCode::FORBIDDEN, 0, 3));
        assert!(!should_retry(StatusCode::NOT_FOUND, 0, 3));
    }

    #[test]
    fn is_blob_upload_unknown_matches_404_with_marker() {
        let err = ocync_distribution::Error::RegistryError {
            status: StatusCode::NOT_FOUND,
            message: r#"{"errors":[{"code":"BLOB_UPLOAD_UNKNOWN","message":"Layers with digests do not exist"}]}"#.into(),
        };
        assert!(is_blob_upload_unknown(&err));
    }

    #[test]
    fn is_blob_upload_unknown_rejects_other_404() {
        let err = ocync_distribution::Error::RegistryError {
            status: StatusCode::NOT_FOUND,
            message: r#"{"errors":[{"code":"NAME_UNKNOWN"}]}"#.into(),
        };
        assert!(!is_blob_upload_unknown(&err));
    }

    /// Free-text mention of the code outside of `errors[].code` must NOT
    /// classify as retryable -- the old substring match would have
    /// false-positived here.
    #[test]
    fn is_blob_upload_unknown_rejects_free_text_mention_of_code() {
        let err = ocync_distribution::Error::RegistryError {
            status: StatusCode::NOT_FOUND,
            message: r#"{"errors":[{"code":"NAME_UNKNOWN","message":"related to BLOB_UPLOAD_UNKNOWN flow"}]}"#.into(),
        };
        assert!(!is_blob_upload_unknown(&err));
    }

    /// A malformed body (not parseable as the OCI error shape) is NOT
    /// retried. Substring matching would have been ambiguous here.
    #[test]
    fn is_blob_upload_unknown_rejects_malformed_body() {
        let err = ocync_distribution::Error::RegistryError {
            status: StatusCode::NOT_FOUND,
            message: "BLOB_UPLOAD_UNKNOWN".into(),
        };
        assert!(!is_blob_upload_unknown(&err));
    }

    #[test]
    fn is_blob_upload_unknown_rejects_non_404() {
        let err = ocync_distribution::Error::RegistryError {
            status: StatusCode::BAD_REQUEST,
            message: r#"{"errors":[{"code":"BLOB_UPLOAD_UNKNOWN"}]}"#.into(),
        };
        assert!(!is_blob_upload_unknown(&err));
    }

    #[test]
    fn is_blob_upload_unknown_rejects_non_registry_error() {
        let err = ocync_distribution::Error::DigestMismatch {
            expected: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap(),
            actual: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
        };
        assert!(!is_blob_upload_unknown(&err));
    }

    #[test]
    fn should_not_retry_on_success() {
        assert!(!should_retry(StatusCode::OK, 0, 3));
        assert!(!should_retry(StatusCode::CREATED, 0, 3));
        assert!(!should_retry(StatusCode::NO_CONTENT, 0, 3));
    }

    #[test]
    fn should_retry_transport_on_registry_error() {
        // RegistryError wraps HTTP responses, not transport failures.
        let err = ocync_distribution::Error::RegistryError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "broke".into(),
        };
        assert!(!should_retry_transport(&err));
    }

    #[test]
    fn should_retry_transport_on_io() {
        let err = ocync_distribution::Error::Io {
            context: "staging read",
            source: std::io::Error::other("staging read failed"),
        };
        assert!(!should_retry_transport(&err));
    }

    /// Positive-path test: a real reqwest connection failure (refused port)
    /// must be classified as retryable through `is_request()` alone.
    ///
    /// Pins the comment-claimed equivalence: connection failures on the
    /// async hyper path surface as `Kind::Request`, so `is_request()` is
    /// sufficient to catch them. If a future reqwest version changes this
    /// wrapping, this test will fail and the predicate set must be
    /// re-evaluated.
    #[tokio::test]
    async fn should_retry_transport_on_connect_failure() {
        ocync_distribution::install_crypto_provider();
        // Connect to a port where nothing is listening -- produces a real
        // reqwest::Error with is_connect()=true and is_request()=true.
        let client = reqwest::Client::new();
        let reqwest_err = client
            .get("http://127.0.0.1:1")
            .send()
            .await
            .expect_err("connect to port 1 should fail");
        assert!(
            reqwest_err.is_connect(),
            "expected is_connect(), got: {reqwest_err}"
        );
        assert!(
            reqwest_err.is_request(),
            "is_request() must cover connect failures (predicate-set invariant); got: {reqwest_err}"
        );
        let err = ocync_distribution::Error::Http(reqwest_err);
        assert!(
            should_retry_transport(&err),
            "connection refused should be retryable"
        );
    }

    /// Positive-path test: a real reqwest request timeout must be
    /// classified as retryable through `is_request()` alone.
    ///
    /// Pins the comment-claimed equivalence: timeouts on the async hyper
    /// path surface as `Kind::Request`, so `is_request()` is sufficient.
    /// Drives the timeout by binding a `TcpListener` that accepts
    /// connections but never sends data, then issuing a reqwest GET with
    /// a 50ms request timeout.
    #[tokio::test]
    async fn should_retry_transport_on_request_timeout() {
        ocync_distribution::install_crypto_provider();

        // Bind an accepting-but-silent TCP listener on an ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Hold each accepted connection open without writing so the
            // client's read times out.
            loop {
                let Ok((_sock, _addr)) = listener.accept().await else {
                    break;
                };
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let reqwest_err = client
            .get(format!("http://127.0.0.1:{port}/v2/"))
            .send()
            .await
            .expect_err("request must time out against silent listener");
        assert!(
            reqwest_err.is_timeout(),
            "expected is_timeout(), got: {reqwest_err}"
        );
        assert!(
            reqwest_err.is_request(),
            "is_request() must cover timeouts (predicate-set invariant); got: {reqwest_err}"
        );
        let err = ocync_distribution::Error::Http(reqwest_err);
        assert!(
            should_retry_transport(&err),
            "request timeout should be retryable"
        );
    }
}
