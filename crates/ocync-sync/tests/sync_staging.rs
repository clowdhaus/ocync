//! Staging integration tests: pull-once fan-out to several targets and staged
//! files on disk.

mod helpers;

use ocync_sync::engine::TagPair;
use ocync_sync::staging::BlobStage;
use wiremock::MockServer;

use helpers::*;

/// With staging on and two targets, each source request is made exactly once
/// (`expect(1)`) and both targets are pushed from the staged files, which
/// exist on disk with the pulled content.
#[tokio::test]
async fn sync_staging_writes_blobs_to_disk() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    let parts = ManifestBuilder::new(b"staging-disk-config")
        .layer(b"staging-disk-layer")
        .build();
    parts
        .mount_source_expecting(&source_server, "repo", "v1", 1, "staged fan-out")
        .await;
    for target in [&target_a, &target_b] {
        parts.mount_target(target, "repo", "v1").await;
    }

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());
    let mapping = mapping_to_targets(
        &source_server,
        &[("target-a", &target_a), ("target-b", &target_b)],
        "repo",
        vec![TagPair::same("v1")],
    );

    // Sequential, so target-a stages the blobs before target-b reads them.
    let report = run_engine(vec![mapping], fast_retry(), 1, empty_cache(), staging, None).await;

    assert_all_synced(&report, 2, "one image per target");
    assert_eq!(report.stats.blobs_transferred, 4, "2 blobs x 2 targets");

    for (digest, data) in [
        (&parts.config_desc.digest, &parts.config_data),
        (&parts.layer_descs[0].digest, &parts.layers_data[0]),
    ] {
        let staged = staging_dir
            .path()
            .join("blobs")
            .join(digest.algorithm())
            .join(digest.hex());
        assert!(staged.exists(), "blob should be staged at {staged:?}");
        assert_eq!(&std::fs::read(&staged).unwrap(), data);
    }
}
