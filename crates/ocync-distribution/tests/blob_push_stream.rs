//! Integration tests for `blob_push_stream` -- streaming PUT uploads.

use bytes::Bytes;
use futures_util::stream;
use http::StatusCode;
use ocync_distribution::Digest;
use ocync_distribution::RepositoryName;
use ocync_distribution::client::RegistryClientBuilder;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Compute the SHA-256 digest for test data.
fn test_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// Build a stream of `Bytes` from raw data, split into the given chunk size.
fn data_stream(
    data: &[u8],
    chunk_size: usize,
) -> impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> {
    let chunks: Vec<Result<Bytes, reqwest::Error>> = data
        .chunks(chunk_size)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    stream::iter(chunks)
}

fn mock_base_url(server: &MockServer) -> Url {
    Url::parse(&server.uri()).unwrap()
}

// ─── Empty stream: zero-byte blob, POST + PUT only, no PATCH ────────────────

#[tokio::test]
async fn empty_stream() {
    let server = MockServer::start().await;
    let data = b"";
    let digest = test_digest(data);

    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-1"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PATCH must NOT be called for empty data.
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(query_param("digest", digest.to_string()))
        .respond_with(ResponseTemplate::new(StatusCode::CREATED))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let slot = client.reserve_upload_slot(None).await;
    let result = slot
        .push_stream(&repo, &digest, None, data_stream(data, 1))
        .await
        .unwrap();

    assert_eq!(result, digest);
}

// ─── Error: POST initiation returns 403 ─────────────────────────────────────

#[tokio::test]
async fn post_initiation_rejected() {
    let server = MockServer::start().await;
    let data = b"some data";
    let digest = test_digest(data);

    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let slot = client.reserve_upload_slot(None).await;
    let err = slot
        .push_stream(&repo, &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("403") || msg.contains("forbidden"),
        "expected 403 error: {msg}"
    );
}

// ─── Error: PUT upload fails ────────────────────────────────────────────────

#[tokio::test]
async fn put_upload_failure() {
    let server = MockServer::start().await;
    let data = b"abcdefgh"; // 8 bytes
    let digest = test_digest(data);
    let upload_path = "/v2/repo/blobs/uploads/uuid-1";

    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED).append_header("Location", upload_path),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: streaming upload returns 500.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let slot = client.reserve_upload_slot(None).await;
    let err = slot
        .push_stream(&repo, &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("500"), "expected 500 error: {msg}");
}

// ─── Error: PUT rejected by registry (e.g., digest mismatch) ───────────────

#[tokio::test]
async fn put_rejected() {
    let server = MockServer::start().await;
    let data = b"payload";
    let digest = test_digest(data);
    let upload_path = "/v2/repo/blobs/uploads/uuid-1";

    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED).append_header("Location", upload_path),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: registry rejects the upload (e.g., digest mismatch).
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("digest invalid"))
        .expect(1)
        .mount(&server)
        .await;

    // PATCH: must NOT be called.
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let slot = client.reserve_upload_slot(None).await;
    let err = slot
        .push_stream(&repo, &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("400"), "expected 400 error: {msg}");
}

// ─── Error: source stream yields an error ───────────────────────────────────

#[tokio::test]
async fn stream_error_propagates() {
    let server = MockServer::start().await;
    let data = b"test";
    let digest = test_digest(data);
    let upload_path = "/v2/repo/blobs/uploads/uuid-1";

    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED).append_header("Location", upload_path),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Build a stream that yields one chunk then an error.
    let error_stream = stream::iter(vec![
        Ok(Bytes::from_static(b"ok")),
        Err(ocync_distribution::test_http_client()
            .get("http://[::0]:1") // unreachable address
            .send()
            .await
            .unwrap_err()),
    ]);

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let slot = client.reserve_upload_slot(None).await;
    let result = slot.push_stream(&repo, &digest, None, error_stream).await;

    assert!(result.is_err(), "stream error should propagate");
}

// ─── Error: POST returns 200 instead of expected 202 ────────────────────────

#[tokio::test]
async fn post_returns_200_is_rejected() {
    let server = MockServer::start().await;
    let data = b"data";
    let digest = test_digest(data);

    // POST returns 200 OK instead of 202 Accepted -- some registries do this.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(200).append_header("Location", "/v2/repo/blobs/uploads/uuid-1"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let slot = client.reserve_upload_slot(None).await;
    let err = slot
        .push_stream(&repo, &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("200"),
        "should reject 200 as unexpected status: {msg}"
    );
}
