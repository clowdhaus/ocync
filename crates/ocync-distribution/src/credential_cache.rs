//! A credential cached until it nears expiry, shared by the cloud auth
//! providers and the AWS API client.

use std::convert::Infallible;
use std::future::Future;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
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
/// A landing never stores a value the registry refused after its refresh
/// began (see [`Self::refuse_if`]); the callers that waited on it still get
/// its value. No other value can be stored while a refresh is in flight, as
/// the callers that need one wait on it.
///
/// `E` is the error [`Self::get_or_refresh_sharing_failure`] hands to the
/// callers that waited on a refresh that failed. A cache that never shares
/// one leaves it [`Infallible`], and its waiters refresh for themselves.
pub(crate) struct CredentialCache<T, E = Infallible> {
    state: RwLock<State<T>>,
    /// A value with less than this left is refreshed.
    refresh_window: Duration,
    flights: Flights<(), Result<T, E>>,
}

struct State<T> {
    entry: Option<CacheEntry<T>>,
    /// Counts the refusals so far, so a refresh can tell whether one came
    /// after it began.
    epoch: u64,
    /// The last value the registry refused.
    refused: Option<Refused<T>>,
}

struct CacheEntry<T> {
    value: T,
    /// When the value expires, or `None` when its lifetime runs past what
    /// the clock can hold.
    expires_at: Option<Instant>,
}

struct Refused<T> {
    is_refused: Box<dyn Fn(&T) -> bool + Send + Sync>,
    /// The epoch at which the refusal was made.
    at: u64,
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
            state: RwLock::new(State {
                entry: None,
                epoch: 0,
                refused: None,
            }),
            refresh_window: window,
            flights: Flights::default(),
        }
    }

    /// Run `refresh` for the callers that need a value, unless one is in
    /// flight: `share` picks what its waiters get from its result. The
    /// result comes with the epoch at which the refresh began.
    async fn refresh<F>(
        &self,
        refresh: impl Future<Output = Result<(T, Duration), F>>,
        share: impl FnOnce(&Result<(T, Duration), F>) -> Option<Result<T, E>>,
    ) -> Landed<(u64, Result<(T, Duration), F>), Result<T, E>> {
        let land = |(began, result): &(u64, Result<(T, Duration), F>)| {
            if let Ok((value, ttl)) = result {
                self.store(value, *ttl, *began);
            }
            share(result)
        };
        let fetch = || {
            let began = self.read().epoch;
            async move { (began, refresh.await) }
        };
        self.flights
            .run(&(), || self.fresh().map(Ok), fetch, land)
            .await
    }

    /// Keep a value from a refresh that began at epoch `began`, unless the
    /// registry has refused it since.
    fn store(&self, value: &T, ttl: Duration, began: u64) {
        let mut state = self.write();
        let refused = state
            .refused
            .as_ref()
            .is_some_and(|refused| refused.at > began && (refused.is_refused)(value));
        if !refused {
            state.entry = Some(CacheEntry::new(value.clone(), ttl));
        }
    }

    /// The value, if fresh.
    fn fresh(&self) -> Option<T> {
        self.read()
            .entry
            .as_ref()
            .filter(|entry| entry.is_fresh(self.refresh_window))
            .map(|entry| entry.value.clone())
    }

    fn read(&self) -> RwLockReadGuard<'_, State<T>> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State<T>> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
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
        match self.refresh(refresh, share).await {
            Landed::Own((_, result)) => result.map(|(value, _)| value),
            Landed::Shared(result) => result,
        }
    }

    /// Clear the cached value. A caller that asks afterwards joins the
    /// refresh in flight, if any: it fetches a new value rather than the
    /// refused one, so the refusals that arrive while it runs cost one
    /// refresh.
    pub(crate) fn clear(&self) {
        self.write().entry = None;
    }

    /// The registry refused a value for which `is_refused` is true. Drop
    /// the cached value if it is that one, and remember `is_refused`, so a
    /// refresh that began before this call and lands that value is not
    /// stored. A refresh that begins afterwards stores it: the source is
    /// issuing it again.
    pub(crate) fn refuse_if(&self, is_refused: impl Fn(&T) -> bool + Send + Sync + 'static) {
        let mut state = self.write();
        if state
            .entry
            .as_ref()
            .is_some_and(|entry| is_refused(&entry.value))
        {
            state.entry = None;
        }
        state.epoch += 1;
        state.refused = Some(Refused {
            is_refused: Box::new(is_refused),
            at: state.epoch,
        });
    }

    /// Replace the cached value (test helper).
    #[cfg(test)]
    pub(crate) fn set(&self, value: T, ttl: Duration) {
        self.write().entry = Some(CacheEntry::new(value, ttl));
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
        match self.refresh(refresh, share).await {
            Landed::Own((_, result)) => result.map(|(value, _)| value),
            Landed::Shared(Ok(value)) => Ok(value),
            Landed::Shared(Err(never)) => match never {},
        }
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

    /// A refresh that yields twice, then lands `landed`.
    async fn slow_load(landed: u32) -> Result<(u32, Duration), Infallible> {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        Ok((landed, Duration::from_secs(1000)))
    }

    /// A refusal of a value that has been replaced leaves the replacement,
    /// and one of the kept value drops it. Breaks if a refusal drops
    /// whatever is kept.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_drops_only_the_value_it_names() {
        let cache: CredentialCache<_> = CredentialCache::new();
        cache.set(2, Duration::from_secs(1000));
        cache.refuse_if(|kept| *kept == 1);
        assert_eq!(cache.get_or_refresh(slow_load(3)).await, Ok(2));
        cache.refuse_if(|kept| *kept == 2);
        assert_eq!(cache.get_or_refresh(slow_load(3)).await, Ok(3));
    }

    /// A refresh in flight when its value is refused does not store it,
    /// though the caller that ran it gets it. Breaks if the refusal runs
    /// before the landing and the refused value is kept.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refresh_in_flight_does_not_land_the_refused_value() {
        let cache: CredentialCache<_> = CredentialCache::new();
        cache.set(1, Duration::from_secs(10));
        let (landed, ()) = tokio::join!(cache.get_or_refresh(slow_load(1)), async {
            tokio::task::yield_now().await;
            cache.refuse_if(|kept| *kept == 1);
        });
        assert_eq!(landed, Ok(1));
        assert_eq!(cache.get_or_refresh(slow_load(2)).await, Ok(2));
    }

    /// A refresh that begins after the refusal stores the value, as the
    /// source is issuing it again. Breaks if a refused value is never kept,
    /// so each use fetches it again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refresh_after_the_refusal_lands_the_same_value() {
        let cache: CredentialCache<_> = CredentialCache::new();
        cache.refuse_if(|kept| *kept == 1);
        assert_eq!(cache.get_or_refresh(slow_load(1)).await, Ok(1));
        assert_eq!(cache.get_or_refresh(slow_load(2)).await, Ok(1));
    }
}
