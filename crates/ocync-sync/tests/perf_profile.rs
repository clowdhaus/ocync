//! CPU profile harness for the sync engine.
//!
//! Pushes a deterministic corpus of small images to one local `registry:2`,
//! then syncs it to a second local `registry:2` so a profiler can sample
//! `SyncEngine::run`. The test is `#[ignore]`'d -- it spins up Docker
//! containers and runs in the seconds-range, so it is for ad-hoc profiling,
//! not CI.
//!
//! ## Usage
//!
//! ```bash
//! samply record --output /tmp/sync.profile -- \
//!   cargo test --release --test perf_profile -- \
//!   --ignored --nocapture --exact profile_small_images
//! ```
//!
//! Then open `/tmp/sync.profile` in samply's web UI.
//!
//! Tune via env vars:
//! - `OCYNC_PROFILE_IMAGES`   number of images (default: 200)
//! - `OCYNC_PROFILE_LAYERS`   layers per image (default: 2)
//! - `OCYNC_PROFILE_BYTES`    bytes per layer (default: 16384)
//! - `OCYNC_PROFILE_WORKERS`  `max_concurrent_transfers` (default: 50)
//!
//! ## Known limitation
//!
//! `registry:2` is HTTP-only. If the production CPU bottleneck is TLS
//! handshakes or rustls work, this harness will under-report it. Treat the
//! profile as authoritative for SHA-256, JSON parsing, and task-scheduling
//! cost; treat it as a lower bound for any per-connection overhead.

mod helpers;

use std::sync::Arc;
use std::time::Instant;

use ocync_distribution::spec::{MediaType, RepositoryName};
use ocync_distribution::{RegistryClient, RegistryClientBuilder};
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};
use url::Url;

use helpers::*;

async fn start_registry() -> (ContainerAsync<GenericImage>, Url) {
    let container = GenericImage::new("registry", "2")
        .with_exposed_port(5000.into())
        .with_wait_for(WaitFor::message_on_stderr("listening on"))
        .start()
        .await
        .expect("registry:2 container failed to start");
    let port = container
        .get_host_port_ipv4(5000)
        .await
        .expect("get_host_port_ipv4 failed");
    let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
    (container, url)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spins up Docker containers; run via samply, not in CI"]
async fn profile_small_images() {
    let n_images = env_usize("OCYNC_PROFILE_IMAGES", 200);
    let n_layers = env_usize("OCYNC_PROFILE_LAYERS", 2);
    let layer_bytes = env_usize("OCYNC_PROFILE_BYTES", 16 * 1024);
    let workers = env_usize("OCYNC_PROFILE_WORKERS", 50);

    let (_src_ctr, src_url) = start_registry().await;
    let (_dst_ctr, dst_url) = start_registry().await;

    let src = Arc::new(
        RegistryClientBuilder::new(src_url.clone())
            .build()
            .expect("source RegistryClient"),
    );
    let dst = Arc::new(
        RegistryClientBuilder::new(dst_url.clone())
            .build()
            .expect("target RegistryClient"),
    );

    eprintln!(
        "[profile] populating source: {n_images} images x {n_layers} layers x {layer_bytes} B"
    );
    let pop_start = Instant::now();
    let repos = populate_source(&src, n_images, n_layers, layer_bytes).await;
    eprintln!("[profile] populate took {:?}", pop_start.elapsed());

    let mappings = repos
        .iter()
        .map(|repo| {
            resolved_mapping(
                Arc::clone(&src),
                repo.as_str(),
                repo.as_str(),
                vec![target_entry("target", Arc::clone(&dst))],
                vec![TagPair::same("v1")],
            )
        })
        .collect::<Vec<_>>();

    let engine = SyncEngine::new(fast_retry(), workers);

    // Everything above this line is setup. Everything below is what we want
    // the profiler to capture. samply records the whole process, so trimming
    // happens in the UI -- use the eprintln markers as anchors.
    eprintln!("[profile] PROFILE BEGIN workers={workers}");
    let sync_start = Instant::now();
    let report = engine
        .run(
            mappings,
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;
    let sync_elapsed = sync_start.elapsed();
    eprintln!("[profile] PROFILE END elapsed={sync_elapsed:?}");

    let synced = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ocync_sync::ImageStatus::Synced))
        .count();
    eprintln!(
        "[profile] images={n_images} synced={synced} blobs_transferred={} bytes={}",
        report.stats.blobs_transferred, report.stats.bytes_transferred,
    );
    assert_eq!(
        synced, n_images,
        "expected all {n_images} images to sync; got {synced}",
    );
}

async fn populate_source(
    client: &RegistryClient,
    n_images: usize,
    n_layers: usize,
    layer_bytes: usize,
) -> Vec<RepositoryName> {
    let mut repos = Vec::with_capacity(n_images);
    for i in 0..n_images {
        let repo = RepositoryName::new(format!("perf/img-{i:04}")).unwrap();
        let config_data = format!("{{\"image\":{i}}}").into_bytes();
        client
            .blob_push(&repo, config_data.as_slice())
            .await
            .expect("config push");

        let mut builder = ManifestBuilder::new(&config_data);
        for l in 0..n_layers {
            let mut layer = vec![0u8; layer_bytes];
            // Vary content per (image, layer) so digests are unique.
            layer[0] = (i & 0xff) as u8;
            layer[1] = (l & 0xff) as u8;
            client.blob_push(&repo, &layer).await.expect("layer push");
            builder = builder.layer(&layer);
        }
        let parts = builder.build();
        client
            .manifest_push(&repo, "v1", &MediaType::OciManifest, &parts.bytes)
            .await
            .expect("manifest push");
        repos.push(repo);
    }
    repos
}
