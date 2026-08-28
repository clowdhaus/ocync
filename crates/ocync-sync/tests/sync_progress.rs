//! Progress reporting integration tests: the in-flight heartbeat contract.

mod helpers;

use std::cell::RefCell;
use std::time::Duration;

use ocync_distribution::spec::MediaType;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::{ProgressReporter, RunProgress};
use ocync_sync::staging::BlobStage;
use ocync_sync::{ImageResult, ShutdownSignal, SyncReport};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// Records every [`RunProgress`] snapshot the engine emits.
#[derive(Default)]
struct RecordingProgress {
    snapshots: RefCell<Vec<RunProgress>>,
}

impl ProgressReporter for RecordingProgress {
    fn image_started(&self, _source: &str, _target: &str) {}
    fn image_completed(&self, _result: &ImageResult) {}
    fn run_heartbeat(&self, snapshot: RunProgress) {
        self.snapshots.borrow_mut().push(snapshot);
    }
    fn run_completed(&self, _report: &SyncReport) {}
}

/// Run a two-image sync with the given heartbeat cadence.
///
/// `source_delay` is applied to the source manifest GET, giving discovery a
/// known duration for the cadence assertion to measure against. Real time, not
/// a paused clock: virtual time auto-advances to any pending deadline, which
/// would make a heartbeat fire for any interval at all.
async fn run_with_heartbeat(
    interval: Duration,
    source_delay: Duration,
) -> (SyncReport, Vec<RunProgress>) {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    for tag in ["v1", "v2"] {
        let parts = ManifestBuilder::new(format!("config-{tag}").as_bytes())
            .layer(format!("layer-{tag}").as_bytes())
            .build();
        // Registered before `mount_source` so this delayed responder is the
        // one that answers the manifest GET.
        Mock::given(method("GET"))
            .and(path(format!("/v2/library/app/manifests/{tag}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(parts.bytes.clone())
                    .insert_header("content-type", MediaType::OciManifest.as_str())
                    .set_delay(source_delay),
            )
            .mount(&source_server)
            .await;
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
///
/// The source responds after a known delay, so the cadence is measurable
/// rather than assumed. Asserting only "at least one" would pass for any
/// interval at all; the lower bound below is what ties the count to the
/// period, and it fails if the interval is widened past the delay.
#[tokio::test]
async fn heartbeat_fires_while_work_is_in_flight() {
    let (report, snapshots) =
        run_with_heartbeat(Duration::from_millis(100), Duration::from_millis(800)).await;

    assert_eq!(report.stats.images_synced, 2);
    assert!(
        snapshots.len() >= 4,
        "800ms of blocked discovery at a 100ms cadence must report repeatedly, got {}",
        snapshots.len()
    );

    // The branch is guarded on work remaining, so every snapshot must show at
    // least one tag discovering or one transfer executing. A snapshot with
    // both at zero would mean the heartbeat outlived the work it describes.
    for s in &snapshots {
        assert!(
            s.in_discovery + s.in_flight > 0,
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
///
/// Real time for the same reason as the test above: a paused clock advances to
/// any pending deadline, so even a ten-minute interval would fire and this
/// would pass for the wrong reason.
#[tokio::test]
async fn heartbeat_stays_silent_for_a_short_run() {
    let (report, snapshots) = run_with_heartbeat(Duration::from_secs(600), Duration::ZERO).await;

    assert_eq!(report.stats.images_synced, 2);
    assert!(
        snapshots.is_empty(),
        "short run must not emit progress, got {} snapshot(s)",
        snapshots.len()
    );
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
