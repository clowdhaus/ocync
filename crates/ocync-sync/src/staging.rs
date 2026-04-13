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

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use ocync_distribution::Digest;

/// Content-addressable blob staging area for multi-target sync.
///
/// When enabled, blobs are pulled once from source and stored locally so all
/// target pushes can read from the local file rather than re-pulling from
/// source. When disabled (single-target), all methods are no-ops.
#[derive(Debug)]
pub struct BlobStage {
    base_dir: Option<PathBuf>,
}

impl BlobStage {
    /// Create a staging area rooted at `base_dir`.
    ///
    /// The directory and its parents are created lazily on the first write.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir: Some(base_dir),
        }
    }

    /// Create a disabled (no-op) staging area for single-target deployments.
    ///
    /// All operations on a disabled stage are no-ops; no disk I/O occurs.
    pub fn disabled() -> Self {
        Self { base_dir: None }
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

        let tmp_path = dest.with_extension(format!("tmp.{}", std::process::id()));

        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(data)?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp_path, &dest)?;

        // fsync parent directory so the rename is durable.
        if let Some(parent) = dest.parent() {
            let dir = std::fs::File::open(parent)?;
            let _ = dir.sync_all();
        }

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
                let file_name = file_entry.file_name();
                let name = file_name
                    .as_ref()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                if name.contains(".tmp.") {
                    std::fs::remove_file(&file_entry)?;
                }
            }
        }

        Ok(())
    }

    /// Evict staged blobs until the total staged size is at or below
    /// `max_bytes`.
    ///
    /// Blobs are evicted in ascending order of their reference count (least-
    /// referenced first). Blobs with no entry in `ref_counts` are treated as
    /// having a reference count of zero (evicted first). Temporary files
    /// (`.tmp.*`) are never evicted. This is a no-op when staging is disabled.
    pub fn evict(
        &self,
        max_bytes: u64,
        ref_counts: &HashMap<Digest, usize>,
    ) -> Result<(), io::Error> {
        let base = match &self.base_dir {
            Some(d) => d,
            None => return Ok(()),
        };

        let blobs_dir = base.join("blobs");
        if !blobs_dir.exists() {
            return Ok(());
        }

        // Collect all staged blobs with their sizes and digests.
        let mut entries: Vec<(PathBuf, u64, Digest)> = Vec::new();
        let mut total_bytes: u64 = 0;

        for algo_entry in read_dir_entries(&blobs_dir)? {
            let algo_os_name = algo_entry.file_name();
            let algo_name = algo_os_name
                .as_ref()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            for file_entry in read_dir_entries(&algo_entry)? {
                let entry_os_name = file_entry.file_name();
                let file_name = entry_os_name
                    .as_ref()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();

                // Skip temporary files.
                if file_name.contains(".tmp.") {
                    continue;
                }

                let digest_str = format!("{algo_name}:{file_name}");
                let digest: Digest = match digest_str.parse() {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                let size = std::fs::metadata(&file_entry).map(|m| m.len()).unwrap_or(0);

                total_bytes += size;
                entries.push((file_entry, size, digest));
            }
        }

        if total_bytes <= max_bytes {
            return Ok(());
        }

        // Sort by ascending reference count (evict least-referenced first).
        entries.sort_by_key(|(_, _, digest)| ref_counts.get(digest).copied().unwrap_or(0));

        for (path, size, _digest) in entries {
            if total_bytes <= max_bytes {
                break;
            }
            std::fs::remove_file(&path)?;
            total_bytes = total_bytes.saturating_sub(size);
        }

        Ok(())
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

/// Read all directory entries as `PathBuf`s.
fn read_dir_entries(dir: &std::path::Path) -> Result<Vec<PathBuf>, io::Error> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        entries.push(entry?.path());
    }
    Ok(entries)
}
