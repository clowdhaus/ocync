//! Shared helpers for integration tests against a local registry (registry:2).

#![allow(dead_code, reason = "each test binary uses only some of these helpers")]

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::{Digest, RegistryClient};
use testcontainers::bollard;
use url::Url;

/// Compute the SHA-256 digest for test data.
pub fn test_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// The label key on every container these tests start, so leftovers can
/// be found without touching another project's containers. As a filter it
/// matches the key with any value.
pub const LABEL_KEY: &str = "dev.ocync.test";

/// The label value on the registry containers. Descriptive only: the
/// orphan sweep matches [`LABEL_KEY`] with any value.
const REGISTRY_LABEL_VALUE: &str = "registry";

/// The name of the registry image the tests run.
pub const REGISTRY_NAME: &str = "registry";

/// The tag of the registry image the tests run.
pub const REGISTRY_TAG: &str = "2";

/// The `key=value` label filter for containers labeled `value`.
pub fn label(value: &str) -> String {
    format!("{LABEL_KEY}={value}")
}

/// testcontainers 0.28's fixed timeout on each Docker request. Not
/// exported, so an upgrade rechecks it.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// testcontainers 0.28's timeout on starting a created container. Not
/// exported, so an upgrade rechecks it.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest a test container can sit between Docker's creation time and
/// running: the create, two file copies (the TLS registry's certificate and
/// key) and the start, each to its timeout. About 7 minutes.
const LONGEST_START: Duration = CLIENT_TIMEOUT
    .saturating_mul(3)
    .saturating_add(START_TIMEOUT);

/// Longer than any test run, a profiling run included, so a sweep never
/// removes a registry another run is still using.
pub const LONGEST_RUN: Duration = Duration::from_secs(24 * 60 * 60);

/// How old a labeled container must be to count as left behind.
#[derive(Debug, Clone, Copy)]
pub struct OrphanAge {
    /// For a container that is not running (created, exited or dead). A
    /// create or start that times out under load is still finished by
    /// Docker later, with no handle left in the test to remove it.
    pub stopped: Duration,
    /// For a container in any other state: one whose start finished after
    /// the test gave up on it is left running.
    pub any_state: Duration,
}

/// What a test run sweeps: stopped containers well past the longest start,
/// and any past the longest run.
const ORPHANS: OrphanAge = OrphanAge {
    stopped: LONGEST_START.saturating_mul(2),
    any_state: LONGEST_RUN,
};

/// Docker's answer for a container or image that does not exist.
pub const NOT_FOUND: u16 = reqwest::StatusCode::NOT_FOUND.as_u16();

/// Docker's answer to force-removing a container whose removal is already
/// in progress.
const CONFLICT: u16 = reqwest::StatusCode::CONFLICT.as_u16();

/// Why a sweep failed.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    /// No Docker client, found as testcontainers finds the daemon.
    #[error("connecting to Docker: {0}")]
    Connect(#[source] testcontainers::core::client::ClientError),
    /// Docker refused to list the containers.
    #[error("listing containers: {0}")]
    List(#[source] bollard::errors::Error),
}

/// What a sweep removed, and the removals Docker refused.
#[derive(Debug, Default)]
pub struct Swept {
    /// How many containers were removed.
    pub removed: usize,
    /// Each container Docker would not remove, such as one stuck as a
    /// zombie, with why. The others are still removed.
    pub failed: Vec<(String, bollard::errors::Error)>,
}

/// The containers matching the label `filter` (`key` or `key=value`), in
/// any state.
pub async fn labeled(
    docker: &bollard::Docker,
    filter: &str,
) -> Result<Vec<bollard::models::ContainerSummary>, bollard::errors::Error> {
    use bollard::query_parameters::ListContainersOptions;

    docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "label".to_owned(),
                vec![filter.to_owned()],
            )])),
            ..Default::default()
        }))
        .await
}

/// Whether a container in `state`, created at `created` seconds past the
/// epoch, counts as left behind at `now` under `ages`. A creation time
/// that cannot be read never counts as old.
pub fn is_orphan(
    state: Option<bollard::models::ContainerSummaryStateEnum>,
    created: i64,
    ages: OrphanAge,
    now: SystemTime,
) -> bool {
    use bollard::models::ContainerSummaryStateEnum as State;

    let threshold = match state {
        Some(State::CREATED | State::EXITED | State::DEAD) => ages.stopped,
        _ => ages.any_state,
    };
    u64::try_from(created)
        .ok()
        .and_then(|created| UNIX_EPOCH.checked_add(Duration::from_secs(created)))
        .and_then(|created| created.checked_add(threshold))
        .is_some_and(|due| due <= now)
}

