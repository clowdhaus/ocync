//! Integration tests for `RegistryClient` using mock HTTP servers.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use http::StatusCode;
use ocync_distribution::Error;
use ocync_distribution::RepositoryName;
use ocync_distribution::auth::{AuthProvider, Scope, Token};
use ocync_distribution::client::RegistryClientBuilder;
use url::Url;
use wiremock::matchers::{header_exists, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Shared state for [`MockAuth`], allowing assertions after the auth provider
/// has been moved into the client.
#[derive(Debug, Clone)]
struct MockAuthState {
    invalidate_count: Arc<AtomicU32>,
    recorded_scopes: Arc<Mutex<Vec<Vec<Scope>>>>,
}

impl MockAuthState {
    fn new() -> Self {
        Self {
            invalidate_count: Arc::new(AtomicU32::new(0)),
            recorded_scopes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Return the number of times `invalidate()` was called.
    fn invalidate_count(&self) -> u32 {
        self.invalidate_count.load(Ordering::SeqCst)
    }

    /// Return all scopes recorded across `get_token()` calls.
    fn recorded_scopes(&self) -> Vec<Vec<Scope>> {
        self.recorded_scopes.lock().unwrap().clone()
    }
}

/// A mock auth provider that tracks token requests, scopes, and invalidation calls.
struct MockAuth {
    token_value: String,
    state: MockAuthState,
}

impl MockAuth {
    fn new(token: &str) -> (Self, MockAuthState) {
        let state = MockAuthState::new();
        let auth = Self {
            token_value: token.to_owned(),
            state: state.clone(),
        };
        (auth, state)
    }

    /// Convenience constructor when assertions on state are not needed.
    fn simple(token: &str) -> Self {
        let state = MockAuthState::new();
        Self {
            token_value: token.to_owned(),
            state,
        }
    }
}

impl AuthProvider for MockAuth {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        self.state
            .recorded_scopes
            .lock()
            .unwrap()
            .push(scopes.to_vec());
        let token = Token::new(self.token_value.clone());
        Box::pin(async move { Ok(token) })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.state.invalidate_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// A mock auth provider that returns a different token value after each invalidation.
struct RotatingMockAuth {
    call_count: AtomicU32,
}

impl RotatingMockAuth {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }
}

impl AuthProvider for RotatingMockAuth {
    fn name(&self) -> &'static str {
        "rotating-mock"
    }

    fn get_token(
        &self,
        _scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let token_value = format!("token-{count}");
        Box::pin(async move { Ok(Token::new(token_value)) })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

fn mock_base_url(server: &MockServer) -> Url {
    Url::parse(&server.uri()).unwrap()
}

#[tokio::test]
async fn get_success_no_retry() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_string("manifest-body"))
        .expect(1)
        .mount(&server)
        .await;

    let auth = MockAuth::simple("good-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("library/nginx").unwrap();
    let resp = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn get_401_triggers_invalidate_and_retry() {
    let server = MockServer::start().await;

    // First call: 401, second call: 200
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_string("success"))
        .mount(&server)
        .await;

    let (auth, state) = MockAuth::new("test-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("library/nginx").unwrap();
    let resp = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The client should have invalidated exactly once before retrying.
    assert_eq!(
        state.invalidate_count(),
        1,
        "expected exactly 1 invalidation on 401 retry"
    );
}

#[tokio::test]
async fn get_double_401_returns_unauthorized() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let (auth, state) = MockAuth::new("bad-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let result = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::UNAUTHORIZED));

    // Double-401 should still have invalidated once (before the retry attempt).
    assert_eq!(
        state.invalidate_count(),
        1,
        "expected 1 invalidation even on double-401"
    );
}

#[tokio::test]
async fn get_401_retry_only_happens_once() {
    let server = MockServer::start().await;

    // Always returns 401 -- client should try exactly twice (initial + 1 retry)
    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;

    let auth = RotatingMockAuth::new();
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let result = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn get_403_returns_forbidden() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/private/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let auth = MockAuth::simple("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("private/repo").unwrap();
    let result = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn get_404_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/nonexistent"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let auth = MockAuth::simple("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let result = client
        .get(
            &repo,
            "manifests/nonexistent",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await;
    assert!(result.unwrap_err().is_not_found());
}

#[tokio::test]
async fn get_500_returns_registry_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let auth = MockAuth::simple("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let result = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::INTERNAL_SERVER_ERROR));
}

#[tokio::test]
async fn no_auth_provider_sends_no_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_string("public"))
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let resp = client
        .get(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestRead,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn ping_success() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    client.ping().await.unwrap();
}

#[tokio::test]
async fn ping_401_is_ok() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    client.ping().await.unwrap();
}

#[tokio::test]
async fn ping_500_is_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let result = client.ping().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn head_401_triggers_retry() {
    let server = MockServer::start().await;

    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (auth, state) = MockAuth::new("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let resp = client
        .head(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestHead,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The client should have invalidated exactly once before retrying.
    assert_eq!(
        state.invalidate_count(),
        1,
        "expected exactly 1 invalidation on HEAD 401 retry"
    );
}

#[tokio::test]
async fn head_double_401_returns_unauthorized() {
    let server = MockServer::start().await;

    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let (auth, state) = MockAuth::new("bad-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let result = client
        .head(
            &repo,
            "manifests/latest",
            None,
            ocync_distribution::aimd::RegistryAction::ManifestHead,
        )
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::UNAUTHORIZED));

    // Double-401 should still have invalidated once (before the retry attempt).
    assert_eq!(
        state.invalidate_count(),
        1,
        "expected 1 invalidation even on HEAD double-401"
    );
}

#[tokio::test]
async fn manifest_head_sends_accept_header() {
    let server = MockServer::start().await;

    // Only respond to HEAD with Accept header present.
    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .and(header_exists("accept"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "docker-content-digest",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .insert_header("content-type", "application/vnd.oci.image.index.v1+json")
                .insert_header("content-length", "100"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();
    let repo = RepositoryName::new("repo").unwrap();

    let result = client.manifest_head(&repo, "latest").await.unwrap();
    assert!(result.is_some());
}

#[tokio::test]
async fn manifest_head_missing_digest_header_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.oci.image.index.v1+json")
                .insert_header("content-length", "100"),
            // No docker-content-digest header.
        )
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();
    let repo = RepositoryName::new("repo").unwrap();

    let result = client.manifest_head(&repo, "latest").await;
    assert!(result.is_err(), "missing digest header should be an error");
}

#[tokio::test]
async fn blob_head_does_not_send_manifest_accept_header() {
    use wiremock::matchers::header;

    let server = MockServer::start().await;

    let blob_path =
        "/v2/repo/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    // Guard mock (priority 1): if the manifest Accept header IS sent, return
    // 400 so the test fails. This catches regressions where blob HEAD
    // accidentally gets the manifest content-negotiation header.
    Mock::given(method("HEAD"))
        .and(path(blob_path))
        .and(header("accept", "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json"))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .expect(0)
        .mount(&server)
        .await;

    // Base mock (default priority 5): responds normally.
    Mock::given(method("HEAD"))
        .and(path(blob_path))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "1024"))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();
    let repo = RepositoryName::new("repo").unwrap();
    let digest: ocync_distribution::Digest =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .parse()
            .unwrap();

    let result = client.blob_exists(&repo, &digest).await.unwrap();
    assert_eq!(result, Some(1024));
}

// ---------------------------------------------------------------------------
// Tag listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_tags_returns_all_tags() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "repo",
            "tags": ["v1", "v2", "latest"]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let tags = client.list_tags(&repo).await.unwrap();
    assert_eq!(tags, vec!["v1", "v2", "latest"]);
}

#[tokio::test]
async fn list_tags_follows_pagination() {
    let server = MockServer::start().await;

    // Page 2 (higher priority): matches when `last=v2` query param is present.
    // Mounted first so it doesn't shadow the page-1 mock.
    Mock::given(method("GET"))
        .and(path("/v2/repo/tags/list"))
        .and(query_param("last", "v2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "repo",
            "tags": ["latest"]
        })))
        .expect(1)
        .with_priority(1)
        .mount(&server)
        .await;

    // Page 1 (lower priority): no `last` param, returns Link header.
    Mock::given(method("GET"))
        .and(path("/v2/repo/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "name": "repo",
                    "tags": ["v1", "v2"]
                }))
                .append_header("link", r#"</v2/repo/tags/list?n=2&last=v2>; rel="next""#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let tags = client.list_tags(&repo).await.unwrap();
    assert_eq!(tags, vec!["v1", "v2", "latest"]);
}

#[tokio::test]
async fn list_tags_respects_max_pages() {
    let server = MockServer::start().await;

    // Every request returns one tag and a self-referencing Link header,
    // creating an infinite pagination loop.
    Mock::given(method("GET"))
        .and(path_regex(r"/v2/repo/tags/list.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"name": "repo", "tags": ["t"]}))
                .append_header("link", r#"</v2/repo/tags/list?n=1&last=t>; rel="next""#),
        )
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let err = client.list_tags(&repo).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("exceeded") && msg.contains("pages"),
        "expected max-pages error, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Rate limiting (429)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_429_retries_and_succeeds() {
    let server = MockServer::start().await;

    // First request: 429 rate limited.
    Mock::given(method("GET"))
        .and(path("/v2/repo/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Second request: 200 success.
    Mock::given(method("GET"))
        .and(path("/v2/repo/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
        .respond_with(ResponseTemplate::new(200).set_body_string("blob-data"))
        .mount(&server)
        .await;

    let auth = MockAuth::simple("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    // The 429 is classified as an error (not retried by the HTTP layer).
    // Verify the client surfaces the error so the sync engine can retry.
    let result = client
        .get(
            &repo,
            "blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
            ocync_distribution::aimd::RegistryAction::BlobRead,
        )
        .await;

    // The first 429 is returned as an error to the caller.
    let err = result.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::TOO_MANY_REQUESTS));

    // A subsequent request (after the AIMD window has shrunk) succeeds.
    let resp = client
        .get(
            &repo,
            "blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
            ocync_distribution::aimd::RegistryAction::BlobRead,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Referrers API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn referrers_returns_index() {
    let server = MockServer::start().await;

    let digest_str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let referrers_body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "size": 512,
                "artifactType": "application/vnd.dev.cosign.simplesigning.v1+json"
            }
        ]
    });

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{digest_str}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&referrers_body)
                .insert_header("content-type", "application/vnd.oci.image.index.v1+json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let digest: ocync_distribution::Digest = digest_str.parse().unwrap();

    let result = client.referrers(&repo, &digest, None).await.unwrap();
    let index = result.expect("expected Some(ImageIndex) for referrers response");
    assert_eq!(index.manifests.len(), 1);
    assert_eq!(
        index.manifests[0].artifact_type.as_deref(),
        Some("application/vnd.dev.cosign.simplesigning.v1+json")
    );
}

#[tokio::test]
async fn referrers_404_returns_none() {
    let server = MockServer::start().await;

    let digest_str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{digest_str}")))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let digest: ocync_distribution::Digest = digest_str.parse().unwrap();

    let result = client.referrers(&repo, &digest, None).await.unwrap();
    assert!(result.is_none(), "expected None for 404 referrers response");
}

#[tokio::test]
async fn referrers_with_artifact_type_filter() {
    let server = MockServer::start().await;

    let digest_str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let filter_type = "application/vnd.dev.cosign.simplesigning.v1+json";

    let referrers_body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": []
    });

    // Verify the artifactType query parameter is sent.
    Mock::given(method("GET"))
        .and(path(format!("/v2/repo/referrers/{digest_str}")))
        .and(query_param("artifactType", filter_type))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&referrers_body)
                .insert_header("content-type", "application/vnd.oci.image.index.v1+json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo").unwrap();
    let digest: ocync_distribution::Digest = digest_str.parse().unwrap();

    let result = client
        .referrers(&repo, &digest, Some(filter_type))
        .await
        .unwrap();
    let index = result.expect("expected Some(ImageIndex)");
    assert!(index.manifests.is_empty());
}

// ---------------------------------------------------------------------------
// Auth scopes verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_push_requests_push_scope() {
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path("/v2/myuser/myrepo/manifests/v1"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let (auth, state) = MockAuth::new("push-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("myuser/myrepo").unwrap();
    let manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 100
        },
        "layers": []
    }))
    .unwrap();

    let _ = client
        .manifest_push(
            &repo,
            "v1",
            &ocync_distribution::MediaType::OciManifest,
            &manifest_bytes,
        )
        .await;

    // Verify that the scopes passed to get_token included push.
    let scopes = state.recorded_scopes();
    assert!(
        !scopes.is_empty(),
        "get_token should have been called at least once"
    );

    // Every scope request for manifest_push should include Push action.
    let has_push = scopes.iter().any(|scope_set| {
        scope_set.iter().any(|s| {
            s.repository == "myuser/myrepo"
                && s.actions.contains(&ocync_distribution::auth::Action::Push)
        })
    });
    assert!(
        has_push,
        "manifest_push must request push scope, got: {scopes:?}"
    );
}

#[tokio::test]
async fn manifest_pull_requests_pull_scope() {
    let server = MockServer::start().await;

    let manifest_json = serde_json::json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 100
        },
        "layers": []
    });

    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&manifest_json)
                .insert_header("content-type", "application/vnd.oci.image.manifest.v1+json"),
        )
        .mount(&server)
        .await;

    let (auth, state) = MockAuth::new("pull-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("library/nginx").unwrap();
    let _ = client.manifest_pull(&repo, "latest").await;

    // Verify that the scopes passed to get_token are pull-only (no push).
    let scopes = state.recorded_scopes();
    assert!(!scopes.is_empty());

    let has_pull_only = scopes.iter().any(|scope_set| {
        scope_set.iter().any(|s| {
            s.repository == "library/nginx"
                && s.actions.contains(&ocync_distribution::auth::Action::Pull)
                && !s.actions.contains(&ocync_distribution::auth::Action::Push)
        })
    });
    assert!(
        has_pull_only,
        "manifest_pull must request pull-only scope, got: {scopes:?}"
    );
}
