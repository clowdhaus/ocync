//! The upstream credential chain cases in `credential-test-data/`, run
//! against the owned chain as `aws-config`'s own test harness runs them:
//! the case's environment and files, the credentials or error text it
//! expects, and no secret in the logs.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use super::super::config::Config;
use super::super::endpoint::Region;
use super::super::host::{Clock, Env, Fs, Host, Network, Platform};
use super::super::profile::{self, Location};
use super::super::replay::Replay;
use super::{Chain, Credentials, Source, error_chain};

/// Where the cases are.
pub(super) const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/aws/credential-test-data");

/// The region STS is called in when neither the environment nor the profile
/// names one, as when a registry gives none.
const DEFAULT_REGION: &str = "us-east-1";

/// The default chain cases not run, by directory name: each reaches a
/// source loaded through `aws-config`, which reads the real host.
const EXCLUDED: &[&str] = &[
    "e2e_fips_and_dual_stack_sso",
    "ecs_assume_role",
    "ecs_credentials",
    "ecs_credentials_invalid_profile",
    "eks_pod_identity_credentials",
    "eks_pod_identity_no_token_file",
    "environment_variables_blank",
    "imds_assume_role",
    "imds_no_iam_role",
    "imds_token_fail",
    "profile_name",
    "profile_overrides_web_identity",
    "profile_static_keys",
    "profile_static_keys_case_insensitive",
    "sso_assume_role",
    "sso_no_token_file",
    "sso_server_error",
    "web_identity_source_profile_no_env",
    "web_identity_token_env",
    "web_identity_token_invalid_jwt",
    "web_identity_token_profile",
    "web_identity_token_source_profile",
];

/// Cases that start at a later source than the environment, by directory
/// name: the sources before it are loaded through `aws-config`, and these
/// cases expect each of them not to be configured.
const STARTS_AT: &[(&str, Source)] = &[
    ("imds_config_with_no_creds", Source::Imds),
    ("imds_default_chain_error", Source::Imds),
    ("imds_default_chain_retries", Source::Imds),
    ("imds_default_chain_success", Source::Imds),
    ("imds_disabled", Source::Imds),
];

/// Cases run at the UNIX epoch, as upstream runs them, so the expiry they
/// record has not passed.
const AT_THE_EPOCH: &[&str] = &[
    "imds_config_with_no_creds",
    "imds_default_chain_retries",
    "imds_default_chain_success",
];

/// The credentials a case expects. The account ID upstream also checks is
/// left out: the owned chain does not carry it.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Expected {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    /// Seconds since the UNIX epoch.
    expiry: Option<u64>,
}

impl From<&Credentials> for Expected {
    fn from(credentials: &Credentials) -> Self {
        Self {
            access_key_id: credentials.access_key_id.clone(),
            secret_access_key: credentials.secret_access_key.clone(),
            session_token: credentials.session_token.clone(),
            expiry: credentials.expiry.map(|t| {
                t.duration_since(UNIX_EPOCH)
                    .expect("an expiry after 1970")
                    .as_secs()
            }),
        }
    }
}

/// What a case expects the chain to answer.
#[derive(Debug, Deserialize)]
enum Outcome {
    /// These credentials.
    Ok(Expected),
    /// An error whose text, sources included, holds this.
    ErrorContains(String),
}

/// A case's `test-case.json`.
#[derive(Debug, Deserialize)]
struct TestCase {
    result: Outcome,
}

/// One case, ready to run.
struct Case {
    name: String,
    host: Host,
    region: Region,
    outcome: Outcome,
    /// The case's recorded HTTP traffic, which the host's network answers.
    replay: Arc<Replay>,
    /// The sources the chain tries.
    order: &'static [Source],
    /// The case allows secrets in the logs, as `EXPOSE_SECRETS_FOR_TESTS`
    /// does upstream.
    secrets_in_logs: SecretsInLogs,
}

/// Whether a case's secrets may appear in the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretsInLogs {
    Allowed,
    Forbidden,
}

