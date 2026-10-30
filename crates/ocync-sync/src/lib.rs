//! OCI registry sync orchestration - tag filtering, transfer planning, and execution.
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests read JSON they wrote themselves"
    )
)]

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
/// Per-run work shared between tasks that need the same content.
pub(crate) mod shared;
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
    /// Whether any image synced or was skipped.
    ///
    /// Public so a caller that tracks failures the report cannot see (a
    /// mapping dropped before the engine ran) can fold them into the exit
    /// code without re-deriving this predicate.
    pub fn has_success(&self) -> bool {
        self.images
            .iter()
            .any(|i| matches!(i.status, ImageStatus::Synced | ImageStatus::Skipped { .. }))
    }

    /// Process exit code: 0 with no failed image, 1 when some failed and some
    /// synced or were skipped, 2 when every image failed.
    pub fn exit_code(&self) -> i32 {
        let has_success = self.has_success();
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
    /// When `true`, the image synced but its referrers (signatures, SBOMs,
    /// attestations) may be missing at the target. Stays `false` when
    /// discovery confirms zero referrers.
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
        /// Number of retry attempts this image made, across all its requests.
        /// Retries spent on work another image did for both (a shared child
        /// manifest, blob copy or artifact) are counted by that image. Retries
        /// pulling the source manifest count for every target it syncs to.
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

/// Which operation failed during image transfer, not why. The `error` string
/// on [`ImageStatus::Failed`] carries the cause, so output formatters can
/// group by operation and keep the full context.
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
    /// of `discovery_cache_hits` and `discovery_cache_misses`: the cache had
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

    fn image_result(status: ImageStatus, artifacts_skipped: bool) -> ImageResult {
        ImageResult {
            image_id: Uuid::now_v7(),
            source: "src".into(),
            target: "tgt".into(),
            status,
            bytes_transferred: 0,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::ZERO,
            artifacts_skipped,
        }
    }

    fn make_report(statuses: Vec<ImageStatus>) -> SyncReport {
        SyncReport {
            run_id: Uuid::now_v7(),
            images: statuses
                .into_iter()
                .map(|status| image_result(status, false))
                .collect(),
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        }
    }

    fn failed(kind: ErrorKind, error: &str, retries: u32, status_code: Option<u16>) -> ImageStatus {
        ImageStatus::Failed {
            kind,
            error: error.into(),
            retries,
            status_code,
        }
    }

    #[test]
    fn exit_code_by_outcome() {
        let cases = [
            ("empty report", vec![], 0),
            (
                "all synced",
                vec![ImageStatus::Synced, ImageStatus::Synced],
                0,
            ),
            (
                "all skipped",
                vec![ImageStatus::Skipped {
                    reason: SkipReason::DigestMatch,
                }],
                0,
            ),
            (
                "mixed success and failure",
                vec![
                    ImageStatus::Synced,
                    failed(ErrorKind::BlobTransfer, "timeout", 3, None),
                ],
                1,
            ),
            (
                "all failed",
                vec![
                    failed(ErrorKind::ManifestPull, "a", 1, None),
                    failed(ErrorKind::ManifestPush, "b", 2, None),
                ],
                2,
            ),
        ];
        for (name, statuses, want) in cases {
            assert_eq!(make_report(statuses).exit_code(), want, "{name}");
        }
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
        assert_eq!(SkipReason::ImmutableTag.to_string(), "immutable tag");
    }

    #[test]
    fn error_kind_display_and_json() {
        // (kind, Display text, JSON string)
        let cases = [
            (ErrorKind::ManifestPull, "manifest pull", "manifest_pull"),
            (ErrorKind::ManifestPush, "manifest push", "manifest_push"),
            (ErrorKind::BlobTransfer, "blob transfer", "blob_transfer"),
            (ErrorKind::ArtifactSync, "artifact sync", "artifact_sync"),
            (
                ErrorKind::RequiredArtifactsMissing,
                "required artifacts missing",
                "required_artifacts_missing",
            ),
        ];
        for (kind, display, json) in cases {
            assert_eq!(kind.to_string(), display, "{json} display");
            assert_eq!(
                serde_json::to_value(&kind).unwrap(),
                serde_json::Value::String(json.into()),
                "{json} json"
            );
        }
    }

    #[test]
    fn blob_transfer_stats_default_is_zeroed() {
        let stats = BlobTransferStats::default();
        assert_eq!(stats.transferred, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.mounted, 0);
    }

    #[test]
    fn image_status_failed_json() {
        // (name, error, retries, status_code)
        let cases = [
            ("no status code", "timeout", 3, None),
            ("with status code", "unauthorized", 1, Some(401)),
        ];
        for (name, error, retries, status_code) in cases {
            let json =
                serde_json::to_value(failed(ErrorKind::ManifestPull, error, retries, status_code))
                    .unwrap();
            assert_eq!(json["status"], "failed", "{name}");
            assert_eq!(json["kind"], "manifest_pull", "{name}");
            assert_eq!(json["error"], error, "{name}");
            assert_eq!(json["retries"], retries, "{name}");
            match status_code {
                Some(code) => assert_eq!(json["status_code"], code, "{name}"),
                None => assert!(json.get("status_code").is_none(), "{name}: None is skipped"),
            }
        }
    }

    #[test]
    fn image_result_json_artifacts_skipped() {
        // (name, flag, expected JSON value; None means the key is omitted)
        let cases = [("false", false, None), ("true", true, Some(true))];
        for (name, flag, want) in cases {
            let json = serde_json::to_value(image_result(ImageStatus::Synced, flag)).unwrap();
            match want {
                Some(v) => assert_eq!(json["artifacts_skipped"], v, "{name}"),
                None => assert!(json.get("artifacts_skipped").is_none(), "{name}: omitted"),
            }
        }
    }

    #[test]
    fn sync_stats_json_artifacts_skipped() {
        // (name, count, expected JSON value; None means the key is omitted)
        let cases = [("zero", 0, None), ("nonzero", 3, Some(3))];
        for (name, count, want) in cases {
            let stats = SyncStats {
                artifacts_skipped: count,
                ..SyncStats::default()
            };
            let json = serde_json::to_value(&stats).unwrap();
            match want {
                Some(v) => assert_eq!(json["artifacts_skipped"], v, "{name}"),
                None => assert!(json.get("artifacts_skipped").is_none(), "{name}: omitted"),
            }
        }
    }
}
