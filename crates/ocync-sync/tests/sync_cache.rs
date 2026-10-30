//! Transfer state cache integration tests: progressive cache population, cross-repo mount,
//! lazy invalidation, cache persistence round-trip, warm cache skip, and batch blob checker
//! pre-population with fallback on error.

mod helpers;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use ocync_distribution::spec::{MediaType, RepositoryName};
use ocync_distribution::{BatchBlobChecker, BlobExistence, Digest};
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;
use ocync_sync::staging::BlobStage;
use ocync_sync::{ErrorKind, ImageStatus};
use wiremock::http::Method;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use helpers::*;

/// How many times a checker was asked, shared with the test.
#[derive(Clone, Default)]
struct Calls {
    /// Calls to `check_blob_existence`.
    checks: Rc<Cell<usize>>,
    /// Calls to `wait_for_blobs_available`, made only by the manifest-commit
    /// gate and counted only by a [`WaitTrackingChecker`].
    waits: Rc<Cell<usize>>,
}

/// Mock batch checker. It asserts the engine passes the target repo (a wrong
/// repo would otherwise go unnoticed) and counts its calls.
struct MockChecker {
    expected_repo: String,
    /// Digests reported present.
    existing: HashSet<Digest>,
    /// Digests a partial failure leaves out of the answer (neither present nor missing).
    unknown: HashSet<Digest>,
    /// Fail every `check_blob_existence` call.
    fails: bool,
    calls: Calls,
}

impl MockChecker {
    fn new(expected_repo: &str, existing: HashSet<Digest>) -> (Self, Calls) {
        let calls = Calls::default();
        (
            Self {
                expected_repo: expected_repo.to_owned(),
                existing,
                unknown: HashSet::new(),
                fails: false,
                calls: calls.clone(),
            },
            calls,
        )
    }

    fn unknown(self, unknown: HashSet<Digest>) -> Self {
        Self { unknown, ..self }
    }

    fn failing(self) -> Self {
        Self {
            fails: true,
            ..self
        }
    }
}

impl BatchBlobChecker for MockChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<BlobExistence, ocync_distribution::Error>> + 'a>> {
        assert_eq!(
            repo.as_str(),
            self.expected_repo,
            "mock: batch checker called with wrong repo"
        );
        Box::pin(async move {
            self.calls.checks.set(self.calls.checks.get() + 1);
            if self.fails {
                return Err(ocync_distribution::Error::EcrApi {
                    reason: "batch API unavailable".into(),
                });
            }
            let checked: Vec<Digest> = digests
                .iter()
                .filter(|d| !self.unknown.contains(d))
                .cloned()
                .collect();
            Ok(BlobExistence::new(&checked, &self.existing))
        })
    }
}

/// A [`MockChecker`] that also overrides `wait_for_blobs_available` to count
/// the manifest-commit gate's calls apart from the pre-discovery
/// `check_blob_existence` calls. Every other checker keeps the trait default.
struct WaitTrackingChecker(MockChecker);

impl WaitTrackingChecker {
    fn new(expected_repo: &str, existing: HashSet<Digest>) -> (Self, Calls) {
        let (checker, calls) = MockChecker::new(expected_repo, existing);
        (Self(checker), calls)
    }
}

impl BatchBlobChecker for WaitTrackingChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<BlobExistence, ocync_distribution::Error>> + 'a>> {
        self.0.check_blob_existence(repo, digests)
    }

    fn wait_for_blobs_available<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
        _deadline: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ocync_distribution::Error>> + 'a>> {
        assert_eq!(repo.as_str(), self.0.expected_repo);
        let all_known = digests.iter().all(|d| self.0.existing.contains(d));
        Box::pin(async move {
            self.0.calls.waits.set(self.0.calls.waits.get() + 1);
            if all_known {
                Ok(())
            } else {
                Err(ocync_distribution::Error::EcrApi {
                    reason: "test: some digests still missing".into(),
                })
            }
        })
    }
}

/// A target entry whose existence checks go through `checker`.
fn batch_target(
    name: &str,
    server: &MockServer,
    checker: impl BatchBlobChecker + 'static,
) -> TargetEntry {
    TargetEntry {
        batch_checker: Some(Rc::new(checker)),
        ..target_entry(name, mock_client(server))
    }
}

/// A cache holding each of `digests` as completed at `repo_name` on `target`.
fn cache_with(
    target: &str,
    repo_name: &str,
    digests: &[&Digest],
) -> Rc<RefCell<TransferStateCache>> {
    let cache = empty_cache();
    for digest in digests {
        cache
            .borrow_mut()
            .set_blob_completed(target, (*digest).clone(), repo(repo_name));
    }
    cache
}

/// The cache an earlier run left, as watch mode keeps it between cycles:
/// each of `digests` present at `repo` on target `target`.
fn earlier_run_cache(digests: &[&Digest]) -> Rc<RefCell<TransferStateCache>> {
    cache_with("target", "repo", digests)
}

/// A retry config like [`fast_retry`] with the manifest-commit wait set.
fn commit_wait_retry(wait: Duration) -> RetryConfig {
    RetryConfig {
        manifest_commit_wait: wait,
        ..fast_retry()
    }
}

/// Two images sharing one base layer and one unique layer each.
///
/// After the first image syncs, the base layer is recorded as completed at
/// `(target, repo)`, so the second image skips its HEAD. Total pushes:
/// base + `layer_a` + `layer_b` = 3, not 4.
#[tokio::test]
async fn sync_progressive_cache_skips_shared_blob_head_check() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts_a = ManifestBuilder::new(b"config-a")
        .layer(b"base-layer-data")
        .layer(b"layer-a-data")
        .build();
    let parts_b = ManifestBuilder::new(b"config-b")
        .layer(b"base-layer-data")
        .layer(b"layer-b-data")
        .build();

    parts_a.mount_source(&source_server, "repo", "v1").await;
    parts_b.mount_source(&source_server, "repo", "v2").await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v2").await;

    // v1: all three blobs are missing. v2: the base layer is a cache hit, so
    // only config_b and layer_b are checked.
    mount_blob_not_found(&target_server, "repo", &parts_a.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts_a.layer_descs[0].digest).await;
    mount_blob_not_found(&target_server, "repo", &parts_a.layer_descs[1].digest).await;
    mount_blob_not_found(&target_server, "repo", &parts_b.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts_b.layer_descs[1].digest).await;

    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v2").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1"), TagPair::same("v2")],
    );

    // Sequential, so v1 populates the cache before v2 starts.
    let report = run_sync_sequential(vec![mapping]).await;

    assert_eq!(report.images.len(), 2);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_status!(report, 1, ImageStatus::Synced);

    assert_eq!(report.images[0].blob_stats.transferred, 3);
    assert_eq!(report.images[0].blob_stats.skipped, 0);
    assert_eq!(report.images[1].blob_stats.transferred, 2);
    assert_eq!(report.images[1].blob_stats.skipped, 1);
    assert_eq!(report.stats.blobs_transferred, 5);
    assert_eq!(report.stats.blobs_skipped, 1);
}

