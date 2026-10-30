//! Retry configuration and backoff logic for transient failures.

use std::cell::Cell;
use std::future::Future;
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
    /// view to converge before a manifest `PUT`, when a
    /// [`BatchBlobChecker`](ocync_distribution::ecr::BatchBlobChecker)
    /// is configured (ECR targets).
    ///
    /// Default 30s: larger than typical ECR consistency windows, bounded so
    /// a stuck view does not hold the engine open. `Duration::ZERO` disables
    /// the wait; the retry path still catches any `BLOB_UPLOAD_UNKNOWN`.
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
    /// Backoff for the given attempt (0-indexed): exponential, capped at
    /// `max_backoff`, then jittered in \[0.75, 1.25). All arithmetic saturates.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let multiplier = self.backoff_multiplier.saturating_pow(attempt);
        let backoff = self.initial_backoff.saturating_mul(multiplier);
        let capped = backoff.min(self.max_backoff);
        jitter(capped)
    }
}

/// Whether attempts remain and `status` is one `ocync_distribution::retry::is_transient_status`
/// names.
pub fn should_retry(status: StatusCode, current_attempt: u32, max_retries: u32) -> bool {
    current_attempt < max_retries && ocync_distribution::retry::is_transient_status(status)
}

/// Whether `error` is a 404 `RegistryError` whose body is OCI error JSON with
/// an `errors[].code == "BLOB_UPLOAD_UNKNOWN"` entry.
///
/// ECR answers a manifest push this way when a blob's PUT-201 has not reached
/// the manifest-validation index yet. A body that does not parse, or that
/// mentions the code only in prose, is not retryable: substring matching
/// would false-positive there.
///
/// The ECR manifest-commit wait ([`RetryConfig::manifest_commit_wait`])
/// removes this race in the common case. Retrying on it stays as the
/// fallback for a wait that times out and for targets without a batch checker.
pub fn is_blob_upload_unknown(error: &ocync_distribution::Error) -> bool {
    let ocync_distribution::Error::RegistryError { status, message } = error else {
        return false;
    };
    if *status != StatusCode::NOT_FOUND {
        return false;
    }
    oci_error_codes(message).any(|code| code == "BLOB_UPLOAD_UNKNOWN")
}

/// The `errors[].code` values of an OCI distribution-spec error body; none
/// when the body is not that shape.
#[expect(clippy::disallowed_methods, reason = "a parse error is dropped unread")]
pub(crate) fn oci_error_codes(message: &str) -> impl Iterator<Item = String> {
    serde_json::from_str::<OciErrorBody>(message)
        .map(|body| body.errors)
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.code)
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

/// Whether a transport-level (non-HTTP) error should be retried.
///
/// True for connection failures, request timeouts, mid-stream send failures
/// and truncated response bodies (which `reqwest` reports as decode
/// failures). These are transient and OCI operations are idempotent. An
/// answer that arrived whole and is not the JSON expected is final.
///
/// `is_request()` covers connect failures and timeouts (both `Kind::Request`
/// on the async hyper path); `is_body()` and `is_decode()` cover the
/// response-body phase. The `should_retry_transport_*` tests pin this.
///
/// Only `Error::Http` is inspected. Other variants are not retried:
/// - `Io`: local filesystem, never transient
/// - `RegistryError`: HTTP-level, retried via [`should_retry`] instead
/// - `RegistryProtocol`: deterministic spec violation
/// - `UnreadableAnswer`: the same body comes back if asked again
/// - `AnswerTooLarge`: asking again does not raise the limit
///
/// The debug log names the variant when a transient error goes unretried, so
/// the match can be extended.
pub fn should_retry_transport(error: &ocync_distribution::Error) -> bool {
    if let ocync_distribution::Error::Http(reqwest_err) = error {
        // One definition, shared with the prepare-phase retry in
        // `ocync_distribution::retry`; two copies drift.
        ocync_distribution::retry::is_transient_transport(reqwest_err)
    } else {
        tracing::debug!(
            error = %error,
            "non-Http error variant not inspectable for transport retry"
        );
        false
    }
}

