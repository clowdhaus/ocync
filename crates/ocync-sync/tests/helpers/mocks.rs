//! Functions that mount wiremock endpoints, for tests where the mock topology is
//! the subject, and the mock batch checkers.

#![allow(dead_code, unused_imports, unreachable_pub)]

use std::cell::Cell;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ocync_distribution::spec::{MediaType, RepositoryName};
use ocync_distribution::{BatchBlobChecker, BlobExistence, Digest};
use ocync_sync::engine::TargetEntry;
use wiremock::http::Method;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

use super::fixtures::target_entry;

/// A manifest GET on `reference` returning `bytes` as `content_type`, not yet
/// mounted so a caller can add `.expect(n)`.
pub fn source_manifest_mock(
    repo: &str,
    reference: &str,
    bytes: &[u8],
    content_type: MediaType,
) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.to_vec())
                .insert_header("content-type", content_type.as_str()),
        )
}

/// Mount a source manifest GET served as an OCI image manifest.
pub async fn mount_source_manifest(server: &MockServer, repo: &str, tag: &str, bytes: &[u8]) {
    source_manifest_mock(repo, tag, bytes, MediaType::OciManifest)
        .mount(server)
        .await;
}

/// Mount the referrers API response for `parent_digest`.
pub async fn mount_referrers(server: &MockServer, repo: &str, parent_digest: &Digest, body: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/referrers/{parent_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.to_vec())
                .insert_header("content-type", MediaType::OciIndex.as_str()),
        )
        .mount(server)
        .await;
}

fn blob_pull_response(data: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_bytes(data.to_vec())
        .insert_header("content-length", data.len().to_string())
}

fn blob_get(repo: &str, digest: &Digest) -> MockBuilder {
    Mock::given(method("GET")).and(path(format!("/v2/{repo}/blobs/{digest}")))
}

/// A blob GET returning `data`, not yet mounted so a caller can add `.expect(n)`.
pub fn blob_pull_mock(repo: &str, digest: &Digest, data: &[u8]) -> Mock {
    blob_get(repo, digest).respond_with(blob_pull_response(data))
}

/// Mount a blob GET.
pub async fn mount_blob_pull(server: &MockServer, repo: &str, digest: &Digest, data: &[u8]) {
    blob_pull_mock(repo, digest, data).mount(server).await;
}

/// Mount a blob GET that answers only after `delay`.
pub async fn mount_blob_pull_delayed(
    server: &MockServer,
    repo: &str,
    digest: &Digest,
    data: &[u8],
    delay: Duration,
) {
    blob_get(repo, digest)
        .respond_with(blob_pull_response(data).set_delay(delay))
        .mount(server)
        .await;
}

/// Mount a blob HEAD returning 404.
pub async fn mount_blob_not_found(server: &MockServer, repo: &str, digest: &Digest) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// A blob HEAD answering `status`, not yet mounted so a caller can add `.expect(n)`.
pub fn blob_head_mock(repo: &str, digest: &Digest, status: u16) -> Mock {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(status).insert_header("content-length", "100"))
}

/// Mount a blob HEAD returning 200.
pub async fn mount_blob_exists(server: &MockServer, repo: &str, digest: &Digest) {
    blob_head_mock(repo, digest, 200).mount(server).await;
}

/// Mount a blob upload: POST initiate, PATCH chunk, PUT finalize.
pub async fn mount_blob_push(server: &MockServer, repo: &str) {
    mount_uploads(server, repo, None).await;
}

/// As [`mount_blob_push`], but the POST and the PUT must each be hit exactly
/// `uploads` times (`0` asserts nothing is uploaded) and the PATCH never.
/// `row` names the table row in a failed expectation.
pub async fn mount_blob_push_expecting(server: &MockServer, repo: &str, uploads: u64, row: &str) {
    mount_uploads(server, repo, Some((uploads, row))).await;
}

