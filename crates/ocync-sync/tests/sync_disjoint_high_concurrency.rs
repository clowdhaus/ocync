//! Ten mappings to one target registry at `max_concurrent_transfers=50` with
//! mostly disjoint blob sets: every image syncs and the report's blob stats
//! match the PUT and mount POST counts the target actually received, so a blob
//! push is never reported as `Skipped` or `Mounted` without a request behind it.
//! Variants: no shared blobs (no leader), one blob shared by one pair (one
//! leader, the follower mounts), and that plus a batch checker that reports
//! every blob missing.

mod helpers;

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ocync_sync::engine::TagPair;
use wiremock::MockServer;
use wiremock::http::Method;

use helpers::*;

/// Every image syncs and the report's per-blob stats reconcile with the target
/// server's actual PUT and mount POST counts.
#[tokio::test(flavor = "current_thread")]
async fn sync_ten_disjoint_mappings_one_shared_blob_high_concurrency() {
    let fixture = build_ten_image_fixture(Sharing::OnePair).await;
    let (report, _) = run_engine_default(&fixture, Checking::HeadPerBlob).await;
    assert_report_matches_target_receipts(&fixture, &report).await;
}

/// As above, with an ECR-style batch checker returning `existing=[]` on every
/// target: cache pre-population must not produce a false `Skipped` or `Mounted`.
#[tokio::test(flavor = "current_thread")]
async fn sync_ten_disjoint_mappings_one_shared_blob_with_batch_checker() {
    let fixture = build_ten_image_fixture(Sharing::OnePair).await;
    let (report, checkers) = run_engine_default(&fixture, Checking::BatchChecker).await;
    assert_report_matches_target_receipts(&fixture, &report).await;
    let checks: Vec<usize> = checkers.iter().map(|c| c.get()).collect();
    assert_eq!(checks, [1; 10], "each mapping asks its checker once");
}

/// Baseline with zero shared blobs: no leader, every image independent.
#[tokio::test(flavor = "current_thread")]
async fn sync_ten_fully_disjoint_mappings_high_concurrency() {
    let fixture = build_ten_image_fixture(Sharing::Disjoint).await;
    let (report, _) = run_engine_default(&fixture, Checking::HeadPerBlob).await;
    assert_report_matches_target_receipts(&fixture, &report).await;
    // No shared blobs -> no mount candidates -> no mount POSTs at all.
    assert_eq!(report.stats.blobs_mounted, 0);
}

/// Which images share a blob.
#[derive(Clone, Copy)]
enum Sharing {
    /// Images 0 and 1 share one layer.
    OnePair,
    /// No image shares a blob.
    Disjoint,
}

/// How the target's blob existence is checked.
#[derive(Clone, Copy)]
enum Checking {
    /// No batch checker: a HEAD per blob.
    HeadPerBlob,
    /// A batch checker that reports every blob missing.
    BatchChecker,
}

struct TenImageFixture {
    /// Held so the source mocks outlive the run.
    _source_server: MockServer,
    target_server: MockServer,
    source_client: Arc<ocync_distribution::RegistryClient>,
    target_client: Arc<ocync_distribution::RegistryClient>,
    images: Vec<ManifestParts>,
    /// `(source_repo, target_repo)` per image index.
    repos: Vec<(String, String)>,
}

async fn build_ten_image_fixture(sharing: Sharing) -> TenImageFixture {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_layer = b"shared-layer-between-images-0-and-1".to_vec();

    let mut images = Vec::with_capacity(10);
    for i in 0..10usize {
        let config = format!("config-image-{i}").into_bytes();
        let layer_a = format!("image-{i}-layer-a").into_bytes();
        let layer_b = format!("image-{i}-layer-b").into_bytes();

        let mut builder = ManifestBuilder::new(&config)
            .layer(&layer_a)
            .layer(&layer_b);
        if matches!(sharing, Sharing::OnePair) && i < 2 {
            builder = builder.layer(&shared_layer);
        }
        images.push(builder.build());
    }

    let repos: Vec<(String, String)> = (0..10)
        .map(|i| (format!("src/img-{i}"), format!("tgt/img-{i}")))
        .collect();

    for (parts, (src, _)) in images.iter().zip(repos.iter()) {
        parts.mount_source(&source_server, src, "latest").await;
    }

    for (_, tgt) in &repos {
        mount_manifest_head_not_found(&target_server, tgt, "latest").await;
        mount_manifest_push(&target_server, tgt, "latest").await;
        mount_blob_push(&target_server, tgt).await;
        // Without a batch checker the engine HEADs every blob. Any of our
        // digests may also be mounted from another repo.
        for parts in &images {
            parts.mount_target_blob_misses(&target_server, tgt).await;
            for digest in parts.blob_digests() {
                mount_blob_mount_accepted(&target_server, tgt, &digest).await;
            }
        }
    }

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    TenImageFixture {
        _source_server: source_server,
        target_server,
        source_client,
        target_client,
        images,
        repos,
    }
}

