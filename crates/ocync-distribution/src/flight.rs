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

/// Whether a fetch is still the one under its key when it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// No other fetch has taken its key since it began.
    Attached,
    /// A [`Flights::forget`] took its key, so callers that asked since have
    /// not joined it and it may be using what the forget dropped.
    Detached,
}

/// A fetch this caller leads. Its entry is the one under `key` only until
/// the fetch lands or is forgotten. Dropping it removes that entry before
/// the sender goes, so a waiter woken by the sender closing never finds
/// it again, and leaves alone an entry a later fetch put there.
struct Lead<'a, K: Eq + Hash, V> {
    flights: &'a Flights<K, V>,
    key: &'a K,
    sender: watch::Sender<Option<V>>,
    receiver: watch::Receiver<Option<V>>,
}

impl<K: Eq + Hash, V> Lead<'_, K, V> {
    /// Whether this fetch's entry is still the one under its key.
    fn standing(&self, in_flight: &HashMap<K, watch::Receiver<Option<V>>>) -> Standing {
        if in_flight
            .get(self.key)
            .is_some_and(|receiver| receiver.same_channel(&self.receiver))
        {
            Standing::Attached
        } else {
            Standing::Detached
        }
    }

    /// Remove this fetch's entry, if it is still the one under its key.
    fn detach(&self, in_flight: &mut HashMap<K, watch::Receiver<Option<V>>>) {
        if self.standing(in_flight) == Standing::Attached {
            in_flight.remove(self.key);
        }
    }
}

impl<K: Eq + Hash, V> Drop for Lead<'_, K, V> {
    fn drop(&mut self) {
        self.detach(&mut self.flights.lock());
    }
}