async fn mount_uploads(server: &MockServer, repo: &str, expected: Option<(u64, &str)>) {
    let session = format!("/v2/{repo}/blobs/uploads/upload-id");
    let counted = |mock: Mock, what: &str| match expected {
        Some((n, row)) => mock.expect(n).named(format!("{row}: upload {what}")),
        None => mock,
    };
    counted(
        Mock::given(method("POST"))
            .and(path(format!("/v2/{repo}/blobs/uploads/")))
            .respond_with(ResponseTemplate::new(202).insert_header("location", session.as_str())),
        "POST",
    )
    .mount(server)
    .await;
    let patch = Mock::given(method("PATCH"))
        .and(path(session.as_str()))
        .respond_with(ResponseTemplate::new(202).insert_header("location", session.as_str()));
    match expected {
        Some((_, row)) => patch.expect(0).named(format!("{row}: upload PATCH")),
        None => patch,
    }
    .mount(server)
    .await;
    counted(
        Mock::given(method("PUT"))
            .and(path(session.as_str()))
            .respond_with(ResponseTemplate::new(201)),
        "PUT",
    )
    .mount(server)
    .await;
}

/// A cross-repo mount POST of `digest` into `repo` answering `status`, not yet
/// mounted so a caller can add `.expect(n)`. Priority 1 so it wins over the
/// generic upload POST (default priority 5), which also matches mount requests.
pub fn blob_mount_mock(repo: &str, digest: &Digest, status: u16) -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/v2/{repo}/blobs/uploads/")))
        .and(query_param("mount", digest.to_string()))
        .respond_with(ResponseTemplate::new(status))
        .with_priority(1)
}

/// Mount a 201 for a cross-repo mount of `digest` into `repo`.
pub async fn mount_blob_mount_accepted(server: &MockServer, repo: &str, digest: &Digest) {
    blob_mount_mock(repo, digest, 201).mount(server).await;
}

/// A 200 manifest response naming `digest`, as a HEAD or GET on it would answer.
pub fn digest_response(digest: &Digest, media_type: MediaType) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("docker-content-digest", digest.to_string())
        .insert_header("content-type", media_type.as_str())
        .insert_header("content-length", "100")
}

/// A manifest HEAD on `reference` answering `response`, not yet mounted so a
/// caller can add `.expect(n)`.
pub fn manifest_head_mock(repo: &str, reference: &str, response: ResponseTemplate) -> Mock {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(response)
}

/// Mount a manifest HEAD returning 404.
pub async fn mount_manifest_head_not_found(server: &MockServer, repo: &str, tag: &str) {
    manifest_head_mock(repo, tag, ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Mount a manifest HEAD returning `digest`.
pub async fn mount_manifest_head_matching(
    server: &MockServer,
    repo: &str,
    tag: &str,
    digest: &Digest,
) {
    manifest_head_mock(repo, tag, digest_response(digest, MediaType::OciManifest))
        .mount(server)
        .await;
}

/// A manifest PUT returning 201, not yet mounted so a caller can add `.expect(n)`.
pub fn manifest_push_mock(repo: &str, reference: &str) -> Mock {
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(201))
}

/// Mount a manifest PUT returning 201.
pub async fn mount_manifest_push(server: &MockServer, repo: &str, reference: &str) {
    manifest_push_mock(repo, reference).mount(server).await;
}

/// Mount a fresh target for blobs given only as digests: manifest HEAD 404,
/// blob HEAD 404s, push endpoints.
pub async fn mount_target_fresh(
    server: &MockServer,
    repo: &str,
    tag: &str,
    blob_digests: &[&Digest],
) {
    mount_manifest_head_not_found(server, repo, tag).await;
    for digest in blob_digests {
        mount_blob_not_found(server, repo, digest).await;
    }
    mount_blob_push(server, repo).await;
    mount_manifest_push(server, repo, tag).await;
}

/// Which request paths [`count_requests`] counts.
#[derive(Clone, Copy)]
pub enum PathMatch<'a> {
    /// Every path.
    Any,
    /// The path equal to this string.
    Exact(&'a str),
    /// Paths containing this substring.
    Contains(&'a str),
}

impl PathMatch<'_> {
    fn matches(self, request_path: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(p) => request_path == p,
            Self::Contains(needle) => request_path.contains(needle),
        }
    }
}

/// Requests `server` received, of `verb` when given, whose path satisfies `path`.
pub async fn count_requests(
    server: &MockServer,
    verb: Option<&Method>,
    path: PathMatch<'_>,
) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| verb.is_none_or(|v| r.method == *v) && path.matches(r.url.path()))
        .count()
}

/// Cross-repo mount POSTs (blob upload initiations carrying a `mount` query)
/// that `server` received.
pub async fn count_mount_posts(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.method == Method::POST
                && r.url.path().ends_with("/blobs/uploads/")
                && r.url.query_pairs().any(|(k, _)| k == "mount")
        })
        .count()
}

