//! Concurrent execution integration tests: non-sequential ordering, parallel
//! transfers, concurrent dedup, mount coordination, and blob concurrency bounds.

mod helpers;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::MediaType;
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ErrorKind, ImageStatus, RunOutcome, SyncReport};
use wiremock::http::Method;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use helpers::*;

/// Run with `max_concurrent`, `stage`, no cache and a live (untriggered)
/// shutdown signal.
async fn run_sync_with_concurrency(
    mappings: Vec<ResolvedMapping>,
    max_concurrent: usize,
    stage: BlobStage,
) -> SyncReport {
    run_engine(
        mappings,
        fast_retry(),
        max_concurrent,
        empty_cache(),
        stage,
        Some(&ShutdownSignal::new()),
    )
    .await
}

/// An index whose second child manifest pull returns 500 fails the whole image.
#[tokio::test]
async fn sync_index_manifest_child_pull_failure() {
    use ocync_distribution::spec::ImageIndex;

    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let amd64_parts = ManifestBuilder::new(b"amd64-config-fail")
        .layer(b"amd64-layer-fail")
        .build();
    let arm64_parts = ManifestBuilder::new(b"arm64-config-fail")
        .layer(b"arm64-layer-fail")
        .build();

    let index = ImageIndex {
        schema_version: 2,
        media_type: None,
        manifests: vec![
            make_descriptor(amd64_parts.digest.clone(), MediaType::OciManifest),
            make_descriptor(arm64_parts.digest.clone(), MediaType::OciManifest),
        ],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let index_bytes = serde_json::to_vec(&index).unwrap();

    source_manifest_mock("repo", "latest", &index_bytes, MediaType::OciIndex)
        .mount(&source_server)
        .await;
    mount_source_manifest(
        &source_server,
        "repo",
        &amd64_parts.digest.to_string(),
        &amd64_parts.bytes,
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", arm64_parts.digest)))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_server, "repo", "latest").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("latest")],
    );
    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Failed { .. });
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.outcome(), RunOutcome::AllFailed);
}

/// Blob 1 (config) push succeeds. Blob 2 (layer) push POST returns 403.
/// Verify image fails, `bytes_transferred` reflects only the first blob,
/// and manifest is NOT pushed.
#[tokio::test]
async fn sync_partial_blob_failure_stops_remaining() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let config_data = b"partial-config";
    let layer_data = b"partial-layer";
    let parts = ManifestBuilder::new(config_data).layer(layer_data).build();

    parts.mount_source(&source_server, "repo", "v1").await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[0].digest).await;

    // Both uploads start; the config's finalization PUT succeeds and the
    // layer's returns 403 (non-retryable). Matched by digest, so the order
    // the two blobs upload in does not matter.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/repo/blobs/uploads/partial-id"),
        )
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/partial-id"))
        .and(query_param("digest", parts.config_desc.digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/blobs/uploads/partial-id"))
        .and(query_param(
            "digest",
            parts.layer_descs[0].digest.to_string(),
        ))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&target_server)
        .await;

    // No manifest PUT mock: a manifest push would 404 and fail the status check.

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    // max_concurrent=1: sequential blob processing, config first.
    let report = run_sync_sequential(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Failed { .. });
    if let ImageStatus::Failed { kind, .. } = &report.images[0].status {
        assert_eq!(kind.to_string(), "blob transfer");
    }
    assert_eq!(
        report.images[0].bytes_transferred,
        config_data.len() as u64,
        "only the config blob should count as transferred"
    );
    assert_eq!(report.images[0].blob_stats.transferred, 1);
    assert_eq!(report.stats.images_failed, 1);
}

/// Two tags (v1, v2) with identical blob digests, run at `max_concurrent = 50`.
/// Dedup must work under real concurrency: both tags sync into one repo, so
/// the second waits on the first's upload claim instead of uploading too.
#[tokio::test]
async fn sync_concurrent_dedup_at_real_concurrency() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"dedup-config-concurrent")
        .layer(b"dedup-layer-concurrent")
        .build();

    // expect(1) on each tag: every manifest is pulled once.
    for tag in ["v1", "v2"] {
        source_manifest_mock("repo", tag, &parts.bytes, MediaType::OciManifest)
            .expect(1)
            .mount(&source_server)
            .await;
    }
    parts.mount_blob_pulls(&source_server, "repo").await;

    parts.mount_target(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_all_synced(&report, 2, "both images should sync");

    // Each shared blob is uploaded once and skipped once.
    let total_skipped: u64 = report.images.iter().map(|r| r.blob_stats.skipped).sum();
    assert_eq!(report.stats.blobs_transferred, 2, "no blob uploaded twice");
    assert_eq!(total_skipped, 2);
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::POST),
            PathMatch::Exact("/v2/repo/blobs/uploads/"),
        )
        .await,
        2,
        "one upload session per distinct blob"
    );
}

/// Run [`SHARED_TAGS`] concurrently from `source` into `target`.
async fn sync_shared_index_tags(source: &MockServer, target: &MockServer) -> SyncReport {
    let mapping = mapping_from_servers(source, target, "repo", same_tags(&SHARED_TAGS));
    run_sync(vec![mapping]).await
}

/// Four tags resolving to one two-child index, synced concurrently into one
/// repo. Each child manifest is pulled from the source and PUT to the target
/// once; each tag's index is still PUT.
#[tokio::test]
async fn sync_tags_sharing_an_index_copy_each_child_once() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let (amd64, arm64) = mount_shared_index(&source_server, &target_server).await;

    let report = sync_shared_index_tags(&source_server, &target_server).await;

    assert_eq!(report.stats.images_synced, 4);
    for child in [&amd64, &arm64] {
        let child_path = format!("/v2/repo/manifests/{}", child.digest);
        assert_eq!(
            count_requests(
                &source_server,
                Some(&Method::GET),
                PathMatch::Exact(&child_path),
            )
            .await,
            1,
            "child {} pulled once per tag",
            child.digest
        );
        assert_eq!(
            count_requests(
                &target_server,
                Some(&Method::PUT),
                PathMatch::Exact(&child_path),
            )
            .await,
            1,
            "child {} PUT once per tag",
            child.digest
        );
    }
    for tag in SHARED_TAGS {
        assert_eq!(
            count_requests(
                &target_server,
                Some(&Method::PUT),
                PathMatch::Exact(&format!("/v2/repo/manifests/{tag}")),
            )
            .await,
            1,
            "tag {tag} must still get its index PUT"
        );
    }
}

/// A child manifest PUT that fails is sent once, not once per tag sharing
/// the index: every tag fails with that error and no tag pushes its index.
#[tokio::test]
async fn sync_tags_sharing_an_index_push_a_failing_child_once() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let (amd64, _arm64) = mount_shared_index(&source_server, &target_server).await;
    let failing = amd64.digest.to_string();
    let failing_path = format!("/v2/repo/manifests/{failing}");
    Mock::given(method("PUT"))
        .and(path(failing_path.clone()))
        .respond_with(ResponseTemplate::new(400).set_body_string("manifest invalid"))
        .with_priority(1)
        .mount(&target_server)
        .await;

    let report = tokio::time::timeout(
        Duration::from_secs(30),
        sync_shared_index_tags(&source_server, &target_server),
    )
    .await
    .expect("tags waiting on a failed child must not hang");

    assert_eq!(report.stats.images_failed, 4);
    for image in &report.images {
        match &image.status {
            ImageStatus::Failed {
                status_code,
                error,
                retries,
                ..
            } => {
                assert_eq!(*status_code, Some(400), "{}", image.target);
                assert!(error.contains(&failing), "{}: {error}", image.target);
                assert_eq!(*retries, 0, "{}: a 400 is never retried", image.target);
            }
            other => panic!("{}: expected Failed, got {other:?}", image.target),
        }
    }
    assert_eq!(
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Exact(&failing_path),
        )
        .await,
        1,
        "a failed child must not be pushed again by each waiting tag"
    );
    for tag in SHARED_TAGS {
        assert_eq!(
            count_requests(
                &target_server,
                Some(&Method::PUT),
                PathMatch::Exact(&format!("/v2/repo/manifests/{tag}")),
            )
            .await,
            0,
            "tag {tag} must not PUT an index whose child failed"
        );
    }
}