/// How the cache knows repo-a held the blobs.
#[derive(Clone, Copy, Debug)]
enum RepoA {
    /// Committed earlier in this process, so it is offered ahead of repos
    /// known only from an earlier run.
    Committed,
    /// Known only from an earlier run and never committed: this run pushes
    /// nothing to it, so its commit watch never fires and the mount must not wait.
    EarlierRun,
}

/// A cache holding the blobs at repo-a makes the engine mount them into
/// repo-b with a cross-repo POST (201 -> Mounted) instead of pulling them.
#[tokio::test]
async fn sync_warm_cache_mounts_from_a_cached_repo() {
    for repo_a in [RepoA::Committed, RepoA::EarlierRun] {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parts = ManifestBuilder::new(b"config-data")
            .layer(b"layer-data")
            .build();

        // Blobs are mounted, not pulled.
        mount_source_manifest(&source_server, "repo-b", "v1", &parts.bytes).await;

        mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
        Mock::given(method("POST"))
            .and(path("/v2/repo-b/blobs/uploads/"))
            .and(query_param("from", "repo-a"))
            .respond_with(ResponseTemplate::new(201))
            .expect(2)
            .named(format!("{repo_a:?}: mount POST from repo-a"))
            .mount(&target_server)
            .await;
        mount_manifest_push(&target_server, "repo-b", "v1").await;

        let cache = cache_with(
            "target",
            "repo-a",
            &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
        );
        if matches!(repo_a, RepoA::Committed) {
            cache
                .borrow_mut()
                .mark_repo_committed("target", &repo("repo-a"));
        }

        let mapping = mapping_from_servers(
            &source_server,
            &target_server,
            "repo-b",
            vec![TagPair::same("v1")],
        );

        // Waiting for repo-a's commit would hold each blob for the 60-second
        // deadline and then push it.
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            run_sync_with_cache(vec![mapping], cache),
        )
        .await
        .unwrap_or_else(|_| panic!("{repo_a:?}: mount waited for a commit that never comes"));

        assert_eq!(report.images.len(), 1, "{repo_a:?}");
        assert_status!(report, 0, ImageStatus::Synced, "{repo_a:?}");
        assert_eq!(report.images[0].blob_stats.mounted, 2, "{repo_a:?}");
        assert_eq!(report.images[0].blob_stats.transferred, 0, "{repo_a:?}");
        assert_eq!(report.images[0].blob_stats.skipped, 0, "{repo_a:?}");
        assert_eq!(report.stats.blobs_mounted, 2, "{repo_a:?}");
        assert_eq!(report.stats.blobs_transferred, 0, "{repo_a:?}");
    }
}

/// A refused mount still checks whether the target repo holds the blob, so a
/// blob already there is not pushed again on a registry with no batch check.
#[tokio::test]
async fn sync_refused_mount_still_checks_the_target_repo() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"refused-config")
        .layer(b"refused-layer")
        .build();

    mount_source_manifest(&source_server, "repo-b", "v1", &parts.bytes).await;

    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param("from", "repo-a"))
        .respond_with(
            ResponseTemplate::new(202)
                .append_header("Location", "/v2/repo-b/blobs/uploads/refused-uuid"),
        )
        .expect(2)
        .mount(&target_server)
        .await;
    for desc in [&parts.config_desc, &parts.layer_descs[0]] {
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/repo-b/blobs/{}", desc.digest)))
            .respond_with(
                ResponseTemplate::new(200).insert_header("content-length", desc.size.to_string()),
            )
            .expect(1)
            .mount(&target_server)
            .await;
    }
    Mock::given(method("PUT"))
        .and(path_regex("^/v2/repo-b/blobs/uploads/.+"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let cache = cache_with(
        "target",
        "repo-a",
        &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
    );

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo-b",
        vec![TagPair::same("v1")],
    );
    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
}

/// An ECR target with `BLOB_MOUNTING` off answers mount POSTs with 202 (not
/// fulfilled). The engine still issues the POST, then falls through to HEAD
/// and push: the 202 fallback is cheap and keeps the optimization working
/// when `BLOB_MOUNTING` is on.
#[tokio::test]
async fn sync_warm_cache_ecr_target_mount_not_fulfilled() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"ecr-config-data")
        .layer(b"ecr-layer-data")
        .build();

    parts.mount_source(&source_server, "repo-b", "v1").await;

    mount_manifest_head_not_found(&target_server, "repo-b", "v1").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo-b/blobs/uploads/"))
        .and(query_param("from", "repo-a"))
        .respond_with(
            ResponseTemplate::new(202)
                .append_header("Location", "/v2/repo-b/blobs/uploads/fallback-uuid"),
        )
        .expect(2)
        .mount(&target_server)
        .await;

    mount_blob_push(&target_server, "repo-b").await;
    mount_manifest_push(&target_server, "repo-b", "v1").await;

    let target_name = "ecr-target";
    let cache = cache_with(
        target_name,
        "repo-a",
        &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
    );
    // A committed repo is a valid mount source.
    cache
        .borrow_mut()
        .mark_repo_committed(target_name, &repo("repo-a"));

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo-b",
        "repo-b",
        vec![target_entry(target_name, ecr_mock_client(&target_server))],
        vec![TagPair::same("v1")],
    );

    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.images[0].blob_stats.mounted, 0);
    assert_eq!(report.images[0].blob_stats.transferred, 2);
    assert_eq!(report.stats.blobs_mounted, 0);
    assert_eq!(report.stats.blobs_transferred, 2);
}

/// Every blob uploads as one POST and one PUT with no PATCH, whatever its size.
#[tokio::test]
async fn sync_small_blob_uses_monolithic_upload() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"small-config")
        .layer(b"small-layer")
        .build();
    parts.mount_source(&source_server, "repo", "v1").await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    parts.mount_target_blob_misses(&target_server, "repo").await;
    let session = "/v2/repo/blobs/uploads/mono-id";
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(202).insert_header("location", session))
        .expect(2)
        .named("one upload POST per blob")
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path(session))
        .respond_with(ResponseTemplate::new(201))
        .expect(2)
        .named("one upload PUT per blob")
        .mount(&target_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(session))
        .respond_with(ResponseTemplate::new(202).insert_header("location", session))
        .expect(0)
        .named("no chunked PATCH")
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let report = run_sync(vec![mapping]).await;

    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.images[0].blob_stats.transferred, 2);
}

