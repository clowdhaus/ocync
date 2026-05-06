//! OCI registry sync orchestration - tag filtering, transfer planning, and execution.

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
/// Tag-version parser, comparator, and range matcher.
pub(crate) mod version;

/// Sync engine - pipelined concurrent orchestration of image transfers.
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

/// Serde helper: skip serializing when value equals its type's default.
fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

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
    /// Whether artifact discovery or transfer was skipped due to a transient error.
    ///
    /// When `true`, the image itself synced successfully but its referrers
    /// (signatures, SBOMs, attestations) may be missing at the target. Only
    /// `true` on the transient-error path -- when discovery confirms zero
    /// referrers, this remains `false`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub artifacts_skipped: bool,
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
        /// What operation failed.
        kind: ErrorKind,
        /// Error message describing the failure.
        error: String,
        /// Number of retry attempts made.
        retries: u32,
        /// HTTP status code from the failing request, if available.
        #[serde(skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
}

/// Reason an image was skipped during sync.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Source and target digests already match.
    DigestMatch,
    /// Target registry has immutable tags enabled and the tag already exists.
    ImmutableTag,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DigestMatch => f.write_str("digest match"),
            Self::ImmutableTag => f.write_str("immutable tag"),
        }
    }
}

/// Classification of the operation that failed during image transfer.
///
/// Classifies *what operation* failed, not *why* it failed. The `error`
/// string on [`ImageStatus::Failed`] carries the cause. This separation
/// lets output formatters group by operation type while preserving full
/// error context.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Source manifest could not be pulled.
    ManifestPull,
    /// Target manifest could not be pushed.
    ManifestPush,
    /// Blob transfer (pull, push, or mount) failed.
    BlobTransfer,
    /// Artifact (referrer) discovery or transfer failed.
    ArtifactSync,
    /// Required artifacts are missing (policy enforcement).
    RequiredArtifactsMissing,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestPull => f.write_str("manifest pull"),
            Self::ManifestPush => f.write_str("manifest push"),
            Self::BlobTransfer => f.write_str("blob transfer"),
            Self::ArtifactSync => f.write_str("artifact sync"),
            Self::RequiredArtifactsMissing => f.write_str("required artifacts missing"),
        }
    }
}

