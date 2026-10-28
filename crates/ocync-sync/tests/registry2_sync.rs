//! Blob downloads and syncs against real `registry:2` containers, for
//! behavior a mock server cannot show, such as HTTP/2 flow control on the
//! source connection.

mod helpers;

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use ocync_distribution::sha256::Sha256;
use ocync_distribution::{Digest, RegistryClientBuilder};
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;

use helpers::*;

/// Layers in the image: more than the target takes uploads for at once
/// while its upload window is still growing.
const LAYERS: usize = 12;

/// Bytes per layer: far above the 65,535 byte window an HTTP/2 connection
/// starts with.
const LAYER_BYTES: usize = 1 << 20;

/// A sync from an HTTP/2 source holding more large blobs than the target
/// uploads at once finishes promptly. Breaks if source blobs are opened
/// ahead of their uploads: their unread bodies fill the source
/// connection's receive window and starve the uploads that are running,
/// so the sync stalls until the source closes the requests.
#[tokio::test(flavor = "current_thread")]
async fn sync_from_an_http2_source_does_not_stall_on_unread_bodies() {
    let (_source_container, source_url) = start_registry_tls().await;
    let (_target_container, target_url) = start_registry_http().await;
    let source = Arc::new(
        RegistryClientBuilder::new(source_url)
            .allow_invalid_certs(true)
            .build()
            .unwrap(),
    );
    let target = Arc::new(RegistryClientBuilder::new(target_url).build().unwrap());
    let repos = populate_source(&source, 1, LAYERS, LAYER_BYTES).await;
    let mapping = resolved_mapping(
        source,
        repos[0].as_str(),
        repos[0].as_str(),
        vec![target_entry("target", target)],
        vec![TagPair::same("v1")],
    );

    let engine = SyncEngine::new(fast_retry(), 4);
    let sync = engine.run(
        vec![mapping],
        empty_cache(),
        BlobStage::disabled(),
        &NullProgress,
        None,
    );
    let report = tokio::time::timeout(Duration::from_secs(60), sync)
        .await
        .expect("the sync stalled");

    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.blobs_transferred, LAYERS as u64 + 1);
    let config = br#"{"image":0}"#.len();
    assert_eq!(
        report.stats.bytes_transferred,
        (LAYERS * LAYER_BYTES + config) as u64
    );
}

/// A blob body being read arrives in full while bodies opened before it
/// are left unread. Breaks if blob downloads share an HTTP/2 connection,
/// whose receive window the unread bodies fill.
#[tokio::test(flavor = "current_thread")]
async fn an_unread_blob_body_does_not_hold_back_another() {
    let (_container, url) = start_registry_tls().await;
    let client = RegistryClientBuilder::new(url)
        .allow_invalid_certs(true)
        .build()
        .unwrap();
    let repos = populate_source(&client, 1, 4, LAYER_BYTES).await;
    let digest = |l| Digest::from_sha256(Sha256::digest(&layer(0, l, LAYER_BYTES)));

    let mut unread = Vec::new();
    for l in 0..3 {
        unread.push(client.blob_pull(&repos[0], &digest(l)).await.unwrap());
    }
    let read = client.blob_pull(&repos[0], &digest(3)).await.unwrap();
    let bytes = read.try_fold(0, |n, chunk| async move { Ok(n + chunk.len()) });
    let bytes = tokio::time::timeout(Duration::from_secs(30), bytes)
        .await
        .expect("the body being read stalled")
        .unwrap();

    assert_eq!(bytes, LAYER_BYTES);
    drop(unread);
}
