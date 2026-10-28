//! Tests of the external account, ported from upstream's
//! `external_account.rs`, `sts_exchange.rs`, `file_sourced.rs` and
//! `url_sourced.rs`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::super::tests::{Fixture, host};
use super::*;
use crate::aws::{Host, Platform};
use crate::google::REGISTRY_SCOPE;

/// The subject token file the tests read.
const TOKEN_FILE: &str = "/var/run/token";

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// An external account file exchanging the token in [`TOKEN_FILE`] at
/// `server`.
fn account_file(server: &MockServer) -> Value {
    serde_json::json!({
        "type": "external_account",
        "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/pool/providers/k8s",
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "token_url": format!("{}/sts", server.uri()),
        "credential_source": {"file": TOKEN_FILE},
    })
}

fn token_host(token: &str) -> Host {
    host(
        Platform::Unix,
        &[],
        &[(TOKEN_FILE, token.as_bytes().to_vec())],
    )
}

async fn build(json: Value, scopes: Scopes, host: Host) -> Result<Account, Error> {
    Account::from_json(json, scopes, &Fixture::new(host).setup()).await
}

async fn token(account: &Account) -> Result<AccessToken, Error> {
    account
        .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
        .await
}

/// The form fields of `request`.
fn form(request: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

fn sts_answer(token: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": token,
        "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
        "token_type": "Bearer",
        "expires_in": 3600,
    }))
}

/// Upstream `exchange_token`,
/// `test_external_account_without_impersonation_success` and
/// `test_kubernetes_wif_direct_identity_parsing`: the form STS is sent, with
/// the file's token whole and the registry scope, and the expiry from
/// `expires_in`. Breaks if a field, its value or the expiry changes.
#[tokio::test]
async fn exchanges_the_subject_token_at_sts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .respond_with(sts_answer("ya29.sts"))
        .expect(1)
        .mount(&server)
        .await;

    let account = build(
        account_file(&server),
        Scopes::Registry,
        token_host("k8s-jwt\n"),
    )
    .await
    .unwrap();
    let token = token(&account).await.unwrap();

    assert_eq!(token.token.expose(), "ya29.sts");
    assert_eq!(
        token.expiry,
        crate::google::Expiry::At(now() + Duration::from_secs(3600))
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        form(&requests[0]),
        HashMap::from(
            [
                ("grant_type", GRANT_TYPE),
                ("requested_token_type", ACCESS_TOKEN_TYPE),
                ("subject_token", "k8s-jwt\n"),
                ("subject_token_type", "urn:ietf:params:oauth:token-type:jwt"),
                ("scope", REGISTRY_SCOPE),
                (
                    "audience",
                    "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/pool/providers/k8s"
                ),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        )
    );
    assert!(requests[0].headers.get("authorization").is_none());
}

/// Upstream `test_external_account_with_impersonation_success` and
/// `test_kubernetes_wif_impersonation_parsing`: STS is asked for the IAM
/// scope, and its token is exchanged at the impersonation URL for one with
/// the registry scope. Breaks if STS gets the registry scope, IAM gets no
/// bearer, or the IAM token is not the one returned.
#[tokio::test]
async fn impersonates_after_the_exchange() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .and(body_string_contains(
            "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fiam",
        ))
        .respond_with(sts_answer("ya29.sts"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken",
        ))
        .and(header("authorization", "Bearer ya29.sts"))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "scope": [REGISTRY_SCOPE], "lifetime": "3600s"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accessToken": "ya29.impersonated", "expireTime": "2023-11-14T22:33:20Z"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut file = account_file(&server);
    file["service_account_impersonation_url"] = format!(
        "{}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken",
        server.uri()
    )
    .into();

    let account = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap();
    let token = token(&account).await.unwrap();

    assert_eq!(token.token.expose(), "ya29.impersonated");
    assert_eq!(
        token.expiry,
        crate::google::Expiry::At(now() + Duration::from_secs(1_200))
    );
}