/// Blobs a sync records survive persist and load at their own repo only.
#[tokio::test]
async fn sync_cache_persist_and_load_round_trip() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"persist-config")
        .layer(b"persist-layer")
        .build();

    parts.mount_source(&source_server, "repo", "v1").await;
    parts.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_to_targets(
        &source_server,
        &[("target-reg", &target_server)],
        "repo",
        vec![TagPair::same("v1")],
    );

    let cache = empty_cache();
    let report = run_sync_with_cache(vec![mapping], Rc::clone(&cache)).await;

    assert_eq!(report.stats.blobs_transferred, 2);

    let tmp_dir = tempfile::tempdir().unwrap();
    let cache_path = tmp_dir.path().join("sync_cache.bin");
    cache.borrow().persist(&cache_path).unwrap();

    let loaded = TransferStateCache::load(&cache_path, Duration::from_secs(3600));

    assert!(
        loaded.blob_present_at("target-reg", &parts.config_desc.digest, &repo("repo")),
        "config blob should be recorded as completed at repo"
    );
    assert!(
        loaded.blob_present_at("target-reg", &parts.layer_descs[0].digest, &repo("repo")),
        "layer blob should be recorded as completed at repo"
    );
    assert!(
        !loaded.blob_present_at("target-reg", &parts.config_desc.digest, &repo("other-repo")),
        "blob should not appear at an unrelated repo"
    );
}

/// After a mount failure triggers lazy invalidation, the cache entry is
/// cleared and the blob is recorded as completed via the fallback push.
#[tokio::test]
async fn sync_lazy_invalidation_clears_cache_and_records_completion() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-cache-verify")
        .layer(b"layer-cache-verify")
        .build();

    parts.mount_source(&source_server, "repo", "v1").await;

    mount_manifest_head_not_found(&target_server, "repo", "v1").await;

    // Mount attempts fail with 404.
    for digest in [&parts.config_desc.digest, &parts.layer_descs[0].digest] {
        Mock::given(method("POST"))
            .and(path("/v2/repo/blobs/uploads/"))
            .and(query_param("mount", digest.to_string()))
            .respond_with(ResponseTemplate::new(404))
            .mount(&target_server)
            .await;
    }

    // Fallback: HEAD 404, then push.
    mount_blob_not_found(&target_server, "repo", &parts.config_desc.digest).await;
    mount_blob_not_found(&target_server, "repo", &parts.layer_descs[0].digest).await;
    mount_blob_push(&target_server, "repo").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    // A committed stale mount source makes the mount be attempted and fail.
    let cache = cache_with(
        "target",
        "stale-repo",
        &[&parts.config_desc.digest, &parts.layer_descs[0].digest],
    );
    cache
        .borrow_mut()
        .mark_repo_committed("target", &repo("stale-repo"));

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync_with_cache(vec![mapping], cache.clone()).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);

    let c = cache.borrow();
    for (what, digest) in [
        ("config", &parts.config_desc.digest),
        ("layer", &parts.layer_descs[0].digest),
    ] {
        assert!(
            !c.blob_present_at("target", digest, &repo("stale-repo")),
            "stale cache entry for {what} at stale-repo should be invalidated"
        );
        assert!(
            c.blob_present_at("target", digest, &repo("repo")),
            "{what} blob should be recorded as completed at repo after fallback push"
        );
    }
}

/// A pre-warmed cache records a blob as completed at `(target, repo)`; the
/// engine skips it with no HEAD, pull or push. No blob mocks exist, so any
/// such request would fail the sync.
#[tokio::test]
async fn sync_warm_cache_skips_blob_head_check() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"cache-skip-config")
        .layer(b"cache-skip-layer")
        .build();

    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    mount_manifest_push(&target_server, "repo", "v1").await;

    let cache = earlier_run_cache(&[&parts.config_desc.digest, &parts.layer_descs[0].digest]);

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );

    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.images[0].blob_stats.skipped, 2);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 2);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
}

/// What the batch check says about one blob.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Answer {
    /// Reported present: no HEAD, no copy.
    Exists,
    /// Reported absent: copied without a HEAD.
    Missing,
    /// Left out of the answer: gets its own HEAD, which finds it.
    Unchecked,
}

/// The existence check a target has.
#[derive(Clone, Copy, Debug)]
enum Check {
    /// No batch checker: every blob gets a HEAD.
    Absent,
    /// The batch call fails: every blob gets a HEAD.
    Fails,
    /// The batch call answers for the config, layer 1 and layer 2.
    Answers([Answer; 3]),
}

/// What the cache holds when the run starts.
#[derive(Clone, Copy, Debug)]
enum Cache {
    Cold,
    /// The config blob is already known at the target repo.
    ConfigVerified,
}

/// The config and two layers of the image the batch table syncs, all the same length.
const BATCH_BLOBS: [&[u8]; 3] = [b"config-batch-0", b"layer-batch-01", b"layer-batch-02"];

