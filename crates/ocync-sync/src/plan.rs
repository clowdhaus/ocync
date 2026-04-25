//! Per-(repo) blob state and cross-repo upload-claim coordination for mount support.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use ocync_distribution::Digest;
use ocync_distribution::spec::RepositoryName;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// State of a blob at a single (target, repo) pair.
///
/// Each repo's lifecycle is independent of every other repo's; mutations
/// to one repo's state never disturb another repo's state. This is the
/// structural fix for the override-bug class where a HEAD-success or
/// batch-check at one repo could break another repo's in-flight upload claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoBlobState {
    /// Verified to exist at the target's repo via HEAD or batch check.
    /// Persistable.
    Verified,
    /// We hold the upload claim for this digest at this repo.
    /// Transient; stripped before persistence.
    Uploading,
    /// We successfully uploaded (or mounted) the blob to this repo.
    /// Persistable.
    Completed,
    /// Either upload to this repo failed, or this repo was selected as
    /// a mount source by another repo and the mount returned 202/4xx/5xx.
    /// Excluded from future mount-source selection regardless of how it
    /// was reached. Transient; stripped before persistence.
    Failed(String),
}

impl RepoBlobState {
    /// Whether this state means "blob is present at this repo for skip purposes".
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Verified | Self::Completed)
    }

    /// Whether this state means "blob is unusable at this repo for mount purposes".
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// Whether this state means "still active or resolvable in this run".
    /// Used by `unresolved_watches_for` to filter dead-end candidates.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Verified | Self::Uploading | Self::Completed)
    }
}

/// Outcome of an atomic check-and-claim on a blob's upload coordination slot.
#[derive(Debug)]
pub enum ClaimAction {
    /// Caller now holds the upload claim. Proceed with mount/HEAD/push;
    /// call `release_blob_claim` when done.
    Claimed,
    /// Another repo holds the claim; await `notify` and retry.
    /// `holder` is included for diagnostic logging.
    Wait {
        /// The repo currently holding the upload claim (for diagnostic logging).
        holder: RepositoryName,
        /// Wakes when the holder calls `release_blob_claim`.
        notify: Rc<Notify>,
    },
}

/// Metadata for a tracked blob at a single target.
///
/// Per-(repo) state lives in `repos`; each repo's lifecycle is independent.
/// The `claim` slot serializes uploads of this digest across repos at the
/// target side: at most one repo holds the claim at a time, and other
/// repos calling `claim_blob_upload` see this as `Some` and wait.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct BlobInfo {
    /// Per-(repo) state. `BTreeMap` for deterministic iteration.
    pub(crate) repos: BTreeMap<RepositoryName, RepoBlobState>,
    /// Cross-repo upload-claim holder. `Some(R)` means R is currently
    /// uploading (or in mount/HEAD/push for) this digest at the target.
    /// Stripped before persistence.
    #[serde(skip)]
    pub(crate) claim: Option<RepositoryName>,
    /// Lazily-created notify; fires when `claim` transitions back to None.
    /// Stripped before persistence.
    #[serde(skip)]
    pub(crate) claim_notify: Option<Rc<Notify>>,
}

/// Per-registry index of tracked blobs.
type BlobIndex = HashMap<Digest, BlobInfo>;

/// Process-global blob coordination map keyed by target registry, then digest.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct BlobDedupMap {
    inner: HashMap<String, BlobIndex>,
}

impl BlobDedupMap {
    /// Get or create the `BlobInfo` entry for the given target and digest.
    fn entry_mut(&mut self, target: &str, digest: &Digest) -> &mut BlobInfo {
        self.inner
            .entry(target.to_owned())
            .or_default()
            .entry(digest.clone())
            .or_default()
    }

    /// Get the per-(repo) state for a blob at a target, if recorded.
    pub(crate) fn repo_state(
        &self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) -> Option<&RepoBlobState> {
        self.inner.get(target)?.get(digest)?.repos.get(repo)
    }

    /// Whether the blob is present (Verified or Completed) at the given (target, repo).
    pub(crate) fn blob_present_at(
        &self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) -> bool {
        self.repo_state(target, digest, repo)
            .is_some_and(RepoBlobState::is_present)
    }