/// A transient IAM failure sends the IAM call again, and only it: the STS
/// exchange before it is not repeated. Breaks if a retry restarts the
/// chain.
#[tokio::test]
async fn retries_only_the_failed_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .respond_with(sts_answer("ya29.sts"))
        .expect(1)
        .mount(&server)
        .await;
    let iam = "/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken";
    Mock::given(method("POST"))
        .and(path(iam))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(iam))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accessToken": "ya29.impersonated", "expireTime": "2023-11-14T22:33:20Z"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut file = account_file(&server);
    file["service_account_impersonation_url"] = format!("{}{iam}", server.uri()).into();

    let account = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap();

    assert_eq!(
        token(&account).await.unwrap().token.expose(),
        "ya29.impersonated"
    );
}

/// Upstream `test_impersonation_flow_sts_call_fails` and `_iam_call_fails`
/// and `exchange_token_err`: a failed STS call stops before IAM, and a
/// failed IAM call is reported. Breaks if IAM is called after a failed
/// exchange or a failure is lost.
#[tokio::test]
async fn reports_a_failed_exchange() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .respond_with(ResponseTemplate::new(400).set_body_string("invalid_grant"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/iam"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut file = account_file(&server);
    file["service_account_impersonation_url"] =
        format!("{}/iam/serviceAccounts/sa", server.uri()).into();
    let account = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap();
    let error = token(&account).await.unwrap_err();
    assert!(
        error.to_string().contains("failed to exchange token"),
        "{error}"
    );
    assert!(error.to_string().contains("invalid_grant"), "{error}");
    assert!(!error.is_transient());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .respond_with(sts_answer("ya29.sts"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/iam/serviceAccounts/sa"))
        .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
        .expect(1)
        .mount(&server)
        .await;
    let mut file = account_file(&server);
    file["service_account_impersonation_url"] =
        format!("{}/iam/serviceAccounts/sa", server.uri()).into();
    let account = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap();
    let error = token(&account).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to generate access token"),
        "{error}"
    );
}

/// Upstream `exchange_token` with client authentication and
/// `sts_handler_ignores_workforce_pool_user_project_with_client_auth`:
/// with both a client ID and secret, Basic auth of `id:secret` in
/// standard padded base64, as RFC 7617 and Google's Go and Python
/// libraries send it, and no `options`. Upstream sends URL-safe base64
/// without padding. Breaks if the encoding changes or `options` is sent
/// beside client auth.
#[tokio::test]
async fn authenticates_the_client() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .and(header("authorization", "Basic Y2xpZW50OnNlY3JldD8+YWI="))
        .respond_with(sts_answer("ya29.sts"))
        .expect(1)
        .mount(&server)
        .await;
    let mut file = account_file(&server);
    file["audience"] =
        "//iam.googleapis.com/locations/global/workforcePools/pool/providers/idp".into();
    file["workforce_pool_user_project"] = "project".into();
    file["client_id"] = "client".into();
    file["client_secret"] = "secret?>ab".into();

    let account = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap();
    token(&account).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(!form(&requests[0]).contains_key("options"));
}

/// Upstream `sts_handler_receives_workforce_pool_user_project`: with no
/// client, the workforce project goes to STS as `options`. Breaks if it is
/// not sent.
#[tokio::test]
async fn sends_the_workforce_project() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sts"))
        .respond_with(sts_answer("ya29.sts"))
        .expect(1)
        .mount(&server)
        .await;
    let mut file = account_file(&server);
    file["audience"] =
        "//iam.googleapis.com/locations/global/workforcePools/pool/providers/idp".into();
    file["workforce_pool_user_project"] = "project".into();

    let account = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap();
    token(&account).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        form(&requests[0]).get("options").map(String::as_str),
        Some(r#"{"userProject":"project"}"#)
    );
}

/// Upstream
/// `builder_workforce_pool_user_project_fails_without_workforce_pool_audience`.
/// Breaks if a workload pool audience may carry a workforce project.
#[tokio::test]
async fn refuses_a_workforce_project_outside_a_workforce_pool() {
    let server = MockServer::start().await;
    let mut file = account_file(&server);
    file["workforce_pool_user_project"] = "project".into();
    let error = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("workforce_pool_user_project"),
        "{error}"
    );
}

