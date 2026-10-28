//! Real `registry:2` containers for tests that need a real registry's wire
//! behavior, such as HTTP/2 flow control, and a corpus to fill them with.

use bytes::Bytes;
use futures_util::stream;
use ocync_distribution::sha256::Sha256;
use ocync_distribution::spec::{MediaType, RepositoryName};
use ocync_distribution::{Digest, RegistryClient};
use rcgen::{CertificateParams, DnType, KeyPair};
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage, ImageExt};
use url::Url;

use super::builders::ManifestBuilder;

/// A `registry:2` container listening on port 5000.
fn registry() -> ContainerRequest<GenericImage> {
    GenericImage::new("registry", "2")
        .with_exposed_port(5000.into())
        .with_wait_for(WaitFor::message_on_stderr("listening on"))
        // Without an init process to reap it, the registry can be left a
        // zombie Docker cannot kill, and the container is never removed.
        .with_host_config_modifier(|config| config.init = Some(true))
}

/// Starts `request` and returns the container with its `scheme` base URL.
async fn start(
    request: ContainerRequest<GenericImage>,
    scheme: &str,
) -> (ContainerAsync<GenericImage>, Url) {
    let container = request
        .start()
        .await
        .expect("registry:2 container failed to start");
    let port = container
        .get_host_port_ipv4(5000)
        .await
        .expect("get_host_port_ipv4 failed");
    let url = Url::parse(&format!("{scheme}://127.0.0.1:{port}")).unwrap();
    (container, url)
}

/// A `registry:2` serving plain HTTP/1.1.
pub async fn start_registry_http() -> (ContainerAsync<GenericImage>, Url) {
    start(registry(), "http").await
}

/// Self-signed cert covering `localhost` + `127.0.0.1`, valid for the
/// run of the test. Generated per test invocation so there's no
/// on-disk artifact to manage.
fn self_signed_localhost() -> (Vec<u8>, Vec<u8>) {
    let key = KeyPair::generate().expect("rcgen KeyPair::generate");
    let mut params = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .expect("rcgen CertificateParams::new");
    params
        .distinguished_name
        .push(DnType::CommonName, "ocync-perf-profile");
    let cert = params.self_signed(&key).expect("rcgen self_signed");
    (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
}

/// A `registry:2` serving TLS with a self-signed certificate, so a client
/// negotiates HTTP/2 with it.
pub async fn start_registry_tls() -> (ContainerAsync<GenericImage>, Url) {
    let (cert_pem, key_pem) = self_signed_localhost();
    let request = registry()
        .with_env_var("REGISTRY_HTTP_TLS_CERTIFICATE", "/certs/tls.crt")
        .with_env_var("REGISTRY_HTTP_TLS_KEY", "/certs/tls.key")
        .with_copy_to("/certs/tls.crt", cert_pem)
        .with_copy_to("/certs/tls.key", key_pem);
    start(request, "https").await
}

async fn push_blob_stream(client: &RegistryClient, repo: &RepositoryName, data: Vec<u8>) {
    let digest = Digest::from_sha256(Sha256::digest(&data));
    let size = data.len() as u64;
    let body = Bytes::from(data);
    let s = stream::once(async move { Ok::<_, ocync_distribution::Error>(body) });
    client
        .blob_push_stream(repo, &digest, Some(size), s)
        .await
        .expect("blob_push_stream");
}

/// The content of layer `l` of image `i` in [`populate_source`]: zeros,
/// varied per (image, layer) so digests are unique.
pub fn layer(i: usize, l: usize, layer_bytes: usize) -> Vec<u8> {
    let mut layer = vec![0u8; layer_bytes];
    layer[0] = (i & 0xff) as u8;
    layer[1] = (l & 0xff) as u8;
    layer
}

/// Pushes `n_images` images, each a config and `n_layers` distinct layers
/// of `layer_bytes`, tagged `v1` in repositories `perf/img-NNNN`.
pub async fn populate_source(
    client: &RegistryClient,
    n_images: usize,
    n_layers: usize,
    layer_bytes: usize,
) -> Vec<RepositoryName> {
    let mut repos = Vec::with_capacity(n_images);
    for i in 0..n_images {
        let repo = RepositoryName::new(format!("perf/img-{i:04}")).unwrap();
        let config_data = format!("{{\"image\":{i}}}").into_bytes();
        // Stream the corpus into the source registry the same way the
        // engine does during sync; otherwise the harness's setup-time SHA
        // dominates the profile and the actual streaming path is invisible.
        push_blob_stream(client, &repo, config_data.clone()).await;

        let mut builder = ManifestBuilder::new(&config_data);
        for l in 0..n_layers {
            let layer = layer(i, l, layer_bytes);
            push_blob_stream(client, &repo, layer.clone()).await;
            builder = builder.layer(&layer);
        }
        let parts = builder.build();
        client
            .manifest_push(&repo, "v1", &MediaType::OciManifest, &parts.bytes)
            .await
            .expect("manifest push");
        repos.push(repo);
    }
    repos
}