/// A blob copy that fails is attempted once, not once per tag sharing it:
/// tags waiting on the failed copy fail with its error instead of repeating
/// it one after another.
#[tokio::test]
async fn sync_tags_sharing_a_blob_attempt_a_failing_copy_once() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let (amd64, _arm64) = mount_shared_index(&source_server, &target_server).await;
    let layer_path = format!("/v2/repo/blobs/{}", amd64.layer_descs[0].digest);
    Mock::given(method("GET"))
        .and(path(layer_path.clone()))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .mount(&source_server)
        .await;

    let report = tokio::time::timeout(
        Duration::from_secs(30),
        sync_shared_index_tags(&source_server, &target_server),
    )
    .await
    .expect("tags waiting on a failed blob copy must not hang");

    assert_eq!(report.stats.images_failed, 4);
    assert_eq!(
        count_requests(
            &source_server,
            Some(&Method::GET),
            PathMatch::Exact(&layer_path),
        )
        .await,
        1,
        "a failed blob copy must not be repeated per tag"
    );
}

/// Tag `one`'s index lists children `a` and `b`; tag `two`'s lists `b` and
/// `c`. `a` fails while `one` is still pulling `b`, which drops that pull.
/// `two`, waiting on `one`'s pull of `b`, must pull `b` itself and sync
/// rather than wait on a pull that will never finish.
#[tokio::test]
async fn sync_dropped_shared_child_pull_is_redone_by_the_waiter() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let a = ManifestBuilder::new(b"cfg-a").layer(b"layer-a").build();
    let b = ManifestBuilder::new(b"cfg-b").layer(b"layer-b").build();
    let c = ManifestBuilder::new(b"cfg-c").layer(b"layer-c").build();
    let one = IndexBuilder::new()
        .manifest(&a, Some(linux("amd64")))
        .manifest(&b, Some(linux("arm64")))
        .build();
    let two = IndexBuilder::new()
        .manifest(&b, Some(linux("arm64")))
        .manifest(&c, Some(linux("s390x")))
        .build();
    one.mount_source(&source_server, "repo", "one").await;
    one.mount_target(&target_server, "repo", "one").await;
    two.mount_source(&source_server, "repo", "two").await;
    two.mount_target(&target_server, "repo", "two").await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", a.digest)))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .mount(&source_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/manifests/{}", b.digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b.bytes.clone())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .set_delay(Duration::from_millis(300)),
        )
        .with_priority(1)
        .mount(&source_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("one"), TagPair::same("two")],
    );
    let report = tokio::time::timeout(Duration::from_secs(10), run_sync(vec![mapping]))
        .await
        .expect("a waiter on a dropped pull must not hang");

    assert_eq!(report.stats.images_failed, 1, "only `one` fails, on `a`");
    assert_eq!(
        report.stats.images_synced, 1,
        "`two` syncs, pulling `b` itself"
    );
}

/// Layer `d` cannot be pulled from repo `r`'s source, but repo `q` holds it
/// committed at the target, so a later task in `r` claims it to mount from
/// `q`. When that mount is not fulfilled, the task reports the recorded
/// failure instead of pulling `d` from the failing source again.
#[tokio::test]
async fn sync_unfulfilled_mount_does_not_repeat_a_failed_copy() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let in_r = ManifestBuilder::new(b"cfg-r").layer(b"layer-d").build();
    let in_q = ManifestBuilder::new(b"cfg-q").layer(b"layer-d").build();
    let d = in_r.layer_descs[0].digest.clone();
    for tag in ["t1", "t2"] {
        in_r.mount_source(&source_server, "r", tag).await;
        in_r.mount_target(&target_server, "r", tag).await;
    }
    in_q.mount_source(&source_server, "q", "u").await;
    in_q.mount_target(&target_server, "q", "u").await;
    let d_path = format!("/v2/r/blobs/{d}");
    Mock::given(method("GET"))
        .and(path(d_path.clone()))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .mount(&source_server)
        .await;

    // One at a time. `t1` leads (it shares the most blobs), so it runs
    // first and records the failed copy; `u` then commits `d` in `q`, and
    // `t2` finds `q` as a mount source.
    let report = run_sync_sequential(vec![
        mapping_from_servers(
            &source_server,
            &target_server,
            "q",
            vec![TagPair::same("u")],
        ),
        mapping_from_servers(
            &source_server,
            &target_server,
            "r",
            vec![TagPair::same("t1"), TagPair::same("t2")],
        ),
    ])
    .await;
    assert_eq!(
        report.stats.images_synced, 1,
        "`u` syncs from its own source"
    );
    assert_eq!(report.stats.images_failed, 2);
    assert_eq!(
        count_requests(
            &source_server,
            Some(&Method::GET),
            PathMatch::Exact(&d_path),
        )
        .await,
        1,
        "the failed copy of `d` into `r` must not be repeated"
    );
}

/// Tag `x` lists children `c1` and `c2`; tag `y` lists `c2` and `c3`. `x`
/// holds `c2`, whose push is slow. `y`'s own child `c3` fails, so `y` fails
/// at once instead of waiting on `c2`, which its index can no longer use.
#[tokio::test]
async fn sync_failed_own_child_does_not_wait_on_shared_children() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let c1 = ManifestBuilder::new(b"cfg-c1").layer(b"layer-c1").build();
    let c2 = ManifestBuilder::new(b"cfg-c2").layer(b"layer-c2").build();
    let c3 = ManifestBuilder::new(b"cfg-c3").layer(b"layer-c3").build();
    let x = IndexBuilder::new()
        .manifest(&c1, Some(linux("amd64")))
        .manifest(&c2, Some(linux("arm64")))
        .build();
    let y = IndexBuilder::new()
        .manifest(&c2, Some(linux("arm64")))
        .manifest(&c3, Some(linux("s390x")))
        .build();
    x.mount_source(&source_server, "repo", "x").await;
    x.mount_target(&target_server, "repo", "x").await;
    y.mount_source(&source_server, "repo", "y").await;
    y.mount_target(&target_server, "repo", "y").await;
    // `y`'s own layer arrives late, so `x` has always claimed `c2` by the
    // time `y` pushes its children.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/blobs/{}", c3.layer_descs[0].digest)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(c3.layers_data[0].clone())
                .set_delay(Duration::from_millis(300)),
        )
        .with_priority(1)
        .mount(&source_server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{}", c2.digest)))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(2)))
        .with_priority(1)
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{}", c3.digest)))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .mount(&target_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("y"), TagPair::same("x")],
    );
    let report = run_sync(vec![mapping]).await;

    let y_result = report
        .images
        .iter()
        .find(|i| i.target.ends_with(":y"))
        .expect("y is reported");
    assert!(
        matches!(y_result.status, ImageStatus::Failed { .. }),
        "{:?}",
        y_result.status
    );
    assert!(
        y_result.duration < Duration::from_secs(1),
        "y waited {:?} on a child its index can no longer use",
        y_result.duration
    );
}