/// The batch check, or its absence or failure, decides which blobs get a HEAD
/// and which are copied. Whatever the check says, the engine asks it with the
/// target repo (not the source repo), and a blob it reports present gets
/// neither a HEAD nor a pull.
#[tokio::test]
async fn sync_batch_checker_answers_decide_what_transfers() {
    struct Case {
        name: &'static str,
        check: Check,
        cache: Cache,
        tags: &'static [&'static str],
        batch_calls: usize,
        /// Blob counts and bytes of each tag's image.
        skipped: u64,
        transferred: u64,
        bytes: u64,
    }
    use Answer::{Exists, Missing, Unchecked};
    let single = Case {
        name: "",
        check: Check::Answers([Exists; 3]),
        cache: Cache::Cold,
        tags: &["v1"],
        batch_calls: 1,
        skipped: 3,
        transferred: 0,
        bytes: 0,
    };
    let cases = [
        Case {
            name: "all exist: no HEAD, no copy",
            ..single
        },
        Case {
            name: "two tags share the checker, one call per tag",
            tags: &["v1", "v2"],
            batch_calls: 2,
            ..single
        },
        Case {
            name: "warm cache and batch agree: the batch is still asked",
            cache: Cache::ConfigVerified,
            ..single
        },
        Case {
            name: "one layer missing: only it is copied",
            check: Check::Answers([Exists, Missing, Exists]),
            skipped: 2,
            transferred: 1,
            bytes: 14,
            ..single
        },
        Case {
            name: "unchecked layer gets its own HEAD, the missing one none",
            check: Check::Answers([Exists, Missing, Unchecked]),
            skipped: 2,
            transferred: 1,
            bytes: 14,
            ..single
        },
        Case {
            name: "nothing exists: every blob copied with no HEAD",
            check: Check::Answers([Missing; 3]),
            skipped: 0,
            transferred: 3,
            bytes: 42,
            ..single
        },
        Case {
            name: "no checker: per-blob HEAD",
            check: Check::Absent,
            batch_calls: 0,
            ..single
        },
        Case {
            name: "failing checker: per-blob HEAD",
            check: Check::Fails,
            ..single
        },
    ];
    for case in cases {
        let name = case.name;
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;
        let parts = ManifestBuilder::new(BATCH_BLOBS[0])
            .layer(BATCH_BLOBS[1])
            .layer(BATCH_BLOBS[2])
            .build();
        let digests = [
            &parts.config_desc.digest,
            &parts.layer_descs[0].digest,
            &parts.layer_descs[1].digest,
        ];
        // Unequal repo names show the engine passes the target repo to the checker.
        let (src, tgt) = ("src/nginx", "tgt/nginx");
        let answers = match case.check {
            Check::Answers(answers) => answers,
            Check::Absent | Check::Fails => [Unchecked; 3],
        };

        for tag in case.tags {
            source_manifest_mock(src, tag, &parts.bytes, MediaType::OciManifest)
                .expect(1)
                .named(format!("{name}: source manifest GET {tag}"))
                .mount(&source_server)
                .await;
            mount_manifest_head_not_found(&target_server, tgt, tag).await;
            manifest_push_mock(tgt, tag)
                .expect(1)
                .named(format!("{name}: manifest PUT {tag}"))
                .mount(&target_server)
                .await;
        }
        for ((digest, data), answer) in digests.into_iter().zip(BATCH_BLOBS).zip(answers) {
            let (pulls, heads) = match answer {
                Exists => (0, 0),
                Missing => (1, 0),
                Unchecked => (0, 1),
            };
            blob_pull_mock(src, digest, data)
                .expect(pulls)
                .named(format!("{name}: blob GET {digest}"))
                .mount(&source_server)
                .await;
            blob_head_mock(tgt, digest, 200)
                .expect(heads)
                .named(format!("{name}: blob HEAD {digest}"))
                .mount(&target_server)
                .await;
        }
        mount_blob_push_expecting(&target_server, tgt, case.transferred, name).await;

        let (target, calls) = match case.check {
            Check::Absent => (
                target_entry("target", mock_client(&target_server)),
                Calls::default(),
            ),
            Check::Fails => {
                let (checker, calls) = MockChecker::new(tgt, HashSet::new());
                (
                    batch_target("target", &target_server, checker.failing()),
                    calls,
                )
            }
            Check::Answers(_) => {
                let pick = |wanted| -> HashSet<Digest> {
                    digests
                        .into_iter()
                        .zip(answers)
                        .filter(|(_, a)| *a == wanted)
                        .map(|(d, _)| d.clone())
                        .collect()
                };
                let (checker, calls) = MockChecker::new(tgt, pick(Exists));
                (
                    batch_target("target", &target_server, checker.unknown(pick(Unchecked))),
                    calls,
                )
            }
        };
        let cache = empty_cache();
        if matches!(case.cache, Cache::ConfigVerified) {
            cache
                .borrow_mut()
                .set_blob_verified("target", digests[0].clone(), repo(tgt));
        }
        let mapping = resolved_mapping(
            mock_client(&source_server),
            src,
            tgt,
            vec![target],
            same_tags(case.tags),
        );

        let report = run_sync_with_cache(vec![mapping], cache).await;

        let images = case.tags.len() as u64;
        assert_eq!(calls.checks.get(), case.batch_calls, "{name}: batch calls");
        assert_eq!(report.images.len(), case.tags.len(), "{name}");
        for (i, image) in report.images.iter().enumerate() {
            assert_status!(report, i, ImageStatus::Synced, "{name}");
            assert_eq!(
                image.blob_stats.skipped, case.skipped,
                "{name}: image {i} skipped"
            );
            assert_eq!(
                image.blob_stats.transferred, case.transferred,
                "{name}: image {i} transferred"
            );
            assert_eq!(
                image.bytes_transferred, case.bytes,
                "{name}: image {i} bytes"
            );
        }
        assert_eq!(report.stats.images_synced, images, "{name}");
        assert_eq!(
            report.stats.blobs_skipped,
            case.skipped * images,
            "{name}: skipped"
        );
        assert_eq!(
            report.stats.blobs_transferred,
            case.transferred * images,
            "{name}: transferred"
        );
        assert_eq!(
            report.stats.bytes_transferred,
            case.bytes * images,
            "{name}: bytes"
        );
    }
}

/// Each target's batch checker decides only that target's blobs: A skips both,
/// B copies the layer the checker did not report, and the source manifest is
/// pulled once.
#[tokio::test]
async fn sync_batch_checker_multi_target_independent_checkers() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    let layer_data = b"layer-multi";
    let parts = ManifestBuilder::new(b"config-multi")
        .layer(layer_data)
        .build();
    let (config, layer) = (&parts.config_desc.digest, &parts.layer_descs[0].digest);

    source_manifest_mock("repo", "v1", &parts.bytes, MediaType::OciManifest)
        .expect(1)
        .mount(&source_server)
        .await;
    // Only target B needs the layer.
    blob_pull_mock("repo", layer, layer_data)
        .expect(1)
        .mount(&source_server)
        .await;

    mount_manifest_head_not_found(&target_a_server, "repo", "v1").await;
    mount_manifest_push(&target_a_server, "repo", "v1").await;

    mount_manifest_head_not_found(&target_b_server, "repo", "v1").await;
    mount_blob_push_expecting(&target_b_server, "repo", 1, "target-b").await;
    mount_manifest_push(&target_b_server, "repo", "v1").await;

    let (checker_a, calls_a) =
        MockChecker::new("repo", HashSet::from([config.clone(), layer.clone()]));
    let (checker_b, calls_b) = MockChecker::new("repo", HashSet::from([config.clone()]));

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            batch_target("target-a", &target_a_server, checker_a),
            batch_target("target-b", &target_b_server, checker_b),
        ],
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(
        calls_a.checks.get(),
        1,
        "target-a batch checker must be called once"
    );
    assert_eq!(
        calls_b.checks.get(),
        1,
        "target-b batch checker must be called once"
    );
    assert_eq!(report.images.len(), 2);

    // Tell the targets apart by blob stats: A skipped 2, B skipped 1 and sent 1.
    let result_a = report
        .images
        .iter()
        .find(|r| r.blob_stats.skipped == 2 && r.blob_stats.transferred == 0)
        .expect("target-a result (all skipped) not found");
    let result_b = report
        .images
        .iter()
        .find(|r| r.blob_stats.skipped == 1 && r.blob_stats.transferred == 1)
        .expect("target-b result (partial transfer) not found");

    assert!(matches!(result_a.status, ImageStatus::Synced));
    assert_eq!(result_a.bytes_transferred, 0);
    assert!(matches!(result_b.status, ImageStatus::Synced));
    assert_eq!(
        result_b.bytes_transferred,
        layer_data.len() as u64,
        "only layer bytes should be transferred to target-b"
    );

    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_skipped, 3);
    assert_eq!(report.stats.blobs_transferred, 1);
    assert_eq!(report.stats.bytes_transferred, layer_data.len() as u64);
}

