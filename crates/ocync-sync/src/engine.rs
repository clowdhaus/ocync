use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use ulid::Ulid;

use crate::plan::BlobDedupMap;
use crate::progress::ProgressReporter;
use crate::{SyncReport, SyncStats};

/// Retry configuration with exponential backoff.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
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
    /// Uses exponential backoff capped at `max_backoff`.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let multiplier = (self.backoff_multiplier as u64).saturating_pow(attempt);
        let backoff = self.initial_backoff.saturating_mul(multiplier as u32);
        std::cmp::min(backoff, self.max_backoff)
    }
}

/// Determine whether a request should be retried based on HTTP status code.
pub fn should_retry(status: u16, current_attempt: u32, max_retries: u32) -> bool {
    if current_attempt >= max_retries {
        return false;
    }
    matches!(status, 429 | 500..=599)
}

/// Top-level engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub global_concurrent_transfers: usize,
    pub retry: RetryConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            global_concurrent_transfers: 50,
            retry: RetryConfig::default(),
        }
    }
}

/// Orchestrates the sync of images from source to target registries.
pub struct SyncEngine {
    config: EngineConfig,
    _semaphore: Arc<Semaphore>,
    _blob_dedup: Arc<Mutex<BlobDedupMap>>,
}

impl SyncEngine {
    pub fn new(config: EngineConfig) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.global_concurrent_transfers));
        let blob_dedup = Arc::new(Mutex::new(BlobDedupMap::new()));
        Self {
            config,
            _semaphore: semaphore,
            _blob_dedup: blob_dedup,
        }
    }

    /// Run the sync. Currently returns an empty report; full implementation
    /// is deferred to the integration PR.
    pub async fn sync(&self, progress: &dyn ProgressReporter) -> SyncReport {
        let report = SyncReport {
            run_id: Ulid::new(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        };
        progress.run_completed(&report);
        report
    }

    /// Access the engine configuration.
    pub fn config(&self) -> &EngineConfig {
        &self.config
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
    fn should_retry_on_429() {
        assert!(should_retry(429, 0, 3));
        assert!(should_retry(429, 2, 3));
        assert!(!should_retry(429, 3, 3));
    }

    #[test]
    fn should_retry_on_5xx() {
        assert!(should_retry(500, 0, 3));
        assert!(should_retry(502, 0, 3));
        assert!(should_retry(503, 1, 3));
        assert!(should_retry(599, 0, 3));
    }

    #[test]
    fn should_not_retry_on_4xx() {
        assert!(!should_retry(400, 0, 3));
        assert!(!should_retry(401, 0, 3));
        assert!(!should_retry(403, 0, 3));
        assert!(!should_retry(404, 0, 3));
    }

    #[test]
    fn should_not_retry_on_success() {
        assert!(!should_retry(200, 0, 3));
        assert!(!should_retry(201, 0, 3));
        assert!(!should_retry(204, 0, 3));
    }

    #[test]
    fn engine_config_defaults() {
        let cfg = EngineConfig::default();
        assert_eq!(cfg.global_concurrent_transfers, 50);
    }

    #[tokio::test]
    async fn sync_returns_empty_report() {
        let engine = SyncEngine::new(EngineConfig::default());
        let progress = crate::progress::NullProgress;
        let report = engine.sync(&progress).await;
        assert!(report.images.is_empty());
        assert_eq!(report.exit_code(), 0);
    }
}