/// Two concurrent transfers to different repos at one registry share a blob.
/// The second must wait for the first's upload to finish, then mount from the
/// first repo, not re-upload. If both raced past the cache check they would
/// both upload in full.
///
/// Order-independent: both targets accept upload and mount, the election
/// picks the uploader, and the assertion is aggregate (2 transferred, 2 mounted).
#[tokio::test]
async fn sync_concurrent_shared_blob_mounts_instead_of_double_uploading() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"cfg-concurrent-mount")
        .layer(b"layer-concurrent-mount")
        .build();

    parts.mount_source(&source_server, "src-a", "v1").await;
    parts.mount_source(&source_server, "src-b", "v1").await;
    for repo in ["tgt-a", "tgt-b"] {
        parts.mount_target(&target_server, repo, "v1").await;
        mount_blob_mount_accepted(&target_server, repo, &parts.config_desc.digest).await;
        mount_blob_mount_accepted(&target_server, repo, &parts.layer_descs[0].digest).await;
    }

    let mappings = v1_mappings(
        &source_server,
        &target_server,
        &[("src-a", "tgt-a"), ("src-b", "tgt-b")],
    );
    let report = run_sync_with_concurrency(mappings, 10, BlobStage::disabled()).await;

    assert_all_synced(&report, 2, "both images should sync");
    // One image uploads 2 blobs (leader), the other mounts 2 (follower).
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_mounted, 2);
    assert!(
        report.images.iter().any(|r| r.blob_stats.mounted > 0),
        "at least one image should mount blobs from the other"
    );
    assert!(
        report.images.iter().any(|r| r.blob_stats.transferred > 0),
        "at least one image should transfer blobs"
    );
}

/// Three images sharing a layer exercise the full leader-follower pipeline:
/// election picks one leader, wave promotion runs the two followers, and
/// the shared layer is mounted from the leader's committed repo.
#[tokio::test(flavor = "current_thread")]
async fn sync_wave_promotion_three_images_shared_layer() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared layer across all three images; config blobs are unique.
    let shared_data = b"shared-layer-wave-test";
    let parts_a = ManifestBuilder::new(b"config-a-wave")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-b-wave")
        .layer(shared_data)
        .build();
    let parts_c = ManifestBuilder::new(b"config-c-wave")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;
    parts_c.mount_source(&source_server, "src-c", "v1").await;

    let shared = &parts_a.layer_descs[0].digest;
    let blobs = [
        &parts_a.config_desc.digest,
        &parts_b.config_desc.digest,
        &parts_c.config_desc.digest,
        shared,
    ];
    for repo in ["tgt-a", "tgt-b", "tgt-c"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
        mount_blob_mount_accepted(&target_server, repo, shared).await;
    }

    let mappings = v1_mappings(
        &source_server,
        &target_server,
        &[("src-a", "tgt-a"), ("src-b", "tgt-b"), ("src-c", "tgt-c")],
    );
    let report = run_sync_with_concurrency(mappings, 10, BlobStage::disabled()).await;

    assert_all_synced(&report, 3, "all three images should sync");
    // 3 unique config blobs transferred + 1 shared layer transferred by leader.
    assert_eq!(report.stats.blobs_transferred, 4);
    // Shared layer mounted by 2 followers.
    assert_eq!(report.stats.blobs_mounted, 2);
}

/// An image with 8 blobs, each delayed 100ms on source pull, runs under
/// `BLOB_CONCURRENCY=6`. We assert on the arrival span of the 8 blob GETs at
/// the source mock (parallel: first 6 cluster at ~0, last 2 at ~100ms;
/// serialized: 8 spread across ~700ms) rather than total wall-clock, so CI
/// runner jitter cannot flake the test.
#[tokio::test(flavor = "current_thread")]
async fn sync_blob_concurrency_processes_multiple_blobs() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // 1 config + 7 layers = 8 blobs, each with unique data.
    let mut builder = ManifestBuilder::new(b"concurrency-config-blob");
    for i in 0..7 {
        builder = builder.layer(format!("concurrency-layer-{i}").as_bytes());
    }
    let parts = builder.build();
    let blobs: Vec<(&Digest, &[u8])> =
        std::iter::once((&parts.config_desc.digest, parts.config_data.as_slice()))
            .chain(
                parts
                    .layer_descs
                    .iter()
                    .map(|d| &d.digest)
                    .zip(parts.layers_data.iter().map(Vec::as_slice)),
            )
            .collect();

    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    let arrivals: Arc<Mutex<Vec<std::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));
    for (digest, data) in &blobs {
        let arrivals = Arc::clone(&arrivals);
        Mock::given(method("GET"))
            .and(path(format!("/v2/repo/blobs/{digest}")))
            .and(move |_req: &Request| {
                arrivals.lock().unwrap().push(std::time::Instant::now());
                true
            })
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(data.to_vec())
                    .insert_header("content-length", data.len().to_string())
                    .set_delay(Duration::from_millis(100)),
            )
            .mount(&source_server)
            .await;
    }

    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let report = run_sync_with_concurrency(vec![mapping], 1, BlobStage::disabled()).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        report.stats.blobs_transferred, 8,
        "all 8 blobs should be transferred"
    );

    let times = arrivals.lock().unwrap();
    assert_eq!(
        times.len(),
        8,
        "expected 8 blob GETs at source, got {}",
        times.len()
    );
    let min = times.iter().min().unwrap();
    let max = times.iter().max().unwrap();
    let span = max.duration_since(*min);
    // Parallel spans ~100ms, serialized ~700ms; 400ms leaves ~300ms margin both ways.
    assert!(
        span < Duration::from_millis(400),
        "expected blob GETs to cluster (parallel: ~100ms span), got {span:?}; sequential would span ~700ms",
    );
}

/// First-failure-stops semantics: when one blob source returns 500, the
/// image fails and not all blobs complete.
#[tokio::test(flavor = "current_thread")]
async fn sync_blob_failure_cancels_remaining_blobs() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // 1 config + 3 layers = 4 blobs.
    let parts = ManifestBuilder::new(b"cancel-config")
        .layer(b"cancel-layer-1")
        .layer(b"cancel-layer-2-will-fail")
        .layer(b"cancel-layer-3-never-reached")
        .build();

    // Layer 2 always returns 500 (priority 1 over the servable mock).
    parts.mount_source(&source_server, "repo", "v1").await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/repo/blobs/{}",
            parts.layer_descs[1].digest
        )))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .with_priority(1)
        .mount(&source_server)
        .await;

    // No manifest PUT mock: a manifest push would 404 and fail the status check.
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    parts.mount_target_blob_misses(&target_server, "repo").await;
    mount_blob_push(&target_server, "repo").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let report = run_sync_with_concurrency(vec![mapping], 1, BlobStage::disabled()).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Failed { .. });
    assert!(
        report.images[0].blob_stats.transferred < 4,
        "transferred {} should be < 4 (cancel should stop remaining blobs)",
        report.images[0].blob_stats.transferred
    );
    assert_eq!(report.stats.images_failed, 1);
}