/// The answer refusing a manifest because the registry lacks a blob it
/// references.
const MANIFEST_BLOB_UNKNOWN: &str =
    r#"{"errors":[{"code":"MANIFEST_BLOB_UNKNOWN","message":"blob unknown to registry"}]}"#;

/// What the batch check answers about the blobs an earlier run cached.
#[derive(Clone, Copy, Debug)]
enum BatchAnswer {
    /// Checked and missing.
    Missing,
    /// Left unknown by a partial failure.
    Unknown,
    /// The whole call failed.
    Failed,
}

/// An earlier run's cache says both blobs are at the target, but the
/// repository has lost them (deleted and recreated, or layers expired). A
/// batch check that finds them missing wins over the cache with no further
/// request. One that cannot answer sends a HEAD to the registry per blob
/// rather than trusting the cache. Either way each blob is pulled once and
/// pushed; trusting the cache would skip them and leave a manifest the
/// registry refuses.
#[tokio::test]
async fn sync_batch_checker_overrides_an_earlier_runs_cache() {
    for answer in [
        BatchAnswer::Missing,
        BatchAnswer::Unknown,
        BatchAnswer::Failed,
    ] {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;
        let config_data = b"config-stale-cache";
        let layer_data = b"layer-stale-cache";
        let parts = ManifestBuilder::new(config_data).layer(layer_data).build();
        let digests = [&parts.config_desc.digest, &parts.layer_descs[0].digest];

        mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;
        for (digest, data) in digests.into_iter().zip([&config_data[..], &layer_data[..]]) {
            blob_pull_mock("repo", digest, data)
                .expect(1)
                .named(format!("{answer:?}: blob GET {digest}"))
                .mount(&source_server)
                .await;
            let heads = match answer {
                BatchAnswer::Missing => 0,
                BatchAnswer::Unknown | BatchAnswer::Failed => 1,
            };
            blob_head_mock("repo", digest, 404)
                .expect(heads)
                .named(format!("{answer:?}: blob HEAD {digest}"))
                .mount(&target_server)
                .await;
        }
        mount_manifest_head_not_found(&target_server, "repo", "v1").await;
        mount_blob_push(&target_server, "repo").await;
        mount_manifest_push(&target_server, "repo", "v1").await;

        let (checker, calls) = MockChecker::new("repo", HashSet::new());
        let checker = match answer {
            BatchAnswer::Missing => checker,
            BatchAnswer::Unknown => checker.unknown(digests.into_iter().cloned().collect()),
            BatchAnswer::Failed => checker.failing(),
        };
        let mapping = resolved_mapping(
            mock_client(&source_server),
            "repo",
            "repo",
            vec![batch_target("target", &target_server, checker)],
            vec![TagPair::same("v1")],
        );
        let report = run_sync_with_cache(vec![mapping], earlier_run_cache(&digests)).await;

        assert_eq!(calls.checks.get(), 1, "{answer:?}");
        assert_status!(report, 0, ImageStatus::Synced, "{answer:?}");
        assert_eq!(
            report.stats.blobs_skipped, 0,
            "{answer:?}: the stale cache must not skip"
        );
        assert_eq!(report.stats.blobs_transferred, 2, "{answer:?}");
        assert_eq!(
            report.stats.bytes_transferred,
            (config_data.len() + layer_data.len()) as u64,
            "{answer:?}",
        );
    }
}
/// What the cache holds when a run starts.
#[derive(Clone, Copy, Debug)]
enum CacheAtStart {
    /// An earlier run recorded both blobs at the target.
    EarlierRun,
    /// An earlier run recorded only the layer, so the config is sent before
    /// the refusal.
    LayerOnly,
    /// Nothing.
    Empty,
}

/// How often the target refuses the manifest before accepting it.
#[derive(Clone, Copy, Debug)]
enum Refusals {
    Once,
    Always,
}

