//! Shared helpers for integration tests against a local registry (registry:2).

#![allow(dead_code, reason = "each test binary uses only some of these helpers")]

use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::{Digest, RegistryClient};
use url::Url;

/// Compute the SHA-256 digest for test data.
pub fn test_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
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
