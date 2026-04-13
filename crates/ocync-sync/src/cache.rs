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
//! [remaining bytes before last 4: postcard-serialized BlobDedupMap]
//! [4 bytes: CRC32 of everything before this]
//! ```
//!
//! Only [`BlobStatus::ExistsAtTarget`] and [`BlobStatus::Completed`] entries
//! are written to disk; transient states are stripped before serialization.

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ocync_distribution::Digest;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::plan::{BlobDedupMap, BlobStatus};

/// Cache file format version.
const CACHE_VERSION: u32 = 1;

/// Header written at the start of every cache file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheHeader {
    /// Format version; bump when the binary layout changes incompatibly.
    version: u32,
    /// Unix timestamp (seconds since epoch) when the file was written.
    written_at: u64,
}

/// Persistent transfer state cache.
///
/// Wraps [`BlobDedupMap`] and adds load/persist operations so that stable blob
/// states survive across process restarts. Only [`BlobStatus::ExistsAtTarget`]
/// and [`BlobStatus::Completed`] entries are persisted; transient states are
/// dropped on write.
///
/// `load` never returns an error — a missing, corrupt, or expired file simply
/// yields an empty cache, and the next sync run will repopulate it.
#[derive(Debug)]
pub struct TransferStateCache {
    dedup: BlobDedupMap,
}

impl TransferStateCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            dedup: BlobDedupMap::new(),
        }
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.dedup.is_empty()
    }

    /// Get the current status for a blob at the given target.
    pub fn blob_status(&self, target: &str, digest: &Digest) -> Option<&BlobStatus> {
        self.dedup.status(target, digest)
    }

    /// Find a cross-repo mount source for a blob at a target.
    ///
    /// Returns the alphabetically first repo at `target` that has `digest`
    /// and is not `current_repo`, or `None` if no candidate exists.
    pub fn blob_mount_source<'a>(
        &'a self,
        target: &str,
        digest: &Digest,
        current_repo: &str,
    ) -> Option<&'a str> {
        self.dedup.mount_source(target, digest, current_repo)
    }

    /// Mark a blob as already existing at the target in the given repo.
    pub fn set_blob_exists(&mut self, target: &str, digest: Digest, repo: String) {
        self.dedup.set_exists(target, &digest, &repo);
    }

    /// Mark a blob as in-progress at the target.
    pub fn set_blob_in_progress(&mut self, target: &str, digest: Digest) {
        self.dedup.set_in_progress(target, &digest);
    }

    /// Mark a blob as successfully transferred to the given repo at the target.
    pub fn set_blob_completed(&mut self, target: &str, digest: Digest, repo: String) {
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
    pub fn blob_known_at_repo(&self, target: &str, digest: &Digest, repo: &str) -> bool {
        self.dedup
            .known_repos(target, digest)
            .is_some_and(|repos| repos.contains(repo))
    }

    /// Remove the entry for the given blob at the target.
    ///
    /// Use this for lazy invalidation when a mount or push fails so the next
    /// sync re-evaluates the blob rather than trusting stale cached state.
    pub fn invalidate_blob(&mut self, target: &str, digest: &Digest) {
        self.dedup.invalidate(target, digest);
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
        let bytes = build_cache_bytes(&persistable)?;

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

        // Best-effort directory fsync — the rename succeeded, so the data is
        // reachable; only crash-before-journal-flush can lose it.
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                if let Err(e) = dir.sync_all() {
                    warn!(
                        path = %parent.display(),
                        error = %e,
                        "directory fsync failed after cache rename"
                    );
                }
            }
        }

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
        let dedup: BlobDedupMap = postcard::from_bytes(body).map_err(|e| {
            warn!(path = %path.display(), error = %e, "cache body deserialization failed, discarding");
            LoadError::Corrupt
        })?;

        Ok(Self { dedup })
    }
}

impl Default for TransferStateCache {
    fn default() -> Self {
        Self::new()
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

/// Serialize a [`BlobDedupMap`] into the on-disk cache format.
fn build_cache_bytes(dedup: &BlobDedupMap) -> Result<Vec<u8>, io::Error> {
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

    let header_len = header_bytes.len() as u32;

    let mut buf = Vec::with_capacity(4 + header_bytes.len() + body_bytes.len() + 4);
    buf.extend_from_slice(&header_len.to_le_bytes());
    buf.extend_from_slice(&header_bytes);
    buf.extend_from_slice(&body_bytes);

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

    #[test]
    fn new_cache_is_empty() {
        assert!(TransferStateCache::new().is_empty());
    }

    #[test]
    fn set_and_read_status() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_exists("reg.io", digest(), "repo/a".into());
        assert_eq!(
            cache.blob_status("reg.io", &digest()),
            Some(&BlobStatus::ExistsAtTarget)
        );
        assert!(!cache.is_empty());
    }

    #[test]
    fn mount_source_delegates_to_dedup() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), "repo/a".into());
        cache.set_blob_completed("reg.io", digest(), "repo/b".into());
        assert_eq!(
            cache.blob_mount_source("reg.io", &digest(), "repo/b"),
            Some("repo/a")
        );
    }

    #[test]
    fn blob_known_at_repo_checks_specific_repo() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_completed("reg.io", digest(), "repo/a".into());
        assert!(cache.blob_known_at_repo("reg.io", &digest(), "repo/a"));
        assert!(!cache.blob_known_at_repo("reg.io", &digest(), "repo/b"));
        assert!(!cache.blob_known_at_repo("other.io", &digest(), "repo/a"));
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_exists("reg.io", digest(), "repo/a".into());
        cache.invalidate_blob("reg.io", &digest());
        assert_eq!(cache.blob_status("reg.io", &digest()), None);
    }

    #[test]
    fn build_cache_bytes_roundtrip() {
        let mut dedup = BlobDedupMap::new();
        dedup.set_completed("reg.io", &digest(), "repo/a");
        let bytes = build_cache_bytes(&dedup).unwrap();
        // Verify CRC covers the entire payload
        let (payload, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(stored, crc32fast::hash(payload));
    }

    #[test]
    fn filter_persistable_excludes_transient() {
        let mut cache = TransferStateCache::new();
        cache.set_blob_exists("reg.io", digest(), "repo/a".into());
        cache.set_blob_in_progress("reg.io", digest2());
        cache.set_blob_failed("reg.io", digest2(), "oops".into());

        // Only ExistsAtTarget should survive
        let filtered = cache.dedup.filter_persistable();
        assert_eq!(
            filtered.status("reg.io", &digest()),
            Some(&BlobStatus::ExistsAtTarget)
        );
        assert_eq!(filtered.status("reg.io", &digest2()), None);
    }
}
