//! A credential cached until it nears expiry, shared by the cloud auth
//! providers and the AWS API client.

use std::convert::Infallible;
use std::future::Future;
use std::ops::ControlFlow;
use std::time::{Duration, SystemTime};

use tokio::sync::{RwLock, RwLockWriteGuard};
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
///
/// `E` is the error [`Self::get_or_refresh_shared`] hands to the callers
/// that waited on a refresh that failed. A cache that never shares one
/// leaves it [`Infallible`].
pub(crate) struct CredentialCache<T, E = Infallible> {
    cached: RwLock<Slot<T, E>>,
    /// A value with less than this left is refreshed.
    refresh_window: Duration,
}

/// What a [`CredentialCache`] holds.
struct Slot<T, E> {
    entry: Option<CacheEntry<T>>,
    /// The last refresh that failed, and when.
    failed: Option<(Instant, E)>,
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

impl<T: Clone + Send + Sync, E> CredentialCache<T, E> {
    /// Create an empty cache that refreshes [`EARLY_REFRESH_WINDOW`] before
    /// expiry.
    pub(crate) fn new() -> Self {
        Self::with_refresh_window(EARLY_REFRESH_WINDOW)
    }

    /// Create an empty cache that refreshes `window` before expiry.
    pub(crate) fn with_refresh_window(window: Duration) -> Self {
        Self {
            cached: RwLock::new(Slot {
                entry: None,
                failed: None,
            }),
            refresh_window: window,
        }
    }

    /// Return the cached value if still valid, otherwise await `refresh` to
    /// obtain a new one and its lifetime.
    ///
    /// The refresh future is created at the call site but only polled on
    /// cache miss, so the API call is never issued when the cache is warm.
    pub(crate) async fn get_or_refresh<F>(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), F>>,
    ) -> Result<T, F> {
        let mut slot = match self.fresh_or_lock().await {
            ControlFlow::Break(value) => return Ok(value),
            ControlFlow::Continue(slot) => slot,
        };
        let (value, ttl) = refresh.await?;
        slot.entry = Some(CacheEntry::new(value.clone(), ttl));
        Ok(value)
    }

    /// As [`Self::get_or_refresh`], but a caller that was waiting on a
    /// refresh that failed gets its error instead of refreshing again. A
    /// failing source then costs one refresh per wave of callers, not one
    /// per caller, while a caller that arrives later tries afresh.
    pub(crate) async fn get_or_refresh_shared(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), E>>,
    ) -> Result<T, E>
    where
        E: Clone,
    {
        let asked = Instant::now();
        let mut slot = match self.fresh_or_lock().await {
            ControlFlow::Break(value) => return Ok(value),
            ControlFlow::Continue(slot) => slot,
        };
        if let Some((at, error)) = &slot.failed
            && *at > asked
        {
            return Err(error.clone());
        }
        match refresh.await {
            Ok((value, ttl)) => {
                slot.entry = Some(CacheEntry::new(value.clone(), ttl));
                slot.failed = None;
                Ok(value)
            }
            Err(error) => {
                slot.failed = Some((Instant::now(), error.clone()));
                Err(error)
            }
        }
    }

    /// The cached value if fresh, found under a read lock that concurrent
    /// callers share, or else the write lock to refresh under. The value is
    /// looked for again under the write lock, as another caller may have
    /// refreshed it while this one waited.
    async fn fresh_or_lock(&self) -> ControlFlow<T, RwLockWriteGuard<'_, Slot<T, E>>> {
        if let Some(value) = self.fresh_in(&*self.cached.read().await) {
            return ControlFlow::Break(value);
        }
        let slot = self.cached.write().await;
        match self.fresh_in(&slot) {
            Some(value) => ControlFlow::Break(value),
            None => ControlFlow::Continue(slot),
        }
    }

    /// The value `slot` holds, if fresh.
    fn fresh_in(&self, slot: &Slot<T, E>) -> Option<T> {
        slot.entry
            .as_ref()
            .filter(|entry| entry.is_fresh(self.refresh_window))
            .map(|entry| entry.value.clone())
    }

    /// Clear the cached value, forcing the next `get_or_refresh` to call
    /// the refresh function.
    pub(crate) async fn clear(&self) {
        self.cached.write().await.entry = None;
    }

    /// Clear the cached value if `matches` says it is the one to drop.
    pub(crate) async fn clear_if(&self, matches: impl FnOnce(&T) -> bool) {
        let mut slot = self.cached.write().await;
        if slot
            .entry
            .as_ref()
            .is_some_and(|entry| matches(&entry.value))
        {
            slot.entry = None;
        }
    }

    /// Replace the cached value (test helper).
    #[cfg(test)]
    pub(crate) async fn set(&self, value: T, ttl: Duration) {
        self.cached.write().await.entry = Some(CacheEntry::new(value, ttl));
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
        let cache: CredentialCache<_> =
            CredentialCache::with_refresh_window(Duration::from_secs(120));
        let load = |value: u32, ttl: u64| async move {
            Ok::<_, Infallible>((value, Duration::from_secs(ttl)))
        };
        assert_eq!(cache.get_or_refresh(load(1, 1000)).await, Ok(1));
        tokio::time::advance(Duration::from_secs(879)).await;
        assert_eq!(cache.get_or_refresh(load(2, 1000)).await, Ok(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(cache.get_or_refresh(load(3, u64::MAX)).await, Ok(3));
        tokio::time::advance(Duration::from_secs(1 << 40)).await;
        assert_eq!(cache.get_or_refresh(load(4, 1000)).await, Ok(3));
    }

    /// Callers waiting on a refresh that fails get its error, and one that
    /// asks afterwards refreshes again. Breaks if each waiter repeats the
    /// failed refresh, or a failure is kept for later callers.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn waiters_share_a_failed_refresh() {
        use std::cell::Cell;

        let cache: CredentialCache<u32, &str> = CredentialCache::new();
        let refreshes = Cell::new(0);
        let failing = || async {
            refreshes.set(refreshes.get() + 1);
            tokio::time::sleep(Duration::from_secs(1)).await;
            Err("down")
        };

        let (first, second) = tokio::join!(
            cache.get_or_refresh_shared(failing()),
            cache.get_or_refresh_shared(failing()),
        );

        assert_eq!((first, second), (Err("down"), Err("down")));
        assert_eq!(refreshes.get(), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        let later = cache
            .get_or_refresh_shared(async { Ok((7, Duration::from_secs(1000))) })
            .await;
        assert_eq!(later, Ok(7));
    }
}
