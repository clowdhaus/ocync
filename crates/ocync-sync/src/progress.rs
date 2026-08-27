//! Progress reporting trait and no-op implementation.

use std::time::Duration;

use crate::{ImageResult, SyncReport};

/// Snapshot of a run that is still in flight.
///
/// Passed to [`ProgressReporter::run_progress`] on a fixed cadence so a long
/// sync emits a liveness signal instead of going silent between the first
/// image starting and the last one finishing.
#[derive(Debug, Clone, Copy)]
pub struct RunProgress {
    /// Tags still in discovery (source HEAD or manifest pull).
    pub discovering: usize,
    /// Discovered transfers waiting for a concurrency permit.
    pub pending: usize,
    /// Transfers currently executing.
    pub in_flight: usize,
    /// Images that have reached a terminal state.
    pub completed: usize,
    /// Wall-clock time since the run started.
    pub elapsed: Duration,
}

/// Reports progress during a sync run.
///
/// No `Send + Sync` bound: the engine runs on a single-threaded tokio runtime
/// with `Rc<RefCell<>>` for shared state, so progress reporters can use non-Send
/// types like `Rc`.
pub trait ProgressReporter {
    /// Called when an image transfer begins.
    fn image_started(&self, source: &str, target: &str);
    /// Called when an individual image transfer completes.
    fn image_completed(&self, result: &ImageResult);
    /// Called on a fixed cadence while the run still has work in flight.
    ///
    /// Discovery and execution can both run for minutes without an image
    /// reaching a terminal state, so this is the only signal a caller has
    /// that the run is progressing rather than wedged.
    fn run_progress(&self, progress: RunProgress);
    /// Called when the entire sync run completes.
    fn run_completed(&self, report: &SyncReport);
}

/// No-op progress reporter for headless / testing use.
#[derive(Debug)]
pub struct NullProgress;

impl ProgressReporter for NullProgress {
    fn image_started(&self, _: &str, _: &str) {}
    fn image_completed(&self, _: &ImageResult) {}
    fn run_progress(&self, _: RunProgress) {}
    fn run_completed(&self, _: &SyncReport) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::*;
    use crate::{ImageStatus, SyncStats};

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
            artifacts_skipped: false,
        };
        p.image_completed(&result);

        p.run_progress(RunProgress {
            discovering: 3,
            pending: 2,
            in_flight: 1,
            completed: 4,
            elapsed: Duration::from_secs(30),
        });

        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::from_secs(5),
        };
        p.run_completed(&report);
    }
}
