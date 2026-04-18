//! Blob deduplication map and transfer ordering for cross-repo mount support.

use std::collections::{BTreeSet, HashMap};

use ocync_distribution::Digest;
use ocync_distribution::spec::RepositoryName;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Status of a blob at the target registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobStatus {
    /// Already exists at target.
    ExistsAtTarget,
    /// Transfer currently in flight.
    InProgress,
    /// Successfully transferred.
    Completed,
    /// Transfer failed with an error message.
    Failed(String),
}

/// Metadata for a tracked blob.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BlobInfo {
    /// Current transfer status.
    pub status: BlobStatus,
    /// Set of repositories at the target that have this blob.
    ///
    /// `BTreeSet` guarantees deterministic iteration order, which makes
    /// [`BlobDedupMap::mount_source`] return a consistent result.
    pub repos: BTreeSet<RepositoryName>,
    /// When [`status`] is [`BlobStatus::InProgress`], the repository whose
    /// transfer is currently uploading this blob. Other concurrent transfers
    /// of the same blob can wait on completion, then mount from this repo
    /// instead of re-pulling and re-pushing.
    ///
    /// Skipped during serialization - this is runtime-only coordination state.
    #[serde(skip)]
    pub uploader_repo: Option<RepositoryName>,
}

/// Per-registry index of tracked blobs.
type BlobIndex = HashMap<Digest, BlobInfo>;

/// Process-global deduplication map keyed by target registry, then by digest.
///
/// Tracks which blobs have already been transferred or exist at a target so
/// we never push the same layer twice. Uses a two-level map to avoid
/// allocations on read-path lookups.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BlobDedupMap {
    inner: HashMap<String, BlobIndex>,
}

impl BlobDedupMap {
    /// Create an empty deduplication map.
    pub(crate) fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Get or create the `BlobInfo` entry for the given target and digest.
    ///
    /// New entries start as `InProgress` - callers always overwrite the status
    /// immediately, so the initial value is never observed.
    fn entry_mut(&mut self, target: &str, digest: &Digest) -> &mut BlobInfo {
        self.inner
            .entry(target.to_owned())
            .or_default()
            .entry(digest.clone())
            .or_insert_with(|| BlobInfo {
                status: BlobStatus::InProgress,
                repos: BTreeSet::new(),
                uploader_repo: None,
            })
    }

    /// Get the current status for a blob at the given target.
    pub(crate) fn status(&self, target: &str, digest: &Digest) -> Option<&BlobStatus> {
        self.inner.get(target)?.get(digest).map(|info| &info.status)
    }

    /// Mark a blob as existing at the target in the given repo.
    pub(crate) fn set_exists(&mut self, target: &str, digest: &Digest, repo: &RepositoryName) {
        let entry = self.entry_mut(target, digest);
        if matches!(entry.status, BlobStatus::Completed | BlobStatus::Failed(_)) {
            warn!(
                target,
                %digest,
                from = ?entry.status,
                to = "ExistsAtTarget",
                "unexpected blob status transition"
            );
        }
        entry.status = BlobStatus::ExistsAtTarget;
        entry.repos.insert(repo.clone());
    }

    /// Mark a blob as in-progress at the target, uploaded by `repo`.
    ///
    /// Recording the uploading repo lets other concurrent transfers of the
    /// same blob wait for completion and then mount from `repo` instead of
    /// re-pulling and re-pushing. See [`in_progress_uploader`].
    ///
    /// `Completed → InProgress` is expected when re-processing a blob for a
    /// different repo at the same target (cross-repo mount fallback). Only
    /// `Failed → InProgress` is warned since it may indicate a logic error.
    pub(crate) fn set_in_progress(&mut self, target: &str, digest: &Digest, repo: &RepositoryName) {
        let entry = self.entry_mut(target, digest);
        if matches!(entry.status, BlobStatus::Failed(_)) {
            warn!(
                target,
                %digest,
                from = ?entry.status,
                to = "InProgress",
                "unexpected blob status transition"
            );
        }
        entry.status = BlobStatus::InProgress;
        entry.uploader_repo = Some(repo.clone());
    }

