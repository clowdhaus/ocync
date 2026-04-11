//! Blob deduplication map and transfer ordering for cross-repo mount support.

use std::collections::{HashMap, HashSet};

use ocync_distribution::Digest;

/// Status of a blob at the target registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobStatus {
    /// Not yet checked.
    Unknown,
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
#[derive(Debug)]
pub struct BlobInfo {
    /// Current transfer status.
    pub status: BlobStatus,
    /// Set of repositories at the target that have this blob.
    pub repos: HashSet<String>,
}

/// Process-global deduplication map keyed by `(target_registry, digest)`.
///
/// Tracks which blobs have already been transferred or exist at a target so
/// we never push the same layer twice.
#[derive(Debug)]
pub struct BlobDedupMap {
    inner: HashMap<(String, Digest), BlobInfo>,
}

impl BlobDedupMap {
    /// Create an empty deduplication map.
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Get the current status for a blob at the given target.
    pub fn status(&self, target: &str, digest: &Digest) -> Option<&BlobStatus> {
        self.inner
            .get(&(target.to_owned(), digest.clone()))
            .map(|info| &info.status)
    }

    /// Mark a blob as existing at the target in the given repo.
    pub fn set_exists(&mut self, target: &str, digest: &Digest, repo: &str) {
        let entry = self
            .inner
            .entry((target.to_owned(), digest.clone()))
            .or_insert_with(|| BlobInfo {
                status: BlobStatus::Unknown,
                repos: HashSet::new(),
            });
        entry.status = BlobStatus::ExistsAtTarget;
        entry.repos.insert(repo.to_owned());
    }

    /// Mark a blob as in-progress at the target.
    pub fn set_in_progress(&mut self, target: &str, digest: &Digest) {
        let entry = self
            .inner
            .entry((target.to_owned(), digest.clone()))
            .or_insert_with(|| BlobInfo {
                status: BlobStatus::Unknown,
                repos: HashSet::new(),
            });
        entry.status = BlobStatus::InProgress;
    }

    /// Mark a blob as completed at the target in the given repo.
    pub fn set_completed(&mut self, target: &str, digest: &Digest, repo: &str) {
        let entry = self
            .inner
            .entry((target.to_owned(), digest.clone()))
            .or_insert_with(|| BlobInfo {
                status: BlobStatus::Unknown,
                repos: HashSet::new(),
            });
        entry.status = BlobStatus::Completed;
        entry.repos.insert(repo.to_owned());
    }

    /// Mark a blob as failed at the target.
    pub fn set_failed(&mut self, target: &str, digest: &Digest, error: String) {
        let entry = self
            .inner
            .entry((target.to_owned(), digest.clone()))
            .or_insert_with(|| BlobInfo {
                status: BlobStatus::Unknown,
                repos: HashSet::new(),
            });
        entry.status = BlobStatus::Failed(error);
    }

    /// Return the set of known repos for a blob at a target.
    pub fn known_repos(&self, target: &str, digest: &Digest) -> Option<&HashSet<String>> {
        self.inner
            .get(&(target.to_owned(), digest.clone()))
            .map(|info| &info.repos)
    }

    /// Find a repo that already has this blob at the target which differs from
    /// `target_repo`, suitable as a cross-repo mount source.
    pub fn mount_source<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        target_repo: &str,
    ) -> Option<&'a str> {
        let info = self.inner.get(&(target.to_owned(), digest.clone()))?;
        info.repos
            .iter()
            .find(|r| r.as_str() != target_repo)
            .map(|r| r.as_str())
    }
}

impl Default for BlobDedupMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Transfer ordering — lower variants are transferred first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransferKind {
    /// Layer / config blob.
    Blob = 0,
    /// Platform-specific manifest.
    PlatformManifest = 1,
    /// Multi-platform index / manifest list.
    Index = 2,
    /// OCI referrer (signatures, SBOMs, attestations).
    Referrer = 3,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_digest() -> Digest {
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            .parse()
            .unwrap()
    }

    #[test]
    fn insert_and_check_status() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        assert!(map.status("reg.io", &d).is_none());
        map.set_exists("reg.io", &d, "library/alpine");
        assert_eq!(map.status("reg.io", &d), Some(&BlobStatus::ExistsAtTarget));
    }

    #[test]
    fn track_multiple_repos() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg.io", &d, "library/alpine");
        map.set_completed("reg.io", &d, "library/nginx");

        let repos = map.known_repos("reg.io", &d).unwrap();
        assert!(repos.contains("library/alpine"));
        assert!(repos.contains("library/nginx"));
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn mount_source_returns_other_repo() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg.io", &d, "library/alpine");
        map.set_completed("reg.io", &d, "library/nginx");

        let source = map.mount_source("reg.io", &d, "library/nginx");
        assert_eq!(source, Some("library/alpine"));
    }

    #[test]
    fn mount_source_returns_none_when_only_self() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_completed("reg.io", &d, "library/alpine");

        let source = map.mount_source("reg.io", &d, "library/alpine");
        assert!(source.is_none());
    }

    #[test]
    fn transfer_kind_ordering() {
        assert!(TransferKind::Blob < TransferKind::PlatformManifest);
        assert!(TransferKind::PlatformManifest < TransferKind::Index);
        assert!(TransferKind::Index < TransferKind::Referrer);
    }

    #[test]
    fn status_transitions() {
        let mut map = BlobDedupMap::new();
        let d = test_digest();

        map.set_in_progress("reg.io", &d);
        assert_eq!(map.status("reg.io", &d), Some(&BlobStatus::InProgress));

        map.set_completed("reg.io", &d, "library/alpine");
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
