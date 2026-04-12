//! Progress reporting trait and no-op implementation.

use crate::{ImageResult, SyncReport};

/// Reports progress during a sync run.
pub trait ProgressReporter: Send + Sync {
    /// Called when an image transfer begins.
    fn image_started(&self, source: &str, target: &str);
    /// Called periodically with blob transfer progress.
    fn blob_progress(&self, source: &str, bytes_transferred: u64, total_bytes: u64);
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
    fn blob_progress(&self, _: &str, _: u64, _: u64) {}
    fn image_completed(&self, _: &ImageResult) {}
    fn run_completed(&self, _: &SyncReport) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ulid::Ulid;

    use super::*;
    use crate::{ImageStatus, SyncStats};

    #[test]
    fn null_progress_methods_do_not_panic() {
        let p = NullProgress;
        p.image_started("source/repo:tag", "target/repo:tag");
        p.blob_progress("source/repo:tag", 512, 1024);

        let result = ImageResult {
            image_id: Ulid::new(),
            source: "src".into(),
            target: "tgt".into(),
            status: ImageStatus::Synced,
            bytes_transferred: 1024,
            duration: Duration::from_secs(1),
        };
        p.image_completed(&result);

        let report = SyncReport {
            run_id: Ulid::new(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::from_secs(5),
        };
        p.run_completed(&report);
    }
}
