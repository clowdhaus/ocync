//! Functions that mount wiremock endpoints, for tests where the mock topology is the subject.

#![allow(dead_code, unused_imports, unreachable_pub)]

use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::MediaType;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// Mount a source manifest GET with an explicit content type (for indexes).
pub async fn mount_source_manifest_with_content_type(
    server: &MockServer,
    repo: &str,
    reference: &str,
    bytes: &[u8],
    content_type: MediaType,
) {
    source_manifest_mock(repo, reference, bytes, content_type)
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

/// A blob GET returning `data`, not yet mounted so a caller can add `.expect(n)`.
pub fn blob_pull_mock(repo: &str, digest: &Digest, data: &[u8]) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(blob_pull_response(data))
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
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
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
/// `uploads` times (`0` asserts nothing is uploaded); PATCH is unconstrained.
pub async fn mount_blob_push_expecting(server: &MockServer, repo: &str, uploads: u64) {
    mount_uploads(server, repo, Some(uploads)).await;
}

async fn mount_uploads(server: &MockServer, repo: &str, uploads: Option<u64>) {
    let session = format!("/v2/{repo}/blobs/uploads/upload-id");
    let counted = |mock: Mock| match uploads {
        Some(n) => mock.expect(n),
        None => mock,
    };
    counted(
        Mock::given(method("POST"))
            .and(path(format!("/v2/{repo}/blobs/uploads/")))
            .respond_with(ResponseTemplate::new(202).insert_header("location", session.as_str())),
    )
    .mount(server)
    .await;
    Mock::given(method("PATCH"))
        .and(path(session.as_str()))
        .respond_with(ResponseTemplate::new(202).insert_header("location", session.as_str()))
        .mount(server)
        .await;
    counted(
        Mock::given(method("PUT"))
            .and(path(session.as_str()))
            .respond_with(ResponseTemplate::new(201)),
    )
    .mount(server)
    .await;
}

/// Mount a 201 for a cross-repo mount of `digest` into `repo`. Priority 1 so
/// it wins over the generic upload POST (default priority 5), which also
/// matches mount requests.
pub async fn mount_blob_mount_accepted(server: &MockServer, repo: &str, digest: &Digest) {
    Mock::given(method("POST"))
        .and(path(format!("/v2/{repo}/blobs/uploads/")))
        .and(query_param("mount", digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(1)
        .mount(server)
        .await;
}

/// Mount a manifest HEAD returning 404.
pub async fn mount_manifest_head_not_found(server: &MockServer, repo: &str, tag: &str) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(ResponseTemplate::new(404))
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
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", digest.to_string())
                .insert_header("content-type", MediaType::OciManifest.as_str())
                .insert_header("content-length", "100"),
        )
        .mount(server)
        .await;
}

/// Mount a manifest PUT returning 201.
pub async fn mount_manifest_push(server: &MockServer, repo: &str, reference: &str) {
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
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

/// Requests `server` received with `verb`, on exactly `only_path` when given.
pub async fn count_requests(server: &MockServer, verb: &str, only_path: Option<&str>) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == verb && only_path.is_none_or(|p| r.url.path() == p))
        .count()
}
