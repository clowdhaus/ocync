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
//! Only [`BlobStatus::ExistsAtTarget`] and [`BlobStatus::Completed`] entries
//! are written to disk; transient states are stripped before serialization.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ocync_distribution::Digest;
use ocync_distribution::spec::{PlatformFilter, RegistryAuthority, RepositoryName};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::plan::{BlobDedupMap, BlobStatus};

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
/// states survive across process restarts. Only [`BlobStatus::ExistsAtTarget`]
/// and [`BlobStatus::Completed`] entries are persisted; transient states are
/// dropped on write.
///
/// `load` never returns an error - a missing, corrupt, or expired file simply
/// yields an empty cache, and the next sync run will repopulate it.
#[derive(Debug, Default)]
pub struct TransferStateCache {
    dedup: BlobDedupMap,
    snapshots: SourceSnapshotMap,
    /// Runtime-only: per-(target, digest) [`Notify`] used to wake waiters when
    /// an in-progress blob upload completes or fails. Concurrent transfers of
    /// the same blob from different source repos wait on this to avoid
    /// redundant uploads - the winner pushes, losers mount from it.
    ///
    /// Lives outside `BlobDedupMap` so it's not part of the persisted cache
    /// format. Entries are cleaned up on persist via `reset_transient_state`.
    /// Nested map avoids allocating `(String, Digest)` keys on the read path.
    blob_notifies: HashMap<String, HashMap<Digest, Rc<Notify>>>,
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

    /// Get the current status for a blob at the given target.
    pub fn blob_status(&self, target: &str, digest: &Digest) -> Option<&BlobStatus> {
        self.dedup.status(target, digest)
    }

