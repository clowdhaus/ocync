//! A credential cached until it nears expiry, shared by the cloud auth
//! providers and the AWS API client.

use std::future::Future;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;
use tokio::time::Instant;

use crate::auth::{EARLY_REFRESH_WINDOW, is_fresh};

/// Time until `until`, or zero once it has passed.
pub(crate) fn remaining(until: SystemTime) -> Duration {
    until
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// Cached credential with TTL tracking.
///
/// Generic cache for credentials that uses a read-lock fast path
/// for concurrent cache hits and a write-lock with double-check for
/// refreshes. Used by the ECR, ECR Public, Azure and Google auth providers,
/// the AWS API client and the instance metadata session token. Time is
/// tokio's, so a test with paused time moves it.
pub(crate) struct CredentialCache<T> {
    cached: RwLock<Option<CacheEntry<T>>>,
    /// A value with less than this left is refreshed.
    refresh_window: Duration,
}

struct CacheEntry<T> {
    value: T,
    /// When the value expires, or `None` when its lifetime runs past what
    /// the clock can hold.
    expires_at: Option<Instant>,
}

impl<T> CacheEntry<T> {
    /// A value with `ttl` left from now.
    fn new(value: T, ttl: Duration) -> Self {
        Self {
            value,
            expires_at: Instant::now().checked_add(ttl),
        }
    }

    /// Whether more than `window` of the value's lifetime is left.
    fn is_fresh(&self, window: Duration) -> bool {
        is_fresh(self.expires_at, window)
    }
}

impl<T: Clone + Send + Sync> CredentialCache<T> {
    /// Create an empty cache that refreshes [`EARLY_REFRESH_WINDOW`] before
    /// expiry.
    pub(crate) fn new() -> Self {
        Self::with_refresh_window(EARLY_REFRESH_WINDOW)
    }

    /// Create an empty cache that refreshes `window` before expiry.
    pub(crate) fn with_refresh_window(window: Duration) -> Self {
        Self {
            cached: RwLock::new(None),
            refresh_window: window,
        }
    }

    /// Return the cached value if still valid, otherwise await `refresh` to
    /// obtain a new one and its lifetime.
    ///
    /// The refresh future is created at the call site but only polled on
    /// cache miss, so the API call is never issued when the cache is warm.
    pub(crate) async fn get_or_refresh<E>(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), E>>,
    ) -> Result<T, E> {
        // Fast path: read-lock check allows concurrent readers.
        {
            let cached = self.cached.read().await;
            if let Some(ref entry) = *cached {
                if entry.is_fresh(self.refresh_window) {
                    return Ok(entry.value.clone());
                }
            }
        }

        // Slow path: write-lock for refresh.
        let mut cached = self.cached.write().await;

        // Double-check after acquiring write lock -- another task may have
        // already refreshed while we waited.
        if let Some(ref entry) = *cached {
            if entry.is_fresh(self.refresh_window) {
                return Ok(entry.value.clone());
            }
        }

        let (value, ttl) = refresh.await?;

        *cached = Some(CacheEntry::new(value.clone(), ttl));

        Ok(value)
    }

    /// Clear the cached value, forcing the next `get_or_refresh` to call
    /// the refresh function.
    pub(crate) async fn clear(&self) {
        *self.cached.write().await = None;
    }

    /// Clear the cached value if `matches` says it is the one to drop.
    pub(crate) async fn clear_if(&self, matches: impl FnOnce(&T) -> bool) {
        let mut cached = self.cached.write().await;
        if cached.as_ref().is_some_and(|entry| matches(&entry.value)) {
            *cached = None;
        }
    }

    /// Replace the cached value (test helper).
    #[cfg(test)]
    pub(crate) async fn set(&self, value: T, ttl: Duration) {
        *self.cached.write().await = Some(CacheEntry::new(value, ttl));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expiry_in_the_past_leaves_no_time() {
        assert_eq!(remaining(SystemTime::UNIX_EPOCH), Duration::ZERO);
    }

    /// A value is served while more than the refresh window is left, and
    /// a lifetime past what the clock can hold never expires. Breaks if
    /// the window is not honored or a long lifetime overflows the clock.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn refreshes_within_the_window() {
        let cache = CredentialCache::with_refresh_window(Duration::from_secs(120));
        let load = |value: u32, ttl: u64| async move {
            Ok::<_, std::convert::Infallible>((value, Duration::from_secs(ttl)))
        };
        assert_eq!(cache.get_or_refresh(load(1, 1000)).await, Ok(1));
        tokio::time::advance(Duration::from_secs(879)).await;
        assert_eq!(cache.get_or_refresh(load(2, 1000)).await, Ok(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(cache.get_or_refresh(load(3, u64::MAX)).await, Ok(3));
        tokio::time::advance(Duration::from_secs(1 << 40)).await;
        assert_eq!(cache.get_or_refresh(load(4, 1000)).await, Ok(3));
    }
}