/// Run the ten mappings; the second value holds each batch checker's call
/// counter, empty without one.
async fn run_engine_default(
    fixture: &TenImageFixture,
    checking: Checking,
) -> (ocync_sync::SyncReport, Vec<Rc<Cell<usize>>>) {
    let mut checkers = Vec::new();
    let mappings: Vec<_> = fixture
        .repos
        .iter()
        .map(|(src, tgt)| {
            let client = Arc::clone(&fixture.target_client);
            let entry = match checking {
                Checking::HeadPerBlob => target_entry("target", client),
                Checking::BatchChecker => {
                    let (checker, checks) = MockChecker::new(tgt, HashSet::new());
                    checkers.push(checks);
                    batch_target("target", client, checker)
                }
            };
            resolved_mapping(
                Arc::clone(&fixture.source_client),
                src,
                tgt,
                vec![entry],
                vec![TagPair::same("latest")],
            )
        })
        .collect();

    // Above the engine default of 10 on purpose: it oversubscribes the
    // per-target upload semaphore (cap 64) with 50 * BLOB_CONCURRENCY(6) = 300
    // wanted streams.
    let report = tokio::time::timeout(Duration::from_secs(30), run_sync(mappings))
        .await
        .expect("engine hung past 30s: leader/follower stall or auth deadlock");
    (report, checkers)
}

async fn assert_report_matches_target_receipts(
    fixture: &TenImageFixture,
    report: &ocync_sync::SyncReport,
) {
    assert_all_synced(report, 10, "ten mappings");

    let declared: Vec<u64> = fixture
        .images
        .iter()
        .map(|p| 1 + p.layer_descs.len() as u64)
        .collect();
    for r in &report.images {
        let i = parse_image_index(&r.source);
        let handled = r.blob_stats.transferred + r.blob_stats.mounted + r.blob_stats.skipped;
        assert_eq!(
            handled, declared[i],
            "image[{i}]: handled blob count {handled} != declared {} (transferred={}, mounted={}, skipped={})",
            declared[i], r.blob_stats.transferred, r.blob_stats.mounted, r.blob_stats.skipped,
        );
    }

    let target = &fixture.target_server;
    let blob_puts = count_requests(
        target,
        Some(&Method::PUT),
        PathMatch::Contains("/blobs/uploads/"),
    )
    .await as u64;
    let blob_mounts = count_mount_posts(target).await as u64;
    let manifest_puts = count_requests(
        target,
        Some(&Method::PUT),
        PathMatch::Contains("/manifests/"),
    )
    .await as u64;

    let stats_transferred: u64 = report.images.iter().map(|r| r.blob_stats.transferred).sum();
    let stats_mounted: u64 = report.images.iter().map(|r| r.blob_stats.mounted).sum();
    let stats_skipped: u64 = report.images.iter().map(|r| r.blob_stats.skipped).sum();

    eprintln!(
        "[repro] target receipts: blob_puts={blob_puts} blob_mounts={blob_mounts} \
         report.transferred={stats_transferred} report.mounted={stats_mounted} \
         report.skipped={stats_skipped}",
    );

    // No skipping: empty cache + fresh target repos. A non-zero
    // `stats_skipped` would mean a blob was reported skipped without an
    // actual reason (false-positive cache state or HEAD).
    assert_eq!(
        stats_skipped, 0,
        "blobs were reported Skipped despite an empty cache and fresh target repos",
    );

    // The decisive checks: report-claimed counts must match what the
    // target actually received.
    assert_eq!(
        blob_puts, stats_transferred,
        "report claims {stats_transferred} blobs transferred but target received {blob_puts} PUT uploads: engine is short-circuiting blob pushes",
    );
    assert_eq!(
        blob_mounts, stats_mounted,
        "report claims {stats_mounted} blobs mounted but target received {blob_mounts} mount POSTs",
    );
    assert_eq!(
        manifest_puts, 10,
        "expected 10 manifest PUTs, got {manifest_puts}"
    );
}

fn parse_image_index(source: &str) -> usize {
    source
        .strip_prefix("src/img-")
        .and_then(|s| s.strip_suffix(":latest"))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("source did not match src/img-<N>:latest: {source}"))
}
