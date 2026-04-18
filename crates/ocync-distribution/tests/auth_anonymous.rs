//! Integration tests for anonymous auth token exchange using mock HTTP servers.

use std::sync::Arc;
use std::time::Duration;

use ocync_distribution::auth::anonymous::AnonymousAuth;
use ocync_distribution::auth::{AuthProvider, Scope};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Start a mock server that responds to `/v2/` with a 401 + WWW-Authenticate
/// challenge pointing to `{mock_server}/token` as the realm.
async fn setup_auth_server() -> MockServer {
    let server = MockServer::start().await;

    let challenge = format!(
        r#"Bearer realm="http://{}/token",service="test.registry.io""#,
        server.address()
    );
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", challenge))
        .mount(&server)
        .await;

    server
}

/// Mount a token endpoint that returns a token with given value and TTL.
async fn mount_token_endpoint(server: &MockServer, token_value: &str, expires_in: u64) {
    let body = serde_json::json!({
        "token": token_value,
        "expires_in": expires_in
    });
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(server)
        .await;
}

/// Mount a token endpoint that returns a specific token for a specific scope.
async fn mount_scoped_token_endpoint(
    server: &MockServer,
    scope: &str,
    token_value: &str,
    expires_in: u64,
) {
    let body = serde_json::json!({
        "token": token_value,
        "expires_in": expires_in
    });
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("scope", scope))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn token_exchange_basic_flow() {
    let server = setup_auth_server().await;
    mount_token_endpoint(&server, "test-token-123", 3600).await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let scopes = [Scope::pull("library/nginx")];
    let token = auth.get_token(&scopes).await.unwrap();

    assert_eq!(token.value(), "test-token-123");
    assert!(!token.is_expired());
}

#[tokio::test]
async fn cached_token_reused_for_same_scope() {
    let server = setup_auth_server().await;

    let body = serde_json::json!({ "token": "cached-token", "expires_in": 3600 });
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let scopes = [Scope::pull("library/nginx")];

    let t1 = auth.get_token(&scopes).await.unwrap();
    let t2 = auth.get_token(&scopes).await.unwrap();

    assert_eq!(t1.value(), t2.value());
}

#[tokio::test]
async fn different_scopes_get_different_tokens() {
    let server = setup_auth_server().await;

    mount_scoped_token_endpoint(
        &server,
        "repository:library/nginx:pull",
        "nginx-token",
        3600,
    )
    .await;
    mount_scoped_token_endpoint(
        &server,
        "repository:myuser/myrepo:pull,push",
        "myrepo-token",
        3600,
    )
    .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());

    let t1 = auth
        .get_token(&[Scope::pull("library/nginx")])
        .await
        .unwrap();
    let t2 = auth
        .get_token(&[Scope::pull_push("myuser/myrepo")])
        .await
        .unwrap();

    assert_eq!(t1.value(), "nginx-token");
    assert_eq!(t2.value(), "myrepo-token");
}

#[tokio::test]
async fn scope_upgrade_fetches_new_token() {
    let server = setup_auth_server().await;

    mount_scoped_token_endpoint(&server, "repository:myrepo:pull", "pull-token", 3600).await;
    mount_scoped_token_endpoint(&server, "repository:myrepo:pull,push", "push-token", 3600).await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());

    let t1 = auth.get_token(&[Scope::pull("myrepo")]).await.unwrap();
    let t2 = auth.get_token(&[Scope::pull_push("myrepo")]).await.unwrap();

    assert_eq!(t1.value(), "pull-token");
    assert_eq!(t2.value(), "push-token");
}

#[tokio::test]
async fn invalidate_clears_cache() {
    let server = setup_auth_server().await;

    let body = serde_json::json!({ "token": "fresh-token", "expires_in": 3600 });
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(2)
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let scopes = [Scope::pull("library/nginx")];

    let _ = auth.get_token(&scopes).await.unwrap();
    auth.invalidate().await;
    let _ = auth.get_token(&scopes).await.unwrap();
}

#[tokio::test]
async fn concurrent_requests_coalesce() {
    let server = setup_auth_server().await;

    let body = serde_json::json!({ "token": "coalesced", "expires_in": 3600 });
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&body)
                .set_delay(Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let auth = Arc::new(AnonymousAuth::with_base_url(
        server.uri(),
        ocync_distribution::test_http_client(),
    ));
    let scopes = vec![Scope::pull("library/nginx")];

    let mut handles = Vec::new();
    for _ in 0..20 {
        let auth = auth.clone();
        let scopes = scopes.clone();
        handles.push(tokio::spawn(async move {
            auth.get_token(&scopes).await.unwrap()
        }));
    }

    for handle in handles {
        let token = handle.await.unwrap();
        assert_eq!(token.value(), "coalesced");
    }
}

#[tokio::test]
async fn no_auth_required_returns_empty_token() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let token = auth.get_token(&[Scope::pull("repo")]).await.unwrap();

    assert_eq!(token.value(), "");
}

#[tokio::test]
async fn missing_www_authenticate_header_errors() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let result = auth.get_token(&[Scope::pull("repo")]).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("WWW-Authenticate"), "error was: {err}");
}

#[tokio::test]
async fn token_response_missing_both_fields_errors() {
    let server = setup_auth_server().await;

    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let result = auth.get_token(&[Scope::pull("repo")]).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn token_with_access_token_field() {
    let server = setup_auth_server().await;

    let body = serde_json::json!({ "access_token": "alt-token", "expires_in": 300 });
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let token = auth.get_token(&[Scope::pull("repo")]).await.unwrap();

    assert_eq!(token.value(), "alt-token");
}

#[tokio::test]
async fn token_endpoint_error_propagates() {
    let server = setup_auth_server().await;

    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let auth = AnonymousAuth::with_base_url(server.uri(), ocync_distribution::test_http_client());
    let result = auth.get_token(&[Scope::pull("repo")]).await;

    assert!(result.is_err());
}
