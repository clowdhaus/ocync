//! Discovery, budget circuit breaker and `head_first` integration tests: wiremock
//! registries drive the sync engine, and each test pins the discovery counters and
//! the exact request counts its path must produce.

mod helpers;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::MediaType;
use ocync_sync::cache::{PlatformFilterKey, SourceSnapshot, TransferStateCache};
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair};
use ocync_sync::progress::NullProgress;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ErrorKind, ImageStatus, SkipReason, SyncReport};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// Discovery HEAD timeout for tests whose source HEAD can hang.
const HEAD_TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Discovery counters as `[cache_hits, cache_misses, head_failures, target_stale,
/// head_first_skips]`.
fn discovery(report: &SyncReport) -> [u64; 5] {
    let s = &report.stats;
    [
        s.discovery_cache_hits,
        s.discovery_cache_misses,
        s.discovery_head_failures,
        s.discovery_target_stale,
        s.discovery_head_first_skips,
    ]
}

fn manifest_path(repo: &str, reference: &str) -> String {
    format!("/v2/{repo}/manifests/{reference}")
}

/// A manifest HEAD or GET response that names `digest`.
fn digest_response(digest: &Digest, media_type: MediaType) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("docker-content-digest", digest.to_string())
        .insert_header("content-type", media_type.as_str())
        .insert_header("content-length", "100")
}

fn head_mock(repo: &str, reference: &str, response: ResponseTemplate) -> Mock {
    Mock::given(method("HEAD"))
        .and(path(manifest_path(repo, reference)))
        .respond_with(response)
}

fn get_mock(repo: &str, reference: &str, response: ResponseTemplate) -> Mock {
    Mock::given(method("GET"))
        .and(path(manifest_path(repo, reference)))
        .respond_with(response)
}

/// A manifest GET the engine must never issue.
async fn forbid_get(server: &MockServer, repo: &str, reference: &str) {
    get_mock(repo, reference, ResponseTemplate::new(200))
        .expect(0)
        .mount(server)
        .await;
}

/// How the source answers the discovery HEAD.
enum SourceHead {
    Digest,
    Status(u16),
    /// Answers after 3s, past [`HEAD_TIMEOUT`].
    TimesOut,
}

fn source_head(kind: &SourceHead, digest: &Digest) -> Mock {
    let response = match kind {
        SourceHead::Digest => digest_response(digest, MediaType::OciManifest),
        SourceHead::Status(code) => ResponseTemplate::new(*code),
        SourceHead::TimesOut => {
            digest_response(digest, MediaType::OciManifest).set_delay(Duration::from_secs(3))
        }
    };
    head_mock("src/repo", "v1", response)
}

fn image(name: &str) -> ManifestParts {
    ManifestBuilder::new(format!("config-{name}").as_bytes())
        .layer(format!("layer-{name}").as_bytes())
        .build()
}

/// Blob HEAD 404s plus the upload endpoints: the target holds none of the blobs.
async fn mount_target_blobs(server: &MockServer, repo: &str, parts: &ManifestParts) {
    parts.mount_target_blob_misses(server, repo).await;
    mount_blob_push(server, repo).await;
}

/// Everything a full pull of `parts` under `tag` needs except the manifest HEADs:
/// one source manifest GET, the blobs, and a target that accepts the pushes.
async fn mount_full_pull(
    source: &MockServer,
    target: &MockServer,
    parts: &ManifestParts,
    tag: &str,
) {
    source_manifest_mock("src/repo", tag, &parts.bytes, MediaType::OciManifest)
        .expect(1)
        .mount(source)
        .await;
    parts.mount_blob_pulls(source, "src/repo").await;
    mount_target_blobs(target, "tgt/repo", parts).await;
    mount_manifest_push(target, "tgt/repo", tag).await;
}

fn seed_snapshot(
    cache: &Rc<RefCell<TransferStateCache>>,
    tag: &str,
    source: &Digest,
    filtered: &Digest,
) {
    cache.borrow_mut().set_source_snapshot(
        snap_key("src/repo", tag),
        SourceSnapshot {
            source_digest: source.clone(),
            filtered_digest: filtered.clone(),
            platform_filter_key: PlatformFilterKey::from_filters(None),
        },
    );
}

fn same_tags(tags: &[&str]) -> Vec<TagPair> {
    tags.iter().map(|t| TagPair::same(*t)).collect()
}

