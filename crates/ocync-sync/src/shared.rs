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
use ocync_distribution::manifest::ManifestPull;
use ocync_distribution::spec::{ImageIndex, RegistryAuthority, RepositoryName};
use tokio::sync::Notify;

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

/// Result of [`SharedWork::claim`].
#[derive(Debug)]
pub(crate) enum Claim<V> {
    /// Caller does the work and must call [`SharedWork::finish`], even on
    /// failure: waiters are only woken by `finish`.
    Claimed,
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

impl<K: Eq + Hash, V: Clone> SharedWork<K, V> {
    /// Atomic check-and-claim on `key`.
    pub(crate) fn claim(&mut self, key: K) -> Claim<V> {
        match self.slots.entry(key) {
            Entry::Occupied(slot) => match slot.get() {
                Slot::InFlight(notify) => Claim::Wait(Rc::clone(notify)),
                Slot::Done(outcome) => Claim::Done(outcome.clone()),
            },
            Entry::Vacant(slot) => {
                slot.insert(Slot::InFlight(Rc::new(Notify::new())));
                Claim::Claimed
            }
        }
    }

    /// Record the outcome of claimed work and wake its waiters.
    pub(crate) fn finish(&mut self, key: K, outcome: Outcome<V>) {
        if let Some(Slot::InFlight(notify)) = self.slots.insert(key, Slot::Done(outcome)) {
            notify.notify_waiters();
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
        let claim = shared.borrow_mut().claim(key.clone());
        match claim {
            Claim::Done(outcome) => return outcome.map_err(crate::Error::Shared),
            Claim::Wait(notify) => notify.notified().await,
            Claim::Claimed => break,
        }
    }
    let outcome = work().await.map_err(Arc::new);
    shared.borrow_mut().finish(key, outcome.clone());
    outcome.map_err(crate::Error::Shared)
}

/// A source manifest by digest: (source registry, source repo, digest).
pub(crate) type SourceManifestKey = (RegistryAuthority, RepositoryName, Digest);

/// A manifest or blob by digest at one target repo: (target, repo, digest).
pub(crate) type TargetDigestKey = (String, RepositoryName, Digest);

/// A blob copied from one source repo to one target repo.
pub(crate) type BlobCopyKey = (RegistryAuthority, RepositoryName, TargetDigestKey);

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
    /// Source manifests pulled by digest: index children and artifacts.
    pub(crate) manifest_pulls: RefCell<SharedWork<SourceManifestKey, ManifestPull>>,
    /// Child manifests pushed by digest to a target repo.
    pub(crate) child_pushes: RefCell<SharedWork<TargetDigestKey, ()>>,
    /// Referrer discovery per source manifest.
    pub(crate) referrers: RefCell<SharedWork<SourceManifestKey, Referrers>>,
    /// Artifacts (manifest and blobs) copied by digest to a target repo.
    pub(crate) artifact_pushes: RefCell<SharedWork<TargetDigestKey, ()>>,
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

    #[test]
    fn claim_waits_while_in_flight_then_returns_the_outcome() {
        let mut work = SharedWork::<&str, u32>::default();
        assert!(matches!(work.claim("k"), Claim::Claimed));
        assert!(matches!(work.claim("k"), Claim::Wait(_)));
        assert!(matches!(work.claim("other"), Claim::Claimed));

        work.finish("k", Ok(7));
        assert!(matches!(work.claim("k"), Claim::Done(Ok(7))));
    }

    #[test]
    fn a_failure_is_returned_to_later_claims_not_retried() {
        let mut work = SharedWork::<&str, ()>::default();
        assert!(matches!(work.claim("k"), Claim::Claimed));
        work.finish("k", Err(Arc::new(failure())));
        assert!(matches!(work.claim("k"), Claim::Done(Err(_))));
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