/// Scale `base` by a random factor in \[0.75, 1.25) to decorrelate concurrent
/// retries. `fastrand`'s thread-local PRNG makes each call one `f64` draw with
/// no syscall.
fn jitter(base: Duration) -> Duration {
    let factor = 0.75 + fastrand::f64() * 0.5;
    base.mul_f64(factor)
}

tokio::task_local! {
    /// Retries `with_retry` has made inside the enclosing [`count_retries`].
    static RETRIES: Cell<u32>;
}

/// Run `fut`, counting the retries [`with_retry`] makes inside it, starting
/// from `already`. The engine wraps each image's discovery, then its
/// execution starting from the discovery count, so a failed image reports
/// the retries it made rather than the configured budget.
pub(crate) async fn count_retries<F: Future>(already: u32, fut: F) -> F::Output {
    RETRIES.scope(Cell::new(already), fut).await
}

/// Retries made so far inside the enclosing [`count_retries`], or 0 outside one.
pub(crate) fn retries_made() -> u32 {
    RETRIES.try_with(Cell::get).unwrap_or(0)
}

/// Count one retry against the enclosing [`count_retries`]. Outside one (the
/// CLI's own walks) there is no image to attribute it to.
fn record_retry() {
    let _ = RETRIES.try_with(|n| n.set(n.get() + 1));
}

/// Whether [`with_retry`] has sent a refused upload again yet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenRetry {
    Available,
    Spent,
}

