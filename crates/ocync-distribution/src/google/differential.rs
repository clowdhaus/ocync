//! Differential test against `google-cloud-auth` 1.18.0: both resolve the
//! same credentials against mock endpoints, and the requests sent and
//! tokens returned are compared. Temporary: removed with the dependency.

use std::collections::BTreeMap;
use std::path::PathBuf;

use google_cloud_auth::credentials::{
    AccessTokenCredentials, external_account, impersonated, mds, service_account, user_account,
};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::service_account::tests::{key_file, pkcs8_pem};
use super::tests::host;
use super::*;
use crate::aws::Platform;

/// The headers compared.
const HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "metadata-flavor",
    "x-goog-user-project",
];

/// A request as compared: method, path, query, compared headers, body.
type Seen = (
    String,
    String,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
    Value,
);

fn seen(request: &Request) -> Seen {
    let body = if request.body.is_empty() {
        Value::Null
    } else if let Ok(json) = serde_json::from_slice(&request.body) {
        json
    } else {
        Value::Object(
            url::form_urlencoded::parse(&request.body)
                .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                .collect(),
        )
    };
    (
        request.method.to_string(),
        request.url.path().to_owned(),
        request.url.query_pairs().into_owned().collect(),
        HEADERS
            .iter()
            .filter_map(|name| {
                request
                    .headers
                    .get(*name)
                    .map(|v| ((*name).to_owned(), v.to_str().unwrap().to_owned()))
            })
            .collect(),
        body,
    )
}

/// A server answering every request by its path's last segment.
async fn server() -> MockServer {
    crate::install_crypto_provider();
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(|request: &Request| {
            let path = request.url.path();
            let body = if path.ends_with(":generateAccessToken") {
                json!({"accessToken": "ya29.iam", "expireTime": "2099-01-01T00:00:00Z"})
            } else if path.ends_with("/sts") {
                json!({"access_token": "ya29.sts", "issued_token_type": "urn:ietf:params:oauth:token-type:access_token", "token_type": "Bearer", "expires_in": 3600})
            } else if path.ends_with("/subject") {
                return ResponseTemplate::new(200).set_body_string(r#"{"id_token":"url-jwt"}"#);
            } else {
                json!({"access_token": "ya29.token", "token_type": "Bearer", "expires_in": 3600})
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    server
}

/// What each side sent and got for `file` with the registry scope, with
/// `server`'s address in place of `{server}`.
async fn compare(
    file: impl Fn(&str) -> Value,
    upstream: impl Fn(Value) -> AccessTokenCredentials,
    files: &[(&str, Vec<u8>)],
) -> ((Vec<Seen>, String), (Vec<Seen>, String)) {
    let theirs = server().await;
    let token = upstream(file(&theirs.uri()))
        .access_token()
        .await
        .unwrap()
        .token;
    let theirs = (
        theirs
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(seen)
            .collect(),
        token,
    );

    let ours = server().await;
    let mut all = files.to_vec();
    all.push(("/adc.json", file(&ours.uri()).to_string().into_bytes()));
    let host = host(Platform::Unix, &[(CREDENTIALS_FILE, "/adc.json")], &all);
    let clients = Clients::new().unwrap();
    let credentials = Credentials::load(&host, &clients, &AwsCredentialProviders::default())
        .await
        .unwrap();
    let token = credentials
        .access_token(&clients, Clock::System)
        .await
        .unwrap();
    let ours = (
        ours.received_requests()
            .await
            .unwrap()
            .iter()
            .map(seen)
            .collect(),
        token.token.expose().to_owned(),
    );
    (theirs, ours)
}

fn user(server: &str) -> Value {
    json!({
        "type": "authorized_user", "client_id": "client", "client_secret": "secret",
        "refresh_token": "refresh", "token_uri": format!("{server}/token"),
        "quota_project_id": "billed",
    })
}

#[tokio::test]
async fn authorized_user_matches_upstream() {
    let (theirs, ours) = compare(
        user,
        |file| {
            user_account::Builder::new(file)
                .with_scopes([REGISTRY_SCOPE])
                .build_access_token_credentials()
                .unwrap()
        },
        &[],
    )
    .await;
    assert_eq!(theirs, ours);
}

#[tokio::test]
async fn external_account_file_with_impersonation_matches_upstream() {
    let file = |server: &str| {
        json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/k8s",
            "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
            "token_url": format!("{server}/sts"),
            "service_account_impersonation_url": format!("{server}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken"),
            "client_id": "client", "client_secret": "secret",
            "credential_source": {"file": TOKEN_FILE, "format": {"type": "json", "subject_token_field_name": "id_token"}},
        })
    };
    let token = br#"{"id_token":"file-jwt"}"#.to_vec();
    let _real = RealTokenFile::write(&token);
    let (theirs, ours) = compare(
        file,
        |file| {
            external_account::Builder::new(file)
                .with_scopes([REGISTRY_SCOPE])
                .build_access_token_credentials()
                .unwrap()
        },
        &[(TOKEN_FILE, token.clone())],
    )
    .await;
    assert_eq!(theirs, ours);
}

#[tokio::test]
async fn external_account_url_matches_upstream() {
    let file = |server: &str| {
        json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/locations/global/workforcePools/pool/providers/idp",
            "subject_token_type": "urn:ietf:params:oauth:token-type:id_token",
            "token_url": format!("{server}/sts"),
            "workforce_pool_user_project": "project",
            "credential_source": {"url": format!("{server}/subject"), "headers": {"metadata": "true"}, "format": {"type": "json", "subject_token_field_name": "id_token"}},
        })
    };
    let (theirs, ours) = compare(
        file,
        |file| {
            external_account::Builder::new(file)
                .with_scopes([REGISTRY_SCOPE])
                .build_access_token_credentials()
                .unwrap()
        },
        &[],
    )
    .await;
    assert_eq!(theirs, ours);
}

