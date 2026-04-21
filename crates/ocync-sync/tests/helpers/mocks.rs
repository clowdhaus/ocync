//! Mock HTTP endpoint mounting functions for wiremock-based integration tests.

#![allow(dead_code, unused_imports, unreachable_pub)]

use ocync_distribution::Digest;
use ocync_distribution::spec::MediaType;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mount mock endpoints for a full image sync on the source server:
/// - GET manifest (returns the given JSON bytes with media type header)
pub async fn mount_source_manifest(server: &MockServer, repo: &str, tag: &str, bytes: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.to_vec())
                .insert_header("content-type", MediaType::OciManifest.as_str()),
        )
        .mount(server)
        .await;
}

/// Mount source manifest GET with an explicit content-type (for index manifests).
pub async fn mount_source_manifest_with_content_type(
    server: &MockServer,
    repo: &str,
    reference: &str,
    bytes: &[u8],
    content_type: MediaType,
) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.to_vec())
                .insert_header("content-type", content_type.as_str()),
        )
        .mount(server)
        .await;
}

/// Mount referrers API GET response for a parent digest.
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

/// Mount mock for blob pull (GET).
pub async fn mount_blob_pull(server: &MockServer, repo: &str, digest: &Digest, data: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(data.to_vec())
                .insert_header("content-length", data.len().to_string()),
        )
        .mount(server)
        .await;
}

/// Mount mock for blob HEAD (exists check) returning 404 (not found).
pub async fn mount_blob_not_found(server: &MockServer, repo: &str, digest: &Digest) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Mount mock for blob HEAD (exists check) returning 200 (exists).
pub async fn mount_blob_exists(server: &MockServer, repo: &str, digest: &Digest) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "100"))
        .mount(server)
        .await;
}

/// Mount mock for blob push: POST initiate + PATCH chunked data + PUT finalize.
pub async fn mount_blob_push(server: &MockServer, repo: &str) {
    // POST: initiate upload.
    Mock::given(method("POST"))
        .and(path(format!("/v2/{repo}/blobs/uploads/")))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", format!("/v2/{repo}/blobs/uploads/upload-id")),
        )
        .mount(server)
        .await;

    // PATCH: accept chunked data (may be called 1+ times).
    Mock::given(method("PATCH"))
        .and(path(format!("/v2/{repo}/blobs/uploads/upload-id")))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", format!("/v2/{repo}/blobs/uploads/upload-id")),
        )
        .mount(server)
        .await;

    // PUT: finalize with digest query param.
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{repo}/blobs/uploads/upload-id")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
}

/// Mount mock for manifest HEAD returning 404.
pub async fn mount_manifest_head_not_found(server: &MockServer, repo: &str, tag: &str) {
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Mount mock for manifest HEAD returning matching digest.
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

/// Mount mock for manifest push (PUT) returning 201.
pub async fn mount_manifest_push(server: &MockServer, repo: &str, reference: &str) {
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{repo}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
}

/// Mount a complete "fresh target" mock set: manifest HEAD 404, all blobs 404, push endpoints.
///
/// Use when you need custom source mocks but the target is standard (everything missing).
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
