//! Content-addressable disk staging for multi-target blob reuse.
//!
//! When a mapping has multiple targets (e.g., `us-ecr`, `eu-ecr`, `ap-ecr`),
//! pulling the same blob N times wastes bandwidth and source registry capacity.
//! [`BlobStage`] solves this by pulling each blob once from source and writing
//! it to a local content-addressable file. All target pushes then read from
//! that file independently.
//!
//! Single-target deployments pay zero overhead: use [`BlobStage::disabled`] and
//! all operations are no-ops.
//!
//! # File layout
//!
//! ```text
//! {base_dir}/blobs/{algorithm}/{hex_digest}
//! ```
//!
//! For example: `{base_dir}/blobs/sha256/e3b0c442...`
//!
//! # Atomic write protocol
//!
//! Writes use a `{digest}.tmp.{pid}` temporary file, fsynced and renamed into
//! place, followed by a directory fsync. Incomplete writes are never visible as
//! cache entries. Orphaned `.tmp.*` files from a previous crash are cleaned up
//! by [`BlobStage::cleanup_tmp_files`].

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use ocync_distribution::Digest;
use tokio::sync::Notify;

use crate::shared::{Claim, ClaimGuard, Work, claim};

/// Shared sequence counter for generating unique temporary file names across
/// both [`BlobStage::write`] and [`BlobStage::begin_write`]. A single counter
/// prevents temp filename collisions when both write paths are used for the
/// same digest.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Best-effort directory fsync after an atomic rename.
///
/// Ensures the rename is durable across power loss. Silently ignored if the
/// parent cannot be opened or the fsync fails - the data is already reachable
/// after rename; only crash-before-journal-flush can lose it.
pub(crate) fn best_effort_dir_fsync(path: &std::path::Path) {
    let Some(parent) = path.parent() else { return };
    let Ok(dir) = std::fs::File::open(parent) else {
        return;
    };
    if let Err(e) = dir.sync_all() {
        tracing::warn!(
            path = %parent.display(),
            error = %e,
            "directory fsync failed after rename"
        );
    }
}

/// Action returned by [`BlobStage::claim_or_check`] for source-pull dedup.
///
/// Prevents concurrent image tasks from pulling the same blob from source
/// when staging is enabled. The first task claims the pull; concurrent tasks
/// wait for the file to appear on disk.
pub(crate) enum StagePullAction<'a> {
    /// Blob already staged on disk. Skip source pull, read from staging.
    Exists,
    /// Claimed: this task should pull from source and stage the blob. Drop
    /// the guard once the blob is staged or the pull is abandoned; that
    /// wakes the waiters.
    Pull(ClaimGuard<'a, Digest, ()>),
    /// Another task is currently pulling this blob. Await the [`Notify`],
    /// then call `claim_or_check` again (the pull may have failed).
    Wait(Rc<Notify>),
}

/// Content-addressable blob staging area for multi-target sync.
///
/// When enabled, blobs are pulled once from source and stored locally so all
/// target pushes can read from the local file rather than re-pulling from
/// source. When disabled (single-target), all methods are no-ops.
///
/// # Staging path requirements
///
/// The staging directory MUST reside on local disk (tmpfs, ext4, XFS, APFS,
/// etc.). The engine performs synchronous I/O on staged files within the
/// single-threaded tokio runtime (see [`file_read_stream`](crate::engine)). A
/// network filesystem (NFS, EFS, CIFS, `GlusterFS`) would block the event loop
/// for the duration of each read call, stalling all concurrent transfers.
///
/// Cross-image source-pull dedup is coordinated via [`claim_or_check`]:
/// the first task to request a digest claims the pull, concurrent tasks wait
/// for the file to appear, then all read from the same staged file.
#[derive(Debug)]
pub struct BlobStage {
    base_dir: Option<PathBuf>,
    /// In-progress source pulls keyed by digest. No outcome is recorded:
    /// the staged file is the record, and a failed pull is retried by a
    /// waiter, whose source may differ.
    pulls: RefCell<Work<Digest, ()>>,
}

