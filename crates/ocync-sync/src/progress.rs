//! Progress reporting trait and no-op implementation.

use std::time::Duration;

use crate::{ImageResult, SyncReport};

/// Snapshot of a run still in flight, passed to
/// [`ProgressReporter::run_heartbeat`] on a fixed cadence.
#[derive(Debug, Clone, Copy)]
pub struct RunProgress {
    /// Tags still in discovery (source HEAD or manifest pull).
    pub in_discovery: usize,
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
    /// Discovery and execution can run for minutes without an image reaching
    /// a terminal state, so this is the only liveness signal.
    fn run_heartbeat(&self, snapshot: RunProgress);
    /// Called when the entire sync run completes.
    fn run_completed(&self, report: &SyncReport);
}

/// No-op progress reporter for headless / testing use.
#[derive(Debug)]
pub struct NullProgress;

impl ProgressReporter for NullProgress {
    fn image_started(&self, _: &str, _: &str) {}
    fn image_completed(&self, _: &ImageResult) {}
    fn run_heartbeat(&self, _: RunProgress) {}
    fn run_completed(&self, _: &SyncReport) {}
}