    /// Set a repo's state to `Verified`. Does not disturb other repos' state
    /// or the cross-repo claim.
    pub(crate) fn set_blob_verified(
        &mut self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) {
        let entry = self.entry_mut(target, digest);
        entry.repos.insert(repo.clone(), RepoBlobState::Verified);
    }

    /// Set a repo's state to `Completed`.
    pub(crate) fn set_blob_completed(
        &mut self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) {
        let entry = self.entry_mut(target, digest);
        entry.repos.insert(repo.clone(), RepoBlobState::Completed);
    }

    /// Set a repo's state to `Failed` (this repo's own upload failed).
    pub(crate) fn set_blob_failed(
        &mut self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
        error: String,
    ) {
        let entry = self.entry_mut(target, digest);
        entry
            .repos
            .insert(repo.clone(), RepoBlobState::Failed(error));
    }

    /// Mark a repo as a stale mount source (mount from this repo was rejected).
    /// Sets the repo's state to `Failed` so it is excluded from future
    /// `committed_mount_sources` and `unresolved_watches_for` results.
    pub(crate) fn mark_blob_repo_stale(
        &mut self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) {
        let entry = self.entry_mut(target, digest);
        entry.repos.insert(
            repo.clone(),
            RepoBlobState::Failed("mount rejected by target".into()),
        );
    }

    /// Atomic check-and-claim on the cross-repo upload coordination slot.
    ///
    /// Returns `ClaimAction::Claimed` if `current_repo` now holds the claim
    /// (state for `current_repo` is set to `Uploading`), or `ClaimAction::Wait`
    /// with the existing holder's notify handle.
    pub(crate) fn claim_blob_upload(
        &mut self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> ClaimAction {
        let entry = self.entry_mut(target, digest);
        match &entry.claim {
            Some(holder) if holder != current_repo => {
                let notify = entry
                    .claim_notify
                    .get_or_insert_with(|| Rc::new(Notify::new()))
                    .clone();
                ClaimAction::Wait {
                    holder: holder.clone(),
                    notify,
                }
            }
            _ => {
                entry.claim = Some(current_repo.clone());
                entry
                    .repos
                    .insert(current_repo.clone(), RepoBlobState::Uploading);
                ClaimAction::Claimed
            }
        }
    }

    /// Release the cross-repo upload claim, firing `claim_notify` to wake
    /// any waiting repos. Called by the holder on terminal outcome.
    pub(crate) fn release_blob_claim(&mut self, target: &str, digest: &Digest) {
        if let Some(index) = self.inner.get_mut(target) {
            if let Some(info) = index.get_mut(digest) {
                info.claim = None;
                if let Some(n) = info.claim_notify.as_ref() {
                    n.notify_waiters();
                }
            }
        }
    }

    /// Return the current claim holder, if any, excluding `current_repo`.
    pub(crate) fn claim_holder<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> Option<&'a RepositoryName> {
        let info = self.inner.get(target)?.get(digest)?;
        info.claim.as_ref().filter(|h| *h != current_repo)
    }

