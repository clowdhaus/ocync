//! Shared helpers for integration tests against a local registry (registry:2).

use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::{Digest, RegistryClient};
use url::Url;

/// Compute the SHA-256 digest for test data.
pub fn test_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// Start a local registry container and return its HTTP base URL.
pub async fn start_registry() -> (
    testcontainers::ContainerAsync<testcontainers::GenericImage>,
    Url,
) {
    use testcontainers::GenericImage;
    use testcontainers::runners::AsyncRunner;

    let container = GenericImage::new("registry", "2")
        .with_exposed_port(5000.into())
        .with_wait_for(testcontainers::core::WaitFor::message_on_stderr(
            "listening on",
        ))
        .start()
        .await
        .expect("failed to start registry container");

    let port = container
        .get_host_port_ipv4(5000)
        .await
        .expect("failed to get mapped port");

    let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
    (container, url)
}

/// Build a `RegistryClient` for a local registry (no auth, no TLS).
pub fn local_client(url: Url) -> RegistryClient {
    RegistryClientBuilder::new(url)
        .build()
        .expect("failed to build RegistryClient")
}
