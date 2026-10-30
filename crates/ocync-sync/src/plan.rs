//! Per-(repo) blob state and upload-claim coordination for mount support.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use ocync_distribution::Digest;
use ocync_distribution::spec::RepositoryName;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// State of a blob at a single (target, repo) pair.
///
/// Each repo's lifecycle is independent: writing one repo's state never
/// disturbs another repo's state or an in-flight upload claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoBlobState {
    /// Verified to exist via HEAD or batch check. Persistable.
    Verified,
    /// We hold the upload claim for this digest at this repo. Transient.
    Uploading,
    /// Uploaded or mounted to this repo. Persistable.
    Completed,
    /// Upload to this repo failed, or a mount from it (as another repo's
    /// source) returned 202/4xx/5xx. Excluded from mount-source selection
    /// either way. Transient.
    Failed(String),
    /// Present according to an earlier run, not yet confirmed by this one. A
    /// batch check that misses the blob, or a manifest refused for a missing
    /// blob, drops it. Persisted as `Verified`.
    Remembered,
}

impl RepoBlobState {
    /// Whether the blob counts as present at this repo for skip purposes.
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Verified | Self::Completed | Self::Remembered)
    }

    /// Whether this state was recorded this run and is not a dead end.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Verified | Self::Uploading | Self::Completed)
    }
}

/// Outcome of an atomic check-and-claim on a blob's upload coordination slot.
#[derive(Debug)]
pub enum ClaimAction {
    /// Caller holds the upload claim; call `release_blob_claim` when done.
    Claimed,
    /// Another task holds the claim, for this repo or another; await
    /// `notify` and retry.
    Wait {
        /// The repo holding the claim, for diagnostic logging.
        holder: RepositoryName,
        /// Wakes when the holder calls `release_blob_claim`.
        notify: Rc<Notify>,
    },
}