/// Two images sharing a blob sync concurrently with staging enabled, to
/// different targets. Concurrent staging writes to the same digest must not
/// panic or produce ENOENT errors (unique tmp paths prevent collision).
#[tokio::test(flavor = "current_thread")]
async fn sync_staging_concurrent_write_no_collision() {
    let source_server = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;

    // Shared blob across both images; unique config blobs per image.
    let shared_data = b"shared-staging-collision-test";
    let parts_a = ManifestBuilder::new(b"staging-cfg-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"staging-cfg-b")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;
    parts_a.mount_target(&target_a, "tgt-a", "v1").await;
    parts_b.mount_target(&target_b, "tgt-b", "v1").await;

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    let source_client = mock_client(&source_server);
    let mapping_a = resolved_mapping(
        Arc::clone(&source_client),
        "src-a",
        "tgt-a",
        vec![target_entry("target-a", mock_client(&target_a))],
        vec![TagPair::same("v1")],
    );
    let mapping_b = resolved_mapping(
        source_client,
        "src-b",
        "tgt-b",
        vec![target_entry("target-b", mock_client(&target_b))],
        vec![TagPair::same("v1")],
    );

    let report = run_sync_with_concurrency(vec![mapping_a, mapping_b], 10, staging).await;

    assert_all_synced(&report, 2, "both images should sync without collision");
    assert!(
        report.stats.blobs_transferred >= 2,
        "at least 2 unique config blobs should be transferred, got {}",
        report.stats.blobs_transferred
    );
}

/// Source-pull dedup: when two images share a blob and staging is enabled,
/// the source blob should be pulled exactly once. The second image's task
/// waits for the first to finish staging, then reads from disk.
#[tokio::test(flavor = "current_thread")]
async fn sync_source_pull_dedup_with_staging() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Shared layer across both images; config blobs are unique.
    let shared_data = b"shared-source-dedup-layer";
    let parts_a = ManifestBuilder::new(b"dedup-cfg-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"dedup-cfg-b")
        .layer(shared_data)
        .build();

    mount_source_manifest(&source_server, "src-a", "v1", &parts_a.bytes).await;
    mount_source_manifest(&source_server, "src-b", "v1", &parts_b.bytes).await;
    mount_blob_pull(
        &source_server,
        "src-a",
        &parts_a.config_desc.digest,
        &parts_a.config_data,
    )
    .await;
    mount_blob_pull(
        &source_server,
        "src-b",
        &parts_b.config_desc.digest,
        &parts_b.config_data,
    )
    .await;
    // The shared blob is served by both repos, `.expect(0..=1)` on each: dedup
    // allows at most one GET across the two.
    let mut shared_pulls = Vec::new();
    for repo in ["src-a", "src-b"] {
        shared_pulls.push(
            Mock::given(method("GET"))
                .and(path(format!(
                    "/v2/{repo}/blobs/{}",
                    parts_a.layer_descs[0].digest
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(shared_data.to_vec())
                        .insert_header("content-length", shared_data.len().to_string()),
                )
                .expect(0..=1)
                .named(format!("shared blob GET {repo}"))
                .mount_as_scoped(&source_server)
                .await,
        );
    }

    let blobs = [
        &parts_a.config_desc.digest,
        &parts_b.config_desc.digest,
        &parts_a.layer_descs[0].digest,
    ];
    for repo in ["tgt-a", "tgt-b"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
    }

    let staging_dir = tempfile::tempdir().unwrap();
    let staging = BlobStage::new(staging_dir.path().to_path_buf());

    // One target client per mapping, so each pulls and pushes on its own connection state.
    let source_client = mock_client(&source_server);
    let mappings = [("src-a", "tgt-a"), ("src-b", "tgt-b")]
        .into_iter()
        .map(|(src, tgt)| {
            resolved_mapping(
                Arc::clone(&source_client),
                src,
                tgt,
                vec![target_entry("target", mock_client(&target_server))],
                vec![TagPair::same("v1")],
            )
        })
        .collect();
    let report = run_sync_with_concurrency(mappings, 10, staging).await;

    // Drop scoped mocks before checking received requests.
    drop(shared_pulls);

    assert_all_synced(&report, 2, "both images should sync");
    // 4 blobs pushed: 2 unique configs + shared layer to each target. Dedup
    // reduces source GETs, not target pushes.
    assert_eq!(report.stats.blobs_transferred, 4);

    // Without dedup the shared blob would be pulled twice, once per image.
    let shared_blob_suffix = format!("/blobs/{}", parts_a.layer_descs[0].digest);
    let source_gets_for_shared = count_requests(
        &source_server,
        Some(&Method::GET),
        PathMatch::Contains(&shared_blob_suffix),
    )
    .await;
    assert_eq!(
        source_gets_for_shared, 1,
        "shared blob should be pulled from source exactly once (dedup), got {source_gets_for_shared}"
    );
}

/// An image with more layers than the per-image blob concurrency limit (6)
/// still syncs: the semaphore neither deadlocks nor drops permits.
#[tokio::test]
async fn sync_image_with_more_layers_than_blob_concurrency_limit() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let mut builder = ManifestBuilder::new(b"config-many-layers");
    for i in 1..=10 {
        builder = builder.layer(format!("layer-{i:02}").as_bytes());
    }
    let parts = builder.build();

    parts.mount_source(&source_server, "repo/many", "v1").await;
    parts.mount_target(&target_server, "repo/many", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo/many",
        vec![TagPair::same("v1")],
    );
    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // 11 blobs total: 1 config + 10 layers.
    assert_eq!(report.images[0].blob_stats.transferred, 11);
    assert_eq!(report.stats.blobs_transferred, 11);
}

/// The leader's manifest push succeeds instantly, while the follower's config
/// pull is delayed. The leader calls `mark_repo_committed` before the follower
/// ever calls `unresolved_watches_for` (fire-before-subscribe).
///
/// Without `send_replace` (plain `send`), the leader's signal would be lost
/// and the follower would deadlock waiting for a commit that already happened.
#[tokio::test(flavor = "current_thread")]
async fn sync_leader_commits_before_follower_subscribes() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-late-subscribe";
    let parts_a = ManifestBuilder::new(b"config-leader-fast")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-follower-slow")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    // Follower source: manifest and shared layer instant, config delayed 200ms
    // so the follower reaches the mount-source check after the leader commits.
    mount_source_manifest(&source_server, "src-b", "v1", &parts_b.bytes).await;
    mount_blob_pull(
        &source_server,
        "src-b",
        &parts_b.layer_descs[0].digest,
        shared_data,
    )
    .await;
    mount_blob_pull_delayed(
        &source_server,
        "src-b",
        &parts_b.config_desc.digest,
        &parts_b.config_data,
        Duration::from_millis(200),
    )
    .await;

    // Both repos accept upload AND mount (symmetric mocks).
    let blobs = [
        &parts_a.config_desc.digest,
        &parts_b.config_desc.digest,
        &parts_a.layer_descs[0].digest,
    ];
    for repo in ["tgt-a", "tgt-b"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
        mount_blob_mount_accepted(&target_server, repo, &parts_a.layer_descs[0].digest).await;
    }

    let mappings = v1_mappings(
        &source_server,
        &target_server,
        &[("src-a", "tgt-a"), ("src-b", "tgt-b")],
    );
    let report = run_sync_with_concurrency(mappings, 10, BlobStage::disabled()).await;

    assert_all_synced(&report, 2, "both images should sync (no deadlock)");
    // Leader transfers shared layer + config; follower mounts shared layer + transfers config.
    assert_eq!(
        report.stats.blobs_transferred + report.stats.blobs_mounted,
        4
    );
    assert!(
        report.stats.blobs_mounted >= 1,
        "follower should mount the shared layer, got {} mounts",
        report.stats.blobs_mounted,
    );
}

/// Leader's manifest push fails (500). Followers waiting on the watch channel
/// must unblock via `notify_repo_failed`, find the repo not committed,
/// and fall through to HEAD+push instead of deadlocking.
#[tokio::test(flavor = "current_thread")]
async fn sync_leader_manifest_push_fails_follower_falls_through() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-leader-fail";
    let parts_a = ManifestBuilder::new(b"config-leader-fail")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-follower-fallback")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;

    // tgt-a (leader): blob uploads succeed but the manifest PUT fails (500).
    parts_a.mount_target(&target_server, "tgt-a", "v1").await;
    Mock::given(method("PUT"))
        .and(path("/v2/tgt-a/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&target_server)
        .await;

    // tgt-b (follower): everything succeeds, including the push fallback; the
    // mount mock covers a mount attempt before it falls through.
    parts_b.mount_target(&target_server, "tgt-b", "v1").await;
    mount_blob_mount_accepted(&target_server, "tgt-b", &parts_a.layer_descs[0].digest).await;

    let mappings = v1_mappings(
        &source_server,
        &target_server,
        &[("src-a", "tgt-a"), ("src-b", "tgt-b")],
    );
    let report = run_sync_with_concurrency(mappings, 10, BlobStage::disabled()).await;

    assert_eq!(report.images.len(), 2);
    assert!(
        report
            .images
            .iter()
            .any(|r| matches!(r.status, ImageStatus::Failed { .. })),
        "leader should fail"
    );
    assert!(
        report
            .images
            .iter()
            .any(|r| matches!(r.status, ImageStatus::Synced)),
        "follower should succeed after leader failure: {:#?}",
        report.images.iter().map(|r| &r.status).collect::<Vec<_>>()
    );
}

/// One image in a [`DeadlockCase`]: a config blob and its layers.
struct CaseImage {
    config: &'static [u8],
    layers: &'static [&'static [u8]],
}

/// Images with overlapping layers, run under the bounded-deadline resolver.
struct DeadlockCase {
    name: &'static str,
    images: &'static [CaseImage],
    timeout: Duration,
    /// Mounts the run must show. 0 where the second leader may legitimately
    /// fall through to a push when no committed source appears in time.
    min_mounted: u64,
}

/// Neither case reaches the blob-semaphore deadlock (shared blob count is
/// below `BLOB_CONCURRENCY`); that is `sync_cross_image_blob_claim_no_deadlock`.
const DEADLOCK_CASES: &[DeadlockCase] = &[
    // A and B share two layers (forcing `elect_leaders` to pick two leaders); A
    // also shares a layer with C and B another, so C is a follower of both.
    DeadlockCase {
        name: "three images, two leaders and a follower",
        images: &[
            CaseImage {
                config: b"config-leader-a-deadlock",
                layers: &[
                    b"shared-ab-layer-1-deadlock-test",
                    b"shared-ab-layer-2-deadlock-test",
                    b"shared-ac-layer-deadlock-test",
                ],
            },
            CaseImage {
                config: b"config-leader-b-deadlock",
                layers: &[
                    b"shared-ab-layer-1-deadlock-test",
                    b"shared-ab-layer-2-deadlock-test",
                    b"shared-bc-layer-deadlock-test",
                ],
            },
            CaseImage {
                config: b"config-follower-c-deadlock",
                layers: &[
                    b"shared-ac-layer-deadlock-test",
                    b"shared-bc-layer-deadlock-test",
                ],
            },
        ],
        timeout: Duration::from_secs(5),
        min_mounted: 1,
    },
    DeadlockCase {
        name: "two images sharing two layers",
        images: &[
            CaseImage {
                config: b"two-leaders-cfg-a",
                layers: &[b"two-leaders-shared-1", b"two-leaders-shared-2"],
            },
            CaseImage {
                config: b"two-leaders-cfg-b",
                layers: &[b"two-leaders-shared-1", b"two-leaders-shared-2"],
            },
        ],
        timeout: Duration::from_secs(10),
        min_mounted: 0,
    },
];

/// Images whose layers overlap never wait on each other's manifest commit in a
/// cycle: whichever leader commits first becomes a mount source for the other,
/// and if none commits in time the deadline trips and the waiter pushes.
#[tokio::test(flavor = "current_thread")]
async fn overlapping_shared_blobs_complete_without_deadlock() {
    for case in DEADLOCK_CASES {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parts: Vec<ManifestParts> = case
            .images
            .iter()
            .map(|image| {
                let mut builder = ManifestBuilder::new(image.config);
                for layer in image.layers {
                    builder = builder.layer(layer);
                }
                builder.build()
            })
            .collect();
        let digests: Vec<Digest> = parts.iter().flat_map(|p| p.blob_digests()).collect();
        let digest_refs: Vec<&Digest> = digests.iter().collect();

        let repos: Vec<(String, String)> = (0..parts.len())
            .map(|i| (format!("src-{i}"), format!("tgt-{i}")))
            .collect();
        for (part, (src, tgt)) in parts.iter().zip(&repos) {
            part.mount_source(&source_server, src, "v1").await;
            mount_target_fresh(&target_server, tgt, "v1", &digest_refs).await;
            for layer in &part.layer_descs {
                mount_blob_mount_accepted(&target_server, tgt, &layer.digest).await;
            }
        }

        let report = tokio::time::timeout(
            case.timeout,
            run_sync_with_concurrency(
                v1_mappings(&source_server, &target_server, &repos),
                10,
                BlobStage::disabled(),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{}: engine deadlocked on a commit-wait cycle", case.name));

        assert_all_synced(&report, parts.len(), &format!("{}: no deadlock", case.name));
        assert!(
            report.stats.blobs_mounted >= case.min_mounted,
            "{}: expected at least {} mounts, got {}",
            case.name,
            case.min_mounted,
            report.stats.blobs_mounted
        );
    }
}

/// Two images share 8 layers (more than `BLOB_CONCURRENCY`=6). Source blobs
/// for image B are delayed 50ms so image A claims all shared blobs first, and
/// B enters claim-waits for every one. If claim-waits held semaphore permits,
/// B's 6 permits would be consumed and its unique config blob could never
/// acquire one, hanging both images. Claim-waits happen outside the
/// semaphore, so both complete.
#[tokio::test(flavor = "current_thread")]
async fn sync_cross_image_blob_claim_no_deadlock() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let mut builder_a = ManifestBuilder::new(b"config-claim-deadlock-a");
    let mut builder_b = ManifestBuilder::new(b"config-claim-deadlock-b");
    for i in 0..8 {
        let layer = format!("shared-claim-deadlock-layer-{i}").into_bytes();
        builder_a = builder_a.layer(&layer);
        builder_b = builder_b.layer(&layer);
    }
    let parts_a = builder_a.build();
    let parts_b = builder_b.build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    // Source B's blob pulls are delayed 50ms.
    parts_b
        .mount_source_delayed(&source_server, "src-b", "v1", Duration::from_millis(50))
        .await;

    let blobs: Vec<&Digest> = [&parts_a.config_desc.digest, &parts_b.config_desc.digest]
        .into_iter()
        .chain(parts_a.layer_descs.iter().map(|d| &d.digest))
        .collect();
    for repo in ["tgt-a", "tgt-b"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
    }

    let mappings = v1_mappings(
        &source_server,
        &target_server,
        &[("src-a", "tgt-a"), ("src-b", "tgt-b")],
    );
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        run_sync_with_concurrency(mappings, 10, BlobStage::disabled()),
    )
    .await
    .expect("engine deadlocked: cross-image blob claim waits exhausted blob semaphore permits");

    assert_all_synced(&report, 2, "both images should sync (no deadlock)");
}

/// Pre-warm the cache with committed repos so the follower's mount-source
/// lookup returns a Tier 1 match (committed + preferred). The follower should
/// mount immediately without waiting on the watch channel, exercising the
/// `Some(repo) if is_committed` branch (not the watch-wait branch).
#[tokio::test(flavor = "current_thread")]
async fn sync_prewarmed_committed_repo_skips_watch_wait() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-prewarm-mount";
    let parts_a = ManifestBuilder::new(b"config-prewarm-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-prewarm-b")
        .layer(shared_data)
        .build();

    parts_b.mount_source(&source_server, "src-b", "v1").await;
    parts_b.mount_target(&target_server, "tgt-b", "v1").await;

    // The shared layer mounts from tgt-a (the pre-warmed committed source).
    Mock::given(method("POST"))
        .and(path("/v2/tgt-b/blobs/uploads/"))
        .and(query_param(
            "mount",
            parts_b.layer_descs[0].digest.to_string(),
        ))
        .and(query_param("from", "tgt-a"))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .expect(1)
        .mount(&target_server)
        .await;

    let target_name = "target";
    let target_auth = mock_target_id("target", &target_server);
    let cache = cache_with(
        &target_auth,
        &repo("tgt-a"),
        &[&parts_a.layer_descs[0].digest],
    );
    cache
        .borrow_mut()
        .mark_repo_committed(&target_auth, &repo("tgt-a"));

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "src-b",
        "tgt-b",
        vec![target_entry(target_name, mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    // Shared layer mounted from pre-warmed tgt-a; config transferred.
    assert_eq!(report.images[0].blob_stats.mounted, 1);
    assert_eq!(report.images[0].blob_stats.transferred, 1);
}

/// Two images at `tgt-a` and `tgt-b` share one layer, and both manifest PUTs
/// answer only after `commit_delay`. Whichever image uploads the layer leaves
/// it at its repo uncommitted for that long, so the other waits on that repo's
/// commit watch. No mount is ever fulfilled.
async fn mount_slow_commit_pair(
    source: &MockServer,
    target: &MockServer,
    commit_delay: Duration,
) -> (ManifestParts, ManifestParts) {
    let shared = b"slow-commit-shared-layer";
    let parts_a = ManifestBuilder::new(b"slow-commit-config-a")
        .layer(shared)
        .build();
    let parts_b = ManifestBuilder::new(b"slow-commit-config-b")
        .layer(shared)
        .build();
    parts_a.mount_source(source, "src-a", "v1").await;
    parts_b.mount_source(source, "src-b", "v1").await;

    for (repo, parts) in [("tgt-a", &parts_a), ("tgt-b", &parts_b)] {
        mount_manifest_head_not_found(target, repo, "v1").await;
        parts.mount_target_blob_misses(target, repo).await;
        blob_mount_mock(repo, &parts.layer_descs[0].digest, 201)
            .expect(0)
            .named(format!("{repo}: no mount is fulfilled"))
            .mount(target)
            .await;
        mount_blob_push(target, repo).await;
        Mock::given(method("PUT"))
            .and(path(format!("/v2/{repo}/manifests/v1")))
            .respond_with(ResponseTemplate::new(201).set_delay(commit_delay))
            .mount(target)
            .await;
    }
    (parts_a, parts_b)
}

/// One mapping per image of [`mount_slow_commit_pair`], each with its own
/// clients: both slow manifest PUTs must run at once, which a shared client's
/// initial manifest-write window of 1 would serialize.
fn slow_commit_mappings(source: &MockServer, target: &MockServer) -> Vec<ResolvedMapping> {
    [("src-a", "tgt-a"), ("src-b", "tgt-b")]
        .into_iter()
        .map(|(src, tgt)| mapping_with_distinct_repos(source, target, src, tgt, same_tags(&["v1"])))
        .collect()
}

/// A wait for another image's manifest commit is bounded by the mount-source
/// wait deadline. The deadline expires before the slow commit, and the waiting
/// image pushes the layer instead of mounting it. Without the bound, it would
/// wait out the slow PUT and then mount.
#[tokio::test(flavor = "current_thread")]
async fn sync_mount_wait_on_an_uncommitted_repo_is_bounded() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    mount_slow_commit_pair(&source_server, &target_server, Duration::from_secs(3)).await;

    let engine = SyncEngine::new(fast_retry(), concurrency(10))
        .with_mount_source_wait_deadline(Duration::from_millis(500));
    let report = tokio::time::timeout(
        Duration::from_secs(20),
        engine.run(
            slow_commit_mappings(&source_server, &target_server),
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        ),
    )
    .await
    .expect("commit wait was not bounded");

    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_mounted, 0);
    // Each image pushes its config and the shared layer.
    assert_eq!(report.stats.blobs_transferred, 4);
}

