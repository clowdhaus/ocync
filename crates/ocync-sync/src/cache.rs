//! Persistent transfer state cache wrapping `BlobDedupMap`, so a later run
//! can skip HEAD checks for blobs already known to exist at a target.
//!
//! # Binary format
//!
//! ```text
//! [4 bytes: header_len as u32 LE]
//! [header_len bytes: postcard-serialized CacheHeader]
//! [postcard-serialized BlobDedupMap]
//! [postcard-serialized SourceSnapshotMap]
//! [4 bytes: CRC32 of everything before this]
//! ```
//!
//! Only `Verified` and `Completed` entries are written to disk; transient
//! states are stripped before serialization.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ocync_distribution::Digest;
use ocync_distribution::spec::{PlatformFilter, RegistryAuthority, RepositoryName};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::plan::{BlobDedupMap, ClaimAction, RepoBlobState, TargetId};

/// Cache file format version. Bump when the layout or the meaning of a key
/// changes; a mismatched file is discarded and rebuilt.
const CACHE_VERSION: u32 = 2;

/// Header written at the start of every cache file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheHeader {
    /// Format version, checked against [`CACHE_VERSION`].
    version: u32,
    /// Unix timestamp (seconds since epoch) when the file was written.
    written_at: u64,
}

/// Key of a source manifest in the snapshot cache: registry authority,
/// repository and tag. A struct rather than an encoded string, so no separator
/// can be ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotKey {
    /// Registry authority (e.g. `cgr.dev:443`).
    authority: RegistryAuthority,
    /// Repository path (e.g. `chainguard/nginx`).
    repo: RepositoryName,
    /// Tag name (e.g. `latest`).
    tag: String,
}

impl SnapshotKey {
    /// Create a new snapshot key.
    pub fn new(authority: &RegistryAuthority, repo: &RepositoryName, tag: &str) -> Self {
        Self {
            authority: authority.clone(),
            repo: repo.clone(),
            tag: tag.to_owned(),
        }
    }
}

/// Canonical form of the active platform filter set, compared by equality to
/// detect a config change: platforms sorted and joined with commas, empty when
/// no filtering is active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformFilterKey(String);

impl PlatformFilterKey {
    /// Canonical key for an optional platform filter slice; empty for `None`
    /// or an empty slice.
    pub fn from_filters(filters: Option<&[PlatformFilter]>) -> Self {
        let Some(filters) = filters else {
            return Self(String::new());
        };
        if filters.is_empty() {
            return Self(String::new());
        }
        let mut sorted: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
        sorted.sort();
        Self(sorted.join(","))
    }
}

/// Cached source manifest state: the digest HEAD returned and the digest left
/// after platform filtering, which lets discovery skip the GET.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    /// Digest HEAD returned (unfiltered for multi-arch).
    pub source_digest: Digest,
    /// Digest of the manifest intended for targets, after platform filtering.
    pub filtered_digest: Digest,
    /// Platform filter active when this entry was written.
    pub platform_filter_key: PlatformFilterKey,
}

/// Map of [`SnapshotKey`] to [`SourceSnapshot`].
type SourceSnapshotMap = HashMap<SnapshotKey, SourceSnapshot>;

/// Persistent transfer state cache.
///
/// Only `Verified` and `Completed` entries are persisted; transient states are
/// dropped on write. `load` never returns an error: a missing, corrupt or
/// expired file yields an empty cache.
#[derive(Debug, Default)]
pub struct TransferStateCache {
    dedup: BlobDedupMap,
    snapshots: SourceSnapshotMap,
    /// Runtime-only: per target and blob, the repos where a committed manifest
    /// references the blob. They are mount sources ahead of repos known only
    /// from an earlier run. Kept across runs in one process: a repo committed
    /// earlier still holds its manifest, and a stale entry self-heals through
    /// `mark_blob_repo_stale` when a mount from it fails.
    committed_blobs: HashMap<TargetId, HashMap<Digest, HashSet<RepositoryName>>>,
    /// Runtime-only: per-(target, repo, digest) channel that turns `true` when
    /// a manifest referencing the blob commits at the repo or the repo fails
    /// (resolved either way). Followers subscribe through
    /// `unresolved_watches_for` and consume it with `wait_for(|&v| v)`, never
    /// `changed()`, which misses a value sent before they subscribed. Fresh
    /// each run.
    commit_watches: HashMap<TargetId, HashMap<(RepositoryName, Digest), watch::Sender<bool>>>,
    /// Runtime-only: repos that failed this run. Their watches are resolved,
    /// and a later subscription for them starts resolved. Fresh each run.
    failed_repos: HashMap<TargetId, HashSet<RepositoryName>>,
}