impl BlobStage {
    /// Create a staging area rooted at `base_dir`.
    ///
    /// The directory and its parents are created lazily on the first write.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir: Some(base_dir),
            pulls: RefCell::new(Work::default()),
        }
    }

    /// Create a disabled (no-op) staging area for single-target deployments.
    ///
    /// All operations on a disabled stage are no-ops; no disk I/O occurs.
    pub fn disabled() -> Self {
        Self {
            base_dir: None,
            pulls: RefCell::new(Work::default()),
        }
    }

    /// Returns `true` if staging is enabled.
    pub fn is_enabled(&self) -> bool {
        self.base_dir.is_some()
    }

    /// Returns `true` if the blob identified by `digest` is already staged.
    ///
    /// Always returns `false` when staging is disabled.
    pub fn exists(&self, digest: &Digest) -> bool {
        self.blob_path(digest).is_some_and(|p| p.exists())
    }

    /// Check-and-claim for source-pull dedup. Only called with staging
    /// enabled.
    pub(crate) fn claim_or_check(&self, digest: &Digest) -> StagePullAction<'_> {
        if self.exists(digest) {
            return StagePullAction::Exists;
        }
        match claim(&self.pulls, digest.clone()) {
            Claim::Claimed(guard) => StagePullAction::Pull(guard),
            Claim::Wait(notify) => StagePullAction::Wait(notify),
            // No outcome is ever recorded; the disk check above decides.
            Claim::Done(_) => StagePullAction::Exists,
        }
    }

    /// Write `data` to the staging area for `digest` using an atomic
    /// tmp-fsync-rename-dirsync sequence.
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if staging is disabled.
    pub fn write(&self, digest: &Digest, data: &[u8]) -> Result<(), io::Error> {
        let dest = self.blob_path(digest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "blob staging is disabled")
        })?;

        // Ensure the parent directory exists.
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dest.with_extension(format!("tmp.{}.{seq}", std::process::id()));

        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(data)?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp_path, &dest)?;

        best_effort_dir_fsync(&dest);

        Ok(())
    }

    /// Read a staged blob into memory.
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if staging is disabled.
    /// Returns [`io::ErrorKind::NotFound`] if the blob is not staged.
    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>, io::Error> {
        let path = self.blob_path(digest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "blob staging is disabled")
        })?;
        std::fs::read(&path)
    }

    /// Delete all orphaned `.tmp.*` files left by a previous crash.
    ///
    /// This should be called once at startup before any other staging
    /// operations. It is a no-op when staging is disabled.
    pub fn cleanup_tmp_files(&self) -> Result<(), io::Error> {
        let base = match &self.base_dir {
            Some(d) => d,
            None => return Ok(()),
        };

        let blobs_dir = base.join("blobs");
        if !blobs_dir.exists() {
            return Ok(());
        }

        for algo_entry in read_dir_entries(&blobs_dir)? {
            for file_entry in read_dir_entries(&algo_entry)? {
                if is_tmp_file(&file_entry) {
                    if let Err(e) = std::fs::remove_file(&file_entry) {
                        tracing::warn!(
                            path = %file_entry.display(),
                            error = %e,
                            "failed to remove orphaned staging tmp file, skipping"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Evict staged blobs until the total staged size is at or below
    /// `max_bytes`.
    ///
    /// Blobs are evicted in order of ascending file size (smallest first).
    /// Temporary files (`.tmp.*`) are never evicted. This is a no-op when
    /// staging is disabled.
    pub fn evict(&self, max_bytes: u64) -> Result<(), io::Error> {
        let base = match &self.base_dir {
            Some(d) => d,
            None => return Ok(()),
        };

        let blobs_dir = base.join("blobs");
        if !blobs_dir.exists() {
            return Ok(());
        }

        // Collect all staged blobs with their sizes.
        let mut entries: Vec<(PathBuf, u64)> = Vec::new();
        let mut total_bytes: u64 = 0;

        for algo_entry in read_dir_entries(&blobs_dir)? {
            for file_entry in read_dir_entries(&algo_entry)? {
                if is_tmp_file(&file_entry) {
                    continue;
                }

                // Skip symlinks to prevent deletion of symlink targets.
                // `symlink_metadata` does not follow symlinks, so `is_file()`
                // returns false for symlinks (unlike `metadata().is_file()`).
                match std::fs::symlink_metadata(&file_entry) {
                    Ok(meta) if !meta.is_file() => {
                        tracing::warn!(
                            path = %file_entry.display(),
                            "unexpected non-regular file in staging blobs dir, skipping"
                        );
                        continue;
                    }
                    Ok(meta) => {
                        total_bytes += meta.len();
                        entries.push((file_entry, meta.len()));
                    }
                    Err(_) => continue,
                }
            }
        }

        if total_bytes <= max_bytes {
            return Ok(());
        }

        // Sort by ascending file size (evict smallest first).
        entries.sort_by_key(|(_, size)| *size);

        for (path, size) in entries {
            if total_bytes <= max_bytes {
                break;
            }
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to evict staged blob, skipping"
                );
                continue;
            }
            total_bytes = total_bytes.saturating_sub(size);
        }

        Ok(())
    }

    /// Begin an atomic staged write. Returns a [`StagedWriter`] that accepts
    /// data in chunks. Call [`StagedWriter::finish`] to fsync and atomically
    /// rename the temp file into place.
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if staging is disabled.
    pub fn begin_write(&self, digest: &Digest) -> Result<StagedWriter, io::Error> {
        let dest = self.blob_path(digest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "blob staging is disabled")
        })?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dest.with_extension(format!("tmp.{}.{seq}", std::process::id()));
        let file = std::fs::File::create(&tmp_path)?;
        Ok(StagedWriter {
            file,
            tmp_path,
            dest,
            finished: false,
        })
    }

    /// Open a staged blob for sequential reading.
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if staging is disabled.
    /// Returns [`io::ErrorKind::NotFound`] if the blob is not staged.
    pub fn open_read(&self, digest: &Digest) -> Result<std::fs::File, io::Error> {
        let path = self.blob_path(digest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "blob staging is disabled")
        })?;
        std::fs::File::open(path)
    }

    /// Return the full path for a staged blob, or `None` if staging is
    /// disabled.
    fn blob_path(&self, digest: &Digest) -> Option<PathBuf> {
        let base = self.base_dir.as_ref()?;
        Some(
            base.join("blobs")
                .join(digest.algorithm())
                .join(digest.hex()),
        )
    }
}