/// Retry an async operation with jittered exponential backoff, up to
/// `config.max_retries` times.
///
/// Retries HTTP 408/429/5xx, transport-level errors, and the ECR 404
/// `BLOB_UPLOAD_UNKNOWN` body marker ([`is_blob_upload_unknown`]).
///
/// A streamed upload answered 401, whose token the client has dropped, is
/// sent once more at once, outside that budget: nothing is throttled, and a
/// second refusal is the credential's, not the token's age.
///
/// Returns the first `Ok` or the final `Err`.
pub async fn with_retry<T, F, Fut>(
    config: &RetryConfig,
    operation: &str,
    f: F,
) -> Result<T, ocync_distribution::Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, ocync_distribution::Error>>,
{
    let mut attempt = 0;
    let mut refusal = TokenRetry::Available;
    loop {
        match f().await {
            Ok(val) => return Ok(val),
            Err(ocync_distribution::Error::UploadTokenRefused { message })
                if refusal == TokenRetry::Available =>
            {
                tracing::warn!(operation, error = %message, "retrying under a new token");
                refusal = TokenRetry::Spent;
                record_retry();
            }
            Err(e) => {
                let retryable = if let Some(status) = e.status_code() {
                    should_retry(status, attempt, config.max_retries)
                        || (attempt < config.max_retries && is_blob_upload_unknown(&e))
                } else {
                    attempt < config.max_retries && should_retry_transport(&e)
                };

                if retryable {
                    let backoff = config.backoff_for(attempt);
                    tracing::warn!(
                        operation,
                        attempt,
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        "retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                    record_retry();
                    continue;
                }
                return Err(e);
            }
        }
    }
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

    /// Assert a duration falls within the jitter range [base*0.75, base*1.25].
    fn assert_in_jitter_range(actual: Duration, base: Duration, name: &str) {
        let lo = base.mul_f64(0.75);
        let hi = base.mul_f64(1.25);
        assert!(
            actual >= lo && actual <= hi,
            "{name}: expected {actual:?} in [{lo:?}, {hi:?}] (base={base:?})"
        );
    }

    #[test]
    fn backoff_for_lands_in_jitter_range_of_base() {
        let with = |multiplier, max_secs| RetryConfig {
            backoff_multiplier: multiplier,
            max_backoff: Duration::from_secs(max_secs),
            ..RetryConfig::default()
        };
        let s = Duration::from_secs;
        // (name, config, attempt, base before jitter)
        let cases = [
            ("exponential 0", with(2, 300), 0, s(1)),
            ("exponential 1", with(2, 300), 1, s(2)),
            ("exponential 2", with(2, 300), 2, s(4)),
            ("exponential 3", with(2, 300), 3, s(8)),
            // 2^10 = 1024s, capped at 5s before jitter.
            ("caps at max_backoff", with(2, 5), 10, s(5)),
            ("multiplier one is constant 0", with(1, 300), 0, s(1)),
            ("multiplier one is constant 5", with(1, 300), 5, s(1)),
            ("multiplier one is constant 100", with(1, 300), 100, s(1)),
            // 3^30 overflows u32; saturating_pow, saturating_mul then min cap it.
            ("saturates on overflow", with(3, 300), 30, s(300)),
            // 0^0 = 1, so attempt 0 keeps initial_backoff; 0^n = 0 after.
            ("multiplier zero attempt 0", with(0, 300), 0, s(1)),
            ("multiplier zero attempt 1", with(0, 300), 1, Duration::ZERO),
            ("multiplier zero attempt 5", with(0, 300), 5, Duration::ZERO),
        ];
        for (name, cfg, attempt, base) in cases {
            assert_in_jitter_range(cfg.backoff_for(attempt), base, name);
        }
    }

    #[test]
    fn jitter_stays_in_range() {
        // Many draws to cover the range; zero must stay zero.
        for base in [Duration::from_secs(10), Duration::ZERO] {
            for _ in 0..100 {
                assert_in_jitter_range(jitter(base), base, "jitter");
            }
        }
    }

    #[test]
    fn should_retry_by_status_and_attempt() {
        use StatusCode as S;
        // (name, status, current attempt, expected); max_retries is 3.
        let cases = [
            ("408 first attempt", S::REQUEST_TIMEOUT, 0, true),
            ("408 budget spent", S::REQUEST_TIMEOUT, 3, false),
            ("429 first attempt", S::TOO_MANY_REQUESTS, 0, true),
            ("429 last attempt", S::TOO_MANY_REQUESTS, 2, true),
            ("429 budget spent", S::TOO_MANY_REQUESTS, 3, false),
            ("500", S::INTERNAL_SERVER_ERROR, 0, true),
            ("502", S::BAD_GATEWAY, 0, true),
            ("503 second attempt", S::SERVICE_UNAVAILABLE, 1, true),
            ("400", S::BAD_REQUEST, 0, false),
            ("401", S::UNAUTHORIZED, 0, false),
            ("403", S::FORBIDDEN, 0, false),
            ("404", S::NOT_FOUND, 0, false),
            ("200", S::OK, 0, false),
            ("201", S::CREATED, 0, false),
            ("204", S::NO_CONTENT, 0, false),
        ];
        for (name, status, attempt, want) in cases {
            assert_eq!(should_retry(status, attempt, 3), want, "{name}");
        }
    }

    #[test]
    fn is_blob_upload_unknown_classifies_errors() {
        let registry = |status, message: &str| ocync_distribution::Error::RegistryError {
            status,
            message: message.into(),
        };
        let digest = |c: char| -> ocync_distribution::Digest {
            format!("sha256:{}", c.to_string().repeat(64))
                .parse()
                .unwrap()
        };
        let cases = [
            (
                "404 with marker",
                registry(
                    StatusCode::NOT_FOUND,
                    r#"{"errors":[{"code":"BLOB_UPLOAD_UNKNOWN","message":"Layers with digests do not exist"}]}"#,
                ),
                true,
            ),
            (
                "other 404 code",
                registry(
                    StatusCode::NOT_FOUND,
                    r#"{"errors":[{"code":"NAME_UNKNOWN"}]}"#,
                ),
                false,
            ),
            // A substring match would false-positive on prose mentions.
            (
                "code only in free text",
                registry(
                    StatusCode::NOT_FOUND,
                    r#"{"errors":[{"code":"NAME_UNKNOWN","message":"related to BLOB_UPLOAD_UNKNOWN flow"}]}"#,
                ),
                false,
            ),
            (
                "malformed body",
                registry(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN"),
                false,
            ),
            (
                "non-404 status",
                registry(
                    StatusCode::BAD_REQUEST,
                    r#"{"errors":[{"code":"BLOB_UPLOAD_UNKNOWN"}]}"#,
                ),
                false,
            ),
            (
                "non-registry error",
                ocync_distribution::Error::DigestMismatch {
                    expected: digest('0'),
                    actual: digest('1'),
                },
                false,
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(is_blob_upload_unknown(&err), want, "{name}");
        }
    }

    #[test]
    fn should_not_retry_transport_for_non_http_errors() {
        let cases = [
            // RegistryError wraps HTTP responses, not transport failures.
            (
                "registry error",
                ocync_distribution::Error::RegistryError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "broke".into(),
                },
            ),
            (
                "io error",
                ocync_distribution::Error::Io {
                    context: "staging read",
                    source: std::io::Error::other("staging read failed").into(),
                },
            ),
        ];
        for (name, err) in cases {
            assert!(!should_retry_transport(&err), "{name}");
        }
    }

    /// A streamed upload whose token was refused is sent once more at once,
    /// after any transient retries, and a second refusal is final. Breaks if
    /// the refusal is never retried, waits out a backoff, is retried only on
    /// the first attempt, or is retried until the attempts run out.
    #[tokio::test(start_paused = true)]
    async fn a_refused_upload_token_is_retried_once() {
        #[derive(Clone, Copy, Debug)]
        enum Failure {
            Refused,
            Unavailable,
        }
        use Failure::{Refused, Unavailable};
        let config = RetryConfig::default();
        // Each case: the failures before success, the calls made, whether it
        // succeeds, and whether it waited out a backoff.
        let cases: [(&[Failure], usize, bool, bool); 3] = [
            (&[Refused], 2, true, false),
            (&[Unavailable, Refused], 3, true, true),
            (&[Refused, Refused, Refused], 2, false, false),
        ];
        for (failures, expected_calls, succeeds, waits) in cases {
            let calls = Cell::new(0);
            let started = tokio::time::Instant::now();
            let result = with_retry(&config, "blob transfer", || {
                let call = calls.get();
                calls.set(call + 1);
                let failure = failures.get(call).map(|failure| match failure {
                    Refused => ocync_distribution::Error::UploadTokenRefused {
                        message: "registry/repo".into(),
                    },
                    Unavailable => ocync_distribution::Error::RegistryError {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        message: "registry/repo".into(),
                    },
                });
                async move { failure.map_or(Ok(()), Err) }
            })
            .await;
            assert_eq!(result.is_ok(), succeeds, "{failures:?}");
            assert_eq!(calls.get(), expected_calls, "{failures:?}");
            assert_eq!(started.elapsed() > Duration::ZERO, waits, "{failures:?}");
        }
    }

    /// A real connection failure (refused port) is retryable through
    /// `is_request()` alone. Breaks if a reqwest upgrade stops wrapping
    /// connect failures as `Kind::Request`; the predicate set then needs
    /// re-evaluating.
    #[tokio::test]
    async fn should_retry_transport_on_connect_failure() {
        ocync_distribution::install_crypto_provider();
        // Nothing listens on port 1: a real connect error.
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
        let err = ocync_distribution::Error::from(reqwest_err);
        assert!(
            should_retry_transport(&err),
            "connection refused should be retryable"
        );
    }

    /// A real request timeout is retryable through `is_request()` alone.
    /// Driven by a `TcpListener` that accepts but never writes, and a 50ms
    /// request timeout.
    #[tokio::test]
    async fn should_retry_transport_on_request_timeout() {
        ocync_distribution::install_crypto_provider();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Hold connections open without writing so the client times out.
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
        let err = ocync_distribution::Error::from(reqwest_err);
        assert!(
            should_retry_transport(&err),
            "request timeout should be retryable"
        );
    }

    /// A failed image reports the retries it made, not the configured budget.
    #[tokio::test(flavor = "current_thread")]
    async fn count_retries_counts_only_retries_made_inside_it() {
        let cfg = RetryConfig {
            max_retries: 5,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            ..RetryConfig::default()
        };
        let calls = Cell::new(0);
        let made = count_retries(1, async {
            let result = with_retry(&cfg, "test", || async {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err(ocync_distribution::Error::RegistryError {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        message: "busy".into(),
                    })
                } else {
                    Ok(())
                }
            })
            .await;
            assert!(result.is_ok());
            retries_made()
        })
        .await;
        assert_eq!(made, 3, "two retries on top of the one already counted");
        assert_eq!(retries_made(), 0, "no count outside count_retries");
    }
}
