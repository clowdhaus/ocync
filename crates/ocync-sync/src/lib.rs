//! OCI registry sync orchestration — tag filtering, transfer planning, and execution.

/// Persistent transfer state cache wrapping [`plan::BlobDedupMap`].
pub mod cache;
/// Error types for sync operations.
pub mod error;

/// Tag filtering pipeline: glob, semver, exclude, sort, and latest.
pub mod filter;
/// Sync planning and transfer ordering.
pub mod plan;
/// Progress reporting trait and types.
pub mod progress;
/// Retry configuration and backoff logic.
pub mod retry;

/// Sync engine -- pipelined concurrent orchestration of image transfers.
pub mod engine;
/// Cooperative shutdown signal for the sync engine.
pub mod shutdown;
/// Content-addressable disk staging for multi-target blob reuse.
pub mod staging;

use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

pub use error::Error;
pub use shutdown::ShutdownSignal;

/// Result of a complete sync run. The engine never "fails" as a whole.
#[derive(Debug, Serialize)]
pub struct SyncReport {
    /// Unique identifier for this sync run.
    pub run_id: Uuid,
    /// Per-image results.
    pub images: Vec<ImageResult>,
    /// Aggregate statistics.
    pub stats: SyncStats,
    /// Wall-clock duration of the entire run.
    pub duration: Duration,
}

impl SyncReport {
    /// Derive process exit code from results.
    pub fn exit_code(&self) -> i32 {
        let has_success = self
            .images
            .iter()
            .any(|i| matches!(i.status, ImageStatus::Synced | ImageStatus::Skipped { .. }));
        let has_failure = self
            .images
            .iter()
            .any(|i| matches!(i.status, ImageStatus::Failed { .. }));
        match (has_success, has_failure) {
            (_, false) => 0,
            (true, true) => 1,
            (false, true) => 2,
        }
    }
}

/// Result of syncing a single image.
#[derive(Debug, Serialize)]
pub struct ImageResult {
    /// Unique identifier for this image transfer.
    pub image_id: Uuid,
    /// Source image reference.
    pub source: String,
    /// Target image reference.
    pub target: String,
    /// Outcome of the transfer.
    pub status: ImageStatus,
    /// Total bytes transferred for this image.
    pub bytes_transferred: u64,
    /// Per-image blob transfer statistics.
    pub blob_stats: BlobTransferStats,
    /// Wall-clock duration of this image transfer.
    pub duration: Duration,
}

/// Per-image blob transfer statistics.
#[derive(Debug, Default, Clone, Serialize)]
pub struct BlobTransferStats {
    /// Blobs transferred over the network (pull+push).
    pub transferred: u64,
    /// Blobs skipped because they already existed at the target (dedup or HEAD).
    pub skipped: u64,
    /// Blobs satisfied via cross-repo mount.
    pub mounted: u64,
}

/// Outcome status for a single image transfer.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ImageStatus {
    /// Image was successfully synced.
    Synced,
    /// Image was skipped (already up-to-date or policy).
    Skipped {
        /// Why the image was skipped.
        reason: SkipReason,
    },
    /// Image transfer failed after exhausting retries.
    Failed {
        /// Error message describing the failure.
        error: String,
        /// Number of retry attempts made.
        retries: u32,
    },
}

/// Reason an image was skipped during sync.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Source and target digests already match.
    DigestMatch,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DigestMatch => f.write_str("digest match"),
        }
    }
}

/// Aggregate statistics for a sync run.
#[derive(Debug, Default, Serialize)]
pub struct SyncStats {
    /// Number of images successfully synced.
    pub images_synced: u64,
    /// Number of images skipped.
    pub images_skipped: u64,
    /// Number of images that failed.
    pub images_failed: u64,
    /// Total blobs transferred over the network (pull+push).
    pub blobs_transferred: u64,
    /// Blobs skipped because they already existed at the target.
    pub blobs_skipped: u64,
    /// Blobs satisfied via cross-repo mount.
    pub blobs_mounted: u64,
    /// Total bytes transferred over the network.
    pub bytes_transferred: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_report(statuses: Vec<ImageStatus>) -> SyncReport {
        SyncReport {
            run_id: Uuid::now_v7(),
            images: statuses
                .into_iter()
                .map(|status| ImageResult {
                    image_id: Uuid::now_v7(),
                    source: "src".into(),
                    target: "tgt".into(),
                    status,
                    bytes_transferred: 0,
                    blob_stats: BlobTransferStats::default(),
                    duration: Duration::ZERO,
                })
                .collect(),
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn exit_code_empty_report() {
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        };
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn exit_code_all_synced() {
        let report = make_report(vec![ImageStatus::Synced, ImageStatus::Synced]);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn exit_code_all_skipped() {
        let report = make_report(vec![ImageStatus::Skipped {
            reason: SkipReason::DigestMatch,
        }]);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn exit_code_mixed_success_and_failure() {
        let report = make_report(vec![
            ImageStatus::Synced,
            ImageStatus::Failed {
                error: "timeout".into(),
                retries: 3,
            },
        ]);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn exit_code_all_failed() {
        let report = make_report(vec![
            ImageStatus::Failed {
                error: "a".into(),
                retries: 1,
            },
            ImageStatus::Failed {
                error: "b".into(),
                retries: 2,
            },
        ]);
        assert_eq!(report.exit_code(), 2);
    }

    #[test]
    fn sync_stats_default_is_zeroed() {
        let stats = SyncStats::default();
        assert_eq!(stats.images_synced, 0);
        assert_eq!(stats.images_skipped, 0);
        assert_eq!(stats.images_failed, 0);
        assert_eq!(stats.blobs_transferred, 0);
        assert_eq!(stats.blobs_skipped, 0);
        assert_eq!(stats.blobs_mounted, 0);
        assert_eq!(stats.bytes_transferred, 0);
    }

    #[test]
    fn skip_reason_display() {
        assert_eq!(SkipReason::DigestMatch.to_string(), "digest match");
    }

    #[test]
    fn blob_transfer_stats_default_is_zeroed() {
        let stats = BlobTransferStats::default();
        assert_eq!(stats.transferred, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.mounted, 0);
    }
}
