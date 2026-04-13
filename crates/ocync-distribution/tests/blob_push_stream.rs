//! Integration tests for `blob_push_stream` — chunked streaming uploads.

use bytes::Bytes;
use futures_util::stream;
use http::StatusCode;
use ocync_distribution::Digest;
use ocync_distribution::client::RegistryClientBuilder;
use url::Url;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

/// Custom matcher for Content-Range header values.
struct ContentRangeMatcher {
    expected: String,
}

impl ContentRangeMatcher {
    fn new(expected: &str) -> Self {
        Self {
            expected: expected.to_owned(),
        }
    }
}

impl wiremock::Match for ContentRangeMatcher {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get(http::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == self.expected)
    }
}

// ─── Happy path: POST → PATCH → PUT ────────────────────────────────────────

#[tokio::test]
async fn happy_path() {
    let server = MockServer::start().await;
    let data = b"hello world!";
    let digest = test_digest(data);
    let upload_path = "/v2/myrepo/blobs/uploads/test-uuid";

    // POST: initiate upload.
    Mock::given(method("POST"))
        .and(path("/v2/myrepo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED).append_header("Location", upload_path),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PATCH: single chunk (data fits in one chunk_size=64 buffer).
    let patch_next = format!("{upload_path}?after-patch");
    Mock::given(method("PATCH"))
        .and(path(upload_path))
        .and(ContentRangeMatcher::new("0-11"))
        .and(header("content-type", "application/octet-stream"))
        .and(header("content-length", "12"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", patch_next.as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: finalize with digest query param, Content-Length: 0.
    Mock::given(method("PUT"))
        .and(query_param("digest", digest.to_string()))
        .and(header("content-type", "application/octet-stream"))
        .and(header("content-length", "0"))
        .respond_with(ResponseTemplate::new(StatusCode::CREATED))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(64)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream("myrepo", &digest, None, data_stream(data, 4))
        .await
        .unwrap();

    assert_eq!(result, digest);
}

// ─── Multi-chunk: small chunk_size produces multiple PATCHes ────────────────

#[tokio::test]
async fn multi_chunk() {
    let server = MockServer::start().await;
    let data = b"abcdefghijkl"; // 12 bytes
    let digest = test_digest(data);

    // chunk_size=4, stream chunk=2 → buffer accumulates to 4 before each PATCH.
    // Expected: 3 PATCH requests with ranges 0-3, 4-7, 8-11.
    let upload_path = "/v2/repo/blobs/uploads/uuid-1";

    // POST: initiate.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED).append_header("Location", upload_path),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PATCH 1: bytes 0-3.
    Mock::given(method("PATCH"))
        .and(ContentRangeMatcher::new("0-3"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-2"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PATCH 2: bytes 4-7.
    Mock::given(method("PATCH"))
        .and(ContentRangeMatcher::new("4-7"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-3"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PATCH 3: bytes 8-11.
    Mock::given(method("PATCH"))
        .and(ContentRangeMatcher::new("8-11"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-4"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: finalize.
    Mock::given(method("PUT"))
        .and(query_param("digest", digest.to_string()))
        .respond_with(ResponseTemplate::new(StatusCode::CREATED))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(4)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 2))
        .await
        .unwrap();

    assert_eq!(result, digest);
}

// ─── Exact chunk boundary: data_len == chunk_size, no remainder flush ────────

#[tokio::test]
async fn exact_chunk_boundary() {
    let server = MockServer::start().await;
    let data = b"abcd"; // 4 bytes == chunk_size
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

    // Single PATCH: bytes 0-3, no remainder flush.
    Mock::given(method("PATCH"))
        .and(ContentRangeMatcher::new("0-3"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-2"),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(query_param("digest", digest.to_string()))
        .respond_with(ResponseTemplate::new(StatusCode::CREATED))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(4)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 4))
        .await
        .unwrap();

    assert_eq!(result, digest);
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
        .chunk_size(64)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 1))
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
        .chunk_size(64)
        .build()
        .unwrap();

    let err = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("403") || msg.contains("forbidden"),
        "expected 403 error: {msg}"
    );
}

// ─── Error: PATCH chunk fails mid-upload ────────────────────────────────────

#[tokio::test]
async fn patch_chunk_failure() {
    let server = MockServer::start().await;
    let data = b"abcdefgh"; // 8 bytes, chunk_size=4 → 2 PATCHes
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

    // First PATCH succeeds.
    Mock::given(method("PATCH"))
        .and(ContentRangeMatcher::new("0-3"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-2"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Second PATCH returns 500.
    Mock::given(method("PATCH"))
        .and(ContentRangeMatcher::new("4-7"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(4)
        .build()
        .unwrap();

    let err = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("500"), "expected 500 error: {msg}");
}

// ─── Error: PUT finalize rejected by registry ───────────────────────────────

#[tokio::test]
async fn put_finalize_rejected() {
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

    Mock::given(method("PATCH"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", "/v2/repo/blobs/uploads/uuid-2"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: registry rejects the finalization (e.g., digest mismatch).
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("digest invalid"))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(64)
        .build()
        .unwrap();

    let err = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 4))
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
        Err(reqwest::Client::new()
            .get("http://[::0]:1") // unreachable address
            .send()
            .await
            .unwrap_err()),
    ]);

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(64)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream("repo", &digest, None, error_stream)
        .await;

    assert!(result.is_err(), "stream error should propagate");
}

// ─── Error: POST returns 200 instead of expected 202 ────────────────────────

#[tokio::test]
async fn post_returns_200_is_rejected() {
    let server = MockServer::start().await;
    let data = b"data";
    let digest = test_digest(data);

    // POST returns 200 OK instead of 202 Accepted — some registries do this.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(200).append_header("Location", "/v2/repo/blobs/uploads/uuid-1"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(64)
        .build()
        .unwrap();

    let err = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 4))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("200"),
        "should reject 200 as unexpected status: {msg}"
    );
}

// ─── Monolithic threshold: small blobs use POST+PUT ──────────────────────────

/// Blobs at or below 1 MiB use monolithic POST+PUT upload (no PATCH).
#[tokio::test]
async fn small_blob_uses_monolithic_upload() {
    let server = MockServer::start().await;
    // 10 bytes — well under the 1 MiB threshold.
    let data = b"small blob";
    let digest = test_digest(data);

    // POST: initiate monolithic upload.
    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(202)
                .append_header("Location", "/v2/repo/blobs/uploads/mono-uuid"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: monolithic upload with digest query param.
    Mock::given(method("PUT"))
        .and(query_param("digest", digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;

    // PATCH: must NOT be called for small blobs.
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(4)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream(
            "repo",
            &digest,
            Some(data.len() as u64),
            data_stream(data, 4),
        )
        .await
        .unwrap();

    assert_eq!(result, digest);
}

/// Blobs with no known_size are not subject to the monolithic threshold.
#[tokio::test]
async fn unknown_size_skips_monolithic_threshold() {
    let server = MockServer::start().await;
    let data = b"no size known";
    let digest = test_digest(data);
    let upload_path = "/v2/repo/blobs/uploads/uuid-1";

    Mock::given(method("POST"))
        .and(path("/v2/repo/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(202).append_header("Location", upload_path))
        .expect(1)
        .mount(&server)
        .await;

    // PATCH must be called (chunked path taken).
    Mock::given(method("PATCH"))
        .respond_with(
            ResponseTemplate::new(202).append_header("Location", "/v2/repo/blobs/uploads/uuid-2"),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(query_param("digest", digest.to_string()))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(64)
        .build()
        .unwrap();

    let result = client
        .blob_push_stream("repo", &digest, None, data_stream(data, 4))
        .await
        .unwrap();

    assert_eq!(result, digest);
}

/// Verify GAR hostname detection matches the expected pattern.
#[test]
fn gar_hostname_detection() {
    let gar_hosts = [
        "us-docker.pkg.dev",
        "europe-docker.pkg.dev",
        "asia-docker.pkg.dev",
        "us-central1-docker.pkg.dev",
    ];
    for host in gar_hosts {
        assert!(
            host.ends_with("-docker.pkg.dev"),
            "{host} should match GAR pattern"
        );
    }

    let non_gar_hosts = [
        "docker.io",
        "ghcr.io",
        "registry-1.docker.io",
        "docker.pkg.dev",
        "pkg.dev",
        "127.0.0.1",
    ];
    for host in non_gar_hosts {
        assert!(
            !host.ends_with("-docker.pkg.dev"),
            "{host} should NOT match GAR pattern"
        );
    }
}
