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
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use ocync_distribution::Digest;
use tokio::sync::Notify;

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
#[derive(Debug)]
pub enum StagePullAction {
    /// Blob already staged on disk. Skip source pull, read from staging.
    Exists,
    /// Claimed: this task should pull from source and stage the blob.
    /// Call [`BlobStage::notify_staged`] after writing, or
    /// [`BlobStage::notify_failed`] on failure.
    Pull,
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
/// Cross-image source-pull dedup is coordinated via [`claim_or_check`]:
/// the first task to request a digest claims the pull, concurrent tasks wait
/// for the file to appear, then all read from the same staged file.
#[derive(Debug)]
pub struct BlobStage {
    base_dir: Option<PathBuf>,
    /// In-progress source pulls keyed by digest. When a task claims a pull,
    /// it inserts a `Notify`; waiters clone and `.notified().await` on it.
    /// Removed on completion ([`notify_staged`]) or failure ([`notify_failed`]).
    pulls: RefCell<HashMap<Digest, Rc<Notify>>>,
}

impl BlobStage {
    /// Create a staging area rooted at `base_dir`.
    ///
    /// The directory and its parents are created lazily on the first write.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir: Some(base_dir),
            pulls: RefCell::new(HashMap::new()),
        }
    }

    /// Create a disabled (no-op) staging area for single-target deployments.
    ///
    /// All operations on a disabled stage are no-ops; no disk I/O occurs.
    pub fn disabled() -> Self {
        Self {
            base_dir: None,
            pulls: RefCell::new(HashMap::new()),
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

    /// Check-and-claim for source-pull dedup.
    ///
    /// Returns [`StagePullAction::Exists`] if the blob is already on disk,
    /// [`StagePullAction::Pull`] if this task should pull it (claiming the
    /// digest), or [`StagePullAction::Wait`] if another task is pulling it.
    ///
    /// When staging is disabled, always returns `Pull` (no dedup possible).
    ///
    /// Callers must call [`notify_staged`] after a successful pull or
    /// [`notify_failed`] on failure. Failing to do so will deadlock waiters.
    pub fn claim_or_check(&self, digest: &Digest) -> StagePullAction {
        if !self.is_enabled() {
            return StagePullAction::Pull;
        }
        if self.exists(digest) {
            return StagePullAction::Exists;
        }
        let mut pulls = self.pulls.borrow_mut();
        if let Some(notify) = pulls.get(digest) {
            StagePullAction::Wait(Rc::clone(notify))
        } else {
            pulls.insert(digest.clone(), Rc::new(Notify::new()));
            StagePullAction::Pull
        }
    }

    /// Signal that a staged pull completed successfully.
    ///
    /// Removes the in-progress entry and wakes any waiting tasks. Waiters
    /// will re-enter [`claim_or_check`] and find the blob on disk via
    /// [`exists`].
    pub fn notify_staged(&self, digest: &Digest) {
        if let Some(notify) = self.pulls.borrow_mut().remove(digest) {
            notify.notify_waiters();
        }
    }

    /// Signal that a staged pull failed.
    ///
    /// Removes the in-progress entry and wakes waiters so one of them can
    /// retry the pull from its own source.
    pub fn notify_failed(&self, digest: &Digest) {
        if let Some(notify) = self.pulls.borrow_mut().remove(digest) {
            notify.notify_waiters();
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

        use std::sync::atomic::{AtomicU64, Ordering};
        static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);
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

                let size = std::fs::metadata(&file_entry).map(|m| m.len()).unwrap_or(0);
                total_bytes += size;
                entries.push((file_entry, size));
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dest.with_extension(format!("tmp.{}.{seq}", std::process::id()));
        let file = std::fs::File::create(&tmp_path)?;
        Ok(StagedWriter {
            file,
            tmp_path,
            dest,
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
/// temporary file remains on disk and will be cleaned up by
/// [`BlobStage::cleanup_tmp_files`] on the next startup.
pub struct StagedWriter {
    file: std::fs::File,
    tmp_path: PathBuf,
    dest: PathBuf,
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
    pub fn finish(self) -> Result<(), io::Error> {
        self.file.sync_all()?;
        std::fs::rename(&self.tmp_path, &self.dest)?;
        best_effort_dir_fsync(&self.dest);
        Ok(())
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
