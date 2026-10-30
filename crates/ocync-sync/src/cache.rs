//! Persistent transfer state cache wrapping [`BlobDedupMap`], so a later run
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

use crate::plan::{BlobDedupMap, ClaimAction, RepoBlobState};

/// Cache file format version. Bump on incompatible layout changes; a mismatched
/// file is discarded and rebuilt.
const CACHE_VERSION: u32 = 1;

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
    /// Runtime-only: (target, repo) pairs whose manifests are committed. They
    /// are mount sources ahead of repos known only from an earlier run. Kept
    /// across runs in one process: a repo committed earlier still holds its
    /// manifest, and a stale entry self-heals through `mark_blob_repo_stale`
    /// when a mount from it fails.
    committed_repos: HashMap<String, HashSet<RepositoryName>>,
    /// Runtime-only: per-(target, repo) channel that turns `true` when the
    /// repo's manifest commits or fails (resolved either way). Followers
    /// subscribe through `unresolved_watches_for` and consume it with
    /// `wait_for(|&v| v)`, never `changed()`, which misses a value sent before
    /// they subscribed. Fresh each run.
    repo_resolved_watches: HashMap<String, HashMap<RepositoryName, watch::Sender<bool>>>,
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
    pub fn blob_present_at(&self, target: &str, digest: &Digest, repo: &RepositoryName) -> bool {
        self.dedup.blob_present_at(target, digest, repo)
    }

    /// Atomic check-and-claim on the upload-coordination slot.
    pub fn claim_blob_upload(
        &mut self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> ClaimAction {
        self.dedup.claim_blob_upload(target, digest, current_repo)
    }

    /// Release the upload claim, waking waiters.
    pub fn release_blob_claim(&mut self, target: &str, digest: &Digest) {
        self.dedup.release_blob_claim(target, digest);
    }

    /// Mark `repo` as a stale mount source (a mount from it was rejected),
    /// excluding it from future `mount_sources` results.
    pub fn mark_blob_repo_stale(&mut self, target: &str, digest: &Digest, repo: &RepositoryName) {
        self.dedup.mark_blob_repo_stale(target, digest, repo);
    }

    /// Set a repo's state to `Verified`.
    pub fn set_blob_verified(&mut self, target: &str, digest: Digest, repo: RepositoryName) {
        self.dedup.set_blob_verified(target, &digest, &repo);
    }

    /// Set a repo's state to `Completed`.
    pub fn set_blob_completed(&mut self, target: &str, digest: Digest, repo: RepositoryName) {
        self.dedup.set_blob_completed(target, &digest, &repo);
    }

    /// Set a repo's state to `Failed` (this repo's own upload failed).
    pub fn set_blob_failed(
        &mut self,
        target: &str,
        digest: Digest,
        repo: RepositoryName,
        error: String,
    ) {
        self.dedup.set_blob_failed(target, &digest, &repo, error);
    }

    /// Mount sources for a blob, excluding `current_repo`.
    ///
    /// Yields first the repos holding the blob whose manifest is committed at
    /// `target` in this process, then the uncommitted repos holding it only
    /// according to an earlier run. An earlier run's repo is offered without
    /// waiting, because its commit watch fires only if this run pushes an image
    /// to it: its manifest most likely already references the blob, and a mount
    /// the registry refuses marks it stale and falls back to a push. Order
    /// within each group is `BTreeMap` order.
    pub fn mount_sources<'a>(
        &'a self,
        target: &'a str,
        digest: &'a Digest,
        current_repo: &'a RepositoryName,
    ) -> impl Iterator<Item = &'a RepositoryName> + 'a {
        let committed_set = self.committed_repos.get(target);
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
    /// `Completed` or `Uploading`, not `Failed`) and whose watch has not yet
    /// fired, so a caller waits only on repos that can still become a mount
    /// source. A repo known only from an earlier run is never waited on:
    /// [`Self::mount_sources`] offers it at once.
    pub fn unresolved_watches_for(
        &mut self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> Vec<watch::Receiver<bool>> {
        let candidates: Vec<RepositoryName> = self
            .dedup
            .repos_iter(target, digest)
            .filter(|(r, state)| *r != current_repo && state.is_active())
            .map(|(r, _)| r.clone())
            .collect();

        let watches = self
            .repo_resolved_watches
            .entry(target.to_owned())
            .or_default();
        candidates
            .into_iter()
            .filter_map(|repo| {
                let tx = watches
                    .entry(repo)
                    .or_insert_with(|| watch::channel(false).0);
                let rx = tx.subscribe();
                if *rx.borrow() { None } else { Some(rx) }
            })
            .collect()
    }

    /// Record that a repo's manifest is committed, waking its watchers.
    pub fn mark_repo_committed(&mut self, target: &str, repo: &RepositoryName) {
        self.committed_repos
            .entry(target.to_owned())
            .or_default()
            .insert(repo.clone());
        self.resolve_repo_watch(target, repo);
    }

    /// Mark a repo's manifest commit as failed, unblocking followers.
    pub fn notify_repo_failed(&mut self, target: &str, repo: &RepositoryName) {
        self.resolve_repo_watch(target, repo);
    }

    /// Fire the repo's watch, creating it if no follower has subscribed yet.
    fn resolve_repo_watch(&mut self, target: &str, repo: &RepositoryName) {
        let tx = self
            .repo_resolved_watches
            .entry(target.to_owned())
            .or_default()
            .entry(repo.clone())
            .or_insert_with(|| watch::channel(false).0);
        tx.send_replace(true);
    }

    /// Start a run from the blob state a reload from disk would give: every
    /// present blob becomes known only from an earlier run, and failures,
    /// upload claims and the last run's watch handles are dropped. Committed
    /// repositories are kept: a mount from one that has since lost the blob
    /// fails and falls back to a push (one request), while forgetting them
    /// would push blobs a mount could have saved.
    pub(crate) fn start_run(&mut self) {
        self.repo_resolved_watches.clear();
        self.dedup.remember_present();
    }

    /// Drop `repo`'s state for each of `digests` that is known only from an
    /// earlier run, returning the digests dropped.
    pub(crate) fn forget_remembered_blobs<'a>(
        &mut self,
        target: &str,
        repo: &RepositoryName,
        digests: impl IntoIterator<Item = &'a Digest>,
    ) -> Vec<&'a Digest> {
        digests
            .into_iter()
            .filter(|digest| self.dedup.forget_remembered(target, digest, repo))
            .collect()
    }

    /// Record each of `digests` present at `repo`, after a manifest that
    /// references them was committed there.
    pub(crate) fn confirm_blobs<'a>(
        &mut self,
        target: &str,
        repo: &RepositoryName,
        digests: impl IntoIterator<Item = &'a Digest>,
    ) {
        for digest in digests {
            self.dedup.confirm(target, digest, repo);
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

    /// Remove dedup entries for targets not in `live_targets`. Call after each
    /// sync cycle so targets dropped from config do not grow the cache without
    /// bound.
    pub fn prune_dedup(&mut self, live_targets: &HashSet<String>) {
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
                "unsupported cache version, discarding"
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
            committed_repos: HashMap::new(),
            repo_resolved_watches: HashMap::new(),
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

    use crate::plan::test_support::{other_digest, repo, test_digest};

    type Setup = fn(&mut TransferStateCache);

    #[test]
    fn new_cache_is_empty() {
        assert!(TransferStateCache::new().is_empty());
    }

    #[test]
    fn mount_sources_by_commit_and_run_state() {
        let cases: [(&str, Setup, &str, &[&str]); 8] = [
            (
                "a repo recorded this run must commit before it is offered",
                |c| c.set_blob_completed("reg.io", test_digest(), repo("repo/a")),
                "repo/b",
                &[],
            ),
            (
                "a committed repo is offered",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.mark_repo_committed("reg.io", &repo("repo/a"));
                },
                "repo/b",
                &["repo/a"],
            ),
            (
                "a failed mount source is excluded",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/b"));
                    c.mark_repo_committed("reg.io", &repo("repo/a"));
                    c.mark_repo_committed("reg.io", &repo("repo/b"));
                    c.mark_blob_repo_stale("reg.io", &test_digest(), &repo("repo/b"));
                },
                "repo/c",
                &["repo/a"],
            ),
            (
                "the requesting repo is never its own source",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.mark_repo_committed("reg.io", &repo("repo/a"));
                },
                "repo/a",
                &[],
            ),
            (
                "an earlier run's repo is offered without a commit",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.start_run();
                },
                "repo/b",
                &["repo/a"],
            ),
            (
                "this run's commits come before an earlier run's repos",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.start_run();
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/b"));
                    c.mark_repo_committed("reg.io", &repo("repo/b"));
                },
                "repo/c",
                &["repo/b", "repo/a"],
            ),
            (
                "a remembered committed repo is offered once",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.mark_repo_committed("reg.io", &repo("repo/a"));
                    c.start_run();
                },
                "repo/b",
                &["repo/a"],
            ),
            (
                "a refused earlier-run mount source is not offered again",
                |c| {
                    c.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
                    c.start_run();
                    c.mark_blob_repo_stale("reg.io", &test_digest(), &repo("repo/a"));
                },
                "repo/b",
                &[],
            ),
        ];
        for (name, setup, requester, want) in cases {
            let mut cache = TransferStateCache::new();
            setup(&mut cache);
            let got: Vec<_> = cache
                .mount_sources("reg.io", &test_digest(), &repo(requester))
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
                |c| c.mark_repo_committed("reg.io", &repo("repo/a")),
                "repo/b",
            ),
            (
                "a repo whose blob state is failed",
                |c| c.mark_blob_repo_stale("reg.io", &test_digest(), &repo("repo/b")),
                "repo/a",
            ),
        ];
        for (name, setup, left) in cases {
            let mut cache = TransferStateCache::new();
            cache.set_blob_completed("reg.io", test_digest(), repo("repo/a"));
            cache.set_blob_completed("reg.io", test_digest(), repo("repo/b"));
            setup(&mut cache);

            let receivers = cache.unresolved_watches_for("reg.io", &test_digest(), &repo("repo/c"));
            assert_eq!(receivers.len(), 1, "{name}");
            cache.mark_repo_committed("reg.io", &repo(left));
            assert!(*receivers[0].borrow(), "{name}: the watch left is {left}'s");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commit_or_failure_resolves_a_repo_watch() {
        type Signal = fn(&mut TransferStateCache, &str, &RepositoryName);
        let signals: [(&str, Signal); 2] = [
            ("committed", TransferStateCache::mark_repo_committed),
            ("failed", TransferStateCache::notify_repo_failed),
        ];
        for (name, signal) in signals {
            let mut cache = TransferStateCache::new();
            cache.set_blob_verified("reg.io", test_digest(), repo("repo/a"));
            let mut waiting =
                cache.unresolved_watches_for("reg.io", &test_digest(), &repo("repo/b"));
            assert_eq!(waiting.len(), 1, "{name}: repo/a is unresolved");

            signal(&mut cache, "reg.io", &repo("repo/a"));
            assert!(
                waiting.pop().unwrap().wait_for(|&v| v).await.is_ok(),
                "{name}: a subscriber from before the signal is woken"
            );
            assert!(
                cache
                    .unresolved_watches_for("reg.io", &test_digest(), &repo("repo/b"))
                    .is_empty(),
                "{name}: a subscriber after the signal finds nothing to wait on"
            );
        }
    }

    #[test]
    fn build_cache_bytes_roundtrip() {
        let mut dedup = BlobDedupMap::default();
        dedup.set_blob_completed("reg.io", &test_digest(), &repo("repo/a"));
        let snapshots = SourceSnapshotMap::default();
        let bytes = build_cache_bytes(&dedup, &snapshots).unwrap();
        let (payload, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(stored, crc32fast::hash(payload));
    }

    #[test]
    fn prune_snapshots_with_empty_live_keys_clears_all() {
        let mut cache = TransferStateCache::new();
        cache.set_source_snapshot(
            SnapshotKey::new(&RegistryAuthority::new("src.io:443"), &repo("r"), "v1"),
            SourceSnapshot {
                source_digest: test_digest(),
                filtered_digest: other_digest(),
                platform_filter_key: PlatformFilterKey::from_filters(None),
            },
        );
        cache.prune_snapshots(&HashSet::new());
        assert!(
            cache
                .source_snapshot(&SnapshotKey::new(
                    &RegistryAuthority::new("src.io:443"),
                    &repo("r"),
                    "v1"
                ))
                .is_none()
        );
    }
}