    /// Find a cross-repo mount source for a blob at a target.
    ///
    /// Prefers repos in `preferred` (leader repos with committed manifests),
    /// then falls back to the alphabetically first candidate.
    pub fn blob_mount_source<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
        preferred: &[RepositoryName],
    ) -> Option<&'a RepositoryName> {
        self.dedup
            .mount_source(target, digest, current_repo, preferred)
    }

    /// Mark a blob as already existing at the target in the given repo.
    pub fn set_blob_exists(&mut self, target: &str, digest: Digest, repo: RepositoryName) {
        self.dedup.set_exists(target, &digest, &repo);
    }

    /// Mark a blob as in-progress at the target, uploaded by `repo`.
    ///
    /// Recording the uploading repo enables concurrent transfers of the same
    /// blob to wait for completion and mount from `repo` via
    /// [`blob_in_progress_uploader`] + [`blob_notify`].
    pub fn set_blob_in_progress(&mut self, target: &str, digest: Digest, repo: RepositoryName) {
        self.dedup.set_in_progress(target, &digest, &repo);
    }

    /// Return the [`Notify`] for a blob at a target, creating it if absent.
    ///
    /// Waiters call `notified().await` on the returned handle before dropping
    /// the cache borrow; the uploading task calls [`notify_blob`] after
    /// marking the blob completed or failed.
    pub fn blob_notify(&mut self, target: &str, digest: &Digest) -> Rc<Notify> {
        self.blob_notifies
            .entry(target.to_owned())
            .or_default()
            .entry(digest.clone())
            .or_insert_with(|| Rc::new(Notify::new()))
            .clone()
    }

    /// Wake all waiters for a blob's transfer completion.
    ///
    /// Called by the uploading task after [`set_blob_completed`] or
    /// [`set_blob_failed`] so concurrent transfers of the same blob can
    /// proceed (either to mount from the completed repo or to retry).
    pub fn notify_blob(&self, target: &str, digest: &Digest) {
        if let Some(n) = self.blob_notifies.get(target).and_then(|m| m.get(digest)) {
            n.notify_waiters();
        }
    }

    /// Return the repository currently uploading `digest` at `target`, if any,
    /// as long as it differs from `current_repo`. Used by concurrent transfers
    /// to identify a mount source before initiating their own upload.
    pub fn blob_in_progress_uploader<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        current_repo: &RepositoryName,
    ) -> Option<&'a RepositoryName> {
        self.dedup
            .in_progress_uploader(target, digest, current_repo)
    }

    /// Mark a blob as successfully transferred to the given repo at the target.
    pub fn set_blob_completed(&mut self, target: &str, digest: Digest, repo: RepositoryName) {
        self.dedup.set_completed(target, &digest, &repo);
    }

    /// Mark a blob as failed at the target.
    pub fn set_blob_failed(&mut self, target: &str, digest: Digest, error: String) {
        self.dedup.set_failed(target, &digest, error);
    }

    /// Check whether a blob is known to exist at a specific repo on the target.
    ///
    /// Returns `true` if the blob has been recorded (via `set_blob_exists` or
    /// `set_blob_completed`) at the given `(target, repo)` pair. This is the
    /// repo-scoped skip check: a blob known at `repo-a` is NOT accessible from
    /// `repo-b` without a mount.
    pub fn blob_known_at_repo(&self, target: &str, digest: &Digest, repo: &RepositoryName) -> bool {
        self.dedup
            .known_repos(target, digest)
            .is_some_and(|repos| repos.contains(repo))
    }

    /// Remove a specific repo from a blob's known repos set at the target.
    ///
    /// Used after a failed mount to discard the stale mount source without
    /// removing the entire blob entry. The blob remains `InProgress` so
    /// concurrent waiters do not re-claim and start duplicate pushes.
    pub fn remove_blob_repo(&mut self, target: &str, digest: &Digest, repo: &RepositoryName) {
        self.dedup.remove_repo(target, digest, repo);
    }

    /// Remove the entry for the given blob at the target.
    ///
    /// Use this for lazy invalidation when a mount or push fails so the next
    /// sync re-evaluates the blob rather than trusting stale cached state.
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
    /// The dedup map accumulates `ExistsAtTarget`/`Completed` entries for every
    /// (target, digest) pair observed. When targets are removed from config,
    /// their entries become stale and grow unboundedly across runs. Call after
    /// each sync cycle with the set of currently-configured target names.
    pub fn prune_dedup(&mut self, live_targets: &HashSet<String>) {
        let removed = self.dedup.retain_targets(live_targets);
        if removed > 0 {
            tracing::debug!(removed, "pruned stale dedup entries for removed targets");
        }
    }

    /// Drop all in-flight blob notify handles.
    ///
    /// [`Notify`] entries are only needed during active blob coordination
    /// within a single sync run. Once the run completes, all pending
    /// transfers have resolved and the handles are stale. Call this at the
    /// end of each sync run to prevent monotonic growth across repeated
    /// runs in a long-lived process.
    pub fn clear_notifies(&mut self) {
        if !self.blob_notifies.is_empty() {
            let count: usize = self.blob_notifies.values().map(|m| m.len()).sum();
            tracing::debug!(count, "clearing stale blob notify handles");
            self.blob_notifies.clear();
        }
    }

    /// Atomically write the cache to `path`.
    ///
    /// Only [`BlobStatus::ExistsAtTarget`] and [`BlobStatus::Completed`]
    /// entries are written. The write sequence is:
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
            blob_notifies: HashMap::new(),
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
    fn set_and_read_status() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_exists("reg.io", digest(), repo("repo/a"));
        assert_eq!(
            cache.blob_status("reg.io", &digest()),
            Some(&BlobStatus::ExistsAtTarget)
        );
        assert!(!cache.is_empty());
    }

    #[test]
    fn mount_source_delegates_to_dedup() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        cache.set_blob_completed("reg.io", digest(), repo("repo/b"));
        assert_eq!(
            cache.blob_mount_source("reg.io", &digest(), &repo("repo/b"), &[]),
            Some(&repo("repo/a"))
        );
    }

    #[test]
    fn blob_known_at_repo_checks_specific_repo() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), repo("repo/a"));
        assert!(cache.blob_known_at_repo("reg.io", &digest(), &repo("repo/a")));
        assert!(!cache.blob_known_at_repo("reg.io", &digest(), &repo("repo/b")));
        assert!(!cache.blob_known_at_repo("other.io", &digest(), &repo("repo/a")));
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_exists("reg.io", digest(), repo("repo/a"));
        cache.invalidate_blob("reg.io", &digest());
        assert_eq!(cache.blob_status("reg.io", &digest()), None);
    }

    #[test]
    fn build_cache_bytes_roundtrip() {
        let mut dedup = BlobDedupMap::new();
        dedup.set_completed("reg.io", &digest(), &repo("repo/a"));
        let snapshots = SourceSnapshotMap::default();
        let bytes = build_cache_bytes(&dedup, &snapshots).unwrap();
        // Verify CRC covers the entire payload
        let (payload, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(stored, crc32fast::hash(payload));
    }

    #[test]
    fn filter_persistable_excludes_transient() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_exists("reg.io", digest(), repo("repo/a"));
        cache.set_blob_in_progress("reg.io", digest2(), repo("repo/a"));
        cache.set_blob_failed("reg.io", digest2(), "oops".into());

        // Only ExistsAtTarget should survive
        let filtered = cache.dedup.filter_persistable();
        assert_eq!(
            filtered.status("reg.io", &digest()),
            Some(&BlobStatus::ExistsAtTarget)
        );
        assert_eq!(filtered.status("reg.io", &digest2()), None);
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
