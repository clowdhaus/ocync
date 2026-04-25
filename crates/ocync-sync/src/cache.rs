//! Persistent transfer state cache wrapping [`BlobDedupMap`].
//!
//! Provides cross-run knowledge so that a CronJob deployment can load a warm
//! cache on startup and skip redundant HEAD checks for blobs already known to
//! exist at a target.
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

use crate::plan::{BlobDedupMap, ClaimAction};

/// Cache file format version. Bump on incompatible layout changes; mismatched
/// versions are discarded and rebuilt from scratch on the next sync cycle.
const CACHE_VERSION: u32 = 1;

/// Header written at the start of every cache file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheHeader {
    /// Format version; bump when the binary layout changes incompatibly.
    version: u32,
    /// Unix timestamp (seconds since epoch) when the file was written.
    written_at: u64,
}

/// Composite key identifying a source manifest in the snapshot cache.
///
/// Combines registry authority, repository, and tag into a structured key
/// for the [`SourceSnapshotMap`]. Using a struct instead of an encoded string
/// eliminates separator ambiguity and gives compile-time key construction
/// correctness.
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

/// Canonical representation of the active platform filter set.
///
/// Platforms are sorted alphabetically and joined with commas. An empty
/// `PlatformFilterKey` means no platform filtering is active. Compared by
/// equality for config change detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformFilterKey(String);

impl PlatformFilterKey {
    /// Compute the canonical key from an optional platform filter slice.
    ///
    /// Returns an empty key for `None` or empty slice.
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

/// Cached source manifest state for the tag digest cache.
///
/// Records what was observed at the source (HEAD digest) and what was
/// produced after platform filtering (filtered digest), enabling
/// HEAD-before-GET skip logic in `discover_tag()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    /// Source manifest digest as returned by HEAD (unfiltered for multi-arch).
    pub source_digest: Digest,
    /// Digest of the manifest intended for targets (after platform filtering).
    pub filtered_digest: Digest,
    /// Platform filter active when this entry was written.
    pub platform_filter_key: PlatformFilterKey,
}

/// Map of [`SnapshotKey`] to [`SourceSnapshot`].
type SourceSnapshotMap = HashMap<SnapshotKey, SourceSnapshot>;

/// Persistent transfer state cache.
///
/// Wraps [`BlobDedupMap`] and adds load/persist operations so that stable blob
/// states survive across process restarts. Only `Verified` and `Completed`
/// entries are persisted; transient states are dropped on write.
///
/// `load` never returns an error - a missing, corrupt, or expired file simply
/// yields an empty cache, and the next sync run will repopulate it.
#[derive(Debug, Default)]
pub struct TransferStateCache {
    dedup: BlobDedupMap,
    snapshots: SourceSnapshotMap,
    /// Runtime-only: tracks which (target, repo) pairs have had their manifests
    /// committed. Only repos here are eligible mount sources. Fresh each run.
    committed_repos: HashMap<String, HashSet<RepositoryName>>,
    /// Runtime-only: per-(target, repo) `watch` channel that transitions from
    /// `false` to `true` when the repo's manifest commits or fails. Followers
    /// subscribe via `repo_committed_watch` and call `wait_for(|&v| v).await`.
    /// Fresh each run.
    repo_committed_watches: HashMap<String, HashMap<RepositoryName, watch::Sender<bool>>>,
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

    /// Whether the blob is present (Verified or Completed) at the given (target, repo).
    pub fn blob_present_at(&self, target: &str, digest: &Digest, repo: &RepositoryName) -> bool {
        self.dedup.blob_present_at(target, digest, repo)
    }

    /// The current cross-repo upload-claim holder for this digest, excluding
    /// `current_repo`. Replaces `blob_in_progress_uploader`.
    pub fn blob_claim_for<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> Option<&'a RepositoryName> {
        self.dedup.claim_holder(target, digest, current_repo)
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

    /// Mark `repo`'s state as a stale mount source (mount from this repo was
    /// rejected). Excluded from future `committed_mount_sources` results.
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

