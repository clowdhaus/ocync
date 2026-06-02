//! High-concurrency, mostly-disjoint mapping reproducer.
//!
//! Mirrors the production failure mode: 10 mappings to one target registry,
//! mostly disjoint blob sets, `max_concurrent_transfers=50`. One pair
//! shares a single layer so `elect_leaders` picks exactly one leader
//! (matching the `total_leaders=1 images=10` log observed against ECR).
//!
//! The engine reports `ImageStatus::Synced` only when the blob loop and
//! manifest push both succeed. If the engine silently short-circuits a blob
//! push (returning `Skipped`/`Mounted` for a blob that was never actually
//! transferred), the per-target request counts will be lower than the
//! per-image stats claim -- and these assertions fire.
//!
//! Three variants:
//! - [`sync_ten_fully_disjoint_mappings_high_concurrency`] -- baseline
//!   with zero shared blobs, exercises the no-leader path.
//! - [`sync_ten_disjoint_mappings_one_shared_blob_high_concurrency`] --
//!   one shared blob, one elected leader, follower mounts.
//! - [`sync_ten_disjoint_mappings_one_shared_blob_with_batch_checker`] --
//!   same shape, plus a `BatchBlobChecker` returning `existing=[]` for
//!   every repo (exercises the ECR cache-pre-population path).

