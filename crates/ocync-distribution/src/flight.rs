//! One fetch in flight per key, its result handed to every caller that
//! waited on it. Nothing outlives the fetch: once it lands its entry is
//! gone, so a caller that asks afterwards finds the stored value or starts
//! a fetch of its own.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};

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

/// A fetch this caller leads. Dropping it, when the fetch lands or its
/// caller is dropped mid-fetch, removes the entry before the sender goes,
/// so a waiter woken by the sender closing never finds the entry again.
struct Lead<'a, K: Eq + Hash, V> {
    in_flight: &'a Mutex<HashMap<K, watch::Receiver<Option<V>>>>,
    key: &'a K,
    sender: watch::Sender<Option<V>>,
}

impl<K: Eq + Hash, V> Drop for Lead<'_, K, V> {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(self.key);
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Flights<K, V> {
    /// The value `stored` finds, else what the fetch in flight for `key`
    /// shares, else the result of `fetch`, run by this caller.
    ///
    /// `stored` is read under the lock a fetch lands under, and `fetch`
    /// stores its value before it returns, so no caller misses both a
    /// fetch that landed and the value it stored. `share` picks what the
    /// waiters get from the result: `None` sends each of them to fetch for
    /// itself, as a fetch whose caller is dropped does.
    pub(crate) async fn run<R, Fut>(
        &self,
        key: K,
        stored: impl Fn() -> Option<V>,
        fetch: impl FnOnce() -> Fut,
        share: impl FnOnce(&R) -> Option<V>,
    ) -> Landed<R, V>
    where
        Fut: Future<Output = R>,
    {
        let mut work = Some((fetch, share));
        loop {
            let role = {
                let mut in_flight = self
                    .in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(value) = stored() {
                    return Landed::Shared(value);
                }
                match in_flight.get(&key) {
                    Some(receiver) => Err(receiver.clone()),
                    None => {
                        let (sender, receiver) = watch::channel(None);
                        in_flight.insert(key.clone(), receiver);
                        Ok(sender)
                    }
                }
            };
            match role {
                Err(mut receiver) => {
                    if let Ok(value) = receiver.wait_for(Option::is_some).await
                        && let Some(value) = &*value
                    {
                        return Landed::Shared(value.clone());
                    }
                }
                Ok(sender) => {
                    let lead = Lead {
                        in_flight: &self.in_flight,
                        key: &key,
                        sender,
                    };
                    let (fetch, share) = work.take().expect("a caller leads at most once");
                    let result = fetch().await;
                    if let Some(value) = share(&result) {
                        lead.sender.send_replace(Some(value));
                    }
                    drop(lead);
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
        let run = || flights.run((), || None, fetch, |value: &u32| Some(*value));
        let (first, second) = tokio::join!(run(), run());
        assert!(matches!(first, Landed::Own(7)), "{first:?}");
        assert!(matches!(second, Landed::Shared(7)), "{second:?}");
        assert_eq!(fetches.get(), 1);
    }

    /// A result shared as `None`, or a fetch whose caller is dropped, sends
    /// each waiter to fetch for itself. Breaks if a waiter is left waiting,
    /// or gets a result that was not shared with it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_waiter_fetches_when_nothing_is_shared() {
        let flights = Flights::<(), u32>::default();
        let fetches = Cell::new(0);
        let fetch = || async {
            fetches.set(fetches.get() + 1);
            tokio::task::yield_now().await;
            fetches.get()
        };
        let unshared = || flights.run((), || None, fetch, |_: &u32| None);
        let (first, second) = tokio::join!(unshared(), unshared());
        assert!(matches!(first, Landed::Own(1)), "{first:?}");
        assert!(matches!(second, Landed::Own(2)), "{second:?}");

        let mut stalled =
            Box::pin(flights.run((), || None, std::future::pending::<u32>, |_| Some(0)));
        // Polled once, so it leads a fetch that never lands.
        std::future::poll_fn(|cx| {
            assert!(stalled.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let waiter = flights.run((), || None, || async { 9 }, |value: &u32| Some(*value));
        let (landed, ()) = tokio::join!(waiter, async move {
            tokio::task::yield_now().await;
            drop(stalled);
        });
        assert!(matches!(landed, Landed::Own(9)), "{landed:?}");
    }
}