/// A `json` format reads the field it names, and must name one; any other
/// type, or none, reads the whole text, as upstream. Breaks if a `json`
/// format with no field loads, which upstream lets fail at every exchange,
/// or an unknown type is refused.
#[test]
fn parses_the_subject_token_format() {
    let format = |json: Value| {
        let file: Option<FormatFile> = serde_json::from_value(json).unwrap();
        Format::try_from(file)
    };
    assert_eq!(
        format(serde_json::json!({"type": "json", "subject_token_field_name": "token"})).unwrap(),
        Format::Json {
            field: "token".into()
        }
    );
    for text in [
        serde_json::json!({"type": "text"}),
        serde_json::json!({"type": "xml"}),
        Value::Null,
    ] {
        assert_eq!(format(text.clone()).unwrap(), Format::Text, "{text}");
    }
    for no_field in [
        serde_json::json!({"type": "json"}),
        serde_json::json!({"type": "json", "subject_token_field_name": ""}),
    ] {
        assert!(
            matches!(format(no_field.clone()), Err(Error::NoSubjectField)),
            "{no_field}"
        );
    }
}

/// Upstream `test_aws_parsing` and the untagged source order: a source with
/// `environment_id` is AWS even beside `url`, `executable` comes before
/// `url`, and `url` before `file`. Breaks if the order changes.
#[test]
fn selects_the_source_in_upstream_order() {
    let parse = |json: Value| serde_json::from_value::<SourceFile>(json).unwrap();
    assert!(matches!(
        parse(serde_json::json!({
            "environment_id": "aws1",
            "region_url": "http://169.254.169.254/latest/meta-data/placement/availability-zone",
            "url": "http://169.254.169.254/latest/meta-data/iam/security-credentials",
            "regional_cred_verification_url": "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
        })),
        SourceFile::Aws { .. }
    ));
    assert!(matches!(
        parse(serde_json::json!({"executable": {"command": "x"}, "url": "u"})),
        SourceFile::Executable
    ));
    assert!(matches!(
        parse(serde_json::json!({"url": "u", "file": "f"})),
        SourceFile::Url(_)
    ));
    assert!(matches!(
        parse(serde_json::json!({"file": "f"})),
        SourceFile::File(_)
    ));
}

/// An AWS source with a malformed field is an error, not the URL source
/// it also names. Breaks if a later source is tried once AWS is selected.
#[test]
fn a_malformed_aws_source_is_an_error() {
    let source = serde_json::from_value::<SourceFile>(serde_json::json!({
        "environment_id": "aws1",
        "region_url": 7,
        "url": "http://169.254.169.254/latest/meta-data/iam/security-credentials",
    }));
    let error = source.err().expect("refused");
    assert_eq!(
        error.to_string(),
        "invalid type: integer `7`, expected a string"
    );
}

