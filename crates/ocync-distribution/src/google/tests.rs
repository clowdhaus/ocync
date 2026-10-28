//! Tests of where the Application Default Credentials come from and how a
//! file's `type` selects them, ported from upstream's `credentials.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use super::service_account::tests::{key_file, pkcs8_pem};
use super::*;
use crate::aws::Fs;

/// A fixed host on `platform` with `vars` and `files`.
pub(crate) fn host(platform: Platform, vars: &[(&str, &str)], files: &[(&str, Vec<u8>)]) -> Host {
    let mut host = Host::fixed();
    host.platform = platform;
    host.env = Env::Fixed(
        vars.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    );
    host.fs = Fs::Fixed(
        files
            .iter()
            .map(|(path, contents)| (PathBuf::from(path), contents.clone()))
            .collect::<HashMap<_, _>>(),
    );
    host
}

/// What a test's [`Setup`] borrows.
pub(super) struct Fixture {
    host: Host,
    clients: Clients,
    aws_credentials: AwsCredentialProviders,
}

impl Fixture {
    /// Clients and empty AWS credentials for `host`.
    pub(super) fn new(host: Host) -> Self {
        Self {
            host,
            clients: Clients::new().unwrap(),
            aws_credentials: AwsCredentialProviders::default(),
        }
    }

    pub(super) fn setup(&self) -> Setup<'_> {
        Setup {
            host: &self.host,
            clients: &self.clients,
            aws_credentials: &self.aws_credentials,
        }
    }
}

async fn load(host: &Host) -> Result<Credentials, Error> {
    Credentials::load(
        host,
        &Clients::new().unwrap(),
        &AwsCredentialProviders::default(),
    )
    .await
}

fn service_account_bytes() -> Vec<u8> {
    key_file(&pkcs8_pem()).to_string().into_bytes()
}

/// Upstream `adc_well_known_path_posix`, `_windows`, `_posix_no_home` and
/// `_no_appdata`. Breaks if Unix reads anything but `HOME`, Windows anything
/// but `APPDATA`, or the directories change.
#[test]
fn finds_the_well_known_path() {
    let unix = Env::Fixed(HashMap::from([("HOME".to_owned(), "/home/u".to_owned())]));
    assert_eq!(
        well_known_path(&unix, Platform::Unix),
        Some(PathBuf::from(
            "/home/u/.config/gcloud/application_default_credentials.json"
        ))
    );
    assert_eq!(well_known_path(&unix, Platform::Windows), None);
    let windows = Env::Fixed(HashMap::from([(
        "APPDATA".to_owned(),
        "C:/Users/u/AppData/Roaming".to_owned(),
    )]));
    assert_eq!(
        well_known_path(&windows, Platform::Windows),
        Some(
            [
                "C:/Users/u/AppData/Roaming",
                "gcloud",
                "application_default_credentials.json"
            ]
            .iter()
            .collect()
        )
    );
    assert_eq!(well_known_path(&windows, Platform::Unix), None);
}

/// A path that is not UTF-8 is still the file read, as upstream reads the
/// variable. Breaks if such a value is treated as unset, which falls back
/// to the well-known file and another identity.
#[cfg(unix)]
#[test]
fn reads_a_path_that_is_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let path = OsStr::from_bytes(b"/adc-\xff.json");
    let mut host = host(Platform::Unix, &[], &[]);
    host.env = Env::FixedOs(HashMap::from([(
        CREDENTIALS_VARIABLE.to_owned(),
        path.to_owned(),
    )]));
    host.fs = Fs::Fixed(HashMap::from([(PathBuf::from(path), b"{}".to_vec())]));

    assert_eq!(find_file(&host).unwrap(), Some(b"{}".to_vec()));
}