/// Shutdown while one image waits on the other's slow manifest commit drains
/// the engine within the drain deadline instead of waiting out the 60s
/// mount-source wait or the 30s commit. Exactly one image has uploaded the
/// shared layer by then: the other was still waiting on its commit.
#[tokio::test(flavor = "current_thread")]
async fn resolver_cancel_during_wait_drops_cleanly() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let (parts, _) =
        mount_slow_commit_pair(&source_server, &target_server, Duration::from_secs(30)).await;

    let shutdown = ShutdownSignal::new();
    let engine = SyncEngine::new(fast_retry(), concurrency(10))
        .with_drain_deadline(Duration::from_millis(100))
        .with_mount_source_wait_deadline(Duration::from_secs(60));
    let shared = &parts.layer_descs[0].digest;
    let shared_uploads = async || {
        count_blob_uploads(
            &target_server,
            PathMatch::Contains("/blobs/uploads/"),
            shared,
        )
        .await
    };
    let manifest_puts = async || {
        count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Contains("/manifests/"),
        )
        .await
    };

    let started = std::time::Instant::now();
    // The manifest PUT is sent after every blob upload and answers after 30s,
    // so shutdown lands inside the commit and the other image's wait.
    let report = run_with_shutdown_at(
        tokio::time::timeout(
            Duration::from_secs(10),
            engine.run(
                slow_commit_mappings(&source_server, &target_server),
                empty_cache(),
                BlobStage::disabled(),
                &NullProgress,
                Some(&shutdown),
            ),
        ),
        &shutdown,
        1,
        manifest_puts,
    )
    .await;
    let report = report.expect("engine must drain on shutdown during the resolver wait");

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "drained after {:?}, not at the drain deadline",
        started.elapsed()
    );
    assert_eq!(report.stats.images_synced, 0, "no image commits in time");
    assert_eq!(
        shared_uploads().await,
        1,
        "the shared layer is uploaded once; its waiter had not pushed it"
    );
}

