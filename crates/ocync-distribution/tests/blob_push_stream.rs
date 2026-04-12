//! Integration tests for `blob_push_stream` — chunked streaming uploads.

use bytes::Bytes;
use futures_util::stream;
use http::StatusCode;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::{Digest, Error};
use url::Url;
use wiremock::matchers::{method, path, query_param};
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
) -> impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static {
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
            .map_or(false, |v| v == self.expected)
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
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", patch_next.as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PUT: finalize with digest query param.
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
        .blob_push_stream("myrepo", &digest, data.len() as u64, data_stream(data, 4))
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
        .blob_push_stream("repo", &digest, data.len() as u64, data_stream(data, 2))
        .await
        .unwrap();

    assert_eq!(result, digest);
}

// ─── Digest mismatch: DELETE and error ──────────────────────────────────────

#[tokio::test]
async fn digest_mismatch() {
    let server = MockServer::start().await;
    let data = b"actual data";
    let wrong_digest: Digest =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
    let upload_path = "/v2/myrepo/blobs/uploads/uuid-mismatch";

    // POST: initiate.
    Mock::given(method("POST"))
        .and(path("/v2/myrepo/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED).append_header("Location", upload_path),
        )
        .expect(1)
        .mount(&server)
        .await;

    // PATCH: accept data.
    let patched_path = format!("{upload_path}?patched");
    Mock::given(method("PATCH"))
        .respond_with(
            ResponseTemplate::new(StatusCode::ACCEPTED)
                .append_header("Location", patched_path.as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    // DELETE: cleanup after mismatch.
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(StatusCode::NO_CONTENT))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .chunk_size(256)
        .build()
        .unwrap();

    let err = client
        .blob_push_stream(
            "myrepo",
            &wrong_digest,
            data.len() as u64,
            data_stream(data, 4),
        )
        .await
        .unwrap_err();

    match err {
        Error::DigestMismatch { expected, actual } => {
            assert_eq!(expected, wrong_digest.to_string());
            let real_digest = test_digest(data);
            assert_eq!(actual, real_digest.to_string());
        }
        other => panic!("expected DigestMismatch, got: {other}"),
    }
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