mod helpers;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ocync_distribution::spec::RepositoryName;
use ocync_distribution::{BatchBlobChecker, Digest};
use ocync_sync::ImageStatus;
use ocync_sync::engine::{RegistryAlias, SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use wiremock::matchers::{method, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// Production-shape reproducer. Asserts every image syncs and that the
/// report's per-blob stats reconcile against the target server's actual
/// PUT and mount POST counts.
#[tokio::test(flavor = "current_thread")]
async fn sync_ten_disjoint_mappings_one_shared_blob_high_concurrency() {
    let fixture = build_ten_image_fixture(/* shared_pair = */ true).await;
    let report = run_engine_default(&fixture, /* with_batch_checker = */ false).await;
    assert_report_matches_target_receipts(&fixture, &report).await;
}

/// As above, but each `TargetEntry` carries an ECR-style batch checker
/// that returns `existing=[]` for every call. Confirms the cache
/// pre-population path doesn't introduce false-Skipped or false-Mounted
/// terminals at high concurrency.
#[tokio::test(flavor = "current_thread")]
async fn sync_ten_disjoint_mappings_one_shared_blob_with_batch_checker() {
    let fixture = build_ten_image_fixture(/* shared_pair = */ true).await;
    let report = run_engine_default(&fixture, /* with_batch_checker = */ true).await;
    assert_report_matches_target_receipts(&fixture, &report).await;
}

/// Sanity baseline: zero shared blobs. `elect_leaders` returns 0 (no
/// leader log fires). Every image processes independently.
#[tokio::test(flavor = "current_thread")]
async fn sync_ten_fully_disjoint_mappings_high_concurrency() {
    let fixture = build_ten_image_fixture(/* shared_pair = */ false).await;
    let report = run_engine_default(&fixture, /* with_batch_checker = */ false).await;
    assert_report_matches_target_receipts(&fixture, &report).await;
    // No shared blobs -> no mount candidates -> no mount POSTs at all.
    assert_eq!(report.stats.blobs_mounted, 0);
}

// ---------------------------------------------------------------------------
// Shared fixture + assertions
// ---------------------------------------------------------------------------

struct TenImageFixture {
    source_server: MockServer,
    target_server: MockServer,
    source_client: Arc<ocync_distribution::RegistryClient>,
    target_client: Arc<ocync_distribution::RegistryClient>,
    images: Vec<ManifestParts>,
    // (source_repo, target_repo) per image index.
    repos: Vec<(String, String)>,
}

async fn build_ten_image_fixture(shared_pair: bool) -> TenImageFixture {
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
        if shared_pair && i < 2 {
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
        // Blob HEAD 404 for every digest this repo might be asked about
        // (without a batch checker, the engine HEADs every blob).
        for parts in &images {
            mount_blob_not_found(&target_server, tgt, &parts.config_desc.digest).await;
            for desc in &parts.layer_descs {
                mount_blob_not_found(&target_server, tgt, &desc.digest).await;
            }
        }
        mount_blob_push(&target_server, tgt).await;

        // Mount mocks: 201 for any cross-repo mount of any of our digests.
        for parts in &images {
            for desc in &parts.layer_descs {
                Mock::given(method("POST"))
                    .and(path_regex(format!("^/v2/{tgt}/blobs/uploads/$")))
                    .and(query_param("mount", desc.digest.to_string()))
                    .respond_with(ResponseTemplate::new(201))
                    .with_priority(1)
                    .mount(&target_server)
                    .await;
            }
            Mock::given(method("POST"))
                .and(path_regex(format!("^/v2/{tgt}/blobs/uploads/$")))
                .and(query_param("mount", parts.config_desc.digest.to_string()))
                .respond_with(ResponseTemplate::new(201))
                .with_priority(1)
                .mount(&target_server)
                .await;
        }
    }

    let source_client = mock_client(&source_server);
    let target_client = mock_client(&target_server);

    TenImageFixture {
        source_server,
        target_server,
        source_client,
        target_client,
        images,
        repos,
    }
}

async fn run_engine_default(
    fixture: &TenImageFixture,
    with_batch_checker: bool,
) -> ocync_sync::SyncReport {
    let mappings: Vec<_> = fixture
        .repos
        .iter()
        .map(|(src, tgt)| {
            let batch_checker: Option<Rc<dyn BatchBlobChecker>> = if with_batch_checker {
                Some(Rc::new(EmptyExistingChecker::new(tgt)))
            } else {
                None
            };
            let entry = TargetEntry {
                name: RegistryAlias::new("target"),
                client: Arc::clone(&fixture.target_client),
                batch_checker,
                existing_tags: HashSet::new(),
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

    // Engine default is 10 (DEFAULT_MAX_CONCURRENT_TRANSFERS); the test
    // deliberately oversubscribes at 50 to exercise the per-target
    // streaming-blob semaphore (cap 64) under a workload that would
    // otherwise demand 50 * BLOB_CONCURRENCY=6 = 300 concurrent streams.
    let engine = SyncEngine::new(fast_retry(), 50);
    tokio::time::timeout(
        Duration::from_secs(30),
        engine.run(
            mappings,
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        ),
    )
    .await
    .expect("engine hung past 30s -- leader/follower stall or auth deadlock")
}

async fn assert_report_matches_target_receipts(
    fixture: &TenImageFixture,
    report: &ocync_sync::SyncReport,
) {
    // Every image must claim Synced.
    assert_eq!(report.images.len(), 10, "expected 10 image results");
    for r in &report.images {
        assert!(
            matches!(r.status, ImageStatus::Synced),
            "{} -> {} not Synced: {:#?}",
            r.source,
            r.target,
            r.status,
        );
    }

    // Per-image declared blob count: config + N layers.
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

    let target_requests = fixture.target_server.received_requests().await.unwrap();

    let blob_puts: u64 = target_requests
        .iter()
        .filter(|r| {
            r.method == wiremock::http::Method::PUT && r.url.path().contains("/blobs/uploads/")
        })
        .count() as u64;
    let blob_mounts: u64 = target_requests
        .iter()
        .filter(|r| {
            r.method == wiremock::http::Method::POST
                && r.url.path().ends_with("/blobs/uploads/")
                && r.url.query_pairs().any(|(k, _)| k == "mount")
        })
        .count() as u64;
    let manifest_puts: u64 = target_requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::PUT && r.url.path().contains("/manifests/"))
        .count() as u64;

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
        "report claims {stats_transferred} blobs transferred but target received {blob_puts} PUT uploads -- engine is short-circuiting blob pushes",
    );
    assert_eq!(
        blob_mounts, stats_mounted,
        "report claims {stats_mounted} blobs mounted but target received {blob_mounts} mount POSTs",
    );
    assert_eq!(
        manifest_puts, 10,
        "expected 10 manifest PUTs, got {manifest_puts}"
    );

    // Suppress unused-field warnings for fields kept on the fixture
    // for diagnostic clarity at failure time.
    let _ = &fixture.source_server;
}

fn parse_image_index(source: &str) -> usize {
    source
        .strip_prefix("src/img-")
        .and_then(|s| s.strip_suffix(":latest"))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("source did not match src/img-<N>:latest: {source}"))
}

// ---------------------------------------------------------------------------
// Test-local batch checker mirroring ECR's BatchCheckLayerAvailability
// against an empty target. Asserts the caller passes the expected repo
// (per-mock contract fidelity).
// ---------------------------------------------------------------------------

struct EmptyExistingChecker {
    expected_repo: String,
    call_count: Arc<AtomicUsize>,
}

impl EmptyExistingChecker {
    fn new(expected_repo: &str) -> Self {
        Self {
            expected_repo: expected_repo.to_owned(),
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl BatchBlobChecker for EmptyExistingChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        _digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<HashSet<Digest>, ocync_distribution::Error>> + 'a>>
    {
        assert_eq!(
            repo.as_str(),
            self.expected_repo,
            "batch checker called with wrong repo",
        );
        Box::pin(async {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(HashSet::new())
        })
    }
}