fn mapping(source: &MockServer, target: &MockServer, tags: Vec<TagPair>) -> ResolvedMapping {
    mapping_with_distinct_repos(source, target, "src/repo", "tgt/repo", tags)
}

/// Verify the cycle's request counts, then clear both servers for the next cycle.
async fn end_cycle(source: &MockServer, target: &MockServer) {
    source.verify().await;
    target.verify().await;
    source.reset().await;
    target.reset().await;
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// A matching snapshot (or a `head_first` match) skips with one source HEAD, one
/// target HEAD and no source manifest GET.
#[tokio::test]
async fn discovery_skips_without_source_get() {
    struct Case {
        name: &'static str,
        source_tag: &'static str,
        target_tag: &'static str,
        head_first: bool,
        seeded: bool,
        want: [u64; 5],
    }
    let cases = [
        Case {
            name: "warm snapshot",
            source_tag: "v1",
            target_tag: "v1",
            head_first: false,
            seeded: true,
            want: [1, 0, 0, 0, 0],
        },
        Case {
            name: "retag: snapshot keyed on the source tag, target HEAD on the target tag",
            source_tag: "v1.0",
            target_tag: "latest",
            head_first: false,
            seeded: true,
            want: [1, 0, 0, 0, 0],
        },
        Case {
            name: "head_first with a warm snapshot takes the snapshot path",
            source_tag: "v1",
            target_tag: "v1",
            head_first: true,
            seeded: true,
            want: [1, 0, 0, 0, 0],
        },
        Case {
            name: "head_first on a cold cache skips on the target HEAD match",
            source_tag: "v1",
            target_tag: "v1",
            head_first: true,
            seeded: false,
            want: [0, 0, 0, 0, 1],
        },
    ];

    for c in cases {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        let digest = make_digest("aabb");

        head_mock(
            "src/repo",
            c.source_tag,
            digest_response(&digest, MediaType::OciManifest),
        )
        .expect(1)
        .mount(&source)
        .await;
        forbid_get(&source, "src/repo", c.source_tag).await;
        head_mock(
            "tgt/repo",
            c.target_tag,
            digest_response(&digest, MediaType::OciManifest),
        )
        .expect(1)
        .mount(&target)
        .await;

        let mut m = mapping(
            &source,
            &target,
            vec![TagPair::retag(c.source_tag, c.target_tag)],
        );
        m.head_first = c.head_first;
        let cache = empty_cache();
        if c.seeded {
            seed_snapshot(&cache, c.source_tag, &digest, &digest);
        }

        let report = run_sync_with_cache(vec![m], cache).await;

        assert_eq!(report.stats.images_skipped, 1, "{}", c.name);
        assert_eq!(report.stats.images_synced, 0, "{}", c.name);
        assert!(
            matches!(
                report.images[0].status,
                ImageStatus::Skipped {
                    reason: SkipReason::DigestMatch
                }
            ),
            "{}: got {:?}",
            c.name,
            report.images[0].status
        );
        assert_eq!(report.images[0].bytes_transferred, 0, "{}", c.name);
        assert_eq!(discovery(&report), c.want, "{}", c.name);
    }
}

/// Every way discovery can fail to confirm the target falls back to one source
/// GET and a full push.
#[tokio::test]
async fn discovery_falls_through_to_full_pull() {
    enum Seed {
        Cold,
        OldDigest,
        Current,
    }
    struct Case {
        name: &'static str,
        source_head: SourceHead,
        seed: Seed,
        target_heads: u64,
        want: [u64; 5],
        /// Whether the snapshot holds the pulled digest afterwards (a failed
        /// source HEAD must not invent one).
        snapshot: bool,
    }
    let cases = [
        Case {
            name: "cold cache",
            source_head: SourceHead::Digest,
            seed: Seed::Cold,
            target_heads: 1,
            want: [0, 1, 0, 0, 0],
            snapshot: true,
        },
        Case {
            name: "source digest changed since the snapshot: a miss, not a head failure",
            source_head: SourceHead::Digest,
            seed: Seed::OldDigest,
            target_heads: 1,
            want: [0, 1, 0, 0, 0],
            snapshot: true,
        },
        Case {
            name: "target stale: discovery and the pre-push check both HEAD the target",
            source_head: SourceHead::Digest,
            seed: Seed::Current,
            target_heads: 2,
            want: [0, 1, 0, 1, 0],
            snapshot: true,
        },
        Case {
            name: "source HEAD 500",
            source_head: SourceHead::Status(500),
            seed: Seed::Cold,
            target_heads: 1,
            want: [0, 1, 1, 0, 0],
            snapshot: false,
        },
        Case {
            name: "source HEAD 404 is no usable digest",
            source_head: SourceHead::Status(404),
            seed: Seed::Cold,
            target_heads: 1,
            want: [0, 1, 1, 0, 0],
            snapshot: false,
        },
        Case {
            name: "source HEAD times out",
            source_head: SourceHead::TimesOut,
            seed: Seed::Cold,
            target_heads: 1,
            want: [0, 1, 1, 0, 0],
            snapshot: false,
        },
        Case {
            name: "source HEAD 500 must not fall back to a valid snapshot",
            source_head: SourceHead::Status(500),
            seed: Seed::Current,
            target_heads: 1,
            want: [0, 1, 1, 0, 0],
            snapshot: true,
        },
    ];

    for c in cases {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        let parts = image("pull");

        source_head(&c.source_head, &parts.digest)
            .expect(1)
            .mount(&source)
            .await;
        mount_full_pull(&source, &target, &parts, "v1").await;
        head_mock("tgt/repo", "v1", ResponseTemplate::new(404))
            .expect(c.target_heads)
            .mount(&target)
            .await;

        let cache = empty_cache();
        match c.seed {
            Seed::Cold => {}
            Seed::OldDigest => {
                seed_snapshot(&cache, "v1", &make_digest("dead"), &make_digest("beef"));
            }
            Seed::Current => seed_snapshot(&cache, "v1", &parts.digest, &parts.digest),
        }

        let report = SyncEngine::new(fast_retry(), concurrency(10))
            .with_source_head_timeout(HEAD_TIMEOUT)
            .run(
                vec![mapping(&source, &target, same_tags(&["v1"]))],
                cache.clone(),
                BlobStage::disabled(),
                &NullProgress,
                None,
            )
            .await;

        assert_eq!(report.stats.images_synced, 1, "{}", c.name);
        assert_eq!(discovery(&report), c.want, "{}", c.name);
        let snapshot = cache
            .borrow()
            .source_snapshot(&snap_key("src/repo", "v1"))
            .map(|s| s.source_digest.clone());
        assert_eq!(
            snapshot,
            c.snapshot.then(|| parts.digest.clone()),
            "{}",
            c.name
        );
    }
}

/// A failed manifest pull must not leave a snapshot that would later skip the image.
#[tokio::test]
async fn discovery_pull_failure_does_not_populate_cache() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;

    head_mock(
        "src/repo",
        "v1",
        digest_response(&make_digest("aabb"), MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    // No `.expect`: the engine retries the GET.
    get_mock("src/repo", "v1", ResponseTemplate::new(500))
        .mount(&source)
        .await;

    let cache = empty_cache();
    let report = run_sync_with_cache(
        vec![mapping(&source, &target, same_tags(&["v1"]))],
        cache.clone(),
    )
    .await;

    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_status!(
        report,
        0,
        ImageStatus::Failed {
            kind: ErrorKind::ManifestPull,
            ..
        }
    );
    // The label names the target registry alias.
    assert_eq!(report.images[0].target, "tgt/repo (target):v1");
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
    assert!(
        cache
            .borrow()
            .source_snapshot(&snap_key("src/repo", "v1"))
            .is_none(),
        "cache must not be populated after pull failure"
    );
}

/// An index with no child matching the platform filter fails the image, and the
/// error names both the requested filter and what the index offers.
#[tokio::test]
async fn discovery_zero_platform_match_returns_error() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;

    let index = IndexBuilder::new()
        .manifest(&image("s390x"), Some(linux("s390x")))
        .build();
    head_mock(
        "src/repo",
        "v1",
        digest_response(&index.digest, MediaType::OciIndex),
    )
    .expect(1)
    .mount(&source)
    .await;
    source_manifest_mock("src/repo", "v1", &index.bytes, MediaType::OciIndex)
        .expect(1)
        .mount(&source)
        .await;

    let mut m = mapping(&source, &target, same_tags(&["v1"]));
    m.platforms = Some(vec!["linux/amd64".parse().unwrap()]);

    let report = run_sync_with_cache(vec![m], empty_cache()).await;

    assert_eq!(report.stats.images_failed, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_status!(
        report,
        0,
        ImageStatus::Failed {
            kind: ErrorKind::ManifestPull,
            ..
        }
    );
    let ImageStatus::Failed { error, .. } = &report.images[0].status else {
        unreachable!("asserted Failed above");
    };
    assert!(
        error.contains("linux/amd64"),
        "error should mention the requested filter: {error}"
    );
    assert!(
        error.contains("linux/s390x"),
        "error should mention the available platform: {error}"
    );
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
}

/// Two targets, one in sync and one stale: only the stale one is pushed, with a
/// single source GET.
#[tokio::test]
async fn discovery_mixed_fanout_one_match_one_stale() {
    let source = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;
    let parts = image("fanout");

    head_mock(
        "src/repo",
        "v1",
        digest_response(&parts.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    mount_full_pull(&source, &target_b, &parts, "v1").await;
    head_mock(
        "tgt/repo",
        "v1",
        digest_response(&parts.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&target_a)
    .await;
    // Discovery's cache validation and the pre-push check both HEAD target B.
    head_mock("tgt/repo", "v1", ResponseTemplate::new(404))
        .expect(2)
        .mount(&target_b)
        .await;

    let m = resolved_mapping(
        mock_client(&source),
        "src/repo",
        "tgt/repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        same_tags(&["v1"]),
    );
    let cache = empty_cache();
    seed_snapshot(&cache, "v1", &parts.digest, &parts.digest);

    let report = run_sync_with_cache(vec![m], cache).await;

    assert_eq!(report.images.len(), 2);
    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 1);
    let skipped = report
        .images
        .iter()
        .find(|r| {
            matches!(
                r.status,
                ImageStatus::Skipped {
                    reason: SkipReason::DigestMatch
                }
            )
        })
        .expect("target A should be skipped with DigestMatch");
    assert_eq!(skipped.bytes_transferred, 0);
    assert_eq!(skipped.blob_stats.transferred, 0);
    let synced = report
        .images
        .iter()
        .find(|r| matches!(r.status, ImageStatus::Synced))
        .expect("target B should be Synced");
    assert_eq!(synced.blob_stats.transferred, 2);
    assert_eq!(
        synced.bytes_transferred,
        (parts.config_data.len() + parts.layers_data[0].len()) as u64
    );
    assert_eq!(discovery(&report), [0, 1, 0, 1, 0]);
}

/// Three tags discovered concurrently: a snapshot hit, a cold miss and a source
/// HEAD failure each take their own route and are counted once.
#[tokio::test]
async fn discovery_concurrent_mixed_outcomes() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let (a, b, c) = (image("a"), image("b"), image("c"));

    // tag-a: snapshot hit.
    head_mock(
        "src/repo",
        "tag-a",
        digest_response(&a.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    forbid_get(&source, "src/repo", "tag-a").await;
    head_mock(
        "tgt/repo",
        "tag-a",
        digest_response(&a.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&target)
    .await;

    // tag-b: cold miss.
    head_mock(
        "src/repo",
        "tag-b",
        digest_response(&b.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    mount_full_pull(&source, &target, &b, "tag-b").await;
    head_mock("tgt/repo", "tag-b", ResponseTemplate::new(404))
        .expect(1)
        .mount(&target)
        .await;

    // tag-c: source HEAD 500.
    head_mock("src/repo", "tag-c", ResponseTemplate::new(500))
        .expect(1)
        .mount(&source)
        .await;
    mount_full_pull(&source, &target, &c, "tag-c").await;
    head_mock("tgt/repo", "tag-c", ResponseTemplate::new(404))
        .expect(1)
        .mount(&target)
        .await;

    let cache = empty_cache();
    seed_snapshot(&cache, "tag-a", &a.digest, &a.digest);

    let report = run_sync_with_cache(
        vec![mapping(
            &source,
            &target,
            same_tags(&["tag-a", "tag-b", "tag-c"]),
        )],
        cache,
    )
    .await;

    assert_eq!(report.stats.images_skipped, 1, "tag-a should be skipped");
    assert_eq!(report.stats.images_synced, 2, "tag-b and tag-c should sync");
    assert_eq!(discovery(&report), [1, 2, 1, 0, 0]);
}

/// The snapshot written by one run is invalidated when the source digest changes
/// before the next run.
#[tokio::test]
async fn discovery_source_change_across_cycles() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let cache = empty_cache();
    let (d1, d2) = (image("cycle-1"), image("cycle-2"));

    head_mock(
        "src/repo",
        "v1",
        digest_response(&d1.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    mount_full_pull(&source, &target, &d1, "v1").await;
    head_mock("tgt/repo", "v1", ResponseTemplate::new(404))
        .expect(1)
        .mount(&target)
        .await;

    let report = run_sync_with_cache(
        vec![mapping(&source, &target, same_tags(&["v1"]))],
        cache.clone(),
    )
    .await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
    assert_eq!(
        cache
            .borrow()
            .source_snapshot(&snap_key("src/repo", "v1"))
            .expect("cache populated after cycle 1")
            .source_digest,
        d1.digest
    );
    end_cycle(&source, &target).await;

    // The source moves to D2 while the target still holds D1.
    head_mock(
        "src/repo",
        "v1",
        digest_response(&d2.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    mount_full_pull(&source, &target, &d2, "v1").await;
    head_mock(
        "tgt/repo",
        "v1",
        digest_response(&d1.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&target)
    .await;

    let report = run_sync_with_cache(
        vec![mapping(&source, &target, same_tags(&["v1"]))],
        cache.clone(),
    )
    .await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
    assert_eq!(
        cache
            .borrow()
            .source_snapshot(&snap_key("src/repo", "v1"))
            .expect("cache updated after cycle 2")
            .source_digest,
        d2.digest
    );
}

/// One source HEAD and one filtered-child pull for `child`, with the index at
/// `v1` unchanged between cycles.
async fn mount_filtered_cycle(
    source: &MockServer,
    target: &MockServer,
    index: &IndexParts,
    child: &ManifestParts,
) {
    head_mock(
        "src/repo",
        "v1",
        digest_response(&index.digest, MediaType::OciIndex),
    )
    .expect(1)
    .mount(source)
    .await;
    source_manifest_mock("src/repo", "v1", &index.bytes, MediaType::OciIndex)
        .expect(1)
        .mount(source)
        .await;
    source_manifest_mock(
        "src/repo",
        &child.digest.to_string(),
        &child.bytes,
        MediaType::OciManifest,
    )
    .expect(1)
    .mount(source)
    .await;
    child.mount_blob_pulls(source, "src/repo").await;

    head_mock("tgt/repo", "v1", ResponseTemplate::new(404))
        .expect(1)
        .mount(target)
        .await;
    mount_target_blobs(target, "tgt/repo", child).await;
    mount_manifest_push(target, "tgt/repo", "v1").await;
    mount_manifest_push(target, "tgt/repo", &child.digest.to_string()).await;
}

/// Changing the platform filter between runs is a cache miss even though the
/// source digest is unchanged, because the filtered manifest differs.
#[tokio::test]
async fn discovery_platform_filter_change_triggers_cache_miss() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let cache = empty_cache();
    let amd64 = image("amd64");
    let arm64 = image("arm64");
    let index = IndexBuilder::new()
        .manifest(&amd64, Some(linux("amd64")))
        .manifest(&arm64, Some(linux("arm64")))
        .build();

    mount_filtered_cycle(&source, &target, &index, &amd64).await;
    let mut m = mapping(&source, &target, same_tags(&["v1"]));
    m.platforms = Some(vec!["linux/amd64".parse().unwrap()]);

    let report = run_sync_with_cache(vec![m], cache.clone()).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
    {
        let c = cache.borrow();
        let snap = c
            .source_snapshot(&snap_key("src/repo", "v1"))
            .expect("cache populated after cycle 1");
        assert_eq!(snap.source_digest, index.digest);
        assert_eq!(
            snap.platform_filter_key,
            PlatformFilterKey::from_filters(Some(&["linux/amd64".parse().unwrap()]))
        );
    }
    end_cycle(&source, &target).await;

    mount_filtered_cycle(&source, &target, &index, &arm64).await;
    let mut m = mapping(&source, &target, same_tags(&["v1"]));
    m.platforms = Some(vec!["linux/arm64".parse().unwrap()]);

    let report = run_sync_with_cache(vec![m], cache.clone()).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
    assert_eq!(
        cache
            .borrow()
            .source_snapshot(&snap_key("src/repo", "v1"))
            .expect("cache updated after cycle 2")
            .platform_filter_key,
        PlatformFilterKey::from_filters(Some(&["linux/arm64".parse().unwrap()]))
    );
}

/// A second run over unchanged sources skips on the snapshot the first run wrote,
/// with no source GET, and drops the snapshot of a tag removed from the mapping.
#[tokio::test]
async fn discovery_snapshot_warms_across_cycles_and_prunes_deleted_tags() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let cache = empty_cache();
    let parts = image("prune");

    for tag in ["v1", "v2"] {
        head_mock(
            "src/repo",
            tag,
            digest_response(&parts.digest, MediaType::OciManifest),
        )
        .expect(1)
        .mount(&source)
        .await;
        mount_full_pull(&source, &target, &parts, tag).await;
        head_mock("tgt/repo", tag, ResponseTemplate::new(404))
            .expect(1)
            .mount(&target)
            .await;
    }

    let report = run_sync_with_cache(
        vec![mapping(&source, &target, same_tags(&["v1", "v2"]))],
        cache.clone(),
    )
    .await;

    assert_eq!(report.stats.images_synced, 2);
    {
        let c = cache.borrow();
        assert!(c.source_snapshot(&snap_key("src/repo", "v1")).is_some());
        assert!(c.source_snapshot(&snap_key("src/repo", "v2")).is_some());
    }
    end_cycle(&source, &target).await;

    // v2 is gone from the mapping.
    head_mock(
        "src/repo",
        "v1",
        digest_response(&parts.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    forbid_get(&source, "src/repo", "v1").await;
    head_mock(
        "tgt/repo",
        "v1",
        digest_response(&parts.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&target)
    .await;

    let report = run_sync_with_cache(
        vec![mapping(&source, &target, same_tags(&["v1"]))],
        cache.clone(),
    )
    .await;

    assert_eq!(report.stats.images_skipped, 1);
    assert_eq!(report.stats.images_synced, 0);
    assert_eq!(discovery(&report), [1, 0, 0, 0, 0]);
    let c = cache.borrow();
    assert!(
        c.source_snapshot(&snap_key("src/repo", "v1")).is_some(),
        "v1 should still be in the snapshot cache"
    );
    assert!(
        c.source_snapshot(&snap_key("src/repo", "v2")).is_none(),
        "v2 should have been pruned from the snapshot cache"
    );
}

// ---------------------------------------------------------------------------
// Budget circuit breaker
// ---------------------------------------------------------------------------

/// GET for every `v<N>` tag of `repo`, reporting a Docker Hub style
/// `ratelimit-remaining` when `remaining` is set.
fn manifest_gets(repo: &str, bytes: &[u8], remaining: Option<u64>) -> Mock {
    let mut response = ResponseTemplate::new(200)
        .set_body_bytes(bytes.to_vec())
        .insert_header("content-type", MediaType::OciManifest.as_str());
    if let Some(remaining) = remaining {
        response = response.insert_header("ratelimit-remaining", format!("{remaining};w=21600"));
    }
    Mock::given(method("GET"))
        .and(path_regex(format!(r"^/v2/{repo}/manifests/v\d+$")))
        .respond_with(response)
}

/// Source blobs, plus a target holding none of `tags` (HEAD 404, push) so every
/// tag needs a full sync.
async fn mount_unsynced_target(
    source: &MockServer,
    target: &MockServer,
    src_repo: &str,
    tgt_repo: &str,
    parts: &ManifestParts,
    tags: &[TagPair],
) {
    parts.mount_blob_pulls(source, src_repo).await;
    mount_target_blobs(target, tgt_repo, parts).await;
    for tag in tags {
        mount_manifest_head_not_found(target, tgt_repo, &tag.target).await;
        mount_manifest_push(target, tgt_repo, &tag.target).await;
    }
}

fn numbered_tags(n: usize) -> Vec<TagPair> {
    (0..n).map(|i| TagPair::same(format!("v{i}"))).collect()
}

/// The breaker never stalls the engine: every image syncs, every manifest is
/// fetched exactly once, and the source client reports the budget the headers gave.
#[tokio::test]
async fn budget_circuit_breaker_syncs_every_image() {
    struct Case {
        name: &'static str,
        tags: usize,
        remaining: Option<u64>,
    }
    let cases = [
        Case {
            name: "zero budget: breaker trips after every discovery, anti-stall finishes",
            tags: 15,
            remaining: Some(0),
        },
        Case {
            name: "fewer than 10 tags: the threshold floor of 1 still trips the breaker",
            tags: 5,
            remaining: Some(0),
        },
        Case {
            name: "no rate-limit header (ECR, GAR): breaker never engages",
            tags: 10,
            remaining: None,
        },
    ];

    for c in cases {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        let parts = image("cb");
        let tags = numbered_tags(c.tags);

        manifest_gets("library/app", &parts.bytes, c.remaining)
            .expect(c.tags as u64)
            .mount(&source)
            .await;
        mount_unsynced_target(&source, &target, "library/app", "mirror/app", &parts, &tags).await;

        let source_client = mock_client(&source);
        let mapping = resolved_mapping(
            Arc::clone(&source_client),
            "library/app",
            "mirror/app",
            vec![target_entry("target-reg", mock_client(&target))],
            tags,
        );

        let report = run_sync(vec![mapping]).await;

        assert_all_synced(&report, c.tags, c.name);
        assert_eq!(
            source_client.rate_limit_remaining(),
            c.remaining,
            "{}: budget read from response headers",
            c.name
        );
    }
}

/// Discovery resumes once the budget refills: the first three manifest GETs report
/// zero remaining, every later one reports 100. Sequenced mocks keep the
/// transition independent of polling order.
#[tokio::test]
async fn budget_circuit_breaker_resumes_on_budget_refill() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let parts = image("refill");
    let num_tags: usize = 12;
    let tags = numbered_tags(num_tags);

    // Same-priority mocks match in insertion order, so the low-budget mock takes
    // the first three GETs and the fallback takes the rest.
    manifest_gets("library/refill", &parts.bytes, Some(0))
        .up_to_n_times(3)
        .expect(3)
        .mount(&source)
        .await;
    manifest_gets("library/refill", &parts.bytes, Some(100))
        .expect((num_tags - 3) as u64..)
        .mount(&source)
        .await;
    mount_unsynced_target(
        &source,
        &target,
        "library/refill",
        "mirror/refill",
        &parts,
        &tags,
    )
    .await;

    let report = run_sync(vec![mapping_with_distinct_repos(
        &source,
        &target,
        "library/refill",
        "mirror/refill",
        tags,
    )])
    .await;

    assert_all_synced(&report, num_tags, "refill");
}

/// One rate-limited source pauses discovery for every source (any-trigger,
/// all-resume), yet both sources finish and only the limited one reports a budget.
#[tokio::test]
async fn budget_circuit_breaker_multi_source_all_or_nothing() {
    let source_a = MockServer::start().await; // rate-limited
    let source_b = MockServer::start().await; // no rate-limit header
    let target = MockServer::start().await;
    let parts_a = image("ms-a");
    let parts_b = image("ms-b");
    let tags = numbered_tags(8);

    manifest_gets("library/alpha", &parts_a.bytes, Some(0))
        .expect(8)
        .mount(&source_a)
        .await;
    manifest_gets("library/beta", &parts_b.bytes, None)
        .expect(8)
        .mount(&source_b)
        .await;
    mount_unsynced_target(
        &source_a,
        &target,
        "library/alpha",
        "mirror/alpha",
        &parts_a,
        &tags,
    )
    .await;
    mount_unsynced_target(
        &source_b,
        &target,
        "library/beta",
        "mirror/beta",
        &parts_b,
        &tags,
    )
    .await;

    let client_a = mock_client(&source_a);
    let client_b = mock_client(&source_b);
    let target_client = mock_client(&target);
    let mapping_a = resolved_mapping(
        Arc::clone(&client_a),
        "library/alpha",
        "mirror/alpha",
        vec![target_entry("target-reg", Arc::clone(&target_client))],
        tags.clone(),
    );
    let mapping_b = resolved_mapping(
        Arc::clone(&client_b),
        "library/beta",
        "mirror/beta",
        vec![target_entry("target-reg", target_client)],
        tags,
    );

    let report = run_sync(vec![mapping_a, mapping_b]).await;

    assert_all_synced(&report, 16, "multi-source");
    assert_eq!(client_a.rate_limit_remaining(), Some(0));
    assert_eq!(client_b.rate_limit_remaining(), None);
}

// ---------------------------------------------------------------------------
// head_first
// ---------------------------------------------------------------------------

/// A `head_first` mapping that cannot confirm the target from HEADs alone makes
/// one source GET, syncs, and counts a cache miss (plus a head failure when the
/// source HEAD itself failed). The target sees a manifest HEAD from `head_first`
/// and another before the push, so no count is pinned on it.
#[tokio::test]
async fn head_first_falls_through_to_full_pull() {
    struct Case {
        name: &'static str,
        source_head: SourceHead,
        target_head_status: u16,
        want: [u64; 5],
    }
    let cases = [
        Case {
            name: "target HEAD 404 is a mismatch",
            source_head: SourceHead::Digest,
            target_head_status: 404,
            want: [0, 1, 0, 0, 0],
        },
        Case {
            name: "target HEAD 500 degrades to a mismatch instead of failing discovery",
            source_head: SourceHead::Digest,
            target_head_status: 500,
            want: [0, 1, 0, 0, 0],
        },
        Case {
            name: "source HEAD 500 is a head failure",
            source_head: SourceHead::Status(500),
            target_head_status: 404,
            want: [0, 1, 1, 0, 0],
        },
    ];

    for c in cases {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        let parts = image("hf");

        source_head(&c.source_head, &parts.digest)
            .expect(1)
            .mount(&source)
            .await;
        mount_full_pull(&source, &target, &parts, "v1").await;
        head_mock(
            "tgt/repo",
            "v1",
            ResponseTemplate::new(c.target_head_status),
        )
        .mount(&target)
        .await;

        let mut m = mapping(&source, &target, same_tags(&["v1"]));
        m.head_first = true;

        let report = run_sync(vec![m]).await;

        assert_eq!(report.stats.images_synced, 1, "{}", c.name);
        assert_eq!(report.stats.images_skipped, 0, "{}", c.name);
        assert_eq!(report.stats.images_failed, 0, "{}", c.name);
        assert_eq!(discovery(&report), c.want, "{}", c.name);
    }
}

/// Two targets, one matching the source HEAD and one not: the matching target is
/// left alone and the other gets the full sync. Not a `head_first` skip, because a
/// GET was still needed.
#[tokio::test]
async fn head_first_partial_target_match_syncs_only_mismatched() {
    let source = MockServer::start().await;
    let target_a = MockServer::start().await;
    let target_b = MockServer::start().await;
    let parts = image("hf-partial");

    head_mock(
        "src/repo",
        "v1",
        digest_response(&parts.digest, MediaType::OciManifest),
    )
    .expect(1)
    .mount(&source)
    .await;
    mount_full_pull(&source, &target_b, &parts, "v1").await;
    head_mock("tgt/repo", "v1", ResponseTemplate::new(404))
        .mount(&target_b)
        .await;
    head_mock(
        "tgt/repo",
        "v1",
        digest_response(&parts.digest, MediaType::OciManifest),
    )
    .mount(&target_a)
    .await;
    // The in-sync target must receive no manifest push.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/v2/tgt/repo/manifests/"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&target_a)
        .await;

    let mut m = resolved_mapping(
        mock_client(&source),
        "src/repo",
        "tgt/repo",
        vec![
            target_entry("target-a", mock_client(&target_a)),
            target_entry("target-b", mock_client(&target_b)),
        ],
        same_tags(&["v1"]),
    );
    m.head_first = true;

    let report = run_sync(vec![m]).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
}

/// A platform filter bypasses `head_first`: the target holds the filtered index
/// digest, which can never equal the unfiltered source HEAD digest.
#[tokio::test]
async fn head_first_bypassed_with_platform_filter() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let child = image("hf-platform");
    let index = IndexBuilder::new()
        .manifest(&child, Some(linux("amd64")))
        .build();

    head_mock(
        "src/repo",
        "v1",
        digest_response(&index.digest, MediaType::OciIndex),
    )
    .expect(1)
    .mount(&source)
    .await;
    source_manifest_mock("src/repo", "v1", &index.bytes, MediaType::OciIndex)
        .expect(1)
        .mount(&source)
        .await;
    child
        .mount_source(&source, "src/repo", &child.digest.to_string())
        .await;
    index.mount_target(&target, "tgt/repo", "v1").await;

    let mut m = mapping(&source, &target, same_tags(&["v1"]));
    m.head_first = true;
    m.platforms = Some(vec!["linux/amd64".parse().unwrap()]);

    let report = run_sync(vec![m]).await;

    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.images_skipped, 0);
    assert_eq!(discovery(&report), [0, 1, 0, 0, 0]);
}
