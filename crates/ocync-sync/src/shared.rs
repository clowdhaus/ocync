//! Per-run shared work: the first task to claim a key does the work, and every
//! other task with the same key waits for and reuses its outcome.
//!
//! Tags that resolve to one index, or images that share a signature, would
//! otherwise each pull and push the same content. Every record here lives for
//! one run, so a failure or a target-side deletion never carries into the next.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::hash::Hash;
use std::rc::Rc;
use std::sync::Arc;

use ocync_distribution::Digest;
use ocync_distribution::RegistryClient;
use ocync_distribution::manifest::ManifestPull;
use ocync_distribution::spec::{ImageIndex, RepositoryName};
use tokio::sync::Notify;

use crate::engine::RegistryAlias;

/// Outcome of a piece of shared work. The error is shared so every task that
/// needed the work reports the same failure.
type Outcome<V> = Result<V, Arc<crate::Error>>;

/// State of one key.
#[derive(Debug)]
enum Slot<V> {
    /// A task is doing the work; `Notify` fires when it finishes.
    InFlight(Rc<Notify>),
    /// The work finished in this run.
    Done(Outcome<V>),
}

/// Result of [`claim`].
pub(crate) enum Claim<'a, K: Eq + Hash, V> {
    /// Caller does the work and reports it through the guard.
    Claimed(ClaimGuard<'a, K, V>),
    /// Another task is doing it; await `notify`, then claim again.
    Wait(Rc<Notify>),
    /// Already done in this run.
    Done(Outcome<V>),
}

/// Per-run record of work done once per key.
#[derive(Debug)]
pub(crate) struct SharedWork<K, V> {
    slots: HashMap<K, Slot<V>>,
}

impl<K, V> Default for SharedWork<K, V> {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }
}

/// A claim on one key. [`ClaimGuard::finish`] records the outcome; dropping
/// the guard unfinished (its task cancelled, or a sibling's error dropping
/// it) frees the key and wakes the waiters, so one of them does the work.
pub(crate) struct ClaimGuard<'a, K: Eq + Hash, V> {
    shared: &'a RefCell<SharedWork<K, V>>,
    key: Option<K>,
}

impl<K: Eq + Hash, V> ClaimGuard<'_, K, V> {
    /// Record the outcome of the claimed work and wake its waiters.
    pub(crate) fn finish(mut self, outcome: Outcome<V>) {
        if let Some(key) = self.key.take()
            && let Some(Slot::InFlight(notify)) = self
                .shared
                .borrow_mut()
                .slots
                .insert(key, Slot::Done(outcome))
        {
            notify.notify_waiters();
        }
    }
}

impl<K: Eq + Hash, V> Drop for ClaimGuard<'_, K, V> {
    fn drop(&mut self) {
        // No borrow is live when a future is dropped on this single-threaded
        // runtime. If one were, the slot would stay claimed.
        if let Some(key) = self.key.take()
            && let Ok(mut work) = self.shared.try_borrow_mut()
            && let Some(Slot::InFlight(notify)) = work.slots.remove(&key)
        {
            notify.notify_waiters();
        }
    }
}

/// Atomic check-and-claim on `key`.
pub(crate) fn claim<K: Eq + Hash + Clone, V: Clone>(
    shared: &RefCell<SharedWork<K, V>>,
    key: K,
) -> Claim<'_, K, V> {
    let mut work = shared.borrow_mut();
    match work.slots.entry(key.clone()) {
        Entry::Occupied(slot) => match slot.get() {
            Slot::InFlight(notify) => Claim::Wait(Rc::clone(notify)),
            Slot::Done(outcome) => Claim::Done(outcome.clone()),
        },
        Entry::Vacant(slot) => {
            slot.insert(Slot::InFlight(Rc::new(Notify::new())));
            Claim::Claimed(ClaimGuard {
                shared,
                key: Some(key),
            })
        }
    }
}

/// Do `work` for `key` unless another task in this run already has, in which
/// case wait for and return its outcome. A failure comes back as
/// [`crate::Error::Shared`] to every task that needed the work.
pub(crate) async fn run_once<K, V, F, Fut>(
    shared: &RefCell<SharedWork<K, V>>,
    key: K,
    work: F,
) -> Result<V, crate::Error>
where
    K: Eq + Hash + Clone,
    V: Clone,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<V, crate::Error>>,
{
    loop {
        match claim(shared, key.clone()) {
            Claim::Done(outcome) => return outcome.map_err(crate::Error::Shared),
            Claim::Wait(notify) => notify.notified().await,
            Claim::Claimed(guard) => {
                let outcome = work().await.map_err(Arc::new);
                guard.finish(outcome.clone());
                return outcome.map_err(crate::Error::Shared);
            }
        }
    }
}

/// Identity of one configured source registry. Each registry entry in the
/// config gets its own client, so two entries for one host (anonymous and
/// logged in, say) are different sources and never share an outcome. The
/// address is stable for the run: every mapping holds its client throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ClientId(usize);

impl ClientId {
    /// The identity of `client`.
    pub(crate) fn of(client: &RegistryClient) -> Self {
        Self(std::ptr::from_ref(client) as usize)
    }
}

/// A source manifest by digest: (source client, source repo, digest).
pub(crate) type SourceManifestKey = (ClientId, RepositoryName, Digest);

/// A manifest or blob by digest at one target repo: (target, repo, digest).
pub(crate) type TargetDigestKey = (RegistryAlias, RepositoryName, Digest);

/// A blob copied from one source repo to one target repo.
pub(crate) type BlobCopyKey = (ClientId, RepositoryName, TargetDigestKey);