impl Case {
    fn load(dir: &Path) -> Self {
        let read = |file: &str| {
            let path = dir.join(file);
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        };
        let vars: HashMap<String, String> = serde_json::from_str(&read("env.json")).unwrap();
        let secrets_in_logs = if vars.contains_key("EXPOSE_SECRETS_FOR_TESTS") {
            SecretsInLogs::Allowed
        } else {
            SecretsInLogs::Forbidden
        };
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let mut files = HashMap::new();
        read_tree(&dir.join("fs"), Path::new("/"), &mut files);
        let replay = Arc::new(Replay::from_json(&read("http-traffic.json")));
        let host = Host {
            env: Env::Fixed(vars),
            fs: Fs::Fixed(files),
            platform: Platform::Unix,
            clock: if AT_THE_EPOCH.contains(&name.as_str()) {
                Clock::Fixed(UNIX_EPOCH)
            } else {
                Clock::System
            },
            network: Network::Replay(Arc::clone(&replay)),
        };
        let start = STARTS_AT
            .iter()
            .find(|(case, _)| *case == name)
            .map_or(Source::Environment, |&(_, source)| source);
        let all: &'static [Source] = &Source::ORDER;
        let at = all.iter().position(|&source| source == start).unwrap();
        let test_case: TestCase = serde_json::from_str(&read("test-case.json")).unwrap();
        Self {
            name,
            region: default_region(&host),
            host,
            outcome: test_case.result,
            replay,
            order: &all[at..],
            secrets_in_logs,
        }
    }

    /// Run the case and check the answer, the logs, and that every recorded
    /// request was sent.
    async fn run(self) {
        let name = &self.name;
        let config = Config::load(&self.host, None);
        let chain = Chain::new(&self.host, &self.region, &config).unwrap();
        let (result, logs) = capture_logs(chain.load_in(self.order)).await;
        self.replay.assert_done();
        match (&self.outcome, &result) {
            (Outcome::Ok(expected), Ok(loaded)) => {
                assert_eq!(&Expected::from(loaded), expected, "{name}");
            }
            (Outcome::ErrorContains(text), Err(error)) => {
                let message = error_chain(error);
                assert!(message.contains(text.as_str()), "{name}: {message}");
            }
            (expected, got) => panic!("{name}: expected {expected:?}, got {got:?}"),
        }
        if let (Outcome::Ok(expected), SecretsInLogs::Forbidden) =
            (&self.outcome, self.secrets_in_logs)
        {
            assert!(
                !logs.contains(&expected.secret_access_key),
                "{name}: the secret key reached the logs:\n{logs}"
            );
        }
    }
}

/// Every file under `dir`, keyed by its path under `root`.
fn read_tree(dir: &Path, root: &Path, files: &mut HashMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = root.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            read_tree(&entry.path(), &path, files);
        } else {
            files.insert(path, std::fs::read(entry.path()).unwrap());
        }
    }
}

/// The region upstream's harness resolves for the case: `AWS_REGION`, then
/// `AWS_DEFAULT_REGION`, then the selected profile's `region`.
fn default_region(host: &Host) -> Region {
    let from_profile = || {
        let location = Location::find(&host.env, host.platform);
        let sections = profile::load(&host.fs, &location).ok()?;
        sections
            .profiles
            .get(&location.profile)?
            .get("region")
            .cloned()
    };
    let region = host
        .env
        .get("AWS_REGION")
        .or_else(|| host.env.get("AWS_DEFAULT_REGION"))
        .or_else(from_profile)
        .unwrap_or_else(|| DEFAULT_REGION.to_owned());
    Region::parse(&region).unwrap()
}

/// Await `future` with every log line at any level captured.
async fn capture_logs<T>(future: impl Future<Output = T>) -> (T, String) {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let output = future.await;
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    (output, logs)
}

/// Every default chain case not excluded passes, and every case is either
/// run or excluded with a reason. Time is paused, so retries back off
/// without waiting. Breaks if a source answers or sends differently from
/// upstream, or a case is dropped or excluded twice.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn upstream_default_chain_cases() {
    let suite = Path::new(DATA).join("default-credential-provider-chain");
    let mut names: Vec<_> = std::fs::read_dir(&suite)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let listed = EXCLUDED
        .iter()
        .chain(AT_THE_EPOCH)
        .chain(STARTS_AT.iter().map(|(name, _)| name));
    for case in listed {
        assert!(names.iter().any(|n| n == case), "no case {case}");
    }
    let mut run = 0;
    for name in &names {
        if EXCLUDED.contains(&name.as_str()) {
            continue;
        }
        Case::load(&suite.join(name)).run().await;
        run += 1;
    }
    assert_eq!(run + EXCLUDED.len(), names.len());
    assert_eq!(names.len(), 29);
}

/// The harness itself: a case expecting other credentials fails. Breaks if
/// the comparison passes whatever the chain answers.
#[tokio::test(flavor = "current_thread")]
#[should_panic(expected = "environment_variables")]
async fn a_wrong_answer_fails_the_case() {
    let mut case = Case::load(
        &Path::new(DATA).join("default-credential-provider-chain/environment_variables"),
    );
    case.outcome = Outcome::Ok(Expected {
        access_key_id: "other".to_owned(),
        secret_access_key: "other".to_owned(),
        session_token: None,
        expiry: None,
    });
    case.run().await;
}
