//! A credential cached until it nears expiry, shared by the cloud auth
//! providers and the AWS API client.

use std::convert::Infallible;
use std::future::Future;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::time::Instant;

use crate::auth::{EARLY_REFRESH_WINDOW, Refusal, is_fresh};
use crate::flight::{Flights, InFlight, Landed};

/// Time until `until`, or zero once it has passed.
pub(crate) fn remaining(until: SystemTime) -> Duration {
    until
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// Read a time sent as seconds since the UNIX epoch, whole or fractional.
/// One before the epoch has passed; one too far ahead to represent reads as
/// no time, so a default lifetime applies rather than the credential
/// failing.
pub(crate) fn epoch_seconds<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SystemTime>, D::Error> {
    let Some(secs) = Option::<f64>::deserialize(deserializer)? else {
        return Ok(None);
    };
    Ok(Duration::try_from_secs_f64(secs.max(0.0))
        .ok()
        .and_then(|since_epoch| SystemTime::UNIX_EPOCH.checked_add(since_epoch)))
}

/// Cached credential with TTL tracking.
///
/// A refresh runs once however many callers need it: the callers that
/// arrive while it is in flight wait for it. Time is tokio's, so a test with
/// paused time moves it.
///
/// A landing never stores a value refused after its refresh began (see
/// [`Self::refuse`]); the callers that waited on it still get its value.
/// No other value can be stored while a refresh is in flight, as the
/// callers that need one wait on it.
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

/// Says whether two values are the same credential, whatever else they
/// hold, such as an expiry.
type Same<T> = fn(&T, &T) -> bool;

struct State<T> {
    entry: Option<CacheEntry<T>>,
    /// The distinct values refused while the refresh in flight ran, with
    /// how each is compared. Cleared when a refresh starts and when it
    /// lands.
    refused: Vec<(T, Same<T>)>,
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
            state: RwLock::new(State {
                entry: None,
                refused: Vec::new(),
            }),
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
    ) -> Landed<Result<(T, Duration), F>, Result<T, E>> {
        let land = |result: &Result<(T, Duration), F>, _| {
            let mut state = self.write();
            let refused = std::mem::take(&mut state.refused);
            if let Ok((value, ttl)) = result {
                if !refused.iter().any(|(refused, same)| same(value, refused)) {
                    state.entry = Some(CacheEntry::new(value.clone(), *ttl));
                }
            }
            drop(state);
            share(result)
        };
        let begin = || self.write().refused.clear();
        self.flights
            .run(&(), || self.fresh().map(Ok), begin, || refresh, land)
            .await
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
            Landed::Own(result) => result.map(|(value, _)| value),
            Landed::Shared(result) => result,
        }
    }

    /// The source or registry refused `refused`, which `same` tells from
    /// other values. Drop the cached value if it is that one. If a refresh
    /// is in flight, remember the refusal until it lands, so it does not
    /// store that value; one value refused again is remembered once. A
    /// refresh that begins afterwards stores it: the source is issuing it
    /// again.
    pub(crate) fn refuse(&self, refused: T, same: Same<T>) -> Refusal {
        // Under the flight lock, as a landing is: no refresh starts or
        // lands between telling whether one is running and recording the
        // refusal against it.
        self.flights.inspect(&(), |in_flight| {
            let mut state = self.write();
            let refusal = if state
                .entry
                .as_ref()
                .is_some_and(|entry| same(&entry.value, &refused))
            {
                state.entry = None;
                Refusal::Current
            } else {
                Refusal::Stale
            };
            let known = state.refused.iter().any(|(known, _)| same(known, &refused));
            if in_flight == InFlight::Running && !known {
                state.refused.push((refused, same));
            }
            refusal
        })
    }

    /// Replace the cached value.
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
            Landed::Own(result) => result.map(|(value, _)| value),
            Landed::Shared(Ok(value)) => Ok(value),
            Landed::Shared(Err(never)) => match never {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds are read whole or fractional. A negative time is the epoch,
    /// so it has passed; one too large to represent, or null, is no time.
    #[test]
    fn epoch_seconds_reads_a_time() {
        for (sent, expected) in [
            ("1700000000", Some(1_700_000_000_000)),
            ("4102444800.5", Some(4_102_444_800_500)),
            ("-1", Some(0)),
            ("1e300", None),
            ("null", None),
        ] {
            let value: serde_json::Value = serde_json::from_str(sent).unwrap();
            let expected =
                expected.map(|millis| SystemTime::UNIX_EPOCH + Duration::from_millis(millis));
            assert_eq!(epoch_seconds(&value).unwrap(), expected, "{sent}");
        }
    }

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

    /// A caller that asks after a refusal joins the refresh in flight.
    /// Breaks if a burst of refusals starts one refresh per refusal.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_joins_the_refresh_in_flight() {
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
            cache.refuse(0, u32::eq);
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
        assert_eq!(cache.refuse(1, u32::eq), Refusal::Stale);
        assert_eq!(cache.get_or_refresh(slow_load(3)).await, Ok(2));
        assert_eq!(cache.refuse(2, u32::eq), Refusal::Current);
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
            cache.refuse(1, u32::eq);
        });
        assert_eq!(landed, Ok(1));
        assert_eq!(cache.get_or_refresh(slow_load(2)).await, Ok(2));
    }

    /// Every value refused while a refresh is in flight is remembered, not
    /// only the last. Breaks if a second refusal makes the cache forget the
    /// first, and the refresh lands the first value.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refresh_does_not_land_any_value_refused_while_it_ran() {
        let cache: CredentialCache<_> = CredentialCache::new();
        cache.set(1, Duration::from_secs(10));
        let (landed, ()) = tokio::join!(cache.get_or_refresh(slow_load(1)), async {
            tokio::task::yield_now().await;
            cache.refuse(1, u32::eq);
            cache.refuse(2, u32::eq);
        });
        assert_eq!(landed, Ok(1));
        assert_eq!(cache.get_or_refresh(slow_load(3)).await, Ok(3));
    }

    /// A refresh that begins after the refusal stores the value, as the
    /// source is issuing it again. Breaks if a refused value is never kept,
    /// so each use fetches it again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refresh_after_the_refusal_lands_the_same_value() {
        let cache: CredentialCache<_> = CredentialCache::new();
        cache.refuse(1, u32::eq);
        assert_eq!(cache.get_or_refresh(slow_load(1)).await, Ok(1));
        assert_eq!(cache.get_or_refresh(slow_load(2)).await, Ok(1));
    }

    /// A refusal records itself only while a refresh is in flight, so
    /// refusals made with none running, or after one was dropped, leave
    /// nothing behind. Breaks if the list grows without a refresh to land.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_with_no_refresh_in_flight_records_nothing() {
        let cache: CredentialCache<u32> = CredentialCache::new();
        cache.refuse(1, u32::eq);
        assert!(cache.read().refused.is_empty());

        // A refresh polled once, then dropped, leaves no flight behind.
        let mut stalled = Box::pin(
            cache.get_or_refresh(std::future::pending::<Result<(u32, Duration), Infallible>>()),
        );
        std::future::poll_fn(|cx| {
            assert!(stalled.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        cache.refuse(2, u32::eq);
        assert_eq!(cache.read().refused.len(), 1);
        drop(stalled);
        cache.refuse(3, u32::eq);
        assert_eq!(cache.read().refused.len(), 1);

        // The next refresh starts with none, and lands with none.
        assert_eq!(cache.get_or_refresh(slow_load(4)).await, Ok(4));
        assert!(cache.read().refused.is_empty());
    }

    /// A value refused again while a refresh is in flight is recorded
    /// once, so the list holds the distinct values refused, not the
    /// refusals. Breaks if each refusal adds an entry.
    #[tokio::test(flavor = "current_thread")]
    async fn many_refusals_of_one_value_record_one_entry() {
        let cache: CredentialCache<u32> = CredentialCache::new();
        cache.set(1, Duration::from_secs(10));
        let (landed, ()) = tokio::join!(cache.get_or_refresh(slow_load(1)), async {
            tokio::task::yield_now().await;
            for _ in 0..5 {
                cache.refuse(1, u32::eq);
            }
            assert_eq!(cache.read().refused.len(), 1);
            cache.refuse(2, u32::eq);
            assert_eq!(cache.read().refused.len(), 2);
        });
        assert_eq!(landed, Ok(1));
    }
}