/// Key of a source manifest pulled by digest.
pub(crate) fn source_key(
    client: &RegistryClient,
    repo: &RepositoryName,
    digest: &Digest,
) -> SourceManifestKey {
    (ClientId::of(client), repo.clone(), digest.clone())
}

/// Key of a manifest or blob by digest at one target repo.
pub(crate) fn target_key(
    target: &RegistryAlias,
    repo: &RepositoryName,
    digest: &Digest,
) -> TargetDigestKey {
    (target.clone(), repo.clone(), digest.clone())
}

/// Referrer discovery result for one source manifest.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Referrers {
    /// Successfully discovered referrers (index may have zero entries).
    Found(ImageIndex),
    /// No referrers exist (API 404 + tag fallback 404).
    NotFound,
    /// Discovery failed due to a transient error.
    Failed,
}

/// All work shared between tasks in one run. Created fresh by every run.
#[derive(Debug, Default)]
pub(crate) struct RunShared {
    /// Index children pulled by digest during discovery.
    pub(crate) child_pulls: RefCell<SharedWork<SourceManifestKey, Rc<ManifestPull>>>,
    /// Artifact manifests pulled by digest during execution. Kept apart from
    /// `child_pulls` so an execution task never waits on discovery.
    pub(crate) artifact_pulls: RefCell<SharedWork<SourceManifestKey, Rc<ManifestPull>>>,
    /// Child manifests pushed by digest to a target repo.
    pub(crate) child_pushes: RefCell<SharedWork<TargetDigestKey, ()>>,
    /// Referrer discovery per source manifest.
    pub(crate) referrers: RefCell<SharedWork<SourceManifestKey, Referrers>>,
    /// Artifacts (manifest and blobs) copied by digest to a target repo.
    pub(crate) artifact_pushes: RefCell<SharedWork<TargetDigestKey, ()>>,
    /// Artifact blobs copied by digest to a target repo.
    pub(crate) artifact_blobs: RefCell<SharedWork<TargetDigestKey, ()>>,
    /// Blob copies that failed after their retries. A task copying the same
    /// blob between the same repos reports this error instead of repeating
    /// the copy.
    pub(crate) blob_failures: RefCell<HashMap<BlobCopyKey, Arc<crate::Error>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> crate::Error {
        crate::Error::LatestWithoutSort
    }

    /// Two config entries for one host (anonymous and logged in, say) are
    /// different sources: one's failure must never reach the other.
    #[test]
    fn clients_for_one_host_are_different_sources() {
        let client = || {
            RegistryClient::builder("https://registry.example.com".parse().unwrap())
                .build()
                .unwrap()
        };
        let (anonymous, logged_in) = (client(), client());
        assert_ne!(ClientId::of(&anonymous), ClientId::of(&logged_in));
        assert_eq!(ClientId::of(&anonymous), ClientId::of(&anonymous));
    }

    #[test]
    fn claim_waits_while_in_flight_then_returns_the_outcome() {
        let work = RefCell::new(SharedWork::<&str, u32>::default());
        let Claim::Claimed(guard) = claim(&work, "k") else {
            panic!("first claim must claim")
        };
        assert!(matches!(claim(&work, "k"), Claim::Wait(_)));
        assert!(matches!(claim(&work, "other"), Claim::Claimed(_)));

        guard.finish(Ok(7));
        assert!(matches!(claim(&work, "k"), Claim::Done(Ok(7))));
    }

    #[test]
    fn a_failure_is_returned_to_later_claims_not_retried() {
        let work = RefCell::new(SharedWork::<&str, ()>::default());
        let Claim::Claimed(guard) = claim(&work, "k") else {
            panic!("first claim must claim")
        };
        guard.finish(Err(Arc::new(failure())));
        assert!(matches!(claim(&work, "k"), Claim::Done(Err(_))));
    }

    /// A claimant dropped before finishing frees the key, so a waiter claims
    /// it and does the work instead of waiting forever.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_claim_wakes_waiters_to_claim_it() {
        let work = RefCell::new(SharedWork::<&str, u32>::default());
        let Claim::Claimed(guard) = claim(&work, "k") else {
            panic!("first claim must claim")
        };
        let Claim::Wait(notify) = claim(&work, "k") else {
            panic!("second claim must wait")
        };
        let woken = notify.notified();
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(1), woken)
            .await
            .expect("dropping the claim must wake its waiters");
        assert!(matches!(claim(&work, "k"), Claim::Claimed(_)));
    }

    /// Two concurrent callers: the second waits on the first and gets its
    /// value without running its own work.
    #[tokio::test(flavor = "current_thread")]
    async fn run_once_runs_work_once_for_concurrent_callers() {
        let shared = RefCell::new(SharedWork::<&str, u32>::default());
        let runs = std::cell::Cell::new(0);
        let work = || async {
            runs.set(runs.get() + 1);
            tokio::task::yield_now().await;
            Ok(5)
        };
        let (a, b) = tokio::join!(run_once(&shared, "k", work), run_once(&shared, "k", work));
        assert_eq!((a.unwrap(), b.unwrap()), (5, 5));
        assert_eq!(runs.get(), 1, "the waiter must not run the work again");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_once_shares_a_failure_with_every_caller() {
        let shared = RefCell::new(SharedWork::<&str, u32>::default());
        let first = run_once(&shared, "k", || async { Err(failure()) }).await;
        let second = run_once(&shared, "k", || async { Ok(1) }).await;
        assert!(matches!(first, Err(crate::Error::Shared(_))));
        assert!(
            matches!(second, Err(crate::Error::Shared(_))),
            "a later caller must get the recorded failure, not run its work"
        );
    }
}
