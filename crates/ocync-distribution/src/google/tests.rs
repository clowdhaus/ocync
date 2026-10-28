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

/// Upstream `adc_path_from_env` and `load_adc_success`: the file the
/// variable names, before the well-known file. Breaks if the variable is
/// read after the well-known file.
#[tokio::test]
async fn reads_the_file_the_variable_names() {
    let host = host(
        Platform::Unix,
        &[(CREDENTIALS_VARIABLE, "/keys/sa.json"), ("HOME", "/home/u")],
        &[
            ("/keys/sa.json", service_account_bytes()),
            (
                "/home/u/.config/gcloud/application_default_credentials.json",
                b"{\"type\":\"authorized_user\"}".to_vec(),
            ),
        ],
    );
    let credentials = load(&host).await.unwrap();
    assert!(
        matches!(credentials, Credentials::ServiceAccount(_)),
        "{credentials:?}"
    );
}

/// Upstream `load_adc_no_file_at_env_is_error` and
/// `create_access_token_credentials_errors_if_adc_env_is_not_a_file`: a
/// missing file the variable names is an error, not the metadata server.
/// Breaks if it falls back.
#[tokio::test]
async fn a_missing_named_file_is_an_error() {
    let host = host(
        Platform::Unix,
        &[(CREDENTIALS_VARIABLE, "/keys/none.json")],
        &[],
    );
    let error = load(&host).await.unwrap_err();
    assert!(matches!(error, Error::MissingFile(_)), "{error}");
    assert!(error.to_string().contains("GOOGLE_APPLICATION_CREDENTIALS"));
}

/// Upstream `load_adc_no_well_known_path_fallback_to_mds`,
/// `load_adc_no_file_at_well_known_path_fallback_to_mds` and
/// `create_access_token_credentials_fallback_to_mds`: with no variable,
/// no home or no well-known file, the metadata server is used. Breaks if
/// either case is an error.
#[tokio::test]
async fn falls_back_to_the_metadata_server() {
    for host in [
        host(Platform::Unix, &[], &[]),
        host(Platform::Unix, &[("HOME", "/home/u")], &[]),
    ] {
        let credentials = load(&host).await.unwrap();
        assert!(
            matches!(credentials, Credentials::Metadata(_)),
            "{credentials:?}"
        );
    }
}

/// The well-known file is read when it exists. Breaks if it is skipped.
#[tokio::test]
async fn reads_the_well_known_file() {
    let host = host(
        Platform::Unix,
        &[("HOME", "/home/u")],
        &[(
            "/home/u/.config/gcloud/application_default_credentials.json",
            service_account_bytes(),
        )],
    );
    let credentials = load(&host).await.unwrap();
    assert!(
        matches!(credentials, Credentials::ServiceAccount(_)),
        "{credentials:?}"
    );
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
    let error = send_once(call, clients.api.get(format!("http://{address}/")))
        .await
        .unwrap_err();

    server.join().unwrap();
    error
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
    assert_eq!(token.expiry, Some(now + Duration::from_secs(60)));
}

/// A failed request's error shows no URL, as its query can hold a token.
/// Breaks if the URL is kept in the error.
#[tokio::test]
async fn a_transport_error_shows_no_url() {
    let clients = Clients::new().unwrap();
    let request = clients
        .metadata
        .get("http://127.0.0.1:1/token?token=s3cret");

    let error = send(Call::SubjectUrl, request).await.unwrap_err();

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

    let error = send(Call::SubjectUrl, clients.api.get(server.uri()))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::BodyTooLarge { .. }), "{error}");
}
