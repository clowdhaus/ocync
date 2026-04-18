//! Integration tests against a local OCI Distribution registry (registry:2).
//!
//! These tests use `testcontainers` to spin up a real CNCF Distribution
//! registry and exercise the full push/pull/HEAD protocol. They catch
//! status code mismatches, auth header formatting, and protocol errors
//! that wiremock mocks can't surface.
//!
//! Mount-specific protocol tests live in the sibling `registry2_mount.rs`.
//!
//! Requirements:
//! - Docker must be running
//! - Run with: `cargo test --package ocync-distribution --test registry2_client`

use bytes::Bytes;
use futures_util::stream;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType, RepositoryName};
use ocync_distribution::{Digest, Error, RegistryClient};
use url::Url;

/// Compute the SHA-256 digest for test data.
fn test_digest(data: &[u8]) -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(data);
    Digest::from_sha256(hash)
}

/// Start a local registry container and return its HTTP base URL.
async fn start_registry() -> (
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
fn local_client(url: Url) -> RegistryClient {
    RegistryClientBuilder::new(url)
        .build()
        .expect("failed to build RegistryClient")
}

/// Build a minimal valid OCI image manifest.
fn build_manifest(
    config_digest: &Digest,
    config_size: u64,
    layer_digest: &Digest,
    layer_size: u64,
) -> Vec<u8> {
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(MediaType::OciManifest),
        config: Descriptor {
            media_type: MediaType::from("application/vnd.oci.image.config.v1+json"),
            digest: config_digest.clone(),
            size: config_size,
            platform: None,
            artifact_type: None,
            annotations: None,
        },
        layers: vec![Descriptor {
            media_type: MediaType::from("application/vnd.oci.image.layer.v1.tar+gzip"),
            digest: layer_digest.clone(),
            size: layer_size,
            platform: None,
            artifact_type: None,
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    serde_json::to_vec(&manifest).expect("serialize manifest")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify the client can ping `/v2/` on a real registry.
#[tokio::test]
async fn ping() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    client.ping().await.expect("ping /v2/ failed");
}

/// Push a small blob via the monolithic (POST+PUT) path and verify it exists.
#[tokio::test]
async fn blob_push_monolithic_and_exists() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/monolithic");

    let data = b"hello from ocync integration test";
    let digest = client
        .blob_push(&repo, data)
        .await
        .expect("blob_push failed");

    assert_eq!(digest, test_digest(data));

    // Verify the blob exists with correct size.
    let size = client
        .blob_exists(&repo, &digest)
        .await
        .expect("blob_exists failed")
        .expect("blob should exist after push");
    assert_eq!(size, data.len() as u64);
}

/// Push a blob via the streaming upload path.
#[tokio::test]
async fn blob_push_stream_chunked() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/streamed");

    let data = vec![0xABu8; 2 * 1024 * 1024];
    let expected_digest = test_digest(&data);
    let data_len = data.len() as u64;

    let body = Bytes::from(data);
    let stream = stream::once(async move { Ok::<_, Error>(body) });
    let digest = client
        .blob_push_stream(&repo, &expected_digest, Some(data_len), stream)
        .await
        .expect("blob_push_stream failed");

    assert_eq!(digest, expected_digest);

    let size = client
        .blob_exists(&repo, &digest)
        .await
        .expect("blob_exists failed")
        .expect("blob should exist after stream push");
    assert_eq!(size, data_len);
}

/// Verify `blob_exists` returns `None` for a nonexistent blob.
#[tokio::test]
async fn blob_exists_returns_none_for_missing() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/missing");

    let fake_digest: Digest =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();

    let result = client
        .blob_exists(&repo, &fake_digest)
        .await
        .expect("blob_exists should not error for missing blob");
    assert!(result.is_none());
}

/// Full round-trip: push config blob, push layer blob, push manifest, then
/// HEAD and GET the manifest back. Verifies the entire push/pull protocol
/// against a real registry.
#[tokio::test]
async fn full_image_roundtrip() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/roundtrip");
    let tag = "v1";

    // 1. Push config blob.
    let config_data =
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
    let config_digest = client
        .blob_push(&repo, config_data.as_slice())
        .await
        .expect("config blob push failed");

    // 2. Push layer blob.
    let layer_data = b"fake-layer-content-for-integration-test";
    let layer_digest = client
        .blob_push(&repo, layer_data.as_slice())
        .await
        .expect("layer blob push failed");

    // 3. Build and push manifest.
    let manifest_bytes = build_manifest(
        &config_digest,
        config_data.len() as u64,
        &layer_digest,
        layer_data.len() as u64,
    );
    let manifest_digest = client
        .manifest_push(&repo, tag, &MediaType::OciManifest, &manifest_bytes)
        .await
        .expect("manifest push failed");

    // 4. HEAD the manifest by tag -- verify digest and media type.
    let head = client
        .manifest_head(&repo, tag)
        .await
        .expect("manifest HEAD failed")
        .expect("manifest HEAD should return Some for existing tag");
    assert_eq!(head.digest, manifest_digest);

    // 5. Pull the manifest by digest -- verify raw bytes match.
    let pulled = client
        .manifest_pull(&repo, &manifest_digest.to_string())
        .await
        .expect("manifest pull failed");
    assert_eq!(pulled.raw_bytes, manifest_bytes);
    assert_eq!(pulled.digest, manifest_digest);
}

/// Verify `manifest_head` returns `None` for a nonexistent tag.
#[tokio::test]
async fn manifest_head_returns_none_for_missing_tag() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/notfound");

    let result = client
        .manifest_head(&repo, "nonexistent-tag")
        .await
        .expect("manifest HEAD should not error for missing tag");

    assert!(result.is_none(), "expected None for missing tag");
}

/// Push the same blob twice and verify idempotency -- no error on second push.
#[tokio::test]
async fn blob_push_idempotent() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/idempotent");

    let data = b"push me twice";
    let d1 = client.blob_push(&repo, data).await.expect("first push");
    let d2 = client.blob_push(&repo, data).await.expect("second push");
    assert_eq!(d1, d2);
}

/// Push blobs and manifest, then pull individual blobs back and verify content.
#[tokio::test]
async fn blob_pull_roundtrip() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/blobpull");

    let data = b"blob-pull-roundtrip-data";
    let digest = client.blob_push(&repo, data).await.expect("push failed");

    let stream = client
        .blob_pull(&repo, &digest)
        .await
        .expect("blob_pull failed");

    use futures_util::StreamExt;
    let mut body = Vec::new();
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.expect("stream chunk error"));
    }
    assert_eq!(body, data);
}
