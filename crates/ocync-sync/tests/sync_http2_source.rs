//! Syncs from a real `registry:2` serving HTTP/2, for flow-control behavior
//! a mock server cannot show.

mod helpers;

use std::sync::Arc;
use std::time::Duration;

use ocync_distribution::RegistryClientBuilder;
use ocync_sync::ImageStatus;
use ocync_sync::engine::{SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;

use helpers::*;

/// One image with more layers than the target takes uploads for at once
/// while its upload window is still growing, each far above the 65,535 byte
/// window an adaptive HTTP/2 connection starts with.
const CORPUS: Corpus = Corpus {
    images: 1,
    layers: 12,
    layer_bytes: 1 << 20,
};

/// A sync from an HTTP/2 source holding more large blobs than the target
/// uploads at once finishes promptly. Breaks if blob downloads share an
/// HTTP/2 connection: bodies opened for uploads still waiting on a limit
/// fill its receive window and starve the uploads that are running, so the
/// sync stalls until the source closes the requests.
#[tokio::test(flavor = "current_thread")]
async fn sync_from_an_http2_source_does_not_stall_on_unread_bodies() {
    let (_source_container, source_url) = start_registry_tls().await;
    let (_target_container, target_url) = start_registry().await;
    let source = Arc::new(
        RegistryClientBuilder::new(source_url)
            .allow_invalid_certs(true)
            .build()
            .unwrap(),
    );
    let target = Arc::new(RegistryClientBuilder::new(target_url).build().unwrap());
    let repos = populate_source(&source, &CORPUS).await;
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
    assert_eq!(report.stats.blobs_transferred, CORPUS.layers as u64 + 1);
    assert_eq!(report.stats.bytes_transferred, CORPUS.image_bytes(0));
}