/// Without a batch check the earlier run's cache is trusted, so no blob gets
/// a HEAD. A manifest refused as invalid (400 or 404, whatever code the
/// registry names for the lost blob) drops those entries, and the run copies
/// the blobs again and pushes the manifest once more, so the image syncs in
/// the same run. A refusal for access, throttling or a server fault, or one
/// that dropped nothing from the cache, is not repaired, and the manifest is
/// pushed again at most once.
#[tokio::test]
async fn sync_refused_manifest_repairs_an_earlier_runs_cache_in_the_same_run() {
    const MANIFEST_INVALID: &str =
        r#"{"errors":[{"code":"MANIFEST_INVALID","message":"layer missing"}]}"#;
    const DENIED: &str = r#"{"errors":[{"code":"DENIED","message":"denied"}]}"#;
    /// One refusal scenario and what the run must do.
    struct Case {
        cache: CacheAtStart,
        status: u16,
        body: &'static str,
        refusals: Refusals,
        synced: bool,
        blob_heads: u64,
        manifest_puts: usize,
        blobs_sent: u64,
    }
    let repaired = |cache, body| Case {
        cache,
        status: 400,
        body,
        refusals: Refusals::Once,
        synced: true,
        blob_heads: 2,
        manifest_puts: 2,
        blobs_sent: 2,
    };
    let cases = [
        repaired(CacheAtStart::EarlierRun, MANIFEST_BLOB_UNKNOWN),
        repaired(CacheAtStart::EarlierRun, MANIFEST_INVALID),
        repaired(CacheAtStart::LayerOnly, MANIFEST_BLOB_UNKNOWN),
        Case {
            refusals: Refusals::Always,
            synced: false,
            ..repaired(CacheAtStart::EarlierRun, MANIFEST_BLOB_UNKNOWN)
        },
        Case {
            status: 403,
            synced: false,
            blob_heads: 0,
            manifest_puts: 1,
            blobs_sent: 0,
            ..repaired(CacheAtStart::EarlierRun, DENIED)
        },
        Case {
            refusals: Refusals::Always,
            synced: false,
            manifest_puts: 1,
            ..repaired(CacheAtStart::Empty, MANIFEST_BLOB_UNKNOWN)
        },
    ];
    for Case {
        cache: cache_at_start,
        status,
        body,
        refusals,
        synced,
        blob_heads: heads,
        manifest_puts: puts,
        blobs_sent: sent,
    } in cases
    {
        let case = format!("{cache_at_start:?} {status} {refusals:?}");
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;
        let config_data = b"config-refused";
        let layer_data = b"layer-refused";
        let parts = ManifestBuilder::new(config_data).layer(layer_data).build();
        let digests = [&parts.config_desc.digest, &parts.layer_descs[0].digest];
        parts.mount_source(&source_server, "repo", "v1").await;
        mount_manifest_head_not_found(&target_server, "repo", "v1").await;
        Mock::given(method("HEAD"))
            .and(path_regex("^/v2/repo/blobs/"))
            .respond_with(ResponseTemplate::new(404))
            .expect(heads)
            .named(format!("{case}: blob HEAD"))
            .mount(&target_server)
            .await;
        mount_blob_push(&target_server, "repo").await;
        let refusal = Mock::given(method("PUT"))
            .and(path("/v2/repo/manifests/v1"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body));
        match refusals {
            Refusals::Once => refusal.up_to_n_times(1),
            Refusals::Always => refusal,
        }
        .mount(&target_server)
        .await;
        mount_manifest_push(&target_server, "repo", "v1").await;

        let mapping = mapping_from_servers(
            &source_server,
            &target_server,
            "repo",
            vec![TagPair::same("v1")],
        );
        let cache = match cache_at_start {
            CacheAtStart::EarlierRun => earlier_run_cache(&digests),
            CacheAtStart::LayerOnly => earlier_run_cache(&digests[1..]),
            CacheAtStart::Empty => empty_cache(),
        };
        let report = run_sync_with_cache(vec![mapping], Rc::clone(&cache)).await;

        assert_eq!(
            matches!(report.images[0].status, ImageStatus::Synced),
            synced,
            "{case}: {:?}",
            report.images[0].status
        );
        let manifest_puts = count_requests(
            &target_server,
            Some(&Method::PUT),
            PathMatch::Exact("/v2/repo/manifests/v1"),
        )
        .await;
        assert_eq!(manifest_puts, puts, "{case}");
        assert_eq!(report.stats.blobs_transferred, sent, "{case}");
        assert_eq!(report.stats.blobs_skipped, 2 - sent, "{case}");
        assert_eq!(
            report.images[0].bytes_transferred,
            if sent == 0 {
                0
            } else {
                (config_data.len() + layer_data.len()) as u64
            },
            "{case}"
        );
        for digest in digests {
            assert!(
                cache
                    .borrow()
                    .blob_present_at("target", digest, &repo("repo")),
                "{case}: {digest}"
            );
        }
    }
}

/// A repair whose blob copy fails reports that failure and does not push
/// the manifest again: the registry would only refuse it once more.
#[tokio::test]
async fn sync_failed_repair_does_not_push_the_manifest_again() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let parts = ManifestBuilder::new(b"config-unrepaired")
        .layer(b"layer-unrepaired")
        .build();
    parts.mount_source(&source_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_server, "repo", "v1").await;
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&target_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/v1"))
        .respond_with(ResponseTemplate::new(400).set_body_string(MANIFEST_BLOB_UNKNOWN))
        .expect(1)
        .mount(&target_server)
        .await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let cache = earlier_run_cache(&[&parts.config_desc.digest, &parts.layer_descs[0].digest]);
    let report = run_sync_with_cache(vec![mapping], cache).await;

    match &report.images[0].status {
        ImageStatus::Failed { kind, error, .. } => {
            assert!(matches!(kind, ErrorKind::BlobTransfer), "{kind:?}: {error}");
            assert!(
                !error.contains("MANIFEST_BLOB_UNKNOWN"),
                "the blob copy's failure is reported, not the refusal: {error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Two tags of one image share the blobs an earlier run cached and the
/// registry has lost. Each tag's manifest is refused once. The first refusal
/// drops the cached blobs; the other finds nothing left to drop but is
/// repaired too, so both tags sync and each blob is sent once.
#[tokio::test]
async fn sync_refused_sibling_is_repaired_after_another_tasks_drop() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let parts = ManifestBuilder::new(b"config-siblings")
        .layer(b"layer-siblings")
        .build();
    let tags = ["latest", "v1"];
    for tag in tags {
        parts.mount_source(&source_server, "repo", tag).await;
        mount_manifest_head_not_found(&target_server, "repo", tag).await;
        Mock::given(method("PUT"))
            .and(path(format!("/v2/repo/manifests/{tag}")))
            .respond_with(ResponseTemplate::new(400).set_body_string(MANIFEST_BLOB_UNKNOWN))
            .up_to_n_times(1)
            .mount(&target_server)
            .await;
        mount_manifest_push(&target_server, "repo", tag).await;
    }
    mount_blob_push(&target_server, "repo").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        tags.into_iter().map(TagPair::same).collect(),
    );
    let cache = earlier_run_cache(&[&parts.config_desc.digest, &parts.layer_descs[0].digest]);
    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_eq!(report.stats.images_synced, 2, "{:?}", report.images);
    assert_eq!(report.stats.blobs_transferred, 2, "each blob is sent once");
    let blob_uploads = count_requests(&target_server, Some(&Method::POST), PathMatch::Any).await;
    assert_eq!(blob_uploads, 2);
}

/// The repair runs for an index child too: a child refused for a blob an
/// earlier run cached gets its blobs copied again and is pushed once more,
/// before the index.
#[tokio::test]
async fn sync_refused_index_child_is_repaired_in_the_same_run() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let child = ManifestBuilder::new(b"config-child")
        .layer(b"layer-child")
        .build();
    let index = IndexBuilder::new().manifest(&child, None).build();
    index.mount_source(&source_server, "repo", "v1").await;
    let child_manifest = format!("/v2/repo/manifests/{}", child.digest);
    Mock::given(method("PUT"))
        .and(path(child_manifest.as_str()))
        .respond_with(ResponseTemplate::new(400).set_body_string(MANIFEST_BLOB_UNKNOWN))
        .up_to_n_times(1)
        .mount(&target_server)
        .await;
    index.mount_target(&target_server, "repo", "v1").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("v1")],
    );
    let cache = earlier_run_cache(&[&child.config_desc.digest, &child.layer_descs[0].digest]);
    let report = run_sync_with_cache(vec![mapping], cache).await;

    assert_status!(report, 0, ImageStatus::Synced);
    let child_pushes = count_requests(
        &target_server,
        Some(&Method::PUT),
        PathMatch::Exact(&child_manifest),
    )
    .await;
    assert_eq!(child_pushes, 2, "refused once, then pushed again");
    assert_eq!(report.stats.blobs_transferred, 2);
    assert_eq!(report.stats.blobs_skipped, 0);
}