/// A blob failure ends the image's other blobs' waits. Image B's shared layer
/// waits on image A, and B's own layer fails to pull. B is reported failed at
/// once instead of after A's delay, and never pushes the shared layer. The
/// delay is A's manifest commit (B waits to mount from A's repo) or A's blob
/// upload (B waits on A's claim of the layer).
#[tokio::test(flavor = "current_thread")]
async fn sync_blob_failure_ends_the_images_other_blob_waits() {
    #[derive(Debug, Clone, Copy)]
    enum Slow {
        Commit,
        Upload,
    }
    const DELAY: Duration = Duration::from_secs(10);
    // Long enough for B's shared layer to reach its wait first, even on a
    // slow host.
    const FAILURE_DELAY: Duration = Duration::from_secs(2);

    // Both cases run at once; each has its own servers.
    futures_util::future::join_all([Slow::Commit, Slow::Upload].map(async |slow| {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parts_a = ManifestBuilder::new(b"cancel-wait-config-a")
            .layer(b"cancel-wait-shared")
            .build();
        let parts_b = ManifestBuilder::new(b"cancel-wait-config-b")
            .layer(b"cancel-wait-shared")
            .layer(b"cancel-wait-unpullable")
            .build();
        parts_a.mount_source(&source_server, "src-a", "v1").await;
        parts_b.mount_source(&source_server, "src-b", "v1").await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/src-b/blobs/{}",
                parts_b.layer_descs[1].digest
            )))
            .respond_with(ResponseTemplate::new(404).set_delay(FAILURE_DELAY))
            .with_priority(1)
            .mount(&source_server)
            .await;

        for (repo, parts) in [("tgt-a", &parts_a), ("tgt-b", &parts_b)] {
            mount_manifest_head_not_found(&target_server, repo, "v1").await;
            parts.mount_target_blob_misses(&target_server, repo).await;
            mount_blob_push(&target_server, repo).await;
        }
        let (commit_delay, upload_delay) = match slow {
            Slow::Commit => (DELAY, Duration::ZERO),
            Slow::Upload => (Duration::ZERO, DELAY),
        };
        Mock::given(method("PUT"))
            .and(path("/v2/tgt-a/manifests/v1"))
            .respond_with(ResponseTemplate::new(201).set_delay(commit_delay))
            .mount(&target_server)
            .await;
        // Outranks the instant finalize in `mount_blob_push`.
        Mock::given(method("PUT"))
            .and(path("/v2/tgt-a/blobs/uploads/upload-id"))
            .respond_with(ResponseTemplate::new(201).set_delay(upload_delay))
            .with_priority(1)
            .mount(&target_server)
            .await;

        // Leader election breaks a tie in favor of the later image, so listing
        // B first makes A the uploader and B the waiter.
        let mut mappings = slow_commit_mappings(&source_server, &target_server);
        mappings.reverse();
        let engine = SyncEngine::new(fast_retry(), concurrency(10))
            .with_mount_source_wait_deadline(Duration::from_secs(60));
        let report = tokio::time::timeout(
            DELAY * 3,
            engine.run(
                mappings,
                empty_cache(),
                BlobStage::disabled(),
                &NullProgress,
                None,
            ),
        )
        .await
        .expect("a blob failure must not leave the run waiting");

        let image_b = report
            .images
            .iter()
            .find(|i| i.target.contains("tgt-b"))
            .expect("image B is reported");
        let failing_digest = parts_b.layer_descs[1].digest.to_string();
        assert!(
            matches!(
                &image_b.status,
                ImageStatus::Failed {
                    kind: ErrorKind::BlobTransfer,
                    error,
                    status_code: Some(404),
                    ..
                } if error.contains(&failing_digest)
            ),
            "{slow:?}: {:?}",
            image_b.status
        );
        // The negative assertion: B ends at its own failure, not when A's
        // slow step lets the waiting layer through.
        assert!(
            image_b.duration < DELAY / 2,
            "{slow:?}: B failed after {:?}, its shared layer kept waiting on A",
            image_b.duration
        );
        assert_eq!(
            count_blob_uploads(
                &target_server,
                PathMatch::Contains("/tgt-b/blobs/uploads/"),
                &parts_b.layer_descs[0].digest,
            )
            .await,
            0,
            "{slow:?}: B never pushes the layer it was waiting on"
        );
    }))
    .await;
}

