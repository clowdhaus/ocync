//! One fetch in flight per key, its result handed to every caller that
//! waited on it. Nothing outlives the fetch: once it lands its entry is
//! gone, so a caller that asks afterwards finds the stored value or starts
//! a fetch of its own.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

/// The fetches in flight, by key.
#[derive(Debug)]
pub(crate) struct Flights<K, V> {
    in_flight: Mutex<HashMap<K, watch::Receiver<Option<V>>>>,
}

impl<K, V> Default for Flights<K, V> {
    fn default() -> Self {
        Self {
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

/// What answered a [`Flights::run`] call.
#[derive(Debug)]
pub(crate) enum Landed<R, V> {
    /// This caller ran the fetch, and has its whole result.
    Own(R),
    /// A stored value, or what another caller's fetch shared.
    Shared(V),
}

/// A fetch this caller leads. Its entry is the one under `key` only until
/// the fetch lands or is forgotten. Dropping it removes that entry before
/// the sender goes, so a waiter woken by the sender closing never finds
/// it again.
struct Lead<'a, K: Eq + Hash, V> {
    flights: &'a Flights<K, V>,
    key: &'a K,
    sender: watch::Sender<Option<V>>,
    receiver: watch::Receiver<Option<V>>,
}

impl<K: Eq + Hash, V> Lead<'_, K, V> {
    /// Remove this fetch's entry, if it is still the one under its key.
    fn detach(&self, in_flight: &mut HashMap<K, watch::Receiver<Option<V>>>) -> bool {
        let owned = in_flight
            .get(self.key)
            .is_some_and(|receiver| receiver.same_channel(&self.receiver));
        if owned {
            in_flight.remove(self.key);
        }
        owned
    }
}

impl<K: Eq + Hash, V> Drop for Lead<'_, K, V> {
    fn drop(&mut self) {
        self.detach(&mut self.flights.lock());
    }
}

impl<K, V> Flights<K, V> {
    fn lock(&self) -> MutexGuard<'_, HashMap<K, watch::Receiver<Option<V>>>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `drop_stored`, which drops stored values, and if it says it
    /// dropped any, cut off the fetches in flight: a caller that asks
    /// afterwards starts a fetch of its own, and a fetch begun before
    /// lands without storing or sharing its result.
    pub(crate) fn forget(&self, drop_stored: impl FnOnce() -> bool) {
        let mut in_flight = self.lock();
        if drop_stored() {
            in_flight.clear();
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Flights<K, V> {
    /// The value `stored` finds, else what the fetch in flight for `key`
    /// shares, else the result of `fetch`, run by this caller.
    ///
    /// `land` is given the result under the lock `stored` is read under,
    /// and only if nothing was forgotten since the fetch began. It stores
    /// the value and returns what the waiters get: `None` sends each of
    /// them to fetch for itself, as a fetch whose caller is dropped does.
    pub(crate) async fn run<R, Fut>(
        &self,
        key: &K,
        stored: impl Fn() -> Option<V>,
        fetch: impl FnOnce() -> Fut,
        land: impl FnOnce(&R) -> Option<V>,
    ) -> Landed<R, V>
    where
        Fut: Future<Output = R>,
    {
        let mut work = Some((fetch, land));
        loop {
            let role = {
                let mut in_flight = self.lock();
                if let Some(value) = stored() {
                    return Landed::Shared(value);
                }
                match in_flight.get(key) {
                    Some(receiver) => Err(receiver.clone()),
                    None => {
                        let (sender, receiver) = watch::channel(None);
                        in_flight.insert(key.clone(), receiver.clone());
                        Ok(Lead {
                            flights: self,
                            key,
                            sender,
                            receiver,
                        })
                    }
                }
            };
            match role {
                Err(mut receiver) => {
                    // Only a value satisfies the wait, so the clone is Some.
                    if let Ok(value) = receiver.wait_for(Option::is_some).await {
                        return Landed::Shared(value.clone().expect("waited for a value"));
                    }
                }
                Ok(lead) => {
                    let (fetch, land) = work.take().expect("a caller leads at most once");
                    let result = fetch().await;
                    // Landed and removed under one lock, so no caller joins
                    // a fetch that has landed.
                    let mut in_flight = self.lock();
                    if lead.detach(&mut in_flight)
                        && let Some(value) = land(&result)
                    {
                        lead.sender.send_replace(Some(value));
                    }
                    drop(in_flight);
                    return Landed::Own(result);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Callers asking while a fetch is in flight get what it shares, and
    /// only its own caller runs it. Breaks if a waiter fetches again.
    #[tokio::test(flavor = "current_thread")]
    async fn waiters_get_the_shared_result() {
        let flights = Flights::<(), u32>::default();
        let fetches = Cell::new(0);
        let fetch = || async {
            fetches.set(fetches.get() + 1);
            tokio::task::yield_now().await;
            7
        };
        let run = || flights.run(&(), || None, fetch, |value: &u32| Some(*value));
        let (first, second) = tokio::join!(run(), run());
        assert!(matches!(first, Landed::Own(7)), "{first:?}");
        assert!(matches!(second, Landed::Shared(7)), "{second:?}");
        assert_eq!(fetches.get(), 1);
    }

    /// A result shared with no one, or a fetch whose caller is dropped,
    /// sends each waiter to fetch for itself. Breaks if a waiter is left
    /// waiting, or gets a result that was not shared with it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_waiter_fetches_when_nothing_is_shared() {
        let flights = Flights::<(), u32>::default();
        let fetches = Cell::new(0);
        let fetch = || async {
            fetches.set(fetches.get() + 1);
            tokio::task::yield_now().await;
            fetches.get()
        };
        let unshared = || flights.run(&(), || None, fetch, |_: &u32| None);
        let (first, second) = tokio::join!(unshared(), unshared());
        assert!(matches!(first, Landed::Own(1)), "{first:?}");
        assert!(matches!(second, Landed::Own(2)), "{second:?}");

        let mut stalled =
            Box::pin(flights.run(&(), || None, std::future::pending::<u32>, |_| Some(0)));
        // Polled once, so it leads a fetch that never lands.
        std::future::poll_fn(|cx| {
            assert!(stalled.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let waiter = flights.run(&(), || None, || async { 9 }, |value: &u32| Some(*value));
        let (landed, ()) = tokio::join!(waiter, async move {
            tokio::task::yield_now().await;
            drop(stalled);
        });
        assert!(matches!(landed, Landed::Own(9)), "{landed:?}");
    }

    /// A fetch begun before a forget neither lands its value nor is joined
    /// by a caller asking after it. Breaks if an invalidated value is
    /// stored, or a later caller gets a result from before the forget.
    #[tokio::test(flavor = "current_thread")]
    async fn a_forget_cuts_off_the_fetch_in_flight() {
        let flights = Flights::<(), u32>::default();
        let landed = Cell::new(Vec::new());
        let land = |value: &u32| {
            let mut seen = landed.take();
            seen.push(*value);
            landed.set(seen);
            Some(*value)
        };
        let slow = || async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            1
        };
        let (before, after) = tokio::join!(flights.run(&(), || None, slow, land), async {
            tokio::task::yield_now().await;
            flights.forget(|| true);
            flights
                .run(&(), || None, || async { 2 }, |value: &u32| Some(*value))
                .await
        });
        assert!(matches!(before, Landed::Own(1)), "{before:?}");
        assert!(matches!(after, Landed::Own(2)), "{after:?}");
        assert_eq!(
            landed.take(),
            Vec::<u32>::new(),
            "a forgotten fetch lands nothing"
        );
    }
}