/// A committed manifest proves its blobs are in the repository. When a
/// second tag in the same repository is then refused for a lost blob, the
/// layer the first tag's commit proved stays cached, so the repair resends
/// only the blob the second tag alone references.
#[tokio::test]
async fn sync_committed_manifest_confirms_its_blobs() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let first = ManifestBuilder::new(b"config-first")
        .layer(b"shared-layer")
        .build();
    let second_config = b"config-second";
    let second = ManifestBuilder::new(second_config)
        .layer(b"shared-layer")
        .build();
    assert_eq!(first.layer_descs[0].digest, second.layer_descs[0].digest);
    // The engine runs the second listed tag first at one image at a time,
    // so the committed image is listed second. Were that order to change,
    // the committed tag would find the shared layer dropped and have no blob
    // push to fall back to, failing this test rather than passing on another
    // path.
    first
        .mount_source(&source_server, "repo", "committed")
        .await;
    second.mount_source(&source_server, "repo", "refused").await;
    for tag in ["committed", "refused"] {
        mount_manifest_head_not_found(&target_server, "repo", tag).await;
    }
    mount_manifest_push(&target_server, "repo", "committed").await;
    Mock::given(method("PUT"))
        .and(path("/v2/repo/manifests/refused"))
        .respond_with(ResponseTemplate::new(400).set_body_string(MANIFEST_BLOB_UNKNOWN))
        .up_to_n_times(1)
        .mount(&target_server)
        .await;
    mount_manifest_push(&target_server, "repo", "refused").await;
    for (digest, heads) in [
        (&second.config_desc.digest, 1),
        (&first.layer_descs[0].digest, 0),
    ] {
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/repo/blobs/{digest}")))
            .respond_with(ResponseTemplate::new(404))
            .expect(heads)
            .mount(&target_server)
            .await;
    }
    mount_blob_push(&target_server, "repo").await;

    let mapping = mapping_from_servers(
        &source_server,
        &target_server,
        "repo",
        vec![TagPair::same("refused"), TagPair::same("committed")],
    );
    let cache = earlier_run_cache(&[
        &first.config_desc.digest,
        &second.config_desc.digest,
        &first.layer_descs[0].digest,
    ]);
    let report = run_engine(
        vec![mapping],
        fast_retry(),
        1,
        Rc::clone(&cache),
        BlobStage::disabled(),
        None,
    )
    .await;

    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_transferred, 1);
    assert_eq!(report.stats.bytes_transferred, second_config.len() as u64);
    let cache = cache.borrow();
    assert!(cache.blob_present_at("target", &first.layer_descs[0].digest, &repo("repo")));
    assert!(cache.blob_present_at("target", &second.config_desc.digest, &repo("repo")));
}

/// Two targets see the same image: one with a batch checker reporting both
/// blobs present (no HEAD), one without (a HEAD per blob, both present). A
/// checker on one target does not change how the other checks.
#[tokio::test]
async fn sync_mixed_batch_and_no_batch_multi_target() {
    let source_server = MockServer::start().await;
    let target_a_server = MockServer::start().await;
    let target_b_server = MockServer::start().await;

    let parts = ManifestBuilder::new(b"config-mixed")
        .layer(b"layer-mixed")
        .build();
    let digests = [&parts.config_desc.digest, &parts.layer_descs[0].digest];

    mount_source_manifest(&source_server, "repo", "v1", &parts.bytes).await;

    mount_manifest_head_not_found(&target_a_server, "repo", "v1").await;
    mount_manifest_push(&target_a_server, "repo", "v1").await;
    mount_manifest_head_not_found(&target_b_server, "repo", "v1").await;
    mount_manifest_push(&target_b_server, "repo", "v1").await;
    for digest in digests {
        blob_head_mock("repo", digest, 200)
            .expect(0)
            .mount(&target_a_server)
            .await;
        blob_head_mock("repo", digest, 200)
            .expect(1)
            .mount(&target_b_server)
            .await;
    }

    let (checker, calls) = MockChecker::new("repo", digests.into_iter().cloned().collect());

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![
            batch_target("target-a", &target_a_server, checker),
            target_entry("target-b", mock_client(&target_b_server)),
        ],
        vec![TagPair::same("v1")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(
        calls.checks.get(),
        1,
        "batch checker must be called once, for target-a only"
    );
    assert_eq!(report.images.len(), 2);
    for img in &report.images {
        assert!(matches!(img.status, ImageStatus::Synced));
        assert_eq!(img.blob_stats.skipped, 2);
        assert_eq!(img.blob_stats.transferred, 0);
        assert_eq!(img.bytes_transferred, 0);
    }
    assert_eq!(report.stats.images_synced, 2);
    assert_eq!(report.stats.blobs_skipped, 4);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
}

/// The batch check covers blobs that come from several child manifests of an
/// index (the multi-arch case): all 4 blobs (2 configs + 2 layers) are
/// reported present, so no source blob is pulled and no target blob gets a HEAD.
#[tokio::test]
async fn sync_batch_checker_index_manifest_all_exist() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    let amd64_parts = ManifestBuilder::new(b"amd64-config-batch")
        .layer(b"amd64-layer-batch")
        .build();
    let arm64_parts = ManifestBuilder::new(b"arm64-config-batch")
        .layer(b"arm64-layer-batch")
        .build();
    let index_parts = IndexBuilder::new()
        .manifest(&amd64_parts, None)
        .manifest(&arm64_parts, None)
        .build();

    // The index by tag, the children by digest. No source blob mocks exist.
    source_manifest_mock("repo", "latest", &index_parts.bytes, MediaType::OciIndex)
        .expect(1)
        .mount(&source_server)
        .await;
    for child in [&amd64_parts, &arm64_parts] {
        let digest = child.digest.to_string();
        source_manifest_mock("repo", &digest, &child.bytes, MediaType::OciManifest)
            .expect(1)
            .mount(&source_server)
            .await;
    }

    mount_manifest_head_not_found(&target_server, "repo", "latest").await;
    let digests: Vec<Digest> = [&amd64_parts, &arm64_parts]
        .iter()
        .flat_map(|m| m.blob_digests())
        .collect();
    for digest in &digests {
        blob_head_mock("repo", digest, 200)
            .expect(0)
            .mount(&target_server)
            .await;
    }

    // Exactly the two children and the index are pushed.
    for reference in [
        amd64_parts.digest.to_string(),
        arm64_parts.digest.to_string(),
        "latest".to_owned(),
    ] {
        Mock::given(method("PUT"))
            .and(path(format!("/v2/repo/manifests/{reference}")))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&target_server)
            .await;
    }

    let (checker, calls) = MockChecker::new("repo", digests.iter().cloned().collect());

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![batch_target("target", &target_server, checker)],
        vec![TagPair::same("latest")],
    );

    let report = run_sync(vec![mapping]).await;

    assert_eq!(
        calls.checks.get(),
        1,
        "batch checker must be called exactly once for index manifest"
    );
    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
    assert_eq!(report.images[0].blob_stats.skipped, 4);
    assert_eq!(report.images[0].blob_stats.transferred, 0);
    assert_eq!(report.images[0].bytes_transferred, 0);
    assert_eq!(report.stats.images_synced, 1);
    assert_eq!(report.stats.blobs_skipped, 4);
    assert_eq!(report.stats.blobs_transferred, 0);
    assert_eq!(report.stats.bytes_transferred, 0);
}