/// A blob queued for the image's blob permit gives up its upload claim when
/// another blob fails. Image B has seven blobs, so its last layer queues
/// behind six slow pulls, one of which fails. Image C needs that layer too and
/// waits on B's claim of it: C finishes at once instead of after the slow
/// pulls.
#[tokio::test(flavor = "current_thread")]
async fn sync_blob_failure_releases_the_claim_of_a_blob_queued_for_a_permit() {
    const SLOW_PULL: Duration = Duration::from_secs(6);
    const FAILURE_DELAY: Duration = Duration::from_secs(1);
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    // Image D syncs to its own target, so it only raises the frequency of B's
    // other blobs: transfers sort by frequency, and without D the layer shared
    // with C would sort first and take a permit instead of queueing for one.
    let other_target_server = MockServer::start().await;

    let shared_with_c = b"claim-release-queued";
    let parts_d = ManifestBuilder::new(b"claim-release-config-b")
        .layer(b"claim-release-unpullable")
        .layer(b"claim-release-slow-1")
        .layer(b"claim-release-slow-2")
        .layer(b"claim-release-slow-3")
        .layer(b"claim-release-slow-4")
        .build();
    // config + 6 layers = 7 blobs for 6 permits; the last layer queues.
    let parts_b = ManifestBuilder::new(b"claim-release-config-b")
        .layer(b"claim-release-unpullable")
        .layer(b"claim-release-slow-1")
        .layer(b"claim-release-slow-2")
        .layer(b"claim-release-slow-3")
        .layer(b"claim-release-slow-4")
        .layer(shared_with_c)
        .build();
    let parts_c = ManifestBuilder::new(b"claim-release-config-c")
        .layer(shared_with_c)
        .build();
    parts_b
        .mount_source_delayed(&source_server, "src-b", "v1", SLOW_PULL)
        .await;
    parts_c.mount_source(&source_server, "src-c", "v1").await;
    parts_d.mount_source(&source_server, "src-d", "v1").await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/src-b/blobs/{}",
            parts_b.layer_descs[0].digest
        )))
        .respond_with(ResponseTemplate::new(404).set_delay(FAILURE_DELAY))
        .with_priority(1)
        .mount(&source_server)
        .await;
    parts_b.mount_target(&target_server, "tgt-b", "v1").await;
    parts_c.mount_target(&target_server, "tgt-c", "v1").await;
    parts_d
        .mount_target(&other_target_server, "tgt-d", "v1")
        .await;

    // Leader election breaks a tie in favor of the later image, so listing C
    // first makes B claim the shared layer before C reaches it.
    let mappings = vec![
        mapping_with_distinct_repos(
            &source_server,
            &target_server,
            "src-c",
            "tgt-c",
            same_tags(&["v1"]),
        ),
        mapping_with_distinct_repos(
            &source_server,
            &other_target_server,
            "src-d",
            "tgt-d",
            same_tags(&["v1"]),
        ),
        mapping_with_distinct_repos(
            &source_server,
            &target_server,
            "src-b",
            "tgt-b",
            same_tags(&["v1"]),
        ),
    ];
    let report = tokio::time::timeout(
        SLOW_PULL * 3,
        SyncEngine::new(fast_retry(), concurrency(10)).run(
            mappings,
            empty_cache(),
            BlobStage::disabled(),
            &NullProgress,
            None,
        ),
    )
    .await
    .expect("a cancelled blob must not keep its claim");

    let image_c = report
        .images
        .iter()
        .find(|i| i.target.contains("tgt-c"))
        .expect("image C is reported");
    assert!(
        matches!(image_c.status, ImageStatus::Synced),
        "{:?}",
        image_c.status
    );
    // The negative assertion: C ends before B's slow pulls do, not after.
    assert!(
        image_c.duration < SLOW_PULL / 2,
        "C finished after {:?}, held up by B's claim of the shared layer",
        image_c.duration
    );
}

/// Two images share a blob and run concurrently; the blob already exists at
/// one of their target repos. Both sync without deadlocking on the shared
/// blob's upload claim, and the repo that has the blob never has it pushed.
#[tokio::test(flavor = "current_thread")]
async fn sync_shared_blob_present_at_one_repo_is_never_pushed_there() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared_data = b"shared-batch-notify-layer";
    let parts_a = ManifestBuilder::new(b"config-batch-notify-a")
        .layer(shared_data)
        .build();
    let parts_b = ManifestBuilder::new(b"config-batch-notify-b")
        .layer(shared_data)
        .build();

    parts_a.mount_source(&source_server, "src-a", "v1").await;
    parts_b.mount_source(&source_server, "src-b", "v1").await;

    // tgt-a: all blobs missing. tgt-b: the shared blob HEAD returns 200, so
    // the per-blob HEAD marks it verified there.
    parts_a.mount_target(&target_server, "tgt-a", "v1").await;
    mount_manifest_head_not_found(&target_server, "tgt-b", "v1").await;
    mount_blob_not_found(&target_server, "tgt-b", &parts_b.config_desc.digest).await;
    Mock::given(method("HEAD"))
        .and(path(format!(
            "/v2/tgt-b/blobs/{}",
            parts_b.layer_descs[0].digest
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", shared_data.len().to_string()),
        )
        .mount(&target_server)
        .await;
    mount_blob_push(&target_server, "tgt-b").await;
    mount_manifest_push(&target_server, "tgt-b", "v1").await;

    for repo in ["tgt-a", "tgt-b"] {
        mount_blob_mount_accepted(&target_server, repo, &parts_a.layer_descs[0].digest).await;
    }

    let mappings = v1_mappings(
        &source_server,
        &target_server,
        &[("src-a", "tgt-a"), ("src-b", "tgt-b")],
    );
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        run_sync_with_concurrency(mappings, 10, BlobStage::disabled()),
    )
    .await
    .expect("engine should not deadlock on the shared blob's claim");

    assert_all_synced(&report, 2, "both images should sync");
    let pushed_to_b = count_blob_uploads(
        &target_server,
        PathMatch::Contains("/v2/tgt-b/blobs/uploads/"),
        &parts_b.layer_descs[0].digest,
    )
    .await;
    assert_eq!(pushed_to_b, 0, "tgt-b already has the shared blob");
}

/// A follower's mount waits for the leader's manifest commit, then succeeds
/// with 201: committed-only mount sources mean every mount attempt has a
/// viable target. Two images share one layer; the follower wakes on the
/// leader's commit and mounts the shared layer rather than pushing it.
#[tokio::test(flavor = "current_thread")]
async fn mount_waits_for_committed_source() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared = b"mount-waits-shared-layer";
    let leader_img = ManifestBuilder::new(b"mount-waits-leader-cfg")
        .layer(shared)
        .build();
    let follower_img = ManifestBuilder::new(b"mount-waits-follower-cfg")
        .layer(shared)
        .build();

    leader_img
        .mount_source(&source_server, "leader", "v1")
        .await;
    follower_img
        .mount_source(&source_server, "follower", "v1")
        .await;

    let blobs = [
        &leader_img.config_desc.digest,
        &follower_img.config_desc.digest,
        &leader_img.layer_descs[0].digest,
    ];
    for repo in ["leader", "follower"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
        mount_blob_mount_accepted(&target_server, repo, &leader_img.layer_descs[0].digest).await;
    }

    let mappings =
        mappings_with_own_clients(&source_server, &target_server, ["leader", "follower"]);
    let report = run_sync(mappings).await;

    assert_all_synced(&report, 2, "both images should sync");
    let total_mounts: u64 = report.images.iter().map(|r| r.blob_stats.mounted).sum();
    assert_eq!(
        total_mounts, 1,
        "the shared layer is mounted once, by the follower (committed-source invariant)"
    );
}