/// Aggregate statistics for a sync run.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
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
    /// Tags where the HEAD optimization avoided the full source pull.
    pub discovery_cache_hits: u64,
    /// Tags where a full source manifest pull was required.
    pub discovery_cache_misses: u64,
    /// Tags where the source HEAD request failed (network, timeout, bad digest).
    /// Subset of `discovery_cache_misses`.
    pub discovery_head_failures: u64,
    /// Tags where the source cache matched but a full pull was still needed
    /// because a target HEAD showed staleness. Counted toward
    /// `discovery_cache_misses` for aggregate purposes.
    pub discovery_target_stale: u64,
    /// Tags where `head_first` avoided the full source GET by confirming all
    /// targets already match the source HEAD digest on cache miss. Independent
    /// of `discovery_cache_hits` and `discovery_cache_misses` -- the cache had
    /// no entry, but no full pull was needed either.
    pub discovery_head_first_skips: u64,
    /// (Tag, target) pairs skipped via immutable-glob match (zero API calls).
    ///
    /// Unit is per-target: 2 tags across 3 targets = 6. Consistent with
    /// `images_skipped` which also counts per-target.
    pub immutable_tag_skips: u64,
    /// Images where artifact discovery or transfer was skipped due to a
    /// transient error (e.g. referrers API returned 500).
    #[serde(default, skip_serializing_if = "is_default")]
    pub artifacts_skipped: u64,
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
                    artifacts_skipped: false,
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
                kind: ErrorKind::BlobTransfer,
                error: "timeout".into(),
                retries: 3,
                status_code: None,
            },
        ]);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn exit_code_all_failed() {
        let report = make_report(vec![
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPull,
                error: "a".into(),
                retries: 1,
                status_code: None,
            },
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                error: "b".into(),
                retries: 2,
                status_code: None,
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
        assert_eq!(stats.artifacts_skipped, 0);
    }

    #[test]
    fn skip_reason_display() {
        assert_eq!(SkipReason::DigestMatch.to_string(), "digest match");
    }

    #[test]
    fn skip_reason_display_immutable_tag() {
        assert_eq!(SkipReason::ImmutableTag.to_string(), "immutable tag");
    }

    #[test]
    fn error_kind_display_manifest_pull() {
        assert_eq!(ErrorKind::ManifestPull.to_string(), "manifest pull");
    }

    #[test]
    fn error_kind_display_manifest_push() {
        assert_eq!(ErrorKind::ManifestPush.to_string(), "manifest push");
    }

    #[test]
    fn error_kind_display_blob_transfer() {
        assert_eq!(ErrorKind::BlobTransfer.to_string(), "blob transfer");
    }

    #[test]
    fn error_kind_display_artifact_sync() {
        assert_eq!(ErrorKind::ArtifactSync.to_string(), "artifact sync");
    }

    #[test]
    fn error_kind_display_required_artifacts_missing() {
        assert_eq!(
            ErrorKind::RequiredArtifactsMissing.to_string(),
            "required artifacts missing"
        );
    }

    #[test]
    fn blob_transfer_stats_default_is_zeroed() {
        let stats = BlobTransferStats::default();
        assert_eq!(stats.transferred, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.mounted, 0);
    }

    #[test]
    fn error_kind_json_serialization() {
        assert_eq!(
            serde_json::to_value(ErrorKind::ManifestPull).unwrap(),
            serde_json::Value::String("manifest_pull".into()),
        );
        assert_eq!(
            serde_json::to_value(ErrorKind::ManifestPush).unwrap(),
            serde_json::Value::String("manifest_push".into()),
        );
        assert_eq!(
            serde_json::to_value(ErrorKind::BlobTransfer).unwrap(),
            serde_json::Value::String("blob_transfer".into()),
        );
        assert_eq!(
            serde_json::to_value(ErrorKind::ArtifactSync).unwrap(),
            serde_json::Value::String("artifact_sync".into()),
        );
        assert_eq!(
            serde_json::to_value(ErrorKind::RequiredArtifactsMissing).unwrap(),
            serde_json::Value::String("required_artifacts_missing".into()),
        );
    }

    #[test]
    fn image_status_failed_json_includes_kind() {
        let status = ImageStatus::Failed {
            kind: ErrorKind::ManifestPull,
            error: "timeout".into(),
            retries: 3,
            status_code: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["status"], "failed");
        assert_eq!(json["kind"], "manifest_pull");
        assert_eq!(json["error"], "timeout");
        assert_eq!(json["retries"], 3);
        assert!(json.get("status_code").is_none(), "None should be skipped");
    }

    #[test]
    fn image_status_failed_json_includes_status_code() {
        let status = ImageStatus::Failed {
            kind: ErrorKind::ManifestPull,
            error: "unauthorized".into(),
            retries: 1,
            status_code: Some(401),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["status_code"], 401);
    }

    #[test]
    fn image_result_json_omits_artifacts_skipped_when_false() {
        let result = ImageResult {
            image_id: Uuid::now_v7(),
            source: "src".into(),
            target: "tgt".into(),
            status: ImageStatus::Synced,
            bytes_transferred: 0,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::ZERO,
            artifacts_skipped: false,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(
            json.get("artifacts_skipped").is_none(),
            "artifacts_skipped=false should be omitted from JSON"
        );
    }

    #[test]
    fn image_result_json_includes_artifacts_skipped_when_true() {
        let result = ImageResult {
            image_id: Uuid::now_v7(),
            source: "src".into(),
            target: "tgt".into(),
            status: ImageStatus::Synced,
            bytes_transferred: 0,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::ZERO,
            artifacts_skipped: true,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(
            json["artifacts_skipped"], true,
            "artifacts_skipped=true should appear in JSON"
        );
    }

    #[test]
    fn sync_stats_json_omits_artifacts_skipped_when_zero() {
        let stats = SyncStats::default();
        let json = serde_json::to_value(&stats).unwrap();
        assert!(
            json.get("artifacts_skipped").is_none(),
            "artifacts_skipped=0 should be omitted from JSON"
        );
    }

    #[test]
    fn sync_stats_json_includes_artifacts_skipped_when_nonzero() {
        let stats = SyncStats {
            artifacts_skipped: 3,
            ..SyncStats::default()
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(
            json["artifacts_skipped"], 3,
            "artifacts_skipped=3 should appear in JSON"
        );
    }
}
