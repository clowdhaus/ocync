//! Retry configuration and backoff logic for transient failures.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
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
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            backoff_multiplier: 2,
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
        let capped = std::cmp::min(backoff, self.max_backoff);
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

/// Determine whether a transport-level (non-HTTP) error should be retried.
///
/// Returns `true` for connection failures and request timeouts surfaced by
/// `reqwest` before any HTTP response is received. These are transient by
/// nature and safe to retry idempotent OCI operations on.
///
/// # Known limitation
///
/// Only inspects `ocync_distribution::Error::Http(reqwest::Error)`. Transport
/// errors that arrive wrapped in other variants are NOT retried:
/// - `Error::Other(...)` -- may contain connection resets from middleware
/// - `Error::RegistryError { source, .. }` -- may wrap a transport error
/// - `Error::Manifest { source, .. }` -- may wrap a transport error
///
/// If operators observe unretrieved transient errors, the debug log below will
/// show which variant was encountered so the match can be extended.
pub fn should_retry_transport(error: &ocync_distribution::Error) -> bool {
    if let ocync_distribution::Error::Http(reqwest_err) = error {
        reqwest_err.is_connect() || reqwest_err.is_timeout()
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
/// decorrelate concurrent retries. Uses [`RandomState`] for per-process
/// entropy without requiring a `rand` dependency.
fn jitter(base: Duration) -> Duration {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(base.as_nanos() as u64);
    let hash = hasher.finish();
    let factor = 0.75 + (hash % 500) as f64 / 1000.0;
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
    fn should_retry_transport_on_other() {
        let err = ocync_distribution::Error::Other("something".into());
        assert!(!should_retry_transport(&err));
    }
}