/// Remove the containers matching the label `filter` (`key` or
/// `key=value`) that `ages` counts as left behind at `now`. Ages are
/// measured against Docker's creation times, so they assume the daemon's
/// clock agrees with `now`'s, as a native daemon's does; a VM daemon whose
/// clock drifts past the stopped age can lose a container still starting.
pub async fn sweep(filter: &str, ages: OrphanAge, now: SystemTime) -> Result<Swept, SweepError> {
    use bollard::query_parameters::RemoveContainerOptions;

    let docker = testcontainers::core::client::docker_client_instance()
        .await
        .map_err(SweepError::Connect)?;
    let containers = labeled(&docker, filter).await.map_err(SweepError::List)?;
    let mut swept = Swept::default();
    for container in containers {
        let (Some(id), Some(created)) = (container.id, container.created) else {
            continue;
        };
        if !is_orphan(container.state, created, ages, now) {
            continue;
        }
        let options = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        match docker.remove_container(&id, Some(options)).await {
            Ok(()) => swept.removed += 1,
            // Already gone, or already being removed by another run.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: NOT_FOUND | CONFLICT,
                ..
            }) => {}
            Err(e) => swept.failed.push((id, e)),
        }
    }
    Ok(swept)
}

/// Set once a process has run its orphan sweep.
static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Whether this process has run its orphan sweep.
pub fn swept() -> bool {
    SWEPT.initialized()
}

/// Whether the `TESTCONTAINERS_COMMAND` value `command` asks for
/// containers to be kept for debugging, as testcontainers reads it.
pub fn keeps(command: Option<&str>) -> bool {
    command == Some("keep")
}

/// Sweep up the test containers earlier runs left behind, whatever their
/// label's value, once per process, unless [`keeps`] says to keep them.
/// The sweep only tidies,
/// so a failure warns and every test goes on. It writes to stderr
/// directly, as the test harness hides what a passing test prints.
async fn sweep_orphans() {
    use std::io::Write as _;

    SWEPT
        .get_or_init(|| async {
            if keeps(std::env::var("TESTCONTAINERS_COMMAND").ok().as_deref()) {
                return;
            }
            let mut stderr = std::io::stderr();
            match sweep(LABEL_KEY, ORPHANS, SystemTime::now()).await {
                Ok(swept) => {
                    if swept.removed > 0 {
                        let _ =
                            writeln!(stderr, "removed {} leftover test containers", swept.removed);
                    }
                    for (id, e) in swept.failed {
                        let _ = writeln!(
                            stderr,
                            "warning: leftover test container {id} not removed: {e}"
                        );
                    }
                }
                Err(e) => {
                    let _ = writeln!(
                        stderr,
                        "warning: sweeping leftover test containers failed: {e}"
                    );
                }
            }
        })
        .await;
}

/// A `registry:2` container listening on port 5000.
fn registry() -> testcontainers::ContainerRequest<testcontainers::GenericImage> {
    use testcontainers::{GenericImage, ImageExt};

    GenericImage::new(REGISTRY_NAME, REGISTRY_TAG)
        .with_exposed_port(5000.into())
        .with_wait_for(testcontainers::core::WaitFor::message_on_stderr(
            "listening on",
        ))
        // Without an init process to reap it, the registry can be left a
        // zombie Docker cannot kill, and the container is never removed.
        .with_host_config_modifier(|config| config.init = Some(true))
        .with_label(LABEL_KEY, REGISTRY_LABEL_VALUE)
}

/// Start `request` and return the container with its `scheme` base URL.
async fn start(
    request: testcontainers::ContainerRequest<testcontainers::GenericImage>,
    scheme: &str,
) -> (
    testcontainers::ContainerAsync<testcontainers::GenericImage>,
    Url,
) {
    use testcontainers::runners::AsyncRunner;

    sweep_orphans().await;
    let container = request
        .start()
        .await
        .expect("failed to start registry container");
    let port = container
        .get_host_port_ipv4(5000)
        .await
        .expect("failed to get mapped port");
    let url = Url::parse(&format!("{scheme}://127.0.0.1:{port}")).unwrap();
    (container, url)
}

/// Start a local registry container and return its HTTP base URL.
pub async fn start_registry() -> (
    testcontainers::ContainerAsync<testcontainers::GenericImage>,
    Url,
) {
    start(registry(), "http").await
}

/// Start a local registry container serving TLS with a self-signed
/// certificate for `localhost` and `127.0.0.1`, generated per call, and
/// return its HTTPS base URL. A client that accepts the certificate
/// negotiates HTTP/2 with it.
pub async fn start_registry_tls() -> (
    testcontainers::ContainerAsync<testcontainers::GenericImage>,
    Url,
) {
    use rcgen::{CertificateParams, DnType, KeyPair};
    use testcontainers::ImageExt;

    let key = KeyPair::generate().expect("rcgen KeyPair::generate");
    let mut params = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .expect("rcgen CertificateParams::new");
    params
        .distinguished_name
        .push(DnType::CommonName, "ocync-test-registry");
    let cert = params.self_signed(&key).expect("rcgen self_signed");
    let request = registry()
        .with_env_var("REGISTRY_HTTP_TLS_CERTIFICATE", "/certs/tls.crt")
        .with_env_var("REGISTRY_HTTP_TLS_KEY", "/certs/tls.key")
        .with_copy_to("/certs/tls.crt", cert.pem().into_bytes())
        .with_copy_to("/certs/tls.key", key.serialize_pem().into_bytes());
    start(request, "https").await
}

/// Build a `RegistryClient` for a local registry (no auth, no TLS).
pub fn local_client(url: Url) -> RegistryClient {
    RegistryClientBuilder::new(url)
        .build()
        .expect("failed to build RegistryClient")
}