impl TransferStateCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.dedup.is_empty() && self.snapshots.is_empty()
    }

    /// Whether the blob is present (`Verified`, `Completed` or `Remembered`) at
    /// the given (target, repo).
    pub fn blob_present_at(
        &self,
        target: &TargetId,
        digest: &Digest,
        repo: &RepositoryName,
    ) -> bool {
        self.dedup.blob_present_at(target, digest, repo)
    }

    /// Atomic check-and-claim on the upload-coordination slot.
    pub fn claim_blob_upload(
        &mut self,
        target: &TargetId,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> ClaimAction {
        self.dedup.claim_blob_upload(target, digest, current_repo)
    }

    /// Release the upload claim, waking waiters.
    pub fn release_blob_claim(&mut self, target: &TargetId, digest: &Digest) {
        self.dedup.release_blob_claim(target, digest);
    }

    /// Mark `repo` as a stale mount source (a mount from it was rejected),
    /// excluding it from future `mount_sources` results.
    pub fn mark_blob_repo_stale(
        &mut self,
        target: &TargetId,
        digest: &Digest,
        repo: &RepositoryName,
    ) {
        self.dedup.mark_blob_repo_stale(target, digest, repo);
    }

    /// Set a repo's state to `Verified`.
    pub fn set_blob_verified(&mut self, target: &TargetId, digest: Digest, repo: RepositoryName) {
        self.dedup.set_blob_verified(target, &digest, &repo);
    }

    /// Set a repo's state to `Completed`.
    pub fn set_blob_completed(&mut self, target: &TargetId, digest: Digest, repo: RepositoryName) {
        self.dedup.set_blob_completed(target, &digest, &repo);
    }

    /// Set a repo's state to `Failed` (this repo's own upload failed).
    pub fn set_blob_failed(
        &mut self,
        target: &TargetId,
        digest: Digest,
        repo: RepositoryName,
        error: String,
    ) {
        self.dedup.set_blob_failed(target, &digest, &repo, error);
    }

    /// Mount sources for a blob, excluding `current_repo`.
    ///
    /// Yields first the repos holding the blob where a committed manifest
    /// references it at `target` in this process, then the uncommitted repos
    /// holding it only according to an earlier run. An earlier run's repo is
    /// offered without waiting, because its commit watch fires only if this run
    /// pushes a manifest referencing the blob to it: its manifest most likely
    /// already does, and a mount the registry refuses marks it stale and falls
    /// back to a push. Order within each group is `BTreeMap` order.
    pub fn mount_sources<'a>(
        &'a self,
        target: &'a TargetId,
        digest: &'a Digest,
        current_repo: &'a RepositoryName,
    ) -> impl Iterator<Item = &'a RepositoryName> + 'a {
        let committed_set = self
            .committed_blobs
            .get(target)
            .and_then(|blobs| blobs.get(digest));
        let is_committed =
            move |r: &RepositoryName| committed_set.is_some_and(|set| set.contains(r));
        let others = move || {
            self.dedup
                .repos_iter(target, digest)
                .filter(move |(r, _)| *r != current_repo)
        };
        let committed = others().filter(move |(r, state)| state.is_present() && is_committed(r));
        let remembered_uncommitted = others()
            .filter(move |(r, state)| **state == RepoBlobState::Remembered && !is_committed(r));
        committed.chain(remembered_uncommitted).map(|(r, _)| r)
    }

    /// Commit watches for repos that hold the blob this run (`Verified`,
    /// `Completed` or `Uploading`, not `Failed`) and whose watch for this blob
    /// has not yet fired, so a caller waits only on repos that can still become
    /// a mount source. A repo known only from an earlier run is never waited
    /// on: [`Self::mount_sources`] offers it at once.
    pub fn unresolved_watches_for(
        &mut self,
        target: &TargetId,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> Vec<watch::Receiver<bool>> {
        let candidates: Vec<RepositoryName> = self
            .dedup
            .repos_iter(target, digest)
            .filter(|(r, state)| *r != current_repo && state.is_active())
            .map(|(r, _)| r.clone())
            .collect();

        let failed = self.failed_repos.get(target);
        let watches = self.commit_watches.entry(target.clone()).or_default();
        candidates
            .into_iter()
            .filter(|repo| !failed.is_some_and(|set| set.contains(repo)))
            .filter_map(|repo| {
                let tx = watches
                    .entry((repo, digest.clone()))
                    .or_insert_with(|| watch::channel(false).0);
                let rx = tx.subscribe();
                if *rx.borrow() { None } else { Some(rx) }
            })
            .collect()
    }

    /// Mark a repo as failed this run, unblocking followers: every watch at the
    /// repo resolves, and a later subscription for it starts resolved.
    pub fn notify_repo_failed(&mut self, target: &TargetId, repo: &RepositoryName) {
        self.failed_repos
            .entry(target.clone())
            .or_default()
            .insert(repo.clone());
        let watches = self.commit_watches.get(target).into_iter().flatten();
        for (_, tx) in watches.filter(|((watch_repo, _), _)| watch_repo == repo) {
            tx.send_replace(true);
        }
    }

    /// Start a run from the blob state a reload from disk would give: every
    /// present blob becomes known only from an earlier run, and failures,
    /// upload claims and the last run's watch handles and repo failures are
    /// dropped. Committed blobs are kept: a mount from a repo that has since
    /// lost the blob fails and falls back to a push (one request), while
    /// forgetting them would push blobs a mount could have saved.
    pub(crate) fn start_run(&mut self) {
        self.commit_watches.clear();
        self.failed_repos.clear();
        self.dedup.remember_present();
    }

    /// Drop `repo`'s state for each of `digests` that is known only from an
    /// earlier run, returning the digests dropped.
    pub(crate) fn forget_remembered_blobs<'a>(
        &mut self,
        target: &TargetId,
        repo: &RepositoryName,
        digests: impl IntoIterator<Item = &'a Digest>,
    ) -> Vec<&'a Digest> {
        digests
            .into_iter()
            .filter(|digest| self.dedup.forget_remembered(target, digest, repo))
            .collect()
    }

    /// Record each of `digests` present and committed at `repo`, after a
    /// manifest that references them was committed there, and wake the
    /// watchers waiting on them at that repo.
    pub fn confirm_blobs<'a>(
        &mut self,
        target: &TargetId,
        repo: &RepositoryName,
        digests: impl IntoIterator<Item = &'a Digest>,
    ) {
        for digest in digests {
            self.dedup.confirm(target, digest, repo);
            self.committed_blobs
                .entry(target.clone())
                .or_default()
                .entry(digest.clone())
                .or_default()
                .insert(repo.clone());
            self.commit_watches
                .entry(target.clone())
                .or_default()
                .entry((repo.clone(), digest.clone()))
                .or_insert_with(|| watch::channel(false).0)
                .send_replace(true);
        }
    }

    /// Look up a cached source snapshot.
    pub fn source_snapshot(&self, key: &SnapshotKey) -> Option<&SourceSnapshot> {
        self.snapshots.get(key)
    }

    /// Record a source snapshot after a successful source pull.
    pub fn set_source_snapshot(&mut self, key: SnapshotKey, snapshot: SourceSnapshot) {
        self.snapshots.insert(key, snapshot);
    }

    /// Remove snapshot entries for tags not in `live_keys`. Call after each
    /// sync cycle so deleted source tags do not grow the cache without bound.
    pub fn prune_snapshots(&mut self, live_keys: &HashSet<SnapshotKey>) {
        self.snapshots.retain(|k, _| live_keys.contains(k));
    }

    /// Remove blob state, committed blobs, commit watches and repo failures for
    /// targets not in `live_targets`. Call after each sync cycle so targets
    /// dropped from config do not grow the cache without bound.
    pub fn prune_dedup(&mut self, live_targets: &HashSet<TargetId>) {
        self.committed_blobs
            .retain(|target, _| live_targets.contains(target));
        self.commit_watches
            .retain(|target, _| live_targets.contains(target));
        self.failed_repos
            .retain(|target, _| live_targets.contains(target));
        let removed = self.dedup.retain_targets(live_targets);
        if removed > 0 {
            tracing::debug!(removed, "pruned stale dedup entries for removed targets");
        }
    }

    /// Atomically write the cache to `path`: temporary file (`{stem}.tmp.{pid}` beside it),
    /// fsync, rename, fsync of the parent directory.
    ///
    /// Returns an `io::Error` on any failure; the original file is untouched if
    /// a step before the rename fails.
    pub fn persist(&self, path: &Path) -> Result<(), io::Error> {
        let persistable = self.dedup.filter_persistable();
        let bytes = build_cache_bytes(&persistable, &self.snapshots)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));

        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp_path, path)?;

        crate::staging::best_effort_dir_fsync(path);

        Ok(())
    }

    /// Load a cache from `path`. Returns an empty cache (never an error) if
    /// the file is missing, corrupt, or older than `max_age`.
    pub fn load(path: &Path, max_age: Duration) -> Self {
        Self::try_load(path, max_age).unwrap_or_default()
    }

    /// Fallible load; [`Self::load`] turns every error into an empty cache.
    fn try_load(path: &Path, max_age: Duration) -> Result<Self, LoadError> {
        let bytes = std::fs::read(path).map_err(|_| LoadError::Missing)?;

        if bytes.len() < 8 {
            warn!(path = %path.display(), "cache file too short, discarding");
            return Err(LoadError::Corrupt);
        }

        let (payload, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        let computed_crc = crc32fast::hash(payload);
        if stored_crc != computed_crc {
            warn!(
                path = %path.display(),
                stored = stored_crc,
                computed = computed_crc,
                "cache CRC32 mismatch, discarding"
            );
            return Err(LoadError::Corrupt);
        }

        if payload.len() < 4 {
            warn!(path = %path.display(), "cache payload too short, discarding");
            return Err(LoadError::Corrupt);
        }
        let header_len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;

        if payload.len() < 4 + header_len {
            warn!(path = %path.display(), "cache header length overflows payload, discarding");
            return Err(LoadError::Corrupt);
        }

        let header: CacheHeader =
            postcard::from_bytes(&payload[4..4 + header_len]).map_err(|e| {
                warn!(path = %path.display(), error = %e, "cache header deserialization failed, discarding");
                LoadError::Corrupt
            })?;

        if header.version != CACHE_VERSION {
            warn!(
                path = %path.display(),
                version = header.version,
                expected = CACHE_VERSION,
                "cache written by another ocync version; starting cold"
            );
            return Err(LoadError::BadVersion);
        }

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let age = now_secs.saturating_sub(header.written_at);
        // Duration::ZERO means "never expire" (TTL disabled); any positive
        // duration is a real expiry threshold.
        if !max_age.is_zero() && Duration::from_secs(age) >= max_age {
            info!(
                path = %path.display(),
                age_secs = age,
                max_age_secs = max_age.as_secs(),
                "cache expired, discarding"
            );
            return Err(LoadError::Expired);
        }

        let body = &payload[4 + header_len..];

        let (dedup, remainder): (BlobDedupMap, &[u8]) =
            postcard::take_from_bytes(body).map_err(|e| {
                warn!(path = %path.display(), error = %e, "cache body deserialization failed, discarding");
                LoadError::Corrupt
            })?;

        let snapshots: SourceSnapshotMap = postcard::from_bytes(remainder).map_err(|e| {
            warn!(path = %path.display(), error = %e, "snapshot section deserialization failed, discarding");
            LoadError::Corrupt
        })?;

        Ok(Self {
            dedup,
            snapshots,
            ..Self::default()
        })
    }
}