/// Upstream `adc_path_from_env`, `load_adc_success`,
/// `load_adc_no_file_at_env_is_error`,
/// `create_access_token_credentials_errors_if_adc_env_is_not_a_file`,
/// `load_adc_no_well_known_path_fallback_to_mds`,
/// `load_adc_no_file_at_well_known_path_fallback_to_mds` and
/// `create_access_token_credentials_fallback_to_mds`: the file the variable
/// names comes first and must exist, then the well-known file, then the
/// metadata server. Breaks if the order changes, a missing named file
/// falls back, or no file is an error.
#[tokio::test]
async fn finds_the_credentials_in_order() {
    let well_known = "/home/u/.config/gcloud/application_default_credentials.json";
    let user = b"{\"type\":\"authorized_user\"}".to_vec();
    let named = [(CREDENTIALS_VARIABLE, "/keys/sa.json"), ("HOME", "/home/u")];
    type Check = fn(&Result<Credentials, Error>) -> bool;
    type Case<'a> = (
        &'a str,
        &'a [(&'a str, &'a str)],
        Vec<(&'a str, Vec<u8>)>,
        Check,
    );
    let service_account: Check = |r| matches!(r, Ok(Credentials(Kind::ServiceAccount(_))));
    let metadata: Check = |r| matches!(r, Ok(Credentials(Kind::Metadata(_))));
    let missing: Check = |r| match r {
        Err(e @ Error::MissingFile(_)) => e.to_string().contains(CREDENTIALS_VARIABLE),
        _ => false,
    };
    let cases: [Case<'_>; 5] = [
        (
            "the named file before the well-known one",
            &named,
            vec![
                ("/keys/sa.json", service_account_bytes()),
                (well_known, user.clone()),
            ],
            service_account,
        ),
        (
            "a missing named file",
            &[(CREDENTIALS_VARIABLE, "/keys/none.json")],
            vec![],
            missing,
        ),
        (
            "the well-known file",
            &[("HOME", "/home/u")],
            vec![(well_known, service_account_bytes())],
            service_account,
        ),
        ("no home", &[], vec![], metadata),
        (
            "no well-known file",
            &[("HOME", "/home/u")],
            vec![],
            metadata,
        ),
    ];
    for (case, vars, files, expected) in cases {
        let host = host(Platform::Unix, vars, &files);
        let found = load(&host).await;
        assert!(expected(&found), "{case}: {found:?}");
    }
}

/// Upstream `create_access_token_credentials_malformed_adc_is_error` and
/// `_adc_unimplemented_credential_type`, and the missing and non-string
/// `type` cases: each is an error naming the problem. Breaks if one is
/// accepted or the unsupported type is not named.
#[tokio::test]
async fn refuses_files_it_cannot_use() {
    for (contents, expected) in [
        ("not json", "failed to parse"),
        ("{}", "no `type` field found"),
        ("{\"type\": 1}", "`type` field is not a string"),
        (
            "{\"type\": \"gdch_service_account\"}",
            "unsupported credential type `gdch_service_account`",
        ),
    ] {
        let host = host(
            Platform::Unix,
            &[(CREDENTIALS_VARIABLE, "/keys/adc.json")],
            &[("/keys/adc.json", contents.as_bytes().to_vec())],
        );
        let error = load(&host).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{contents}: {error}");
    }
}

/// A secret never prints. Breaks if `Secret`'s `Debug` shows its value.
#[test]
fn a_secret_never_prints() {
    let secret = Secret::from("ya29.secret".to_owned());
    assert_eq!(format!("{secret:?}"), REDACTED);
}

/// An answer cut short while its body is read may be sent again, from the
/// metadata server too. Breaks if a body read failure is final, which a
/// reset after the headers would make of a throttled Google endpoint or a
/// GKE metadata server minting a token.
#[tokio::test]
async fn a_body_cut_short_is_transient() {
    for call in [Call::UserToken, Call::Metadata] {
        let error = cut_short(call).await;
        assert!(matches!(error, Error::Transport { .. }), "{error}");
        assert!(error.is_transient(), "{call}: {error}");
    }
}