/// Metadata for a tracked blob at a single target.
///
/// The `claim` slot serializes uploads of this digest at the target: one task
/// holds it, and every other task calling `claim_blob_upload` waits, including
/// one for the holder's own repo.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct BlobInfo {
    /// Per-(repo) state. `BTreeMap` for deterministic iteration.
    pub(crate) repos: BTreeMap<RepositoryName, RepoBlobState>,
    /// Repo whose task is uploading (or mounting, HEAD-checking or pushing)
    /// this digest at the target. Not persisted.
    #[serde(skip)]
    pub(crate) claim: Option<RepositoryName>,
    /// Lazily created; fires when `claim` returns to `None`. Not persisted.
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
    fn entry_mut(&mut self, target: &str, digest: &Digest) -> &mut BlobInfo {
        self.inner
            .entry(target.to_owned())
            .or_default()
            .entry(digest.clone())
            .or_default()
    }

    /// The recorded state of a blob at one (target, repo), if any.
    pub(crate) fn repo_state(
        &self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) -> Option<&RepoBlobState> {
        self.inner.get(target)?.get(digest)?.repos.get(repo)
    }

    /// Whether the blob is present (`Verified`, `Completed` or `Remembered`) at
    /// the given (target, repo).
    pub(crate) fn blob_present_at(
        &self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) -> bool {
        self.repo_state(target, digest, repo)
            .is_some_and(RepoBlobState::is_present)
    }

    /// Set a repo's state to `Verified`, leaving other repos and the claim alone.
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

    /// Mark a repo as a stale mount source (a mount from it was rejected) by
    /// setting `Failed`, which excludes it from `mount_sources` and
    /// `unresolved_watches_for`.
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

    /// Atomic check-and-claim on the upload coordination slot.
    ///
    /// Returns `ClaimAction::Claimed` (and sets `current_repo` to `Uploading`)
    /// or `ClaimAction::Wait` with the holder's notify handle. A holder in
    /// `current_repo` also means wait: two images syncing into one repo share
    /// the upload.
    pub(crate) fn claim_blob_upload(
        &mut self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> ClaimAction {
        let entry = self.entry_mut(target, digest);
        match &entry.claim {
            Some(holder) => {
                let notify = entry
                    .claim_notify
                    .get_or_insert_with(|| Rc::new(Notify::new()))
                    .clone();
                ClaimAction::Wait {
                    holder: holder.clone(),
                    notify,
                }
            }
            None => {
                entry.claim = Some(current_repo.clone());
                entry
                    .repos
                    .insert(current_repo.clone(), RepoBlobState::Uploading);
                ClaimAction::Claimed
            }
        }
    }

    /// Release the upload claim and wake waiters. Called by the holder on a
    /// terminal outcome.
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

    /// `(repo, state)` pairs for a blob at a target; empty if untracked.
    pub(crate) fn repos_iter<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
    ) -> impl Iterator<Item = (&'a RepositoryName, &'a RepoBlobState)> + 'a {
        self.inner
            .get(target)
            .and_then(|m| m.get(digest))
            .into_iter()
            .flat_map(|info| info.repos.iter())
    }

    /// Drop `repo`'s state for the blob if it is only [`RepoBlobState::Remembered`],
    /// returning whether it was. State this run recorded is kept, since a batch
    /// check can lag an upload made seconds before it.
    pub(crate) fn forget_remembered(
        &mut self,
        target: &str,
        digest: &Digest,
        repo: &RepositoryName,
    ) -> bool {
        let Some(info) = self.inner.get_mut(target).and_then(|m| m.get_mut(digest)) else {
            return false;
        };
        if info.repos.get(repo) != Some(&RepoBlobState::Remembered) {
            return false;
        }
        info.repos.remove(repo);
        true
    }

    /// Record the blob as `Verified` at `repo`, which a manifest committed
    /// there proves. A live upload claim, presence this run recorded, and a
    /// `Failed` mark are left alone: the mark also excludes a mount source
    /// the registry refused, which a commit does not make mountable.
    pub(crate) fn confirm(&mut self, target: &str, digest: &Digest, repo: &RepositoryName) {
        if let None | Some(RepoBlobState::Remembered) = self.repo_state(target, digest, repo) {
            self.set_blob_verified(target, digest, repo);
        }
    }

    /// Start a run from what persisting and reloading would give: present
    /// states become [`RepoBlobState::Remembered`]; `Uploading`, `Failed` and
    /// claims are dropped.
    pub(crate) fn remember_present(&mut self) {
        for index in self.inner.values_mut() {
            index.retain(|_, info| {
                info.repos.retain(|_, state| state.is_present());
                for state in info.repos.values_mut() {
                    *state = RepoBlobState::Remembered;
                }
                info.claim = None;
                info.claim_notify = None;
                !info.repos.is_empty()
            });
        }
        self.inner.retain(|_, index| !index.is_empty());
    }

    /// A copy with only persistable state: `Verified` and `Completed` survive,
    /// `Remembered` becomes `Verified`, and `Uploading`, `Failed` and the claim
    /// slot are stripped.
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
                            .filter_map(|(r, state)| {
                                let kept = match state {
                                    RepoBlobState::Verified | RepoBlobState::Remembered => {
                                        RepoBlobState::Verified
                                    }
                                    RepoBlobState::Completed => RepoBlobState::Completed,
                                    RepoBlobState::Uploading | RepoBlobState::Failed(_) => {
                                        return None;
                                    }
                                };
                                Some((r.clone(), kept))
                            })
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

    /// Whether the map holds no entries.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.values().all(|index| index.is_empty())
    }

    /// Drop entries for targets not in `live_targets`, returning the number of
    /// blob entries removed.
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

    /// A new run marks present states `Remembered`, which a missing batch
    /// answer drops. State this run recorded, at the same repo or
    /// another, is kept, and `Remembered` persists as `Verified`.
    #[test]
    fn forgets_only_remembered_state() {
        let d = test_digest();
        let mut map = BlobDedupMap::default();
        map.set_blob_completed("reg.io", &d, &repo("x"));
        map.set_blob_verified("reg.io", &d, &repo("y"));
        map.remember_present();
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Remembered)
        );
        assert!(map.blob_present_at("reg.io", &d, &repo("x")));
        assert_eq!(
            map.filter_persistable()
                .repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Verified)
        );

        map.forget_remembered("reg.io", &d, &repo("x"));
        assert!(!map.blob_present_at("reg.io", &d, &repo("x")));
        assert!(
            map.blob_present_at("reg.io", &d, &repo("y")),
            "another repo is untouched"
        );

        map.set_blob_completed("reg.io", &d, &repo("x"));
        map.forget_remembered("reg.io", &d, &repo("x"));
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Completed),
            "state this run recorded is kept"
        );

        map.remember_present();
        map.confirm("reg.io", &d, &repo("x"));
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Verified)
        );
        map.forget_remembered("reg.io", &d, &repo("y"));
        map.confirm("reg.io", &d, &repo("y"));
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("y")),
            Some(&RepoBlobState::Verified),
            "a commit records a blob a refusal dropped"
        );
        assert!(matches!(
            map.claim_blob_upload("reg.io", &d, &repo("z")),
            ClaimAction::Claimed
        ));
        map.confirm("reg.io", &d, &repo("z"));
        assert_eq!(
            map.repo_state("reg.io", &d, &repo("z")),
            Some(&RepoBlobState::Uploading),
            "a commit never overwrites a live upload claim"
        );
        map.set_blob_failed("reg.io", &d, &repo("w"), "mount refused".into());
        map.confirm("reg.io", &d, &repo("w"));
        assert!(
            map.repo_state("reg.io", &d, &repo("w"))
                .is_some_and(|state| matches!(state, RepoBlobState::Failed(_))),
            "a commit keeps a refused mount source excluded"
        );
    }

    /// A new run, such as a watch-mode cycle, starts from what persisting
    /// and reloading would give: a failure, an upload or a claim from the
    /// last run does not carry over.
    #[test]
    fn run_start_drops_what_persistence_drops() {
        let d = test_digest();
        let other: Digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000001"
                .parse()
                .unwrap();
        let mut map = BlobDedupMap::default();
        map.set_blob_completed("reg.io", &d, &repo("x"));
        map.set_blob_failed("reg.io", &d, &repo("y"), "mount refused".into());
        assert!(matches!(
            map.claim_blob_upload("reg.io", &d, &repo("z")),
            ClaimAction::Claimed
        ));
        map.set_blob_failed("reg.io", &other, &repo("x"), "upload failed".into());

        map.remember_present();

        assert_eq!(
            map.repo_state("reg.io", &d, &repo("x")),
            Some(&RepoBlobState::Remembered)
        );
        assert_eq!(map.repo_state("reg.io", &d, &repo("y")), None);
        assert_eq!(map.repo_state("reg.io", &d, &repo("z")), None);
        assert!(
            matches!(
                map.claim_blob_upload("reg.io", &d, &repo("y")),
                ClaimAction::Claimed
            ),
            "the last run's claim is released"
        );
        assert_eq!(map.repo_state("reg.io", &other, &repo("x")), None);
    }

    /// Writing Y's state must not disturb X's `Uploading` state or release X's
    /// claim.
    #[test]
    fn writing_one_repos_state_leaves_another_repos_claim_held() {
        type WriteY = fn(&mut BlobDedupMap);
        let cases: [(&str, WriteY, RepoBlobState); 2] = [
            (
                "a batch check verifies Y",
                |m| m.set_blob_verified("reg.io", &test_digest(), &repo("y")),
                RepoBlobState::Verified,
            ),
            (
                "Y's own upload fails",
                |m| m.set_blob_failed("reg.io", &test_digest(), &repo("y"), "io error".into()),
                RepoBlobState::Failed("io error".into()),
            ),
        ];
        for (name, write_y, want_y) in cases {
            let mut map = BlobDedupMap::default();
            let d = test_digest();
            assert!(
                matches!(
                    map.claim_blob_upload("reg.io", &d, &repo("x")),
                    ClaimAction::Claimed
                ),
                "{name}: first claim should succeed"
            );

            write_y(&mut map);

            assert_eq!(
                map.repo_state("reg.io", &d, &repo("x")),
                Some(&RepoBlobState::Uploading),
                "{name}: X is unchanged"
            );
            assert_eq!(
                map.repo_state("reg.io", &d, &repo("y")),
                Some(&want_y),
                "{name}: Y holds the written state"
            );
            match map.claim_blob_upload("reg.io", &d, &repo("z")) {
                ClaimAction::Wait { holder, notify: _ } => {
                    assert_eq!(holder, repo("x"), "{name}");
                }
                ClaimAction::Claimed => panic!("{name}: X's claim must still be held"),
            }
        }
    }

    /// Releasing the claim wakes waiters and clears the holder.
    #[tokio::test(flavor = "current_thread")]
    async fn claim_release_fires_notify_for_waiting_repos() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut map = BlobDedupMap::default();
                let d = test_digest();

                match map.claim_blob_upload("reg.io", &d, &repo("x")) {
                    ClaimAction::Claimed => {}
                    _ => panic!(),
                }
                let notify = match map.claim_blob_upload("reg.io", &d, &repo("y")) {
                    ClaimAction::Wait { holder, notify } => {
                        assert_eq!(holder, repo("x"));
                        notify
                    }
                    _ => panic!("Y should wait"),
                };

                let waiter = tokio::task::spawn_local(async move {
                    notify.notified().await;
                });
                tokio::task::yield_now().await;

                map.release_blob_claim("reg.io", &d);

                waiter.await.expect("waiter should complete");

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
        match map.claim_blob_upload("reg.io", &d, &repo("z")) {
            ClaimAction::Claimed => {}
            ClaimAction::Wait { .. } => panic!("a released claim is free again"),
        }
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
        map.set_blob_verified("reg.io", &d, &repo("y"));

        assert_eq!(
            map.repo_state("reg.io", &d, &repo("y")),
            Some(&RepoBlobState::Verified),
        );
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
        // d2 holds only a Failed state, so its whole entry is stripped.
        map.set_blob_failed("reg.io", &d2, &repo("z"), "oops".into());

        let persisted = map.filter_persistable();

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
        assert!(persisted.repos_iter("reg.io", &d2).next().is_none());
    }

    /// A second image syncing into the holder's own repo waits for the
    /// holder instead of uploading the same blob alongside it.
    #[test]
    fn claim_held_by_same_repo_returns_wait() {
        let mut map = BlobDedupMap::default();
        let d = test_digest();

        match map.claim_blob_upload("reg.io", &d, &repo("x")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        match map.claim_blob_upload("reg.io", &d, &repo("x")) {
            ClaimAction::Wait { holder, notify: _ } => assert_eq!(holder, repo("x")),
            ClaimAction::Claimed => panic!("a second claim in the same repo must wait"),
        }

        map.release_blob_claim("reg.io", &d);
        match map.claim_blob_upload("reg.io", &d, &repo("x")) {
            ClaimAction::Claimed => {}
            _ => panic!("a released claim is free again"),
        }
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