/// Reasons a cache load fails; [`TransferStateCache::load`] maps all to empty.
#[derive(Debug)]
enum LoadError {
    Missing,
    Corrupt,
    BadVersion,
    Expired,
}

/// Serialize the maps into the on-disk cache format.
fn build_cache_bytes(
    dedup: &BlobDedupMap,
    snapshots: &SourceSnapshotMap,
) -> Result<Vec<u8>, io::Error> {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let header = CacheHeader {
        version: CACHE_VERSION,
        written_at: now_secs,
    };

    let header_bytes = postcard::to_allocvec(&header)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let body_bytes =
        postcard::to_allocvec(dedup).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let snapshot_bytes = postcard::to_allocvec(snapshots)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let header_len = header_bytes.len() as u32;

    let mut buf =
        Vec::with_capacity(4 + header_bytes.len() + body_bytes.len() + snapshot_bytes.len() + 4);
    buf.extend_from_slice(&header_len.to_le_bytes());
    buf.extend_from_slice(&header_bytes);
    buf.extend_from_slice(&body_bytes);
    buf.extend_from_slice(&snapshot_bytes);

    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::plan::test_support::{other_digest, repo, target_id, test_digest};

    type Setup = fn(&mut TransferStateCache);

    #[test]
    fn new_cache_is_empty() {
        assert!(TransferStateCache::new().is_empty());
    }

    #[test]
    fn mount_sources_by_commit_and_run_state() {
        let cases: [(&str, Setup, &str, &[&str]); 9] = [
            (
                "a repo recorded this run must commit before it is offered",
                |c| c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a")),
                "repo/b",
                &[],
            ),
            (
                "a committed repo is offered",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/a"), [&test_digest()]);
                },
                "repo/b",
                &["repo/a"],
            ),
            (
                "a commit referencing another blob does not offer the repo",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.set_blob_completed(&target_id("reg.io"), other_digest(), repo("repo/a"));
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/a"), [&other_digest()]);
                },
                "repo/b",
                &[],
            ),
            (
                "a failed mount source is excluded",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/b"));
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/a"), [&test_digest()]);
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/b"), [&test_digest()]);
                    c.mark_blob_repo_stale(&target_id("reg.io"), &test_digest(), &repo("repo/b"));
                },
                "repo/c",
                &["repo/a"],
            ),
            (
                "the requesting repo is never its own source",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/a"), [&test_digest()]);
                },
                "repo/a",
                &[],
            ),
            (
                "an earlier run's repo is offered without a commit",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.start_run();
                },
                "repo/b",
                &["repo/a"],
            ),
            (
                "this run's commits come before an earlier run's repos",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.start_run();
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/b"));
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/b"), [&test_digest()]);
                },
                "repo/c",
                &["repo/b", "repo/a"],
            ),
            (
                "a remembered committed repo is offered once",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.confirm_blobs(&target_id("reg.io"), &repo("repo/a"), [&test_digest()]);
                    c.start_run();
                },
                "repo/b",
                &["repo/a"],
            ),
            (
                "a refused earlier-run mount source is not offered again",
                |c| {
                    c.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
                    c.start_run();
                    c.mark_blob_repo_stale(&target_id("reg.io"), &test_digest(), &repo("repo/a"));
                },
                "repo/b",
                &[],
            ),
        ];
        for (name, setup, requester, want) in cases {
            let mut cache = TransferStateCache::new();
            setup(&mut cache);
            let got: Vec<_> = cache
                .mount_sources(&target_id("reg.io"), &test_digest(), &repo(requester))
                .cloned()
                .collect();
            let want: Vec<_> = want.iter().map(|r| repo(r)).collect();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn unresolved_watches_skip_fired_and_failed_repos() {
        // The third field is the repo whose watch must be the one left.
        let cases: [(&str, Setup, &str); 2] = [
            (
                "a repo whose watch already fired",
                |c| c.confirm_blobs(&target_id("reg.io"), &repo("repo/a"), [&test_digest()]),
                "repo/b",
            ),
            (
                "a repo whose blob state is failed",
                |c| c.mark_blob_repo_stale(&target_id("reg.io"), &test_digest(), &repo("repo/b")),
                "repo/a",
            ),
        ];
        for (name, setup, left) in cases {
            let mut cache = TransferStateCache::new();
            cache.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/a"));
            cache.set_blob_completed(&target_id("reg.io"), test_digest(), repo("repo/b"));
            setup(&mut cache);

            let receivers =
                cache.unresolved_watches_for(&target_id("reg.io"), &test_digest(), &repo("repo/c"));
            assert_eq!(receivers.len(), 1, "{name}");
            cache.confirm_blobs(&target_id("reg.io"), &repo(left), [&test_digest()]);
            assert!(*receivers[0].borrow(), "{name}: the watch left is {left}'s");
        }
    }

    /// Pruning drops every per-target map for a target no longer in the
    /// config, not only its blob state, and leaves the live target alone.
    #[test]
    fn prune_dedup_drops_committed_blobs_watches_and_failures_of_removed_targets() {
        let (live, gone) = (target_id("live.io"), target_id("gone.io"));
        let mut cache = TransferStateCache::new();
        for t in [&live, &gone] {
            cache.set_blob_completed(t, test_digest(), repo("repo/a"));
            cache.confirm_blobs(t, &repo("repo/a"), [&test_digest()]);
            cache.notify_repo_failed(t, &repo("repo/a"));
        }

        cache.prune_dedup(&HashSet::from([live.clone()]));

        assert!(cache.blob_present_at(&live, &test_digest(), &repo("repo/a")));
        assert!(cache.committed_blobs.contains_key(&live));
        assert!(cache.commit_watches.contains_key(&live));
        assert!(cache.failed_repos.contains_key(&live));
        assert!(!cache.blob_present_at(&gone, &test_digest(), &repo("repo/a")));
        assert!(!cache.committed_blobs.contains_key(&gone));
        assert!(!cache.commit_watches.contains_key(&gone));
        assert!(!cache.failed_repos.contains_key(&gone));
    }

    /// A commit is a fact about one blob at one repo. A manifest referencing
    /// only D1 makes the repo a source for D1 and resolves D1's watch, while
    /// D2, held at the same repo, stays unresolved and not a source until a
    /// manifest referencing it commits. A repo failure then resolves what is
    /// outstanding.
    #[tokio::test(flavor = "current_thread")]
    async fn commit_is_per_blob_and_failure_is_per_repo() {
        let (t, a, b) = (target_id("reg.io"), repo("repo/a"), repo("repo/b"));
        let (d1, d2) = (test_digest(), other_digest());
        let mut cache = TransferStateCache::new();
        for d in [&d1, &d2] {
            cache.set_blob_completed(&t, d.clone(), a.clone());
        }
        let sources = |c: &TransferStateCache, d: &Digest| -> Vec<RepositoryName> {
            c.mount_sources(&t, d, &b).cloned().collect()
        };
        let mut waiting_d1 = cache.unresolved_watches_for(&t, &d1, &b);
        let mut waiting_d2 = cache.unresolved_watches_for(&t, &d2, &b);
        assert_eq!((waiting_d1.len(), waiting_d2.len()), (1, 1));

        cache.confirm_blobs(&t, &a, [&d1]);
        assert!(waiting_d1.pop().unwrap().wait_for(|&v| v).await.is_ok());
        assert_eq!(
            sources(&cache, &d1),
            std::slice::from_ref(&a),
            "D1 is committed at a"
        );
        assert!(sources(&cache, &d2).is_empty(), "D2 is not committed at a");
        assert!(!*waiting_d2[0].borrow(), "D2's watch is unresolved");
        assert_eq!(cache.unresolved_watches_for(&t, &d2, &b).len(), 1);

        cache.confirm_blobs(&t, &a, [&d2]);
        assert!(waiting_d2.pop().unwrap().wait_for(|&v| v).await.is_ok());
        assert_eq!(
            sources(&cache, &d2),
            std::slice::from_ref(&a),
            "D2 is committed at a"
        );

        let d3 = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .parse::<Digest>()
            .unwrap();
        cache.set_blob_completed(&t, d3.clone(), a.clone());
        let mut waiting_d3 = cache.unresolved_watches_for(&t, &d3, &b);
        assert_eq!(waiting_d3.len(), 1, "D3 waits for a commit");
        cache.notify_repo_failed(&t, &a);
        assert!(waiting_d3.pop().unwrap().wait_for(|&v| v).await.is_ok());
        assert!(sources(&cache, &d3).is_empty(), "failure commits nothing");
        let d4 = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
            .parse::<Digest>()
            .unwrap();
        cache.set_blob_completed(&t, d4.clone(), a.clone());
        assert!(
            cache.unresolved_watches_for(&t, &d4, &b).is_empty(),
            "a subscription after the failure starts resolved"
        );
    }

    #[test]
    fn build_cache_bytes_roundtrip() {
        let mut dedup = BlobDedupMap::default();
        dedup.set_blob_completed(&target_id("reg.io"), &test_digest(), &repo("repo/a"));
        let snapshots = SourceSnapshotMap::default();
        let bytes = build_cache_bytes(&dedup, &snapshots).unwrap();
        let (payload, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(stored, crc32fast::hash(payload));
    }

    #[test]
    fn prune_snapshots_keeps_only_live_keys() {
        let key = |tag| SnapshotKey::new(&RegistryAuthority::new("src.io:443"), &repo("r"), tag);
        let snapshot = |digest| SourceSnapshot {
            source_digest: digest,
            filtered_digest: other_digest(),
            platform_filter_key: PlatformFilterKey::from_filters(None),
        };
        let mut cache = TransferStateCache::new();
        cache.set_source_snapshot(key("v1"), snapshot(test_digest()));
        cache.set_source_snapshot(key("v2"), snapshot(other_digest()));

        cache.prune_snapshots(&HashSet::from([key("v1")]));
        assert!(
            cache.source_snapshot(&key("v1")).is_some(),
            "listed key kept"
        );
        assert!(
            cache.source_snapshot(&key("v2")).is_none(),
            "unlisted key dropped"
        );

        cache.prune_snapshots(&HashSet::new());
        assert!(
            cache.source_snapshot(&key("v1")).is_none(),
            "no live keys clears all"
        );
    }
}