impl<K: Eq + Hash, V> Flights<K, V> {
    fn lock(&self) -> MutexGuard<'_, HashMap<K, watch::Receiver<Option<V>>>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `drop_stored`, which drops stored values and returns their
    /// keys, and detach the fetches in flight for those keys, under one
    /// lock: a caller that asks afterwards finds neither and starts a fetch
    /// of its own, while each detached fetch still lands, as
    /// [`Standing::Detached`], and serves the callers already waiting on
    /// it. Fetches for other keys are left alone. Returns the keys dropped.
    pub(crate) fn forget(&self, drop_stored: impl FnOnce() -> Vec<K>) -> Vec<K> {
        let mut in_flight = self.lock();
        let dropped = drop_stored();
        for key in &dropped {
            in_flight.remove(key);
        }
        dropped
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Flights<K, V> {
    /// The value `stored` finds, else what the fetch in flight for `key`
    /// shares, else the result of `fetch`, run by this caller.
    ///
    /// `land` is given the result, and the fetch's [`Standing`], under the
    /// lock `stored` is read under. It stores the value, if it is to be
    /// kept, and returns what the waiters get: `None` sends each of them to
    /// fetch for itself, as a fetch whose caller is dropped does.
    pub(crate) async fn run<R, Fut>(
        &self,
        key: &K,
        stored: impl Fn() -> Option<V>,
        fetch: impl FnOnce() -> Fut,
        land: impl FnOnce(&R, Standing) -> Option<V>,
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
                    let standing = lead.standing(&in_flight);
                    lead.detach(&mut in_flight);
                    if let Some(value) = land(&result, standing) {
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
        let run = || flights.run(&(), || None, fetch, |value: &u32, _| Some(*value));
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
        let unshared = || flights.run(&(), || None, fetch, |_: &u32, _| None);
        let (first, second) = tokio::join!(unshared(), unshared());
        assert!(matches!(first, Landed::Own(1)), "{first:?}");
        assert!(matches!(second, Landed::Own(2)), "{second:?}");

        let mut stalled =
            Box::pin(flights.run(&(), || None, std::future::pending::<u32>, |_, _| Some(0)));
        // Polled once, so it leads a fetch that never lands.
        std::future::poll_fn(|cx| {
            assert!(stalled.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let waiter = flights.run(&(), || None, || async { 9 }, |value: &u32, _| Some(*value));
        let (landed, ()) = tokio::join!(waiter, async move {
            tokio::task::yield_now().await;
            drop(stalled);
        });
        assert!(matches!(landed, Landed::Own(9)), "{landed:?}");
    }

    /// A forget that dropped something sends a later caller to fetch for
    /// itself, while the fetch it detached still lands and serves the caller
    /// waiting on it, and leaves the later fetch's entry alone. Breaks if a
    /// later caller joins the detached fetch, its waiter is dropped, or its
    /// landing removes the later entry and a fourth caller fetches again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_forget_detaches_the_fetch_in_flight() {
        let flights = Flights::<(), u32>::default();
        let share = |value: &u32, _| Some(*value);
        let slow = || async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            1
        };
        let yields = |count: u32, value: u32| async move {
            for _ in 0..count {
                tokio::task::yield_now().await;
            }
            value
        };
        let (leader, waiter, later, fourth) = tokio::join!(
            flights.run(&(), || None, slow, share),
            flights.run(&(), || None, || async { 0 }, share),
            async {
                tokio::task::yield_now().await;
                flights.forget(|| vec![()]);
                flights.run(&(), || None, || yields(4, 2), share).await
            },
            async {
                yields(3, 0).await;
                flights.run(&(), || None, || async { 3 }, share).await
            },
        );
        assert!(matches!(leader, Landed::Own(1)), "{leader:?}");
        assert!(matches!(waiter, Landed::Shared(1)), "{waiter:?}");
        assert!(matches!(later, Landed::Own(2)), "{later:?}");
        assert!(matches!(fourth, Landed::Shared(2)), "{fourth:?}");
    }

    /// A forget that dropped nothing leaves the fetch in flight to be
    /// joined. Breaks if it detaches anyway and the later caller fetches.
    #[tokio::test(flavor = "current_thread")]
    async fn a_forget_that_drops_nothing_leaves_the_fetch_alone() {
        let flights = Flights::<(), u32>::default();
        let share = |value: &u32, _| Some(*value);
        let slow = || async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            1
        };
        let (leader, later) = tokio::join!(flights.run(&(), || None, slow, share), async {
            tokio::task::yield_now().await;
            flights.forget(Vec::new);
            flights.run(&(), || None, || async { 2 }, share).await
        });
        assert!(matches!(leader, Landed::Own(1)), "{leader:?}");
        assert!(matches!(later, Landed::Shared(1)), "{later:?}");
    }

    /// A forget takes only the keys it names, and the fetch it detaches
    /// lands as detached while the others land attached. Breaks if a forget
    /// detaches another key's fetch, or a landing is not told it was
    /// detached.
    #[tokio::test(flavor = "current_thread")]
    async fn a_landing_is_told_whether_it_is_still_attached() {
        let flights = Flights::<&str, u32>::default();
        let standings = std::cell::RefCell::new(Vec::new());
        let (flights_ref, standings_ref) = (&flights, &standings);
        let run = |key: &'static str| async move {
            flights_ref
                .run(
                    &key,
                    || None,
                    || async {
                        tokio::task::yield_now().await;
                        tokio::task::yield_now().await;
                        1
                    },
                    |_: &u32, standing| {
                        standings_ref.borrow_mut().push((key, standing));
                        Some(1)
                    },
                )
                .await
        };
        let (_, _, dropped) = tokio::join!(run("a"), run("b"), async {
            tokio::task::yield_now().await;
            flights.forget(|| vec!["a"])
        });
        assert_eq!(dropped, ["a"]);
        let mut standings = standings.into_inner();
        standings.sort_by_key(|(key, _)| *key);
        assert_eq!(
            standings,
            [("a", Standing::Detached), ("b", Standing::Attached)]
        );
    }
}
