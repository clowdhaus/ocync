//! A credential cached until it nears expiry, shared by the cloud auth
//! providers and the AWS API client.

use std::convert::Infallible;
use std::future::Future;
use std::sync::{PoisonError, RwLock, RwLockWriteGuard};
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use crate::auth::{EARLY_REFRESH_WINDOW, is_fresh};
use crate::flight::{Flights, Landed};

/// Time until `until`, or zero once it has passed.
pub(crate) fn remaining(until: SystemTime) -> Duration {
    until
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// Cached credential with TTL tracking.
///
/// A refresh runs once however many callers need it: the callers that
/// arrive while it is in flight wait for it. Used by the ECR, ECR Public,
/// Azure and Google auth providers, the AWS API client and the instance
/// metadata session token. Time is tokio's, so a test with paused time
/// moves it.
///
/// `E` is the error [`Self::get_or_refresh_sharing_failure`] hands to the
/// callers that waited on a refresh that failed. A cache that never shares
/// one leaves it [`Infallible`], and its waiters refresh for themselves.
pub(crate) struct CredentialCache<T, E = Infallible> {
    entry: RwLock<Option<CacheEntry<T>>>,
    /// A value with less than this left is refreshed.
    refresh_window: Duration,
    flights: Flights<(), Result<T, E>>,
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

impl<T: Clone, E: Clone> CredentialCache<T, E> {
    /// Create an empty cache that refreshes [`EARLY_REFRESH_WINDOW`] before
    /// expiry.
    pub(crate) fn new() -> Self {
        Self::with_refresh_window(EARLY_REFRESH_WINDOW)
    }

    /// Create an empty cache that refreshes `window` before expiry.
    pub(crate) fn with_refresh_window(window: Duration) -> Self {
        Self {
            entry: RwLock::new(None),
            refresh_window: window,
            flights: Flights::default(),
        }
    }

    /// Run `refresh` for the callers that need a value, unless one is in
    /// flight: `share` picks what its waiters get from its result.
    async fn refresh<F>(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), F>>,
        share: impl FnOnce(&Result<(T, Duration), F>) -> Option<Result<T, E>>,
    ) -> Result<T, Landed<F, E>> {
        let land = |result: &Result<(T, Duration), F>| {
            if let Ok((value, ttl)) = result {
                *self.write() = Some(CacheEntry::new(value.clone(), *ttl));
            }
            share(result)
        };
        match self
            .flights
            .run(&(), || self.fresh().map(Ok), || refresh, land)
            .await
        {
            Landed::Own(Ok((value, _))) | Landed::Shared(Ok(value)) => Ok(value),
            Landed::Own(Err(error)) => Err(Landed::Own(error)),
            Landed::Shared(Err(error)) => Err(Landed::Shared(error)),
        }
    }

    /// The value, if fresh.
    fn fresh(&self) -> Option<T> {
        self.entry
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|entry| entry.is_fresh(self.refresh_window))
            .map(|entry| entry.value.clone())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Option<CacheEntry<T>>> {
        self.entry.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// As [`CredentialCache::get_or_refresh`], but the callers waiting on a
    /// refresh that fails get its error instead of refreshing again. A
    /// failing source then costs one refresh per wave of callers, not one
    /// per caller. A caller that asks after the failure refreshes again.
    pub(crate) async fn get_or_refresh_sharing_failure(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), E>>,
    ) -> Result<T, E> {
        let share = |result: &Result<(T, Duration), E>| {
            Some(
                result
                    .as_ref()
                    .map(|(value, _)| value.clone())
                    .map_err(E::clone),
            )
        };
        self.refresh(refresh, share)
            .await
            .map_err(|(Landed::Own(error) | Landed::Shared(error))| error)
    }

    /// Clear the cached value. A caller that asks afterwards joins the
    /// refresh in flight, if any, since it fetches a new value: a burst of
    /// refusals costs one refresh.
    pub(crate) fn clear(&self) {
        *self.write() = None;
    }

    /// As [`Self::clear`], if `matches` says the cached value is the one to
    /// drop.
    pub(crate) fn clear_if(&self, matches: impl FnOnce(&T) -> bool) {
        let mut entry = self.write();
        if entry.as_ref().is_some_and(|entry| matches(&entry.value)) {
            *entry = None;
        }
    }

    /// Replace the cached value (test helper).
    #[cfg(test)]
    pub(crate) fn set(&self, value: T, ttl: Duration) {
        *self.write() = Some(CacheEntry::new(value, ttl));
    }
}

impl<T: Clone> CredentialCache<T> {
    /// Return the cached value if still valid, otherwise await `refresh` to
    /// obtain a new one and its lifetime. A caller waiting on a refresh
    /// that fails refreshes for itself, as the error cannot be shared.
    ///
    /// The refresh future is created at the call site but only polled on
    /// cache miss, so the API call is never issued when the cache is warm.
    pub(crate) async fn get_or_refresh<F>(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), F>>,
    ) -> Result<T, F> {
        let share = |result: &Result<(T, Duration), F>| {
            result.as_ref().ok().map(|(value, _)| Ok(value.clone()))
        };
        self.refresh(refresh, share)
            .await
            .map_err(|error| match error {
                Landed::Own(error) => error,
                Landed::Shared(never) => match never {},
            })
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

    /// Callers waiting on a refresh get its value even when it is already
    /// within the refresh window. Breaks if a waiter refreshes again.
    #[tokio::test(flavor = "current_thread")]
    async fn waiters_share_a_short_lived_value() {
        use std::cell::Cell;

        let cache: CredentialCache<_> =
            CredentialCache::with_refresh_window(Duration::from_secs(120));
        let refreshes = Cell::new(0);
        let short = || async {
            refreshes.set(refreshes.get() + 1);
            tokio::task::yield_now().await;
            Ok::<_, Infallible>((7, Duration::from_secs(10)))
        };
        let (first, second) =
            tokio::join!(cache.get_or_refresh(short()), cache.get_or_refresh(short()));
        assert_eq!((first, second), (Ok(7), Ok(7)));
        assert_eq!(refreshes.get(), 1);
    }

    /// A caller that asks after a clear joins the refresh in flight. Breaks
    /// if a burst of refusals starts one refresh per refusal.
    #[tokio::test(flavor = "current_thread")]
    async fn a_clear_joins_the_refresh_in_flight() {
        use std::cell::Cell;

        let cache: CredentialCache<_> = CredentialCache::new();
        let refreshes = Cell::new(0);
        let load = |value: u32| {
            let refreshes = &refreshes;
            async move {
                refreshes.set(refreshes.get() + 1);
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                Ok::<_, Infallible>((value, Duration::from_secs(1000)))
            }
        };
        let (before, after) = tokio::join!(cache.get_or_refresh(load(1)), async {
            tokio::task::yield_now().await;
            cache.clear();
            cache.get_or_refresh(load(2)).await
        });
        assert_eq!((before, after), (Ok(1), Ok(1)));
        assert_eq!(refreshes.get(), 1);
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
            tokio::task::yield_now().await;
            Err("down")
        };

        let (first, second) = tokio::join!(
            cache.get_or_refresh_sharing_failure(failing()),
            cache.get_or_refresh_sharing_failure(failing()),
        );

        assert_eq!((first, second), (Err("down"), Err("down")));
        assert_eq!(refreshes.get(), 1);
        let later = cache
            .get_or_refresh_sharing_failure(async { Ok((7, Duration::from_secs(1000))) })
            .await;
        assert_eq!(later, Ok(7));
    }
}