/// The error of `call` to a server that closes the connection partway
/// through its answer's body.
async fn cut_short(call: Call) -> Error {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.read(&mut [0; 1024]).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{")
            .unwrap();
    });
    let clients = Clients::new().unwrap();

    // One try: the server answers once, and the retry is not under test.
    let client = clients.client(call);
    let request = client.get(format!("http://{address}/")).build().unwrap();
    let error = send_once(call, client, request).await.unwrap_err();

    server.join().unwrap();
    error
}

/// A source token no header can carry is an error before any request is
/// built. Breaks if it reaches the request builder, where it would surface
/// as a network failure.
#[test]
fn refuses_a_source_token_no_header_can_carry() {
    let error = SourceAuth::new("ya29.bad\ntoken", None).unwrap_err();
    assert!(matches!(error, Error::SourceToken(_)), "{error}");
}

/// A lifetime past what the clock can hold is refused. Breaks if it
/// silently becomes a token with no expiry.
#[test]
fn refuses_a_lifetime_the_clock_cannot_hold() {
    let now = SystemTime::UNIX_EPOCH;
    assert!(matches!(
        expiring("t".to_owned(), now, Some(u64::MAX)),
        Err(Error::ExpiresIn(u64::MAX))
    ));
    let token = expiring("t".to_owned(), now, Some(60)).unwrap();
    assert_eq!(token.expiry, Expiry::At(now + Duration::from_secs(60)));
}

/// Upstream's status rule: a 5xx, 408 or 429 may succeed if sent again,
/// any other refusal and an answer that does not decode may not. Breaks if
/// one of the three transient kinds stops being retried on its own, or a
/// refusal is retried.
#[test]
fn classifies_answers() {
    let status = |code: u16| Error::Status {
        call: Call::UserToken,
        status: http::StatusCode::from_u16(code).unwrap(),
        body: String::new(),
    };
    for (code, transient) in [
        (500, true),
        (503, true),
        (408, true),
        (429, true),
        (400, false),
        (401, false),
        (403, false),
        (404, false),
    ] {
        assert_eq!(status(code).is_transient(), transient, "{code}");
    }
    let undecodable = Error::Decode {
        call: Call::UserToken,
        source: serde_json::from_str::<Value>("{").unwrap_err(),
    };
    assert!(!undecodable.is_transient());
}

/// A request that does not connect is final for the EC2 instance metadata
/// service, which a host either has or never will, and is sent again for a
/// GKE metadata server or a token sidecar still starting. Breaks if either
/// fails the run at once, or a missing EC2 service is retried.
#[tokio::test]
async fn a_refused_connection_is_final_only_for_the_ec2_metadata_service() {
    let clients = Clients::new().unwrap();
    for (call, transient) in [
        (Call::Metadata, true),
        (Call::AwsRegion, false),
        (Call::SubjectUrl(Route::Direct), true),
    ] {
        let client = clients.client(call);
        let request = client.get("http://127.0.0.1:1/token").build().unwrap();

        let error = send_once(call, client, request).await.unwrap_err();

        assert!(matches!(error, Error::Transport { .. }), "{error}");
        assert_eq!(error.is_transient(), transient, "{call:?}");
    }
}

/// A failed request's error shows no URL, as its query can hold a token.
/// Breaks if the URL is kept in the error.
#[tokio::test]
async fn a_transport_error_shows_no_url() {
    let clients = Clients::new().unwrap();
    let error = send(&clients, Call::AwsRegion, |client| {
        client.get("http://127.0.0.1:1/token?token=s3cret")
    })
    .await
    .unwrap_err();

    assert!(
        !crate::error::error_chain(&error).contains("s3cret"),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains("s3cret"), "{error:?}");
}

/// An answer longer than any token is refused rather than read whole.
/// Breaks if the body is read past the cap.
#[tokio::test]
async fn refuses_an_answer_past_the_cap() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_BODY + 1]))
        .mount(&server)
        .await;
    let clients = Clients::new().unwrap();

    let error = send(&clients, Call::SubjectUrl(Route::Proxied), |client| {
        client.get(server.uri())
    })
    .await
    .unwrap_err();

    assert!(matches!(error, Error::BodyTooLarge { .. }), "{error}");
}
