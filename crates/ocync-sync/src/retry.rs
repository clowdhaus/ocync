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
    /// Uses exponential backoff capped at `max_backoff`. All arithmetic
    /// saturates instead of overflowing.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let multiplier = self.backoff_multiplier.saturating_pow(attempt);
        let backoff = self.initial_backoff.saturating_mul(multiplier);
        std::cmp::min(backoff, self.max_backoff)
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

    #[test]
    fn backoff_exponential() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.backoff_for(0), Duration::from_secs(1));
        assert_eq!(cfg.backoff_for(1), Duration::from_secs(2));
        assert_eq!(cfg.backoff_for(2), Duration::from_secs(4));
        assert_eq!(cfg.backoff_for(3), Duration::from_secs(8));
    }

    #[test]
    fn backoff_caps_at_max() {
        let cfg = RetryConfig {
            max_backoff: Duration::from_secs(5),
            ..RetryConfig::default()
        };
        // 2^10 = 1024 seconds, but capped at 5
        assert_eq!(cfg.backoff_for(10), Duration::from_secs(5));
    }

    #[test]
    fn backoff_with_multiplier_one_is_constant() {
        let cfg = RetryConfig {
            backoff_multiplier: 1,
            ..RetryConfig::default()
        };
        assert_eq!(cfg.backoff_for(0), Duration::from_secs(1));
        assert_eq!(cfg.backoff_for(5), Duration::from_secs(1));
        assert_eq!(cfg.backoff_for(100), Duration::from_secs(1));
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
        assert_eq!(result, Duration::from_secs(300));
    }

    #[test]
    fn backoff_with_multiplier_zero_gives_zero_after_first() {
        let cfg = RetryConfig {
            backoff_multiplier: 0,
            ..RetryConfig::default()
        };
        // 0^0 = 1, so attempt 0 gives initial_backoff * 1
        assert_eq!(cfg.backoff_for(0), Duration::from_secs(1));
        // 0^n = 0 for n > 0, so all subsequent attempts give zero backoff
        assert_eq!(cfg.backoff_for(1), Duration::ZERO);
        assert_eq!(cfg.backoff_for(5), Duration::ZERO);
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
}