    /// Iterate `(repo, state)` pairs for a blob at a target.
    /// Returns an empty iterator if the entry doesn't exist.
    pub(crate) fn repos_iter<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
    ) -> Box<dyn Iterator<Item = (&'a RepositoryName, &'a RepoBlobState)> + 'a> {
        match self.inner.get(target).and_then(|m| m.get(digest)) {
            Some(info) => Box::new(info.repos.iter()),
            None => Box::new(std::iter::empty()),
        }
    }

    /// Remove the entry for the given blob at the target.
    /// Used for lazy invalidation when a complete cache entry is stale.
    pub(crate) fn invalidate(&mut self, target: &str, digest: &Digest) {
        if let Some(index) = self.inner.get_mut(target) {
            index.remove(digest);
        }
    }

    /// Return a copy with only persistable per-(repo) state entries.
    /// `Verified` and `Completed` survive; `Uploading` and `Failed` are stripped.
    /// The `claim` slot is also stripped.
    pub(crate) fn filter_persistable(&self) -> BlobDedupMap {
        let inner = self
            .inner
            .iter()
            .filter_map(|(target, index)| {
                let filtered: HashMap<Digest, BlobInfo> = index
                    .iter()
                    .filter_map(|(digest, info)| {
                        let kept_repos: BTreeMap<RepositoryName, RepoBlobState> = info
                            .repos
                            .iter()
                            .filter(|(_, state)| {
                                matches!(state, RepoBlobState::Verified | RepoBlobState::Completed)
                            })
                            .map(|(r, s)| (r.clone(), s.clone()))
                            .collect();
                        if kept_repos.is_empty() {
                            None
                        } else {
                            Some((
                                digest.clone(),
                                BlobInfo {
                                    repos: kept_repos,
                                    claim: None,
                                    claim_notify: None,
                                },
                            ))
                        }
                    })
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some((target.clone(), filtered))
                }
            })
            .collect();
        BlobDedupMap { inner }
    }

    /// Returns `true` if the map contains no entries.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.values().all(|index| index.is_empty())
    }

    /// Drop entries for targets not in `live_targets`.
    /// Returns the number of individual blob entries removed.
    pub(crate) fn retain_targets(&mut self, live_targets: &HashSet<String>) -> usize {
        let mut removed = 0usize;
        self.inner.retain(|target, index| {
            if live_targets.contains(target) {
                true
            } else {
                removed += index.len();
                false
            }
        });
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_digest() -> Digest {
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            .parse()
            .unwrap()
    }

    fn other_digest() -> Digest {
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .parse()
            .unwrap()
    }

    fn repo(name: &str) -> RepositoryName {
        RepositoryName::new(name).unwrap()
    }

    /// Per-(repo) state independence: writing Y's state must not disturb X's
    /// Uploading state, must not release the claim, must not fire `claim_notify`.
    #[test]
    fn set_verified_at_one_repo_does_not_disturb_uploading_at_another() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        // X claims and is Uploading.
        match map.claim_blob_upload("reg.io", &d, &repo("x")) {
            ClaimAction::Claimed => {}
            ClaimAction::Wait { .. } => panic!("first claim should succeed"),
        }
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Uploading),
        );

        // Y's batch check finds D at Y -> Verified.
        map.set_blob_verified("reg.io", &d, &repo("y"));

        // X is unchanged.
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Uploading),
        );
        // Y is Verified.
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("y")),
            Some(&RepoBlobState::Verified),
        );
        // Claim is still X.
        assert_eq!(map.claim_holder("reg.io", &d, &repo("z")), Some(&repo("x")),);
    }

    /// `set_blob_failed` at one repo must not release another repo's claim.
    #[test]
    fn set_failed_at_one_repo_does_not_release_other_repos_claim() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        // X holds the claim.
        match map.claim_blob_upload("reg.io", &d, &repo("x")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        // Y attempts upload, fails.
        map.set_blob_failed("reg.io", &d, &repo("y"), "io error".into());

        // Claim is still X.
        assert_eq!(map.claim_holder("reg.io", &d, &repo("z")), Some(&repo("x")),);
        // X's state is unchanged.
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Uploading),
        );
    }

    /// Releasing the claim wakes waiters and clears the holder.
    #[tokio::test(flavor = "current_thread")]
    async fn claim_release_fires_notify_for_waiting_repos() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut map = BlobDedupMap::default();
                let d = test_digest();

                // X claims.
                match map.claim_blob_upload("reg.io", &d, &repo("x")) {
                    ClaimAction::Claimed => {}
                    _ => panic!(),
                }
                // Y tries -> Wait.
                let notify = match map.claim_blob_upload("reg.io", &d, &repo("y")) {
                    ClaimAction::Wait { holder, notify } => {
                        assert_eq!(holder, repo("x"));
                        notify
                    }
                    _ => panic!("Y should wait"),
                };

                // Spawn a task awaiting the notify.
                let waiter = tokio::task::spawn_local(async move {
                    notify.notified().await;
                });
                tokio::task::yield_now().await;

                // X releases.
                map.release_blob_claim("reg.io", &d);

                // Waiter wakes.
                waiter.await.expect("waiter should complete");

                // Now Y can claim.
                match map.claim_blob_upload("reg.io", &d, &repo("y")) {
                    ClaimAction::Claimed => {}
                    _ => panic!("Y should claim after X releases"),
                }
            })
            .await;
    }

    /// After mount success: `set_blob_completed` + `release_blob_claim` leaves
    /// the entry in the right shape. Verifies the post-mount cleanup sequence.
    #[test]
    fn claim_release_after_mount_success_path() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        match map.claim_blob_upload("reg.io", &d, &repo("y")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        map.set_blob_completed("reg.io", &d, &repo("y"));
        map.release_blob_claim("reg.io", &d);

        assert_eq!(
            map.repo_state("reg.io", &d, &repo("y")),
            Some(&RepoBlobState::Completed),
        );
        assert!(map.claim_holder("reg.io", &d, &repo("z")).is_none());
    }

    /// `mark_blob_repo_stale` transitions Completed to Failed; subsequent
    /// `repos_iter` shows the Failed state.
    #[test]
    fn mark_blob_repo_stale_transitions_completed_to_failed() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        map.set_blob_completed("reg.io", &d, &repo("x"));
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Completed),
        );

        map.mark_blob_repo_stale("reg.io", &d, &repo("x"));
        match map.repo_state("reg.io", &d, &repo("x")) {
            Some(RepoBlobState::Failed(_)) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Re-entrant `set_blob_verified` at the claim holder is allowed; the claim
    /// slot is unchanged and the holder is responsible for releasing.
    #[test]
    fn batch_check_at_self_repo_with_claim_held() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        match map.claim_blob_upload("reg.io", &d, &repo("y")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        // Y's batch check finds D at Y.
        map.set_blob_verified("reg.io", &d, &repo("y"));

        // State transitions Uploading -> Verified.
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("y")),
            Some(&RepoBlobState::Verified),
        );
        // Claim still held by Y.
        let entry = map.entry_mut("reg.io", &d);
        assert_eq!(entry.claim, Some(repo("y")));
    }

    /// `filter_persistable` retains Verified|Completed and strips Uploading|Failed.
    #[test]
    fn persistence_strips_uploading_and_failed_states() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();
        let d2 = other_digest();

        map.set_blob_verified("reg.io", &d, &repo("a"));
        map.set_blob_completed("reg.io", &d, &repo("b"));
        match map.claim_blob_upload("reg.io", &d, &repo("c")) {
            ClaimAction::Claimed => {} // c is Uploading
            _ => panic!(),
        }
        map.set_blob_failed("reg.io", &d, &repo("e"), "oops".into());
        // d2: only Failed -- whole entry should be stripped.
        map.set_blob_failed("reg.io", &d2, &repo("z"), "oops".into());

        let persisted = map.filter_persistable();

        // d's entry exists with only A and B.
        assert_eq!(
            persisted.repo_state("reg.io", &d, &repo("a")),
            Some(&RepoBlobState::Verified),
        );
        assert_eq!(
            persisted.repo_state("reg.io", &d, &repo("b")),
            Some(&RepoBlobState::Completed),
        );
        assert!(persisted.repo_state("reg.io", &d, &repo("c")).is_none());
        assert!(persisted.repo_state("reg.io", &d, &repo("e")).is_none());
        // d2 has no persistable entries -- the entry should be gone entirely.
        assert!(persisted.repos_iter("reg.io", &d2).next().is_none());
    }

    /// Persistence strips the claim slot. Round-trip via postcard.
    #[test]
    fn persistence_strips_claim_slot() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        match map.claim_blob_upload("reg.io", &d, &repo("a")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        map.set_blob_completed("reg.io", &d, &repo("a"));

        let persisted = map.filter_persistable();
        let entry = persisted
            .inner
            .get("reg.io")
            .and_then(|m| m.get(&d))
            .expect("entry should persist because A is Completed");
        assert!(entry.claim.is_none(), "claim must be stripped");
        assert!(
            entry.claim_notify.is_none(),
            "claim_notify must be stripped"
        );

        // Round-trip via postcard to confirm serde::skip works.
        let bytes = postcard::to_allocvec(&persisted).expect("serialize");
        let _restored: BlobDedupMap = postcard::from_bytes(&bytes).expect("deserialize");
    }
}
