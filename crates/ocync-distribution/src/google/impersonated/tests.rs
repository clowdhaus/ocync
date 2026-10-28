//! Tests of the impersonated service account, ported from upstream's
//! `impersonated.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::tests::{Fixture, host};
use super::*;
use crate::aws::Platform;
use crate::google::REGISTRY_SCOPE;

/// The IAM path the tests impersonate at.
const IAM_PATH: &str =
    "/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken";

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// An impersonated account at `server` whose source is an authorized user
/// refreshing at `server`.
fn impersonated_file(server: &MockServer) -> Value {
    serde_json::json!({
        "type": "impersonated_service_account",
        "service_account_impersonation_url": format!("{}{IAM_PATH}", server.uri()),
        "source_credentials": {
            "type": "authorized_user",
            "client_id": "client",
            "client_secret": "secret",
            "refresh_token": "refresh",
            "token_uri": format!("{}/token", server.uri()),
            "quota_project_id": "billed",
        },
    })
}

async fn build(json: Value, scopes: Scopes) -> Result<Account, Error> {
    let fixture = Fixture::new(host(Platform::Unix, &[], &[]));
    Account::from_json(json, scopes, &fixture.setup()).await
}

async fn mount_source(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_json(serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": "client",
            "client_secret": "secret",
            "refresh_token": "refresh",
            "scopes": null,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ya29.source", "token_type": "Bearer", "expires_in": 3600
        })))
        .expect(1)
        .mount(server)
        .await;
}

fn iam_answer(expire_time: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "accessToken": "ya29.impersonated", "expireTime": expire_time
    }))
}

async fn token(account: &Account) -> Result<AccessToken, Error> {
    account
        .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
        .await
}

/// Upstream `test_impersonated_service_account`,
/// `test_generate_access_token_success` and
/// `test_impersonated_does_not_propagate_settings_to_source`: the source
/// refreshes with its own default scopes, and IAM gets its token, its quota
/// project, the registry scope and an hour's lifetime, with no `delegates`.
/// Breaks if the registry scope reaches the source, a header is not
/// forwarded, or the body or expiry changes.
#[tokio::test]
async fn impersonates_with_the_source_token() {
    let server = MockServer::start().await;
    mount_source(&server).await;
    Mock::given(method("POST"))
        .and(path(IAM_PATH))
        .and(header("authorization", "Bearer ya29.source"))
        .and(header("x-goog-user-project", "billed"))
        .and(header("content-type", "application/json"))
        .and(body_json(serde_json::json!({
            "scope": [REGISTRY_SCOPE], "lifetime": "3600s"
        })))
        .respond_with(iam_answer("2023-11-14T22:33:20Z"))
        .expect(1)
        .mount(&server)
        .await;

    let account = build(impersonated_file(&server), Scopes::Registry)
        .await
        .unwrap();
    let token = token(&account).await.unwrap();

    assert_eq!(token.token.expose(), "ya29.impersonated");
    assert_eq!(token.expiry, Expiry::At(now() + Duration::from_secs(1200)));
}

/// Upstream `test_with_delegates`, `test_scopes_from_json` and
/// `test_impersonated_service_account_default_scope`: `delegates` are
/// sent, the file's scopes or the default for a source, and the registry
/// scope in place of the file's. Breaks if delegates are dropped or the
/// scope precedence changes.
#[tokio::test]
async fn sends_delegates_and_chooses_scopes() {
    let server = MockServer::start().await;
    mount_source(&server).await;
    Mock::given(method("POST"))
        .and(path(IAM_PATH))
        .and(body_json(serde_json::json!({
            "delegates": ["d@p.iam.gserviceaccount.com"],
            "scope": [REGISTRY_SCOPE],
            "lifetime": "3600s"
        })))
        .respond_with(iam_answer("2023-11-14T22:33:20Z"))
        .expect(1)
        .mount(&server)
        .await;
    let mut file = impersonated_file(&server);
    file["delegates"] = serde_json::json!(["d@p.iam.gserviceaccount.com"]);
    file["scopes"] = serde_json::json!(["https://www.googleapis.com/auth/other"]);
    let account = build(file.clone(), Scopes::Registry).await.unwrap();
    token(&account).await.unwrap();

    let from_file = build(file.clone(), Scopes::Default).await.unwrap();
    assert_eq!(
        from_file.scopes.joined(" "),
        "https://www.googleapis.com/auth/other"
    );
    file.as_object_mut().unwrap().remove("scopes");
    let default = build(file, Scopes::Default).await.unwrap();
    assert_eq!(default.scopes.joined(" "), DEFAULT_SCOPE);
}

