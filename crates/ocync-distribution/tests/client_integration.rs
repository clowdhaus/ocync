//! Integration tests for `RegistryClient` using mock HTTP servers.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};

use http::StatusCode;
use ocync_distribution::Error;
use ocync_distribution::RepositoryName;
use ocync_distribution::auth::{AuthProvider, Scope, Token};
use ocync_distribution::client::RegistryClientBuilder;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A mock auth provider that tracks token requests and invalidation calls.
struct MockAuth {
    token_value: String,
    invalidate_count: AtomicU32,
}

impl MockAuth {
    fn new(token: &str) -> Self {
        Self {
            token_value: token.to_owned(),
            invalidate_count: AtomicU32::new(0),
        }
    }
}

impl AuthProvider for MockAuth {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn get_token(
        &self,
        _scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        let token = Token::new(self.token_value.clone());
        Box::pin(async move { Ok(token) })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.invalidate_count.fetch_add(1, Ordering::SeqCst);
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

    let auth = MockAuth::new("good-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("library/nginx");
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

    let auth = MockAuth::new("test-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("library/nginx");
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
async fn get_double_401_returns_unauthorized() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let auth = MockAuth::new("bad-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo");
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
}

#[tokio::test]
async fn get_401_retry_only_happens_once() {
    let server = MockServer::start().await;

    // Always returns 401 — client should try exactly twice (initial + 1 retry)
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

    let repo = RepositoryName::new("repo");
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

    let auth = MockAuth::new("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("private/repo");
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

    let auth = MockAuth::new("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo");
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

    let auth = MockAuth::new("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo");
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

    let repo = RepositoryName::new("repo");
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

    let auth = MockAuth::new("token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo");
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
}

#[tokio::test]
async fn head_double_401_returns_unauthorized() {
    let server = MockServer::start().await;

    Mock::given(method("HEAD"))
        .and(path("/v2/repo/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let auth = MockAuth::new("bad-token");
    let client = RegistryClientBuilder::new(mock_base_url(&server))
        .auth(auth)
        .build()
        .unwrap();

    let repo = RepositoryName::new("repo");
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
}
