//! The upstream credential chain cases in `credential-test-data/`, run
//! against the owned chain as `aws-config`'s own test harness runs them:
//! the case's environment and files, the credentials or error text it
//! expects, and no secret in the logs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use tracing_subscriber::filter::dynamic_filter_fn;
use tracing_subscriber::layer::{Layer, SubscriberExt};

use super::super::config::Config;
use super::super::endpoint::Region;
use super::super::host::{Clock, Env, Fs, Host, Network};
use super::super::profile::{self, Location};
use super::super::replay::Replay;
use super::{Chain, Credentials, Source, error_chain};

/// Where the cases are.
pub(super) const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/aws/credential-test-data");

/// The region STS is called in when neither the environment nor the profile
/// names one, as when a registry gives none.
const DEFAULT_REGION: &str = "us-east-1";

/// Cases run with one attempt per call, as upstream runs them, since each
/// records a single answer to a call the client would otherwise retry.
const UNRETRIED: &[&str] = &["sso_server_error"];

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
    /// The case in `dir`, trying the sources of `order`.
    fn load(dir: &Path, order: &'static [Source]) -> Self {
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
            clock: if AT_THE_EPOCH.contains(&name.as_str()) {
                Clock::Fixed(UNIX_EPOCH)
            } else {
                Clock::System
            },
            network: Network::Replay(Arc::clone(&replay)),
            ..Host::fixed()
        };
        let test_case: TestCase = serde_json::from_str(&read("test-case.json")).unwrap();
        Self {
            name,
            region: default_region(&host),
            host,
            outcome: test_case.result,
            replay,
            order,
            secrets_in_logs,
        }
    }

    /// Run the case and check the answer, the logs, and that every recorded
    /// request was sent.
    async fn run(self) {
        let name = &self.name;
        let config = Config::load(&self.host, None);
        let mut retry = config.retry().unwrap();
        if UNRETRIED.contains(&name.as_str()) {
            retry.max_attempts = NonZeroU32::MIN;
        }
        let chain = Chain::new(&self.host, &self.region, config, &retry).unwrap();
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

thread_local! {
    /// The log lines of this thread's capture, `None` outside one.
    static CAPTURED: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Writes a log line into this thread's capture.
struct Captured;

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        CAPTURED.with_borrow_mut(|captured| {
            if let Some(captured) = captured {
                captured.extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Await `future` with every log line it writes at any level captured.
///
/// One global subscriber serves every test in the process, because a
/// subscriber set per test races with the others over tracing's global
/// level and callsite caches and loses lines when tests run in parallel.
/// Lines go to the capture of the thread that wrote them, so the future must
/// run on this thread, as a `current_thread` runtime does.
pub(super) async fn capture_logs<T>(future: impl Future<Output = T>) -> (T, String) {
    static SUBSCRIBER: Once = Once::new();
    SUBSCRIBER.call_once(|| {
        let capturing = dynamic_filter_fn(|_, _| CAPTURED.with_borrow(Option::is_some));
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(|| Captured)
            .with_filter(capturing);
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            .unwrap();
    });
    CAPTURED.set(Some(Vec::new()));
    let output = future.await;
    let logs = CAPTURED.take().unwrap();
    (output, String::from_utf8(logs).unwrap())
}

/// Run every case of `suite` through the sources of `order`, and check that
/// the suite holds `cases`.
async fn run_suite(suite: &str, order: &'static [Source], cases: usize) {
    let suite = Path::new(DATA).join(suite);
    let mut names: Vec<_> = std::fs::read_dir(&suite)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), cases);
    for name in &names {
        Case::load(&suite.join(name), order).run().await;
    }
}

/// Every default chain case passes through the whole chain. Time is
/// paused, so retries back off without waiting. Breaks if a source answers
/// or sends differently from upstream, or a case is dropped.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn upstream_default_chain_cases() {
    let suite = Path::new(DATA).join("default-credential-provider-chain");
    for case in AT_THE_EPOCH.iter().chain(UNRETRIED) {
        assert!(suite.join(case).is_dir(), "no case {case}");
    }
    run_suite("default-credential-provider-chain", &Source::ORDER, 29).await;
}

/// Every profile source case passes through the profile source alone, as
/// upstream runs them. Breaks if the profile resolves, assumes a role or
/// reports differently from upstream.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn upstream_profile_cases() {
    run_suite("profile-provider", &[Source::Profile], 18).await;
}

/// The harness itself: a case expecting other credentials fails. Breaks if
/// the comparison passes whatever the chain answers.
#[tokio::test(flavor = "current_thread")]
#[should_panic(expected = "environment_variables")]
async fn a_wrong_answer_fails_the_case() {
    let mut case = Case::load(
        &Path::new(DATA).join("default-credential-provider-chain/environment_variables"),
        &Source::ORDER,
    );
    case.outcome = Outcome::Ok(Expected {
        access_key_id: "other".to_owned(),
        secret_access_key: "other".to_owned(),
        session_token: None,
        expiry: None,
    });
    case.run().await;
}

/// The capture holds what the awaited future logs at any level, and nothing
/// another thread logs meanwhile or this thread logs outside it. Every line
/// comes from one log site, as when parallel tests share one. Breaks if a
/// site first reached outside a capture stays unheard inside one, a level is
/// filtered out, or lines reach a capture they were not written in.
#[tokio::test(flavor = "current_thread")]
async fn captures_only_the_awaited_futures_lines() {
    fn log(line: &str) {
        tracing::trace!("{line}");
    }
    // Installs the subscriber, as an earlier test would.
    capture_logs(async {}).await;
    log("before the capture");
    let ((), logs) = capture_logs(async {
        log("inside the capture");
        std::thread::spawn(|| log("on another thread"))
            .join()
            .unwrap();
    })
    .await;
    log("after the capture");
    assert!(logs.contains("inside the capture"), "{logs}");
    for line in [
        "before the capture",
        "on another thread",
        "after the capture",
    ] {
        assert!(!logs.contains(line), "{line}: {logs}");
    }
}