/// Upstream `test_generate_access_token_403`, `test_missing_expiry`,
/// `test_invalid_expiry_format` and
/// `token_provider_malformed_response_is_nonretryable`, and an `expireTime`
/// already past, where upstream panics. Breaks if any is accepted or one
/// panics.
#[tokio::test]
async fn refuses_bad_answers() {
    for (answer, expected) in [
        (
            ResponseTemplate::new(403).set_body_string("denied"),
            "failed to generate access token, status 403",
        ),
        (
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"accessToken": "ya29"})),
            "unreadable response",
        ),
        (iam_answer("yesterday"), "invalid expireTime"),
        (iam_answer("2023-11-14T22:13:19Z"), "expired"),
    ] {
        let server = MockServer::start().await;
        mount_source(&server).await;
        Mock::given(method("POST"))
            .and(path(IAM_PATH))
            .respond_with(answer)
            .mount(&server)
            .await;
        let account = build(impersonated_file(&server), Scopes::Registry)
            .await
            .unwrap();
        let error = token(&account).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        assert!(!error.is_transient());
    }
}

/// Upstream `test_missing_impersonation_url_fail`,
/// `test_nested_impersonated_credentials_fail`,
/// `test_malformed_impersonated_credentials_fail`,
/// `test_invalid_source_credential_type_fail` and the impersonation URL's
/// one `/serviceAccounts/`. Breaks if any is accepted.
#[tokio::test]
async fn refuses_bad_files() {
    let server = MockServer::start().await;
    let file = impersonated_file(&server);
    let mut no_url = file.clone();
    no_url
        .as_object_mut()
        .unwrap()
        .remove("service_account_impersonation_url");
    let mut nested = file.clone();
    nested["source_credentials"] = file.clone();
    let mut malformed = file.clone();
    malformed["source_credentials"] = serde_json::json!({"type": "authorized_user"});
    let mut unsupported = file.clone();
    unsupported["source_credentials"] = serde_json::json!({"type": "gdch_service_account"});
    let mut bad_url = file.clone();
    bad_url["service_account_impersonation_url"] = "https://iam.googleapis.com/v1/x".into();
    for (file, expected) in [
        (no_url, "service_account_impersonation_url"),
        (nested, "delegates"),
        (malformed, "client_id"),
        (unsupported, "gdch_service_account"),
        (bad_url, "invalid service account impersonation URL"),
    ] {
        let error = crate::error::error_chain(&build(file, Scopes::Registry).await.unwrap_err());
        assert!(error.contains(expected), "{expected}: {error}");
    }
}

/// An impersonation URL names exactly one service account. Breaks if none,
/// or two, are accepted.
#[test]
fn parses_the_impersonation_url() {
    let one = "https://iam/v1/projects/-/serviceAccounts/sa@p:generateAccessToken";
    assert_eq!(parse_impersonation_url(one).unwrap().as_str(), one);
    for bad in [
        "https://iam/v1/projects/-/sa@p",
        "https://iam/serviceAccounts/a/serviceAccounts/b",
    ] {
        assert!(matches!(
            parse_impersonation_url(bad),
            Err(Error::ImpersonationUrl)
        ));
    }
    assert!(matches!(
        parse_impersonation_url("not a url/serviceAccounts/sa"),
        Err(Error::Url { .. })
    ));
}