/// One `v1` mapping per repo (the same name at source and target), each with
/// its own clients so no AIMD window is shared between them.
fn mappings_with_own_clients(
    source: &MockServer,
    target: &MockServer,
    repos: [&str; 2],
) -> Vec<ResolvedMapping> {
    repos
        .map(|repo| mapping_from_servers(source, target, repo, same_tags(&["v1"])))
        .into()
}

/// Run `mappings` against a 10ms mount-source wait deadline, failing the test
/// if the run outlives its 10s guard.
async fn run_with_short_mount_wait(
    mappings: Vec<ResolvedMapping>,
    cache: Rc<RefCell<TransferStateCache>>,
) -> SyncReport {
    let engine = SyncEngine::new(fast_retry(), concurrency(10))
        .with_mount_source_wait_deadline(Duration::from_millis(10));
    tokio::time::timeout(
        Duration::from_secs(10),
        engine.run(
            mappings,
            cache,
            BlobStage::disabled(),
            &NullProgress,
            Some(&ShutdownSignal::new()),
        ),
    )
    .await
    .expect("the mount-source wait deadline did not bound the run")
}

/// When the leader's manifest commit never arrives before the per-blob
/// deadline, the follower falls back to push rather than hanging.
///
/// A 10ms deadline keeps the test fast. The leader's manifest PUT returns
/// 500, so the leader fails and never commits; the follower's deadline trips
/// and it pushes its own copy of the shared blob.
#[tokio::test(flavor = "current_thread")]
async fn mount_resolver_timeout_falls_back_to_push() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let shared = b"deadline-shared-layer";
    let leader_img = ManifestBuilder::new(b"deadline-leader-cfg")
        .layer(shared)
        .build();
    let follower_img = ManifestBuilder::new(b"deadline-follower-cfg")
        .layer(shared)
        .build();

    leader_img
        .mount_source(&source_server, "leader", "v1")
        .await;
    follower_img
        .mount_source(&source_server, "follower", "v1")
        .await;

    let blobs = [
        &leader_img.config_desc.digest,
        &follower_img.config_desc.digest,
        &leader_img.layer_descs[0].digest,
    ];
    for repo in ["leader", "follower"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
    }
    // The leader fails, and its watch fires via notify_repo_failed.
    Mock::given(method("PUT"))
        .and(path("/v2/leader/manifests/v1"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&target_server)
        .await;

    let report = run_with_short_mount_wait(
        mappings_with_own_clients(&source_server, &target_server, ["leader", "follower"]),
        empty_cache(),
    )
    .await;

    assert_eq!(report.images.len(), 2);
    let status_of = |repo: &str| {
        let source = format!("{repo}:v1");
        let image = report.images.iter().find(|r| r.source == source);
        &image
            .unwrap_or_else(|| panic!("no result for {source}"))
            .status
    };
    assert!(
        matches!(status_of("follower"), ImageStatus::Synced),
        "the follower syncs via push fallback after the deadline: {:?}",
        status_of("follower")
    );
    assert!(
        matches!(
            status_of("leader"),
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                ..
            }
        ),
        "the leader's manifest PUT answers 500: {:?}",
        status_of("leader")
    );
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(
        report.stats.blobs_mounted, 0,
        "no mounts should succeed when leader never commits"
    );
}

/// Prior-run state cycle: both repos have the shared blob from a prior run,
/// but neither is committed. The bounded deadline breaks the cycle; both fall
/// through to push and both manifests commit.
///
/// shortcut: the cycle is seeded in the cache, not built by two real runs;
/// upgrade when the mock server can carry state across runs.
#[tokio::test(flavor = "current_thread")]
async fn prior_run_cycle_breaks_via_deadline() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let img_a = ManifestBuilder::new(b"cycle-a-cfg")
        .layer(b"cycle-shared")
        .build();
    let img_b = ManifestBuilder::new(b"cycle-b-cfg")
        .layer(b"cycle-shared")
        .build();

    img_a.mount_source(&source_server, "repo-a", "v1").await;
    img_b.mount_source(&source_server, "repo-b", "v1").await;

    let blobs = [
        &img_a.config_desc.digest,
        &img_b.config_desc.digest,
        &img_a.layer_descs[0].digest,
    ];
    for repo in ["repo-a", "repo-b"] {
        mount_target_fresh(&target_server, repo, "v1", &blobs).await;
    }

    let layer = &img_a.layer_descs[0].digest;
    let cache = cache_with(
        &mock_target_id("target", &target_server),
        &repo("repo-b"),
        &[layer],
    );
    cache.borrow_mut().set_blob_completed(
        &mock_target_id("target", &target_server),
        layer.clone(),
        repo("repo-a"),
    );

    let report = run_with_short_mount_wait(
        mappings_with_own_clients(&source_server, &target_server, ["repo-a", "repo-b"]),
        cache,
    )
    .await;

    assert_all_synced(
        &report,
        2,
        "both images should sync after deadline breaks cycle",
    );
}

/// After a mount source is pre-marked as stale in the cache, the resolver
/// skips it and uses the remaining committed source to mount successfully.
///
/// shortcut: the stale exclusion is seeded in the cache, not produced by a
/// 202 on a mount; upgrade when a mount mock can answer per `from` repo.
#[tokio::test(flavor = "current_thread")]
async fn resolver_retries_mount_after_source_marked_stale() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let img = ManifestBuilder::new(b"retry-cfg")
        .layer(b"retry-layer")
        .build();
    img.mount_source(&source_server, "tgt-b", "v1").await;

    // The config blob is not in the cache, so it uploads.
    img.mount_target(&target_server, "tgt-b", "v1").await;
    mount_blob_mount_accepted(&target_server, "tgt-b", &img.layer_descs[0].digest).await;

    // tgt-a is committed and valid; stale-src was rejected earlier and must be excluded.
    let layer = &img.layer_descs[0].digest;
    let cache = cache_with(
        &mock_target_id("target", &target_server),
        &repo("tgt-a"),
        &[layer],
    );
    {
        let mut c = cache.borrow_mut();
        c.mark_repo_committed(&mock_target_id("target", &target_server), &repo("tgt-a"));
        c.mark_blob_repo_stale(
            &mock_target_id("target", &target_server),
            layer,
            &repo("stale-src"),
        );
    }

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "tgt-b",
        vec![TagPair::same("v1")],
    );
    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(
        report.images[0].blob_stats.mounted, 1,
        "layer should be mounted from non-stale committed source"
    );
    assert_eq!(
        report.images[0].blob_stats.transferred, 1,
        "config should be pushed (not in cache)"
    );
}

/// A copy between two repositories of one registry, with one client as
/// source and target, finishes when that client allows one upload at a
/// time. Breaks if a blob download takes an upload permit, which it would
/// hold while its own upload waits for the permit.
#[tokio::test]
async fn sync_within_one_registry_does_not_wait_on_its_own_download() {
    let server = MockServer::start().await;
    let img = ManifestBuilder::new(b"cfg").layer(b"layer").build();
    img.mount_source(&server, "src", "v1").await;
    img.mount_target(&server, "dst", "v1").await;
    let client = Arc::new(
        ocync_distribution::RegistryClientBuilder::new(server.uri().parse().unwrap())
            .blob_upload_concurrency(std::num::NonZeroUsize::MIN)
            .build()
            .unwrap(),
    );
    let mapping = resolved_mapping(
        Arc::clone(&client),
        "src",
        "dst",
        vec![target_entry("same", client)],
        vec![TagPair::same("v1")],
    );

    let report = tokio::time::timeout(Duration::from_secs(10), run_sync(vec![mapping]))
        .await
        .expect("the copy waited on its own download");

    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.blobs_transferred, 2);
}
