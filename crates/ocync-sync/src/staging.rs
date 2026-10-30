//! Content-addressable disk staging for multi-target blob reuse: each blob is
//! pulled from the source once, written here, and read from this file by every
//! target push. [`BlobStage::disabled`] makes every operation a no-op for
//! single-target runs.
//!
//! # File layout
//!
//! ```text
//! {base_dir}/blobs/{algorithm}/{hex_digest}
//! ```
//!
//! # Atomic write protocol
//!
//! Writes go to a `{digest}.tmp.{pid}.{seq}` file, are fsynced and renamed into
//! place, then the directory is fsynced, so an incomplete write is never
//! visible as a staged blob. [`BlobStage::cleanup_tmp_files`] removes orphans
//! left by a crash.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use ocync_distribution::Digest;
use tokio::sync::Notify;

use crate::shared::{Claim, ClaimGuard, Work, claim};

/// Sequence for temporary file names, shared by [`BlobStage::write`] and
/// [`BlobStage::begin_write`] so the two never collide on one digest.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Best-effort directory fsync after an atomic rename, so the rename survives
/// power loss. An open failure is ignored and an fsync failure is logged: the
/// data is already reachable, and only a crash before the journal flush can
/// lose it.
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

/// Action returned by [`BlobStage::claim_or_check`] so concurrent image tasks
/// pull one blob from the source only once.
pub(crate) enum StagePullAction<'a> {
    /// Blob already staged: skip the source pull.
    Exists,
    /// This task pulls from the source and stages the blob. Dropping the
    /// guard, once the blob is staged or the pull is abandoned, wakes waiters.
    Pull(ClaimGuard<'a, Digest, ()>),
    /// Another task is pulling this blob. Await the [`Notify`], then call
    /// `claim_or_check` again (the pull may have failed).
    Wait(Rc<Notify>),
}

/// Content-addressable blob staging area for multi-target sync. When disabled
/// (single-target), every method is a no-op.
///
/// # Staging path requirements
///
/// The staging directory MUST reside on local disk (tmpfs, ext4, XFS, APFS,
/// etc.). The engine reads staged files synchronously on the single-threaded
/// tokio runtime, so a network filesystem (NFS, EFS, CIFS, `GlusterFS`) would
/// block the event loop for each read and stall all concurrent transfers.
#[derive(Debug)]
pub struct BlobStage {
    base_dir: Option<PathBuf>,
    /// In-progress source pulls keyed by digest. No outcome is recorded:
    /// the staged file is the record, and a failed pull is retried by a
    /// waiter, whose source may differ.
    pulls: RefCell<Work<Digest, ()>>,
}

impl BlobStage {
    /// Create a staging area rooted at `base_dir`, created on first write.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir: Some(base_dir),
            pulls: RefCell::new(Work::default()),
        }
    }

    /// Create a disabled staging area for single-target deployments.
    pub fn disabled() -> Self {
        Self {
            base_dir: None,
            pulls: RefCell::new(Work::default()),
        }
    }

    /// Whether staging is enabled.
    pub fn is_enabled(&self) -> bool {
        self.base_dir.is_some()
    }

    /// Whether the blob is already staged; always `false` when disabled.
    pub fn exists(&self, digest: &Digest) -> bool {
        self.blob_path(digest).is_some_and(|p| p.exists())
    }

    /// Check-and-claim for source-pull dedup. Only called with staging enabled.
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

    /// Atomically write `data` as the staged blob for `digest`.
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if staging is disabled.
    pub fn write(&self, digest: &Digest, data: &[u8]) -> Result<(), io::Error> {
        let dest = self.blob_path(digest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "blob staging is disabled")
        })?;

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

    /// Delete orphaned `.tmp.*` files left by a crash. Call once at startup,
    /// before other staging operations. No-op when disabled.
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

    /// Evict staged blobs, smallest first, until the total staged size is at or
    /// below `max_bytes`. Temporary files (`.tmp.*`) are never evicted. No-op
    /// when disabled.
    pub fn evict(&self, max_bytes: u64) -> Result<(), io::Error> {
        let base = match &self.base_dir {
            Some(d) => d,
            None => return Ok(()),
        };

        let blobs_dir = base.join("blobs");
        if !blobs_dir.exists() {
            return Ok(());
        }

        let mut entries: Vec<(PathBuf, u64)> = Vec::new();
        let mut total_bytes: u64 = 0;

        for algo_entry in read_dir_entries(&blobs_dir)? {
            for file_entry in read_dir_entries(&algo_entry)? {
                if is_tmp_file(&file_entry) {
                    continue;
                }

                // `symlink_metadata` does not follow symlinks, so a symlink is
                // skipped rather than having its target deleted.
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

    /// Begin a chunked atomic write; [`StagedWriter::finish`] renames it into
    /// place.
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

    /// Open a staged blob for reading.
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if staging is disabled.
    /// Returns [`io::ErrorKind::NotFound`] if the blob is not staged.
    pub fn open_read(&self, digest: &Digest) -> Result<std::fs::File, io::Error> {
        let path = self.blob_path(digest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "blob staging is disabled")
        })?;
        std::fs::File::open(path)
    }

    /// Path of a staged blob, or `None` if staging is disabled.
    fn blob_path(&self, digest: &Digest) -> Option<PathBuf> {
        let base = self.base_dir.as_ref()?;
        Some(
            base.join("blobs")
                .join(digest.algorithm())
                .join(digest.hex()),
        )
    }
}

/// A staged write in progress. Dropping it without calling
/// [`finish`](Self::finish) removes the temporary file, so error paths leave no
/// partial files.
pub struct StagedWriter {
    file: std::fs::File,
    tmp_path: PathBuf,
    dest: PathBuf,
    /// Set by [`finish`](Self::finish); `Drop` removes the temp file otherwise.
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

    /// Fsync, rename into the content-addressable location, then fsync the
    /// directory.
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

/// Whether `path` names a staging temporary file: a non-empty digest prefix
/// followed by `.tmp.`.
fn is_tmp_file(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.find(".tmp.").is_some_and(|pos| pos > 0))
}

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
