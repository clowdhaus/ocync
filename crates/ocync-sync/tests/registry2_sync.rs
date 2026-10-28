//! Syncs between real `registry:2` containers, for behavior a mock server
//! cannot show, such as HTTP/2 flow control on the source connection.

mod helpers;

use std::sync::Arc;
use std::time::Duration;

use ocync_distribution::RegistryClientBuilder;
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