/// The wait on blob visibility before a manifest `PUT` (ECR's consistency
/// window: a blob `PUT-201` can land before the manifest validator sees it)
/// calls the checker's `wait_for_blobs_available` exactly once per manifest
/// commit when `manifest_commit_wait` is non-zero, and never when it is zero.
#[tokio::test]
async fn manifest_commit_wait_fires_only_when_enabled() {
    async fn run_one(commit_wait: Duration) -> Calls {
        let source_server = MockServer::start().await;
        let target_server = MockServer::start().await;

        let parts = ManifestBuilder::new(b"cfg-mw").layer(b"layer-mw").build();
        // Source and target repos differ so the checker sees the target repo.
        mount_source_manifest(&source_server, "src/repo", "v1", &parts.bytes).await;
        mount_manifest_head_not_found(&target_server, "tgt/repo", "v1").await;
        mount_manifest_push(&target_server, "tgt/repo", "v1").await;

        // All blobs exist, so no upload happens; the manifest PUT still fires
        // and is what triggers the wait.
        let (checker, calls) =
            WaitTrackingChecker::new("tgt/repo", parts.blob_digests().collect::<HashSet<_>>());

        let mapping = resolved_mapping(
            mock_client(&source_server),
            "src/repo",
            "tgt/repo",
            vec![batch_target("target", &target_server, checker)],
            vec![TagPair::same("v1")],
        );

        let report = run_engine(
            vec![mapping],
            commit_wait_retry(commit_wait),
            4,
            empty_cache(),
            BlobStage::disabled(),
            None,
        )
        .await;
        assert_status!(
            report,
            0,
            ImageStatus::Synced,
            "commit wait {commit_wait:?}"
        );
        calls
    }

    let off = run_one(Duration::ZERO).await;
    assert_eq!(
        off.waits.get(),
        0,
        "with manifest_commit_wait=0 the commit-gate wait must not fire"
    );
    assert!(off.checks.get() >= 1, "pre-discovery batch check expected");

    let on = run_one(Duration::from_millis(200)).await;
    assert_eq!(
        on.waits.get(),
        1,
        "with manifest_commit_wait>0 the commit-gate wait must fire once per manifest"
    );
}

/// Four tags sharing one index wait on blob visibility once: the first task
/// claims every child and waits on their blobs, and the others, finding the
/// children claimed, push only their index and skip the wait.
#[tokio::test]
async fn manifest_commit_wait_runs_once_for_tags_sharing_an_index() {
    let source_server = MockServer::start().await;
    let target_server = MockServer::start().await;
    let (amd64, arm64) = mount_shared_index(&source_server, &target_server).await;
    let existing: HashSet<Digest> = [&amd64, &arm64]
        .iter()
        .flat_map(|m| m.blob_digests())
        .collect();
    let (checker, calls) = WaitTrackingChecker::new("repo", existing);

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![batch_target("target", &target_server, checker)],
        same_tags(&SHARED_TAGS),
    );
    let report = run_engine(
        vec![mapping],
        commit_wait_retry(Duration::from_millis(200)),
        4,
        empty_cache(),
        BlobStage::disabled(),
        None,
    )
    .await;

    assert_eq!(report.stats.images_synced, 4);
    assert_eq!(
        calls.waits.get(),
        1,
        "the blob-visibility wait must not run once per tag"
    );
}

/// Two indexes in one repo share child `c1`, whose push fails. Run one at a
/// time, the second finds `c1` already failed and fails at once, without
/// waiting on blob visibility for its own child `c3`.
#[tokio::test]
async fn manifest_commit_wait_skipped_once_a_shared_child_failed() {
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
        .manifest(&c1, Some(linux("amd64")))
        .manifest(&c3, Some(linux("s390x")))
        .build();
    x.mount_source(&source_server, "repo", "x").await;
    x.mount_target(&target_server, "repo", "x").await;
    y.mount_source(&source_server, "repo", "y").await;
    y.mount_target(&target_server, "repo", "y").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/repo/manifests/{}", c1.digest)))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .mount(&target_server)
        .await;
    let existing: HashSet<Digest> = [&c1, &c2, &c3]
        .iter()
        .flat_map(|m| m.blob_digests())
        .collect();
    let (checker, calls) = WaitTrackingChecker::new("repo", existing);

    let mapping = resolved_mapping(
        mock_client(&source_server),
        "repo",
        "repo",
        vec![batch_target("target", &target_server, checker)],
        vec![TagPair::same("x"), TagPair::same("y")],
    );
    let report = run_engine(
        vec![mapping],
        commit_wait_retry(Duration::from_millis(200)),
        1,
        empty_cache(),
        BlobStage::disabled(),
        None,
    )
    .await;

    assert_eq!(report.stats.images_failed, 2);
    assert_eq!(
        calls.waits.get(),
        1,
        "the second index must not wait on blobs it can never use"
    );
}

/// A repo-failed watch is cleared when a run starts, with cache pruning off
/// too, so a run in the same process does not see a stale fired watch and
/// wait on, or mount from, a repo that never committed.
#[tokio::test]
async fn run_clears_repo_watches_without_cache_pruning() {
    let cache = empty_cache();
    cache
        .borrow_mut()
        .notify_repo_failed("target", &repo("repo"));

    SyncEngine::new(fast_retry(), concurrency(50))
        .with_cache_pruning(false)
        .run(
            Vec::new(),
            Rc::clone(&cache),
            BlobStage::disabled(),
            &NullProgress,
            None,
        )
        .await;

    let digest: Digest = format!("sha256:{}", "a".repeat(64)).parse().unwrap();
    let mut cache = cache.borrow_mut();
    cache.set_blob_verified("target", digest.clone(), repo("repo"));
    assert_eq!(
        cache
            .unresolved_watches_for("target", &digest, &repo("other"))
            .len(),
        1,
        "the previous run's fired watch carried into the next run"
    );
}
