//! Progress reporting integration tests: the in-flight heartbeat contract.

mod helpers;

use std::cell::RefCell;
use std::time::Duration;

use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::{ProgressReporter, RunProgress};
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageResult, ImageStatus, ShutdownSignal, SyncReport};
use wiremock::MockServer;

use helpers::*;

/// Records every [`RunProgress`] snapshot the engine emits.
#[derive(Default)]
struct RecordingProgress {
    snapshots: RefCell<Vec<RunProgress>>,
}

impl ProgressReporter for RecordingProgress {
    fn image_started(&self, _source: &str, _target: &str) {}
    fn image_completed(&self, _result: &ImageResult) {}
    fn run_progress(&self, progress: RunProgress) {
        self.snapshots.borrow_mut().push(progress);
    }
    fn run_completed(&self, _report: &SyncReport) {}
}

/// Run a two-image sync with the given heartbeat cadence.
async fn run_with_heartbeat(interval: Duration) -> (SyncReport, Vec<RunProgress>) {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    for tag in ["v1", "v2"] {
        let parts = ManifestBuilder::new(format!("config-{tag}").as_bytes())
            .layer(format!("layer-{tag}").as_bytes())
            .build();
        parts.mount_source(&source_server, "library/app", tag).await;
        parts.mount_target(&target_server, "mirror/app", tag).await;
    }

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "library/app",
        "mirror/app",
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    let progress = RecordingProgress::default();
    let report = SyncEngine::new(fast_retry(), 50)
        .with_heartbeat_interval(interval)
        .run(
            vec![mapping],
            empty_cache(),
            BlobStage::disabled(),
            &progress,
            None,
        )
        .await;

    let snapshots = progress.snapshots.take();
    (report, snapshots)
}

/// A run that outlives the heartbeat cadence reports itself as in progress.
///
/// Without this the engine is silent between the first image starting and the
/// last one finishing, which is indistinguishable from a wedged run.
#[tokio::test]
async fn heartbeat_fires_while_work_is_in_flight() {
    let (report, snapshots) = run_with_heartbeat(Duration::from_millis(1)).await;

    assert_eq!(report.stats.images_synced, 2);
    assert!(
        !snapshots.is_empty(),
        "a run longer than the heartbeat interval must report progress"
    );

    // The branch is guarded on work remaining, so every snapshot must show at
    // least one tag discovering or one transfer executing. A snapshot with
    // both at zero would mean the heartbeat outlived the work it describes.
    for s in &snapshots {
        assert!(
            s.discovering + s.in_flight > 0,
            "heartbeat fired with nothing in flight: {s:?}"
        );
        assert!(
            s.completed <= 2,
            "completed count exceeds the images in the run: {s:?}"
        );
    }
}

/// The heartbeat is timer-gated, not emitted unconditionally: a run that
/// finishes well inside one interval reports nothing.
#[tokio::test]
async fn heartbeat_stays_silent_for_a_short_run() {
    let (report, snapshots) = run_with_heartbeat(Duration::from_secs(600)).await;

    assert_eq!(report.stats.images_synced, 2);
    assert!(
        snapshots.is_empty(),
        "short run must not emit progress, got {} snapshot(s)",
        snapshots.len()
    );
}

/// The heartbeat must not keep the engine alive past its work. A completed run
/// returns rather than looping on the timer.
#[tokio::test]
async fn heartbeat_does_not_block_run_completion() {
    let (report, _) = run_with_heartbeat(Duration::from_millis(1)).await;

    assert_eq!(report.images.len(), 2);
    for image in &report.images {
        assert!(
            matches!(image.status, ImageStatus::Synced),
            "expected all images synced, got {:?}",
            image.status
        );
    }
}

/// Shutdown freezes discovery, so undiscovered tags stop being work the engine
/// can do. The heartbeat branch must go with them: if it stays enabled it
/// starves the loop's exit and the engine hangs for a full interval.
#[tokio::test]
async fn heartbeat_does_not_stall_shutdown_with_discovery_outstanding() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config").layer(b"layer").build();
    parts
        .mount_source(&source_server, "library/app", "v1")
        .await;
    parts.mount_target(&target_server, "mirror/app", "v1").await;

    let mapping = mapping_with_distinct_repos(
        &source_server,
        &target_server,
        "library/app",
        "mirror/app",
        vec![TagPair::same("v1")],
    );

    // Triggered up front so the engine shuts down with discovery still queued.
    let shutdown = ShutdownSignal::new();
    shutdown.trigger();

    // An interval far longer than the test's patience: if the guard regresses,
    // the engine waits this out instead of exiting and the timeout fires.
    let engine =
        SyncEngine::new(fast_retry(), 10).with_heartbeat_interval(Duration::from_secs(600));

    let progress = RecordingProgress::default();
    let run = engine.run(
        vec![mapping],
        empty_cache(),
        BlobStage::disabled(),
        &progress,
        Some(&shutdown),
    );

    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("engine must exit on shutdown, not wait out the heartbeat interval");
}