/// Blob upload finalizations (PUT) of `digest` that `server` received on a
/// path satisfying `path`. The query carries the digest URL-encoded, so it is
/// matched on its hex.
pub async fn count_blob_uploads(
    server: &MockServer,
    path: PathMatch<'_>,
    digest: &Digest,
) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.method == Method::PUT
                && path.matches(r.url.path())
                && r.url.query().is_some_and(|q| q.contains(digest.hex()))
        })
        .count()
}

/// How many times a checker was asked, shared with the test.
#[derive(Clone, Default)]
pub struct Calls {
    /// Calls to `check_blob_existence`.
    pub checks: Rc<Cell<usize>>,
    /// Calls to `wait_for_blobs_available`, made only by the manifest-commit
    /// gate and counted only by a [`WaitTrackingChecker`].
    pub waits: Rc<Cell<usize>>,
}

/// What a [`MockChecker`] does when asked.
enum Mode {
    /// Answers the call.
    Answers {
        /// Digests reported present.
        existing: HashSet<Digest>,
        /// Digests a partial failure leaves out of the answer (neither present nor missing).
        unknown: HashSet<Digest>,
    },
    /// Fails every `check_blob_existence` call.
    Fails,
}

/// Mock batch checker. It asserts the engine passes the target repo (a wrong
/// repo would otherwise go unnoticed) and counts its calls.
pub struct MockChecker {
    expected_repo: String,
    mode: Mode,
    calls: Calls,
}

impl MockChecker {
    /// A checker reporting `existing` present and every other digest missing.
    pub fn new(expected_repo: &str, existing: HashSet<Digest>) -> (Self, Calls) {
        let calls = Calls::default();
        (
            Self {
                expected_repo: expected_repo.to_owned(),
                mode: Mode::Answers {
                    existing,
                    unknown: HashSet::new(),
                },
                calls: calls.clone(),
            },
            calls,
        )
    }

    /// Leave `unknown` out of the answer. Panics on a failing checker, which
    /// has no answer to leave them out of.
    pub fn unknown(mut self, unknown: HashSet<Digest>) -> Self {
        match &mut self.mode {
            Mode::Answers {
                unknown: left_out, ..
            } => *left_out = unknown,
            Mode::Fails => panic!("a failing checker has no answer to leave digests out of"),
        }
        self
    }

    /// Fail every `check_blob_existence` call.
    pub fn failing(self) -> Self {
        Self {
            mode: Mode::Fails,
            ..self
        }
    }

    fn assert_repo(&self, repo: &RepositoryName) {
        assert_eq!(
            repo.as_str(),
            self.expected_repo,
            "mock: batch checker called with wrong repo"
        );
    }
}

impl BatchBlobChecker for MockChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<BlobExistence, ocync_distribution::Error>> + 'a>> {
        self.assert_repo(repo);
        Box::pin(async move {
            self.calls.checks.set(self.calls.checks.get() + 1);
            let Mode::Answers { existing, unknown } = &self.mode else {
                return Err(ocync_distribution::Error::EcrApi {
                    reason: "batch API unavailable".into(),
                });
            };
            let checked: Vec<Digest> = digests
                .iter()
                .filter(|d| !unknown.contains(d))
                .cloned()
                .collect();
            Ok(BlobExistence::new(&checked, existing))
        })
    }
}

/// A [`MockChecker`] that also overrides `wait_for_blobs_available` to count
/// the manifest-commit gate's calls apart from the pre-discovery
/// `check_blob_existence` calls. Every other checker keeps the trait default.
pub struct WaitTrackingChecker(MockChecker);

impl WaitTrackingChecker {
    /// A checker reporting `existing` present and every other digest missing.
    pub fn new(expected_repo: &str, existing: HashSet<Digest>) -> (Self, Calls) {
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
        self.0.assert_repo(repo);
        let Mode::Answers { existing, .. } = &self.0.mode else {
            unreachable!("a wait-tracking checker always answers");
        };
        let all_known = digests.iter().all(|d| existing.contains(d));
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

/// A target entry on `client` whose existence checks go through `checker`.
pub fn batch_target(
    name: &str,
    client: Arc<ocync_distribution::RegistryClient>,
    checker: impl BatchBlobChecker + 'static,
) -> TargetEntry {
    TargetEntry {
        batch_checker: Some(Rc::new(checker)),
        ..target_entry(name, client)
    }
}