    /// Iterator over committed mount sources for a blob, excluding `current_repo`.
    /// Yields only repos with state in `{Verified, Completed}` AND a committed
    /// manifest at `target`. Iteration is in `BTreeMap` order (deterministic).
    pub fn committed_mount_sources<'a>(
        &'a self,
        target: &'a str,
        digest: &'a Digest,
        current_repo: &'a RepositoryName,
    ) -> impl Iterator<Item = &'a RepositoryName> + 'a {
        let committed = self.committed_repos.get(target);
        self.dedup
            .repos_iter(target, digest)
            .filter(move |(r, state)| {
                *r != current_repo
                    && state.is_present()
                    && committed.is_some_and(|set| set.contains(*r))
            })
            .map(|(r, _)| r)
    }

    /// `Watch::Receivers` for repos that:
    ///   1. Have the blob with state in {Verified, Completed, Uploading} (NOT Failed)
    ///   2. Have not yet had their `repo_committed_watch` fire
    ///
    /// Used by the resolver to subscribe only to repos still able to become a
    /// committed mount source.
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
            .repo_committed_watches
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

    /// Record that a repo has had its manifest committed.
    pub fn mark_repo_committed(&mut self, target: &str, repo: &RepositoryName) {
        self.committed_repos
            .entry(target.to_owned())
            .or_default()
            .insert(repo.clone());
        let tx = self
            .repo_committed_watches
            .entry(target.to_owned())
            .or_default()
            .entry(repo.clone())
            .or_insert_with(|| watch::channel(false).0);
        tx.send_replace(true);
    }

    /// Mark a repo's manifest commit as failed, unblocking followers.
    pub fn notify_repo_failed(&mut self, target: &str, repo: &RepositoryName) {
        let tx = self
            .repo_committed_watches
            .entry(target.to_owned())
            .or_default()
            .entry(repo.clone())
            .or_insert_with(|| watch::channel(false).0);
        tx.send_replace(true);
    }

    /// Subscribe to a repo's manifest-commit watch.
    pub fn repo_committed_watch(
        &mut self,
        target: &str,
        repo: &RepositoryName,
    ) -> watch::Receiver<bool> {
        self.repo_committed_watches
            .entry(target.to_owned())
            .or_default()
            .entry(repo.clone())
            .or_insert_with(|| watch::channel(false).0)
            .subscribe()
    }

    /// Whether a repo is committed at the target.
    pub fn is_repo_committed(&self, target: &str, repo: &RepositoryName) -> bool {
        self.committed_repos
            .get(target)
            .is_some_and(|set| set.contains(repo))
    }

    /// Drop runtime-only watch handles. Called between sync runs in long-lived
    /// processes to prevent monotonic growth of the watch map.
    pub fn clear_notifies(&mut self) {
        if !self.repo_committed_watches.is_empty() {
            let count: usize = self.repo_committed_watches.values().map(|m| m.len()).sum();
            tracing::debug!(count, "clearing stale repo-committed watch handles");
            self.repo_committed_watches.clear();
        }
    }

    /// Lazy invalidation: drop the entire entry for the blob.
    pub fn invalidate_blob(&mut self, target: &str, digest: &Digest) {
        self.dedup.invalidate(target, digest);
    }

    /// Look up a cached source snapshot.
    pub fn source_snapshot(&self, key: &SnapshotKey) -> Option<&SourceSnapshot> {
        self.snapshots.get(key)
    }

    /// Record a source snapshot after a successful source pull.
    pub fn set_source_snapshot(&mut self, key: SnapshotKey, snapshot: SourceSnapshot) {
        self.snapshots.insert(key, snapshot);
    }

    /// Remove snapshot entries for tags not in the provided set of live keys.
    ///
    /// Call after each sync cycle to prevent unbounded cache growth when
    /// source tags are deleted.
    pub fn prune_snapshots(&mut self, live_keys: &HashSet<SnapshotKey>) {
        self.snapshots.retain(|k, _| live_keys.contains(k));
    }

    /// Remove dedup entries for targets no longer in the active configuration.
    ///
    /// The dedup map accumulates `Verified`/`Completed` entries for every
    /// (target, digest) pair observed. When targets are removed from config,
    /// their entries become stale and grow unboundedly across runs. Call after
    /// each sync cycle with the set of currently-configured target names.
    pub fn prune_dedup(&mut self, live_targets: &HashSet<String>) {
        let removed = self.dedup.retain_targets(live_targets);
        if removed > 0 {
            tracing::debug!(removed, "pruned stale dedup entries for removed targets");
        }
    }

    /// Atomically write the cache to `path`.
    ///
    /// Only `Verified` and `Completed` entries are written. The write sequence
    /// is:
    /// 1. Serialize to a temporary file (`{path}.tmp.{pid}`)
    /// 2. fsync the temporary file
    /// 3. Rename to `path` (atomic on POSIX)
    /// 4. fsync the parent directory
    ///
    /// Returns an `io::Error` on any failure; the original file is not
    /// modified if any step before the rename fails.
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

    /// Load a cache from `path`.
    ///
    /// Returns an empty cache (never an error) if the file is missing,
    /// corrupt, or older than `max_age`. Callers should treat an empty cache
    /// as a cold start and let the next sync run repopulate it.
    pub fn load(path: &Path, max_age: Duration) -> Self {
        Self::try_load(path, max_age).unwrap_or_default()
    }

    /// Internal fallible load; all error paths convert to an empty cache in
    /// [`Self::load`].
    fn try_load(path: &Path, max_age: Duration) -> Result<Self, LoadError> {
        let bytes = std::fs::read(path).map_err(|_| LoadError::Missing)?;

        if bytes.len() < 8 {
            warn!(path = %path.display(), "cache file too short, discarding");
            return Err(LoadError::Corrupt);
        }

        // Last 4 bytes are the CRC32 of everything before them.
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

        // First 4 bytes: header length.
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
            repo_committed_watches: HashMap::new(),
        })
    }
}

