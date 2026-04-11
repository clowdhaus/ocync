//! OCI registry sync orchestration — tag filtering, transfer planning, and execution.

/// Error types for sync operations.
pub mod error;

/// Tag filtering pipeline: glob, semver, exclude, sort, and latest.
pub mod filter;
/// Progress reporting trait and types.
pub mod progress;

use std::time::Duration;

pub use error::Error;
use serde::Serialize;
use ulid::Ulid;

/// Result of a complete sync run. The engine never "fails" as a whole.
#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub run_id: Ulid,
    pub images: Vec<ImageResult>,
    pub stats: SyncStats,
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

#[derive(Debug, Serialize)]
pub struct ImageResult {
    pub image_id: Ulid,
    pub source: String,
    pub target: String,
    pub status: ImageStatus,
    pub bytes_transferred: u64,
    pub duration: Duration,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ImageStatus {
    Synced,
    Skipped { reason: SkipReason },
    Failed { error: String, retries: u32 },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    DigestMatch,
    SkipExisting,
    ImmutableTag,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncStats {
    pub images_synced: u64,
    pub images_skipped: u64,
    pub images_failed: u64,
    pub layers_transferred: u64,
    pub layers_unique_refs: u64,
    pub layers_mounted: u64,
    pub bytes_transferred: u64,
    pub total_image_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_report(statuses: Vec<ImageStatus>) -> SyncReport {
        SyncReport {
            run_id: Ulid::new(),
            images: statuses
                .into_iter()
                .map(|status| ImageResult {
                    image_id: Ulid::new(),
                    source: "src".into(),
                    target: "tgt".into(),
                    status,
                    bytes_transferred: 0,
                    duration: Duration::ZERO,
                })
                .collect(),
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        }
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
        assert_eq!(stats.layers_transferred, 0);
        assert_eq!(stats.layers_unique_refs, 0);
        assert_eq!(stats.layers_mounted, 0);
        assert_eq!(stats.bytes_transferred, 0);
        assert_eq!(stats.total_image_size, 0);
    }
}
