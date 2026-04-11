use crate::{ImageResult, SyncReport};

/// Reports progress during a sync run.
pub trait ProgressReporter: Send + Sync {
    fn image_started(&self, source: &str, target: &str);
    fn blob_progress(&self, source: &str, bytes_transferred: u64, total_bytes: u64);
    fn image_completed(&self, result: &ImageResult);
    fn run_completed(&self, report: &SyncReport);
}

/// No-op progress reporter for headless / testing use.
pub struct NullProgress;

impl ProgressReporter for NullProgress {
    fn image_started(&self, _: &str, _: &str) {}
    fn blob_progress(&self, _: &str, _: u64, _: u64) {}
    fn image_completed(&self, _: &ImageResult) {}
    fn run_completed(&self, _: &SyncReport) {}
}
