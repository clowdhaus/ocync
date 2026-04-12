//! Progress reporting trait and no-op implementation.

use crate::{ImageResult, SyncReport};

/// Reports progress during a sync run.
pub trait ProgressReporter: Send + Sync {
    /// Called when an image transfer begins.
    fn image_started(&self, source: &str, target: &str);
    /// Called when an individual image transfer completes.
    fn image_completed(&self, result: &ImageResult);
    /// Called when the entire sync run completes.
    fn run_completed(&self, report: &SyncReport);
}

/// No-op progress reporter for headless / testing use.
#[derive(Debug)]
pub struct NullProgress;

impl ProgressReporter for NullProgress {
    fn image_started(&self, _: &str, _: &str) {}
    fn image_completed(&self, _: &ImageResult) {}
    fn run_completed(&self, _: &SyncReport) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::*;
    use crate::{ImageStatus, SyncStats};

    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn null_progress_is_send_sync() {
        _assert_send_sync::<NullProgress>();
    }

    #[test]
    fn null_progress_methods_do_not_panic() {
        let p = NullProgress;
        p.image_started("source/repo:tag", "target/repo:tag");

        let result = ImageResult {
            image_id: Uuid::now_v7(),
            source: "src".into(),
            target: "tgt".into(),
            status: ImageStatus::Synced,
            bytes_transferred: 1024,
            blob_stats: crate::BlobTransferStats::default(),
            duration: Duration::from_secs(1),
        };
        p.image_completed(&result);

        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::from_secs(5),
        };
        p.run_completed(&report);
    }
}
