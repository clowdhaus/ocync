//! Tests of where the Application Default Credentials come from and how a
//! file's `type` selects them, ported from upstream's `credentials.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use super::service_account::tests::{key_file, pkcs8_pem};
use super::*;

/// A host with `vars` and `files`.
pub(super) fn host(platform: Platform, vars: &[(&str, &str)], files: &[(&str, Vec<u8>)]) -> Host {
    Host {
        env: Env::Fixed(
            vars.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        ),
        fs: Fs::Fixed(
            files
                .iter()
                .map(|(path, contents)| (PathBuf::from(path), contents.clone()))
                .collect::<HashMap<_, _>>(),
        ),
        platform,
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

/// Upstream `adc_path_from_env` and `load_adc_success`: the file the
/// variable names, before the well-known file. Breaks if the variable is
/// read after the well-known file.
#[tokio::test]
async fn reads_the_file_the_variable_names() {
    let host = host(
        Platform::Unix,
        &[(CREDENTIALS_FILE, "/keys/sa.json"), ("HOME", "/home/u")],
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
        &[(CREDENTIALS_FILE, "/keys/none.json")],
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
            &[(CREDENTIALS_FILE, "/keys/adc.json")],
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
