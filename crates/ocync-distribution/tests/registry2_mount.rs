//! Cross-repo blob mount protocol tests against the OCI reference
//! `registry:2` image (CNCF Distribution).
//!
//! These are Layer 1 protocol tests per `docs/specs/benchmark-design-v2.md`:
//! they answer the question "does the wire protocol fulfill mount the way
//! the OCI spec says it should?" using the reference implementation as the
//! baseline.
//!
//! The complementary suite, `ecr_mount.rs`, answers the same question
//! against real AWS ECR — gated behind `--features ecr-integration`.
//! Together they guard the `MountResult::Mounted` vs
//! `MountResult::NotSupported` decision at the client layer.
//!
//! Requirements:
//! - Docker must be running
//! - Run with: `cargo test --package ocync-distribution --test registry2_mount`

use bytes::Bytes;
use futures_util::StreamExt;
use ocync_distribution::blob::MountResult;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::spec::RepositoryName;
use ocync_distribution::{Digest, RegistryClient};
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

/// Push a blob to `repo` via the monolithic path; returns its digest.
async fn push_blob(client: &RegistryClient, repo: &RepositoryName, data: &[u8]) -> Digest {
    client
        .blob_push(repo, data)
        .await
        .expect("blob_push failed")
}

/// Verify a blob exists in `repo` via HEAD; returns its size.
async fn assert_blob_exists(
    client: &RegistryClient,
    repo: &RepositoryName,
    digest: &Digest,
) -> u64 {
    client
        .blob_exists(repo, digest)
        .await
        .expect("blob_exists HEAD failed")
        .expect("blob should exist")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: push a blob to `source`, mount it to `target`, registry
/// responds 201 and `MountResult::Mounted` is surfaced to the caller.
///
/// Pins the protocol-compliant baseline — the OCI reference fulfills mount.
/// Any deviation in the client's response classification (e.g. treating 201
/// as a fallback upload) would fail here.
#[tokio::test]
async fn mount_committed_blob_returns_mounted() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);

    let source_repo = RepositoryName::new("mount/source");
    let target_repo = RepositoryName::new("mount/target");
    let data = b"registry2 mount test blob";
    let digest = push_blob(&client, &source_repo, data).await;

    let result = client
        .blob_mount(&target_repo, &digest, &source_repo)
        .await
        .expect("blob_mount request failed");

    assert!(
        matches!(result, MountResult::Mounted),
        "expected registry:2 to fulfill mount, got {result:?}"
    );

    // Target repo now has the blob at the same digest.
    let size = assert_blob_exists(&client, &target_repo, &digest).await;
    assert_eq!(size, data.len() as u64);
}

/// When the source blob doesn't exist in `source_repo`, the registry must
/// return 202 (not 201) and the client surfaces `FallbackUpload`.
///
/// This pins the negative case: without a committed source, mount cannot
/// succeed. Asserts the client returns `FallbackUpload` with a non-empty
/// upload URL (so a fallback PATCH upload could proceed from here).
#[tokio::test]
async fn mount_missing_source_returns_fallback_upload() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);

    let source_repo = RepositoryName::new("missing/source");
    let target_repo = RepositoryName::new("missing/target");
    // Digest of data that was never pushed anywhere.
    let digest = test_digest(b"blob that does not exist in any repo");

    let result = client
        .blob_mount(&target_repo, &digest, &source_repo)
        .await
        .expect("blob_mount request failed");

    match result {
        MountResult::FallbackUpload { upload_url } => {
            assert!(
                !upload_url.is_empty(),
                "fallback upload URL must be populated so PATCH can proceed"
            );
        }
        other => panic!("expected FallbackUpload when source missing, got {other:?}"),
    }
}

/// After a successful mount, the source blob must remain intact in the
/// source repository. Mount is a reference/copy-on-write operation, not a
/// move.
#[tokio::test]
async fn mount_preserves_source_blob() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);

    let source_repo = RepositoryName::new("preserve/source");
    let target_repo = RepositoryName::new("preserve/target");
    let data = b"preserve-after-mount test data";
    let digest = push_blob(&client, &source_repo, data).await;

    // Mount to target.
    let result = client
        .blob_mount(&target_repo, &digest, &source_repo)
        .await
        .expect("blob_mount request failed");
    assert!(matches!(result, MountResult::Mounted));

    // Source still has the blob at the original size; content pulls back bit-for-bit.
    assert_blob_exists(&client, &source_repo, &digest).await;
    let stream = client
        .blob_pull(&source_repo, &digest)
        .await
        .expect("source blob_pull after mount failed");
    let mut body = Vec::new();
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.expect("pull chunk"));
    }
    assert_eq!(
        Bytes::from(body),
        Bytes::from_static(data),
        "source blob content changed after mount"
    );
}

/// Cross-repo mount across different repository prefixes (not sibling names).
/// Exercises that the `from=` query parameter accepts arbitrary repo paths,
/// not just same-prefix siblings.
#[tokio::test]
async fn mount_cross_repo_different_prefix() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);

    let source_repo = RepositoryName::new("org-a/service-x");
    let target_repo = RepositoryName::new("org-b/mirror/service-x");
    let data = b"cross-prefix mount blob";
    let digest = push_blob(&client, &source_repo, data).await;

    let result = client
        .blob_mount(&target_repo, &digest, &source_repo)
        .await
        .expect("blob_mount request failed");

    assert!(
        matches!(result, MountResult::Mounted),
        "cross-prefix mount must succeed, got {result:?}"
    );
    let size = assert_blob_exists(&client, &target_repo, &digest).await;
    assert_eq!(size, data.len() as u64);
}