#[tokio::test]
async fn impersonated_user_matches_upstream() {
    let file = |server: &str| {
        json!({
            "type": "impersonated_service_account",
            "service_account_impersonation_url": format!("{server}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken"),
            "delegates": ["d@p.iam.gserviceaccount.com"],
            "source_credentials": user(server),
        })
    };
    let (theirs, ours) = compare(
        file,
        |file| {
            impersonated::Builder::new(file)
                .with_scopes([REGISTRY_SCOPE])
                .build_access_token_credentials()
                .unwrap()
        },
        &[],
    )
    .await;
    assert_eq!(theirs, ours);
}

#[tokio::test]
async fn metadata_server_matches_upstream() {
    let theirs = server().await;
    let token = mds::Builder::default()
        .with_endpoint(theirs.uri())
        .with_scopes([REGISTRY_SCOPE])
        .build_access_token_credentials()
        .unwrap()
        .access_token()
        .await
        .unwrap()
        .token;
    let theirs: (Vec<Seen>, String) = (
        theirs
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(seen)
            .collect(),
        token,
    );

    let ours = server().await;
    let host_port = ours.uri().trim_start_matches("http://").to_owned();
    let host = host(Platform::Unix, &[("GCE_METADATA_HOST", &host_port)], &[]);
    let clients = Clients::new().unwrap();
    let credentials = Credentials::load(&host, &clients, &AwsCredentialProviders::default())
        .await
        .unwrap();
    let token = credentials
        .access_token(&clients, Clock::System)
        .await
        .unwrap();
    let ours = (
        ours.received_requests()
            .await
            .unwrap()
            .iter()
            .map(seen)
            .collect(),
        token.token.expose().to_owned(),
    );
    assert_eq!(theirs, ours);
}

#[tokio::test]
async fn service_account_jwt_matches_upstream() {
    crate::install_crypto_provider();
    let file = key_file(&pkcs8_pem());
    let theirs = service_account::Builder::new(file.clone())
        .with_access_specifier(service_account::AccessSpecifier::from_scopes([
            REGISTRY_SCOPE,
        ]))
        .build_access_token_credentials()
        .unwrap()
        .access_token()
        .await
        .unwrap()
        .token;
    let ours = super::service_account::ServiceAccount::from_json(file, Scopes::Registry)
        .unwrap()
        .access_token(SystemTime::now())
        .unwrap();
    let parts = |jwt: &str| -> Vec<Value> {
        jwt.split('.')
            .take(2)
            .map(|part| {
                use base64::Engine as _;
                serde_json::from_slice(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(part)
                        .unwrap(),
                )
                .unwrap()
            })
            .collect()
    };
    let (theirs, ours) = (parts(&theirs), parts(ours.token.expose()));
    assert_eq!(theirs[0], ours[0]);
    let without_times = |claims: &Value| {
        let mut claims = claims.clone();
        let map = claims.as_object_mut().unwrap();
        let (exp, iat) = (map.remove("exp").unwrap(), map.remove("iat").unwrap());
        (claims, exp.as_u64().unwrap() - iat.as_u64().unwrap())
    };
    assert_eq!(without_times(&theirs[1]), without_times(&ours[1]));
    let iat = |claims: &Value| claims["iat"].as_u64().unwrap();
    assert!(iat(&theirs[1]).abs_diff(iat(&ours[1])) <= 1);
}

/// The subject token file upstream reads from the real file system.
const TOKEN_FILE: &str = "/tmp/ocync-differential-subject-token";

/// [`TOKEN_FILE`] written for upstream, removed when dropped.
struct RealTokenFile(PathBuf);

impl RealTokenFile {
    fn write(contents: &[u8]) -> Self {
        std::fs::write(TOKEN_FILE, contents).unwrap();
        Self(PathBuf::from(TOKEN_FILE))
    }
}

impl Drop for RealTokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
