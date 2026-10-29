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

/// Whether a fetch is in flight for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InFlight {
    /// A fetch has begun and not yet landed or been dropped.
    Running,
    /// No fetch is in flight.
    Idle,
}

/// Whether a fetch is still the one under its key when it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// No [`Flights::forget`] has taken its key since it began.
    Attached,
    /// A [`Flights::forget`] took its key, so callers that asked since have
    /// not joined it and it may be using what the forget dropped.
    Detached,
}

/// A fetch this caller leads. Its entry is the one under `key` only until
/// the fetch lands or is forgotten. Dropping it removes that entry before
/// the sender goes, so a waiter woken by the sender closing never finds it
/// again, and leaves alone an entry a later fetch put there.
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

    /// Run `inspect`, given whether a fetch for `key` is in flight, under
    /// the lock fetches are started and landed under: no fetch starts or
    /// lands until it returns, so what it records is ordered against both.
    pub(crate) fn inspect<T>(&self, key: &K, inspect: impl FnOnce(InFlight) -> T) -> T {
        let in_flight = self.lock();
        inspect(if in_flight.contains_key(key) {
            InFlight::Running
        } else {
            InFlight::Idle
        })
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
    /// `begin` runs under the lock `stored` is read under, as the fetch is
    /// registered, so whatever it sets up is in place before any caller can
    /// see the fetch in flight. It must not panic. `fetch` runs after the
    /// lock is released, to make the future this caller awaits. `land` is
    /// given the result, and the fetch's [`Standing`], under the lock
    /// `stored` is read under. It stores the value, if it is to be kept,
    /// and returns what the waiters get: `None` sends each of them to fetch
    /// for itself, as a fetch whose caller is dropped does.
    pub(crate) async fn run<R, Fut>(
        &self,
        key: &K,
        stored: impl Fn() -> Option<V>,
        begin: impl FnOnce(),
        fetch: impl FnOnce() -> Fut,
        land: impl FnOnce(&R, Standing) -> Option<V>,
    ) -> Landed<R, V>
    where
        Fut: Future<Output = R>,
    {
        let mut work = Some((begin, fetch, land));
        loop {
            let role = {
                let mut in_flight = self.lock();
                if let Some(value) = stored() {
                    return Landed::Shared(value);
                }
                match in_flight.get(key) {
                    Some(receiver) => Err(receiver.clone()),
                    None => {
                        let (begin, fetch, land) =
                            work.take().expect("a caller leads at most once");
                        begin();
                        let (sender, receiver) = watch::channel(None);
                        in_flight.insert(key.clone(), receiver.clone());
                        Ok((
                            Lead {
                                flights: self,
                                key,
                                sender,
                                receiver,
                            },
                            fetch,
                            land,
                        ))
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
                Ok((lead, fetch, land)) => {
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
        let run = || flights.run(&(), || None, || {}, fetch, |value: &u32, _| Some(*value));
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
        let unshared = || flights.run(&(), || None, || {}, fetch, |_: &u32, _| None);
        let (first, second) = tokio::join!(unshared(), unshared());
        assert!(matches!(first, Landed::Own(1)), "{first:?}");
        assert!(matches!(second, Landed::Own(2)), "{second:?}");

        let mut stalled = Box::pin(flights.run(
            &(),
            || None,
            || {},
            std::future::pending::<u32>,
            |_, _| Some(0),
        ));
        // Polled once, so it leads a fetch that never lands.
        std::future::poll_fn(|cx| {
            assert!(stalled.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let waiter = flights.run(
            &(),
            || None,
            || {},
            || async { 9 },
            |value: &u32, _| Some(*value),
        );
        let (landed, ()) = tokio::join!(waiter, async move {
            tokio::task::yield_now().await;
            drop(stalled);
        });
        assert!(matches!(landed, Landed::Own(9)), "{landed:?}");
    }

    /// A fetch is running from the moment its leader starts until it lands
    /// or its leader is dropped. Breaks if a landed or abandoned fetch still
    /// reads as running, so callers record state against a flight that is
    /// gone.
    #[tokio::test(flavor = "current_thread")]
    async fn a_fetch_is_running_until_it_lands_or_is_dropped() {
        let flights = Flights::<(), u32>::default();
        let running = || flights.inspect(&(), |in_flight| in_flight == InFlight::Running);
        assert!(!running());
        let fetch = || async {
            tokio::task::yield_now().await;
            1
        };
        let (landed, during) = tokio::join!(
            flights.run(&(), || None, || {}, fetch, |value: &u32, _| Some(*value)),
            async { running() },
        );
        assert!(matches!(landed, Landed::Own(1)), "{landed:?}");
        assert!(during);
        assert!(!running());

        let mut stalled = Box::pin(flights.run(
            &(),
            || None,
            || {},
            std::future::pending::<u32>,
            |_, _| Some(0),
        ));
        std::future::poll_fn(|cx| {
            assert!(stalled.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(running());
        drop(stalled);
        assert!(!running());
    }

    /// `inspect`, `begin` and `land` run holding the lock fetches start and
    /// land under, and `fetch` runs after it is released. Breaks if any of
    /// the first three runs outside the lock, as a fetch could then start
    /// or land between a check and the record made from it, or if `fetch`
    /// runs inside it, as a panic there would meet the lock again in
    /// `Lead::drop`.
    #[tokio::test(flavor = "current_thread")]
    async fn inspect_begin_and_land_run_under_the_lock() {
        let flights = Flights::<(), u32>::default();
        let held = || flights.in_flight.try_lock().is_err();
        assert!(flights.inspect(&(), |_| held()));

        let began_under_lock = Cell::new(false);
        let fetched_under_lock = Cell::new(true);
        let landed_under_lock = Cell::new(false);
        let begin = || began_under_lock.set(held());
        let fetch = || {
            fetched_under_lock.set(held());
            async { 1 }
        };
        let land = |value: &u32, _| {
            landed_under_lock.set(held());
            Some(*value)
        };
        flights.run(&(), || None, begin, fetch, land).await;
        assert!(began_under_lock.get());
        assert!(!fetched_under_lock.get());
        assert!(landed_under_lock.get());
    }

    /// A fetch that panics while making its future unwinds without
    /// deadlocking on the lock, and leaves no fetch in flight. Breaks if
    /// the lock is held when `fetch` runs, as `Lead::drop` then waits on it
    /// forever.
    #[test]
    fn a_fetch_that_panics_unwinds_without_deadlock() {
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let flights = Flights::<(), u32>::default();
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(flights.run(
                    &(),
                    || None,
                    || {},
                    || -> std::future::Ready<u32> { panic!("building the future") },
                    |value: &u32, _| Some(*value),
                ))
            }));
            let running = flights.inspect(&(), |in_flight| in_flight == InFlight::Running);
            let _ = done.send((panicked.is_err(), running));
        });
        let (panicked, running) = outcome
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the unwind deadlocked");
        assert!(panicked);
        assert!(!running);
    }

    /// A forget detaches the fetches in flight for the keys it drops: a
    /// caller that asks afterwards starts its own, the detached fetch lands
    /// as `Detached` and still serves the callers that joined it, and a
    /// fetch for another key is left attached. Breaks if a forget detaches
    /// nothing, or too much.
    #[tokio::test(flavor = "current_thread")]
    async fn a_forget_detaches_only_the_keys_it_drops() {
        let flights = Flights::<u8, u32>::default();
        let standings = std::cell::RefCell::new(Vec::new());
        let run = |key: u8, value: u32| {
            let flights = &flights;
            let standings = &standings;
            async move {
                flights
                    .run(
                        &key,
                        || None,
                        || {},
                        || async move {
                            tokio::task::yield_now().await;
                            tokio::task::yield_now().await;
                            value
                        },
                        |value: &u32, standing| {
                            standings.borrow_mut().push((key, standing));
                            Some(*value)
                        },
                    )
                    .await
            }
        };
        let forgetting = async {
            tokio::task::yield_now().await;
            assert_eq!(flights.forget(|| vec![1]), vec![1]);
            // Asks after the forget, so it starts its own fetch.
            run(1, 20).await
        };
        let (first, other, joined, later) =
            tokio::join!(run(1, 10), run(2, 30), run(1, 11), forgetting);
        assert!(matches!(first, Landed::Own(10)), "{first:?}");
        assert!(matches!(other, Landed::Own(30)), "{other:?}");
        assert!(matches!(joined, Landed::Shared(10)), "{joined:?}");
        assert!(matches!(later, Landed::Own(20)), "{later:?}");
        let standings = standings.into_inner();
        assert!(
            standings.contains(&(1, Standing::Detached)),
            "{standings:?}"
        );
        assert!(
            standings.contains(&(2, Standing::Attached)),
            "{standings:?}"
        );
    }
}