/// A staged write in progress.
///
/// Data is written to a temporary file via [`write_chunk`](Self::write_chunk).
/// Call [`finish`](Self::finish) to fsync and atomically rename into the
/// content-addressable location. If dropped without calling `finish`, the
/// temporary file is removed immediately by the [`Drop`] impl to prevent
/// partial files from accumulating during error paths.
pub struct StagedWriter {
    file: std::fs::File,
    tmp_path: PathBuf,
    dest: PathBuf,
    /// Set to `true` by [`finish`](Self::finish). When `false` at drop time,
    /// the temporary file is removed to prevent partial files from accumulating.
    finished: bool,
}

impl std::fmt::Debug for StagedWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedWriter")
            .field("dest", &self.dest)
            .finish_non_exhaustive()
    }
}

impl StagedWriter {
    /// Write a chunk of data to the staged file.
    pub fn write_chunk(&mut self, data: &[u8]) -> Result<(), io::Error> {
        use std::io::Write;
        self.file.write_all(data)
    }

    /// Fsync the file and atomically rename it into the content-addressable
    /// location, followed by a directory fsync for durability.
    pub fn finish(mut self) -> Result<(), io::Error> {
        self.file.sync_all()?;
        std::fs::rename(&self.tmp_path, &self.dest)?;
        best_effort_dir_fsync(&self.dest);
        self.finished = true;
        Ok(())
    }
}

impl Drop for StagedWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

/// Check whether a file is a staging temporary file.
///
/// Temp files follow the naming convention `{hex_digest}.tmp.{pid}`, so the
/// filename always contains `.tmp.` as a non-leading infix. This is stricter
/// than plain `contains(".tmp.")` because it also verifies the prefix is a
/// plausible hex digest (non-empty portion before the first `.tmp.`).
fn is_tmp_file(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            // Pattern: {hex_digest}.tmp.{pid}
            // The hex digest is at least 1 char, so ".tmp." must not be at position 0.
            name.find(".tmp.").is_some_and(|pos| pos > 0)
        })
}

/// Read all directory entries as `PathBuf`s.
fn read_dir_entries(dir: &std::path::Path) -> Result<Vec<PathBuf>, io::Error> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        entries.push(entry?.path());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> Digest {
        Digest::from_sha256(ocync_distribution::sha256::Sha256::digest(b"blob"))
    }

    #[test]
    fn a_staged_blob_is_read_from_disk_not_pulled() {
        let dir = tempfile::tempdir().unwrap();
        let stage = BlobStage::new(dir.path().to_path_buf());
        stage.write(&digest(), b"blob").unwrap();
        assert!(matches!(
            stage.claim_or_check(&digest()),
            StagePullAction::Exists
        ));
    }

    /// A pull abandoned before staging (failed, or resolved by mount or
    /// HEAD) lets a waiter pull from its own source.
    #[test]
    fn a_dropped_pull_claim_lets_the_next_caller_pull() {
        let dir = tempfile::tempdir().unwrap();
        let stage = BlobStage::new(dir.path().to_path_buf());
        let StagePullAction::Pull(guard) = stage.claim_or_check(&digest()) else {
            panic!("first caller must pull")
        };
        assert!(matches!(
            stage.claim_or_check(&digest()),
            StagePullAction::Wait(_)
        ));
        drop(guard);
        assert!(matches!(
            stage.claim_or_check(&digest()),
            StagePullAction::Pull(_)
        ));
    }
}
