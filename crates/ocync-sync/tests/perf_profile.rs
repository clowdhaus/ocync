//! CPU profile harness for the sync engine.
//!
//! Pushes a deterministic corpus of small images to one local `registry:2`,
//! then syncs it to a second local `registry:2` so a profiler can sample
//! `SyncEngine::run`. Tests are `#[ignore]`'d -- they spin up Docker
//! containers and run in the seconds-range, intended for ad-hoc profiling.
//!
//! ## Tests
//!
//! - `profile_small_images` -- HTTP-only. Baseline. Useful for SHA-256,
//!   JSON parsing, and task-scheduling cost.
//! - `profile_small_images_tls` -- TLS-terminated via `registry:2`'s
//!   built-in HTTPS support with a self-signed cert generated at test
//!   start. Surfaces rustls handshake + AEAD cost.
//!
//! ## Usage
//!
//! ```bash
//! samply record --output /tmp/sync.profile -- \
//!   cargo test --profile profiling --test perf_profile -- \
//!   --ignored --nocapture --exact profile_small_images
//! ```
//!
//! Then `samply load /tmp/sync.profile` to view the trace. Look for the
//! `[profile] PROFILE BEGIN` / `[profile] PROFILE END` stderr markers to
//! scope the analysis window in the UI.
//!
//! ## Tunables
//!
//! - `OCYNC_PROFILE_IMAGES`        number of images (default: 200)
//! - `OCYNC_PROFILE_LAYERS`        layers per image (default: 2)
//! - `OCYNC_PROFILE_BYTES`         bytes per layer (default: 16384). Bump to
//!   ~5 MB to amplify SHA-256 cost like a real container layer.
//! - `OCYNC_PROFILE_WORKERS`       `max_concurrent_transfers` (default: 50;
//!   note the engine default is 10 -- the harness deliberately oversubscribes
//!   to stress h2 stream budgeting).
//! - `OCYNC_PROFILE_HTTP1=1`       refuse to negotiate HTTP/2 via ALPN.
//!   Used to isolate HTTP/2 multiplexing stalls from the rest of the path.
//! - `OCYNC_PROFILE_H2_ADAPTIVE=0` disable HTTP/2 adaptive-window sizing
//!   for A/B testing the pre-fix behavior.
//! - `OCYNC_PROFILE_STREAM_CAP`    override per-`RegistryClient`
//!   `streaming_blob_concurrency` (default: 64). Lower to verify the
//!   per-target stream cap binds.

mod helpers;

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures_util::stream;
use ocync_distribution::sha256::Sha256;
use ocync_distribution::spec::{MediaType, RepositoryName};
use ocync_distribution::{Digest, RegistryClient, RegistryClientBuilder};
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use rcgen::{CertificateParams, DnType, KeyPair};
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use url::Url;