/// An executable source is refused by name. Breaks if it is run or
/// accepted.
#[tokio::test]
async fn refuses_an_executable_source() {
    let server = MockServer::start().await;
    let mut file = account_file(&server);
    file["credential_source"] = serde_json::json!({"executable": {"command": "/bin/token"}});
    let error = build(file, Scopes::Registry, token_host("jwt"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("executable"), "{error}");
}

/// Upstream `get_text_token`, `get_json_token`,
/// `get_json_token_missing_field`, `get_text_token_from_empty_file`,
/// `get_json_token_from_empty_file` and `file_not_found`. Breaks if text
/// is trimmed, a JSON field is not read, or an empty or missing file is
/// treated otherwise.
#[tokio::test]
async fn reads_file_subject_tokens() {
    let clients = Clients::new().unwrap();
    let read = |contents: Option<&str>, format: Format| {
        let fs = match contents {
            Some(contents) => token_host(contents).fs,
            None => host(Platform::Unix, &[], &[]).fs,
        };
        let source = Source::File {
            path: PathBuf::from(TOKEN_FILE),
            format,
            fs,
        };
        let clients = clients.clone();
        async move {
            source
                .subject_token(&clients, Clock::Fixed(now()))
                .await
                .map(|token| token.expose().to_owned())
        }
    };
    let json = || Format::Json {
        field: "access_token".into(),
    };
    assert_eq!(
        read(Some(" text \n"), Format::Text).await.unwrap(),
        " text \n"
    );
    assert_eq!(read(Some(""), Format::Text).await.unwrap(), "");
    assert_eq!(
        read(Some(r#"{"access_token": "jwt"}"#), json())
            .await
            .unwrap(),
        "jwt"
    );
    assert!(matches!(
        read(Some(r#"{"other": "jwt"}"#), json()).await,
        Err(Error::SubjectField(_))
    ));
    assert!(matches!(
        read(Some(""), json()).await,
        Err(Error::SubjectJson(_))
    ));
    assert!(matches!(
        read(None, Format::Text).await,
        Err(Error::ReadFile { .. })
    ));
}

/// Upstream `url_sourced` `get_json_token`, `get_text_token` and
/// `get_json_token_missing_field`, with the file's headers sent. Breaks if
/// a header is dropped or the format is ignored.
#[tokio::test]
async fn reads_url_subject_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(header("metadata", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"jwt"}"#))
        .mount(&server)
        .await;
    let clients = Clients::new().unwrap();
    let read = |format: Format| {
        let source = Source::Url {
            url: Url::parse(&format!("{}/token", server.uri())).unwrap(),
            route: Route::Direct,
            headers: header_map(HashMap::from([("metadata".into(), "true".into())])).unwrap(),
            format,
        };
        let clients = clients.clone();
        async move {
            source
                .subject_token(&clients, Clock::Fixed(now()))
                .await
                .map(|token| token.expose().to_owned())
        }
    };
    assert_eq!(
        read(Format::Json {
            field: "access_token".into()
        })
        .await
        .unwrap(),
        "jwt"
    );
    assert_eq!(
        read(Format::Text).await.unwrap(),
        r#"{"access_token":"jwt"}"#
    );
    assert!(matches!(
        read(Format::Json {
            field: "missing".into()
        })
        .await,
        Err(Error::SubjectField(_))
    ));
}

/// A URL source's headers are parsed at load. Breaks if a header that
/// cannot be sent is accepted.
#[test]
fn refuses_a_bad_url_source_header() {
    for (name, value) in [("bad name", "v"), ("ok", "bad\nvalue")] {
        let error = header_map(HashMap::from([(name.to_owned(), value.to_owned())])).unwrap_err();
        assert!(
            matches!(&error, Error::SubjectHeader(found) if found == name),
            "{error}"
        );
    }
}

/// A URL source's header values never print. Breaks if a token in one
/// reaches a `Debug` print of the credentials.
#[test]
fn url_source_headers_never_print() {
    let headers = header_map(HashMap::from([(
        "authorization".to_owned(),
        "bearer gha-token".to_owned(),
    )]))
    .unwrap();
    let printed = format!("{headers:?}");
    assert!(!printed.contains("gha-token"), "{printed}");
}

/// A URL source at a link-local or loopback address is fetched with no
/// proxy, and any other through it. Breaks if Azure's instance metadata
/// service is fetched through the proxy.
#[test]
fn fetches_a_local_url_source_without_the_proxy() {
    for (url, expected) in [
        (
            "http://169.254.169.254/metadata/identity/oauth2/token",
            Route::Direct,
        ),
        ("http://[fe80::1]/token", Route::Direct),
        ("http://127.0.0.1:8080/token", Route::Direct),
        ("http://localhost:8080/token", Route::Direct),
        ("http://[fd00:ec2::254]/latest/api/token", Route::Direct),
        ("http://metadata.google.internal/token", Route::Direct),
        ("https://sts.example.com/token", Route::Proxied),
        ("http://169.254.169.254.example.com/token", Route::Proxied),
    ] {
        assert_eq!(route(&Url::parse(url).unwrap()), expected, "{url}");
    }
    let clients = Clients::new().unwrap();
    assert!(std::ptr::eq(
        clients.client(Call::SubjectUrl(Route::Direct)),
        &clients.direct
    ));
    assert!(std::ptr::eq(
        clients.client(Call::SubjectUrl(Route::Proxied)),
        &clients.proxied
    ));
}
