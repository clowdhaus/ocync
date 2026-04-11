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