/// Reasons a cache load can fail internally (all converted to empty in the
/// public API).
#[derive(Debug)]
enum LoadError {
    Missing,
    Corrupt,
    BadVersion,
    Expired,
}

/// Serialize a [`BlobDedupMap`] and [`SourceSnapshotMap`] into the on-disk
/// cache format.
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

    use crate::plan::{ClaimAction, RepoBlobState};

    const TEST_DIGEST: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn digest() -> Digest {
        TEST_DIGEST.parse().unwrap()
    }

    fn digest2() -> Digest {
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .parse()
            .unwrap()
    }

    fn repo(name: &str) -> RepositoryName {
        RepositoryName::new(name).unwrap()
    }

    #[test]
    fn new_cache_is_empty() {
        assert!(TransferStateCache::new().is_empty());
    }

    #[test]
    fn committed_mount_sources_excludes_uncommitted_repos() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));

        let sources: Vec<_> = cache
            .committed_mount_sources("reg.io", &digest(), &repo("repo/b"))
            .cloned()
            .collect();
        assert!(sources.is_empty(), "uncommitted repo must be excluded");
    }

    #[test]
    fn committed_mount_sources_yields_repo_after_mark_committed() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        cache.mark_repo_committed("reg.io", &repo("repo/a"));

        let sources: Vec<_> = cache
            .committed_mount_sources("reg.io", &digest(), &repo("repo/b"))
            .cloned()
            .collect();
        assert_eq!(sources, vec![repo("repo/a")]);
    }

    #[test]
    fn committed_mount_sources_excludes_failed_repos() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        cache.set_blob_completed("reg.io", digest(), repo("repo/b"));
        cache.mark_repo_committed("reg.io", &repo("repo/a"));
        cache.mark_repo_committed("reg.io", &repo("repo/b"));

        cache.mark_blob_repo_stale("reg.io", &digest(), &repo("repo/b"));

        let sources: Vec<_> = cache
            .committed_mount_sources("reg.io", &digest(), &repo("repo/c"))
            .cloned()
            .collect();
        assert_eq!(
            sources,
            vec![repo("repo/a")],
            "failed mount-source must be excluded",
        );
    }

    #[test]
    fn committed_mount_sources_excludes_self() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        cache.mark_repo_committed("reg.io", &repo("repo/a"));

        let sources: Vec<_> = cache
            .committed_mount_sources("reg.io", &digest(), &repo("repo/a"))
            .cloned()
            .collect();
        assert!(sources.is_empty());
    }

    #[test]
    fn unresolved_watches_for_excludes_already_fired_watches() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        cache.set_blob_completed("reg.io", digest(), repo("repo/b"));

        cache.mark_repo_committed("reg.io", &repo("repo/a"));

        let receivers = cache.unresolved_watches_for("reg.io", &digest(), &repo("repo/c"));
        assert_eq!(receivers.len(), 1);
    }

    #[test]
    fn unresolved_watches_for_excludes_failed_repos() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        cache.set_blob_completed("reg.io", digest(), repo("repo/b"));
        cache.mark_blob_repo_stale("reg.io", &digest(), &repo("repo/b"));

        let receivers = cache.unresolved_watches_for("reg.io", &digest(), &repo("repo/c"));
        assert_eq!(receivers.len(), 1);
    }

    #[test]
    fn claim_blob_upload_returns_wait_when_held() {
        let mut cache = TransferStateCache::new();
        match cache.claim_blob_upload("reg.io", &digest(), &repo("x")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        match cache.claim_blob_upload("reg.io", &digest(), &repo("y")) {
            ClaimAction::Wait { holder, notify: _ } => {
                assert_eq!(holder, repo("x"));
            }
            _ => panic!("y should be told to wait"),
        }
    }

    #[test]
    fn blob_present_at_returns_true_for_verified_and_completed_only() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_verified("reg.io", digest(), repo("a"));
        cache.set_blob_completed("reg.io", digest(), repo("b"));
        match cache.claim_blob_upload("reg.io", &digest(), &repo("c")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        cache.set_blob_failed("reg.io", digest(), repo("d"), "oops".into());

        assert!(cache.blob_present_at("reg.io", &digest(), &repo("a")));
        assert!(cache.blob_present_at("reg.io", &digest(), &repo("b")));
        assert!(!cache.blob_present_at("reg.io", &digest(), &repo("c")));
        assert!(!cache.blob_present_at("reg.io", &digest(), &repo("d")));
    }

    #[test]
    fn is_repo_committed_tracks_state() {
        let mut cache = TransferStateCache::new();
        assert!(!cache.is_repo_committed("reg.io", &repo("repo/a")));

        cache.mark_repo_committed("reg.io", &repo("repo/a"));
        assert!(cache.is_repo_committed("reg.io", &repo("repo/a")));
        assert!(!cache.is_repo_committed("reg.io", &repo("repo/b")));
    }

    #[test]
    fn repo_committed_watch_late_subscriber_sees_true() {
        let mut cache = TransferStateCache::new();
        cache.mark_repo_committed("reg.io", &repo("repo/a"));
        let rx = cache.repo_committed_watch("reg.io", &repo("repo/a"));
        assert!(*rx.borrow());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repo_committed_wait_for_returns_on_late_subscribe() {
        let mut cache = TransferStateCache::new();
        cache.mark_repo_committed("reg.io", &repo("repo/a"));

        let mut rx = cache.repo_committed_watch("reg.io", &repo("repo/a"));
        let result = rx.wait_for(|&v| v).await;
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repo_failed_wait_for_returns_on_late_subscribe() {
        let mut cache = TransferStateCache::new();
        cache.notify_repo_failed("reg.io", &repo("repo/a"));

        let mut rx = cache.repo_committed_watch("reg.io", &repo("repo/a"));
        let result = rx.wait_for(|&v| v).await;
        assert!(result.is_ok());
        assert!(!cache.is_repo_committed("reg.io", &repo("repo/a")));
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_verified("reg.io", digest(), repo("repo/a"));
        cache.invalidate_blob("reg.io", &digest());
        assert!(!cache.blob_present_at("reg.io", &digest(), &repo("repo/a")));
    }

    #[test]
    fn build_cache_bytes_roundtrip() {
        let mut dedup = BlobDedupMap::default();
        dedup.set_blob_completed("reg.io", &digest(), &repo("repo/a"));
        let snapshots = SourceSnapshotMap::default();
        let bytes = build_cache_bytes(&dedup, &snapshots).unwrap();
        let (payload, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(stored, crc32fast::hash(payload));
    }

    #[test]
    fn filter_persistable_excludes_transient() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_verified("reg.io", digest(), repo("repo/a"));
        match cache.claim_blob_upload("reg.io", &digest2(), &repo("repo/a")) {
            ClaimAction::Claimed => {}
            _ => panic!(),
        }
        cache.set_blob_failed("reg.io", digest2(), repo("repo/a"), "oops".into());

        let filtered = cache.dedup.filter_persistable();
        assert_eq!(
            filtered.repo_state("reg.io", &digest(), &repo("repo/a")),
            Some(&RepoBlobState::Verified),
        );
        assert!(
            filtered
                .repo_state("reg.io", &digest2(), &repo("repo/a"))
                .is_none()
        );
    }

    #[test]
    fn prune_snapshots_with_empty_live_keys_clears_all() {
        let mut cache = TransferStateCache::new();
        cache.set_source_snapshot(
            SnapshotKey::new(&RegistryAuthority::new("src.io:443"), &repo("r"), "v1"),
            SourceSnapshot {
                source_digest: digest(),
                filtered_digest: digest2(),
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