use helpers::*;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn start_registry_http() -> (ContainerAsync<GenericImage>, Url) {
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

/// Self-signed cert covering `localhost` + `127.0.0.1`, valid for the
/// run of the test. Generated per test invocation so there's no
/// on-disk artifact to manage.
fn self_signed_localhost() -> (Vec<u8>, Vec<u8>) {
    let key = KeyPair::generate().expect("rcgen KeyPair::generate");
    let mut params = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .expect("rcgen CertificateParams::new");
    params
        .distinguished_name
        .push(DnType::CommonName, "ocync-perf-profile");
    let cert = params.self_signed(&key).expect("rcgen self_signed");
    (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
}

async fn start_registry_tls() -> (ContainerAsync<GenericImage>, Url) {
    let (cert_pem, key_pem) = self_signed_localhost();
    let container = GenericImage::new("registry", "2")
        .with_exposed_port(5000.into())
        .with_wait_for(WaitFor::message_on_stderr("listening on"))
        .with_env_var("REGISTRY_HTTP_TLS_CERTIFICATE", "/certs/tls.crt")
        .with_env_var("REGISTRY_HTTP_TLS_KEY", "/certs/tls.key")
        .with_copy_to("/certs/tls.crt", cert_pem)
        .with_copy_to("/certs/tls.key", key_pem)
        .start()
        .await
        .expect("registry:2 (TLS) container failed to start");
    let port = container
        .get_host_port_ipv4(5000)
        .await
        .expect("get_host_port_ipv4 failed");
    let url = Url::parse(&format!("https://127.0.0.1:{port}")).unwrap();
    (container, url)
}

fn make_client(url: Url, accept_invalid_certs: bool) -> Arc<RegistryClient> {
    let force_http1 = std::env::var("OCYNC_PROFILE_HTTP1").ok().as_deref() == Some("1");
    // Adaptive window is on by default in the builder. Set
    // OCYNC_PROFILE_H2_ADAPTIVE=0 to disable for A/B testing the
    // pre-fix behavior.
    let mut builder = RegistryClientBuilder::new(url)
        .allow_invalid_certs(accept_invalid_certs)
        .force_http1(force_http1);
    if std::env::var("OCYNC_PROFILE_H2_ADAPTIVE").ok().as_deref() == Some("0") {
        builder = builder.http2_adaptive_window(false);
    }
    if let Some(cap) = std::env::var("OCYNC_PROFILE_STREAM_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        builder = builder.streaming_blob_concurrency(cap);
    }
    Arc::new(builder.build().expect("RegistryClient"))
}

async fn run_profile(src: Arc<RegistryClient>, dst: Arc<RegistryClient>, label: &str) {
    let n_images = env_usize("OCYNC_PROFILE_IMAGES", 200);
    let n_layers = env_usize("OCYNC_PROFILE_LAYERS", 2);
    let layer_bytes = env_usize("OCYNC_PROFILE_BYTES", 16 * 1024);
    let workers = env_usize("OCYNC_PROFILE_WORKERS", 50);

    eprintln!(
        "[profile] {label}: populating source: {n_images} images x {n_layers} layers x {layer_bytes} B"
    );
    let pop_start = Instant::now();
    let repos = populate_source(&src, n_images, n_layers, layer_bytes).await;
    eprintln!("[profile] {label}: populate took {:?}", pop_start.elapsed());

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

    eprintln!("[profile] PROFILE BEGIN {label} workers={workers}");
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
    eprintln!("[profile] PROFILE END {label} elapsed={sync_elapsed:?}");

    let synced = report
        .images
        .iter()
        .filter(|r| matches!(r.status, ocync_sync::ImageStatus::Synced))
        .count();
    eprintln!(
        "[profile] {label}: images={n_images} synced={synced} blobs_transferred={} bytes={}",
        report.stats.blobs_transferred, report.stats.bytes_transferred,
    );
    assert_eq!(
        synced, n_images,
        "expected all {n_images} images to sync; got {synced}",
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spins up Docker containers; run via samply, not in CI"]
async fn profile_small_images() {
    let (_src_ctr, src_url) = start_registry_http().await;
    let (_dst_ctr, dst_url) = start_registry_http().await;
    let src = make_client(src_url, false);
    let dst = make_client(dst_url, false);
    run_profile(src, dst, "http").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spins up Docker containers; run via samply, not in CI"]
async fn profile_small_images_tls() {
    let (_src_ctr, src_url) = start_registry_tls().await;
    let (_dst_ctr, dst_url) = start_registry_tls().await;
    let src = make_client(src_url, true);
    let dst = make_client(dst_url, true);
    run_profile(src, dst, "tls").await;
}

async fn push_blob_stream(client: &RegistryClient, repo: &RepositoryName, data: Vec<u8>) {
    let digest = Digest::from_sha256(Sha256::digest(&data));
    let size = data.len() as u64;
    let body = Bytes::from(data);
    let s = stream::once(async move { Ok::<_, ocync_distribution::Error>(body) });
    client
        .blob_push_stream(repo, &digest, Some(size), s)
        .await
        .expect("blob_push_stream");
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
        // Stream the corpus into the source registry the same way the
        // engine does during sync; otherwise the harness's setup-time SHA
        // dominates the profile and the actual streaming path is invisible.
        push_blob_stream(client, &repo, config_data.clone()).await;

        let mut builder = ManifestBuilder::new(&config_data);
        for l in 0..n_layers {
            let mut layer = vec![0u8; layer_bytes];
            // Vary content per (image, layer) so digests are unique.
            layer[0] = (i & 0xff) as u8;
            layer[1] = (l & 0xff) as u8;
            push_blob_stream(client, &repo, layer.clone()).await;
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