    /// Return the uploading repository if a blob is currently in-progress in
    /// a repository other than `target_repo`, suitable as a mount source once
    /// the upload completes.
    ///
    /// Returns `None` if the blob is not in-progress, or if the uploader is
    /// `target_repo` itself (no cross-repo mount possible).
    pub(crate) fn in_progress_uploader<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        target_repo: &RepositoryName,
    ) -> Option<&'a RepositoryName> {
        let info = self.inner.get(target)?.get(digest)?;
        if !matches!(info.status, BlobStatus::InProgress) {
            return None;
        }
        info.uploader_repo.as_ref().filter(|r| *r != target_repo)
    }

    /// Mark a blob as completed at the target in the given repo.
    pub(crate) fn set_completed(&mut self, target: &str, digest: &Digest, repo: &RepositoryName) {
        let entry = self.entry_mut(target, digest);
        if matches!(entry.status, BlobStatus::Failed(_)) {
            warn!(
                target,
                %digest,
                from = ?entry.status,
                to = "Completed",
                "unexpected blob status transition"
            );
        }
        entry.status = BlobStatus::Completed;
        entry.repos.insert(repo.clone());
    }

    /// Mark a blob as failed at the target.
    pub(crate) fn set_failed(&mut self, target: &str, digest: &Digest, error: String) {
        let entry = self.entry_mut(target, digest);
        if matches!(
            entry.status,
            BlobStatus::Completed | BlobStatus::ExistsAtTarget
        ) {
            warn!(
                target,
                %digest,
                from = ?entry.status,
                to = "Failed",
                "unexpected blob status transition"
            );
        }
        entry.status = BlobStatus::Failed(error);
    }

    /// Return the set of known repos for a blob at a target.
    pub(crate) fn known_repos(
        &self,
        target: &str,
        digest: &Digest,
    ) -> Option<&BTreeSet<RepositoryName>> {
        self.inner.get(target)?.get(digest).map(|info| &info.repos)
    }

    /// Find a repo that already has this blob at the target which differs from
    /// `target_repo`, suitable as a cross-repo mount source.
    ///
    /// When `preferred` is non-empty, tries those repos first (in order).
    /// Falls back to the alphabetically first candidate via `BTreeSet`.
    /// Preferred repos are typically leader images whose manifests are
    /// committed, making mount more likely to succeed on ECR.
    pub(crate) fn mount_source<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        target_repo: &RepositoryName,
        preferred: &[RepositoryName],
    ) -> Option<&'a RepositoryName> {
        let info = self.inner.get(target)?.get(digest)?;
        for pref in preferred {
            if pref != target_repo && info.repos.contains(pref) {
                return info.repos.get(pref);
            }
        }
        info.repos.iter().find(|r| *r != target_repo)
    }

    /// Remove the entry for the given blob at the target.
    ///
    /// Used for lazy invalidation when a mount or push fails so the next sync
    /// attempt re-evaluates the blob's state rather than trusting stale data.
    pub(crate) fn invalidate(&mut self, target: &str, digest: &Digest) {
        if let Some(index) = self.inner.get_mut(target) {
            index.remove(digest);
        }
    }

    /// Return a copy of this map containing only entries whose status is
    /// [`BlobStatus::ExistsAtTarget`] or [`BlobStatus::Completed`].
    ///
    /// Transient states (`InProgress`, `Failed`) are excluded so
    /// only stable, verified results are written to the on-disk cache.
    pub(crate) fn filter_persistable(&self) -> BlobDedupMap {
        let inner = self
            .inner
            .iter()
            .filter_map(|(target, index)| {
                let filtered: HashMap<Digest, BlobInfo> = index
                    .iter()
                    .filter(|(_, info)| {
                        matches!(
                            info.status,
                            BlobStatus::ExistsAtTarget | BlobStatus::Completed
                        )
                    })
                    .map(|(digest, info)| {
                        (
                            digest.clone(),
                            BlobInfo {
                                status: info.status.clone(),
                                repos: info.repos.clone(),
                                uploader_repo: None,
                            },
                        )
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
}

impl Default for BlobDedupMap {
    fn default() -> Self {
        Self::new()
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

    fn repo(name: &str) -> RepositoryName {
        RepositoryName::new(name)
    }

    #[test]
    fn insert_and_check_status() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        assert!(map.status("reg.io", &d).is_none());
        map.set_exists("reg.io", &d, &repo("library/alpine"));
        assert_eq!(map.status("reg.io", &d), Some(&BlobStatus::ExistsAtTarget));
    }

    #[test]
    fn track_multiple_repos() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg.io", &d, &repo("library/alpine"));
        map.set_completed("reg.io", &d, &repo("library/nginx"));

        let repos = map.known_repos("reg.io", &d).unwrap();
        assert!(repos.contains(&repo("library/alpine")));
        assert!(repos.contains(&repo("library/nginx")));
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn mount_source_returns_other_repo() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg.io", &d, &repo("library/alpine"));
        map.set_completed("reg.io", &d, &repo("library/nginx"));

        let source = map.mount_source("reg.io", &d, &repo("library/nginx"), &[]);
        assert_eq!(source, Some(&repo("library/alpine")));
    }

    #[test]
    fn mount_source_returns_none_when_only_self() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg.io", &d, &repo("library/alpine"));

        let source = map.mount_source("reg.io", &d, &repo("library/alpine"), &[]);
        assert!(source.is_none());
    }

    #[test]
    fn mount_source_deterministic_with_multiple_repos() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        // Insert in non-alphabetical order to verify BTreeSet ordering
        map.set_completed("reg.io", &d, &repo("library/redis"));
        map.set_completed("reg.io", &d, &repo("library/nginx"));
        map.set_completed("reg.io", &d, &repo("library/alpine"));

        // BTreeSet iterates alphabetically: alpine, nginx, redis
        let source = map.mount_source("reg.io", &d, &repo("library/redis"), &[]);
        assert_eq!(source, Some(&repo("library/alpine")));

        let source = map.mount_source("reg.io", &d, &repo("library/alpine"), &[]);
        assert_eq!(source, Some(&repo("library/nginx")));
    }

    #[test]
    fn mount_source_prefers_leader_repos() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        // Three repos: alpine (alphabetically first), nginx, redis.
        map.set_completed("reg.io", &d, &repo("library/alpine"));
        map.set_completed("reg.io", &d, &repo("library/nginx"));
        map.set_completed("reg.io", &d, &repo("library/redis"));

        // Without preferred, alphabetically first (alpine) wins.
        let source = map.mount_source("reg.io", &d, &repo("library/redis"), &[]);
        assert_eq!(source, Some(&repo("library/alpine")));

        // With preferred=[nginx], nginx wins over alphabetical alpine.
        let preferred = [repo("library/nginx")];
        let source = map.mount_source("reg.io", &d, &repo("library/redis"), &preferred);
        assert_eq!(source, Some(&repo("library/nginx")));

        // Preferred repo that IS target_repo is skipped; falls to next preferred.
        let preferred = [repo("library/redis"), repo("library/nginx")];
        let source = map.mount_source("reg.io", &d, &repo("library/redis"), &preferred);
        assert_eq!(source, Some(&repo("library/nginx")));

        // Preferred repo not in the set falls back to alphabetical.
        let preferred = [repo("library/unknown")];
        let source = map.mount_source("reg.io", &d, &repo("library/redis"), &preferred);
        assert_eq!(source, Some(&repo("library/alpine")));
    }

    #[test]
    fn different_targets_tracked_independently() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg-a.io", &d, &repo("library/alpine"));
        map.set_in_progress("reg-b.io", &d, &repo("library/alpine"));

        assert_eq!(map.status("reg-a.io", &d), Some(&BlobStatus::Completed));
        assert_eq!(map.status("reg-b.io", &d), Some(&BlobStatus::InProgress));
        assert!(map.status("reg-c.io", &d).is_none());
    }

    #[test]
    fn status_transitions() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_in_progress("reg.io", &d, &repo("library/alpine"));
        assert_eq!(map.status("reg.io", &d), Some(&BlobStatus::InProgress));

        map.set_completed("reg.io", &d, &repo("library/alpine"));
        assert_eq!(map.status("reg.io", &d), Some(&BlobStatus::Completed));
    }

    #[test]
    fn failed_status() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_failed("reg.io", &d, "connection refused".into());
        assert_eq!(
            map.status("reg.io", &d),
            Some(&BlobStatus::Failed("connection refused".into()))
        );
    }
}
