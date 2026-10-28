//! Shared helpers for integration tests against a local registry (registry:2).

#![allow(dead_code, reason = "each test binary uses only some of these helpers")]

use std::collections::HashMap;
use std::time::Duration;

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

/// The `key=value` label filter for containers labeled `value`.
pub fn label(value: &str) -> String {
    format!("{LABEL_KEY}={value}")
}

/// testcontainers' fixed timeout on each Docker request.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// testcontainers' timeout on starting a created container.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest a test container can sit between Docker's creation time and
/// running: the create, two file copies (the TLS registry's certificate and
/// key) and the start, each to its timeout. About 7 minutes.
const LONGEST_START: Duration = CLIENT_TIMEOUT
    .saturating_mul(3)
    .saturating_add(START_TIMEOUT);

/// Longer than any test runs.
pub const LONGEST_TEST: Duration = Duration::from_secs(60 * 60);

/// How old a labeled container must be to count as left behind.
#[derive(Debug, Clone, Copy)]
pub struct OrphanAge {
    /// For a container that is not running (created, exited or dead). A
    /// create or start that times out under load is still finished by
    /// Docker later, with no handle left in the test to remove it.
    pub stopped: Duration,
    /// For any container, running or not: one whose start finished after
    /// the test gave up on it is left running.
    pub any_state: Duration,
}

/// What a test run sweeps: stopped containers well past the longest start,
/// and any past the longest test.
const ORPHANS: OrphanAge = OrphanAge {
    stopped: LONGEST_START.saturating_mul(2),
    any_state: LONGEST_TEST,
};

/// Why a sweep failed.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    /// No Docker client, found as testcontainers finds the daemon.
    #[error("connecting to Docker: {0}")]
    Connect(#[source] testcontainers::core::client::ClientError),
    /// Docker refused a list or a removal.
    #[error("Docker: {0}")]
    Docker(#[source] bollard::errors::Error),
}

/// Remove the containers matching the label `filter` (`key` or
/// `key=value`) that `ages` counts as left behind, returning how many were
/// removed. Ages are measured on this host's clock against Docker's
/// creation times, so they assume the daemon's clock agrees, as a local
/// daemon's does.
pub async fn sweep(filter: &str, ages: OrphanAge) -> Result<usize, SweepError> {
    use bollard::query_parameters::{ListContainersOptions, RemoveContainerOptions};

    let docker = testcontainers::core::client::docker_client_instance()
        .await
        .map_err(SweepError::Connect)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past 1970");
    let created_before = |age: Duration| (now.saturating_sub(age).as_secs()) as i64;
    let mut removed = 0;
    for (statuses, age) in [
        (&["created", "exited", "dead"][..], ages.stopped),
        (&[][..], ages.any_state),
    ] {
        let mut filters = HashMap::from([("label".to_owned(), vec![filter.to_owned()])]);
        if !statuses.is_empty() {
            filters.insert(
                "status".to_owned(),
                statuses.iter().map(|&status| status.to_owned()).collect(),
            );
        }
        let containers = docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .map_err(SweepError::Docker)?;
        for container in containers {
            let (Some(id), Some(created)) = (container.id, container.created) else {
                continue;
            };
            if created > created_before(age) {
                continue;
            }
            let options = RemoveContainerOptions {
                force: true,
                ..Default::default()
            };
            match docker.remove_container(&id, Some(options)).await {
                Ok(()) => removed += 1,
                // Another run removed it first.
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404 | 409,
                    ..
                }) => {}
                Err(e) => return Err(SweepError::Docker(e)),
            }
        }
    }
    Ok(removed)
}

/// Set once a process has run its orphan sweep.
static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Whether this process has run its orphan sweep.
pub fn swept() -> bool {
    SWEPT.initialized()
}

/// Sweep up the test containers earlier runs left behind, whatever their
/// label's value, once per process. The sweep only tidies, so a failure
/// warns and every test goes on. It writes to stderr directly, as the test
/// harness hides what a passing test prints.
async fn sweep_orphans() {
    use std::io::Write as _;

    SWEPT
        .get_or_init(|| async {
            let _ = match sweep(LABEL_KEY, ORPHANS).await {
                Ok(0) => Ok(()),
                Ok(removed) => writeln!(
                    std::io::stderr(),
                    "removed {removed} leftover test containers"
                ),
                Err(e) => writeln!(
                    std::io::stderr(),
                    "warning: sweeping leftover test containers failed: {e}"
                ),
            };
        })
        .await;
}

/// A `registry:2` container listening on port 5000.
fn registry() -> testcontainers::ContainerRequest<testcontainers::GenericImage> {
    use testcontainers::{GenericImage, ImageExt};

    GenericImage::new("registry", "2")
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
