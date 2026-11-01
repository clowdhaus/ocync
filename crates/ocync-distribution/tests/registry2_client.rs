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

#[allow(unreachable_pub)]
mod common;

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::{TryStreamExt, stream};
use http::Version;
use ocync_distribution::aimd::RegistryAction;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::spec::{Descriptor, ImageManifest, MediaType, RepositoryName};
use ocync_distribution::{Digest, Error};
use testcontainers::bollard;

use common::{local_client, start_registry, start_registry_tls, test_digest};

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

/// The registry runs under Docker's init process, which reaps it so the
/// container can be removed. Breaks if `start_registry` drops `--init`.
#[tokio::test]
async fn registry_runs_under_init() {
    let (container, _url) = start_registry().await;
    let mut exec = container
        .exec(testcontainers::core::ExecCommand::new([
            "cat",
            "/proc/1/comm",
        ]))
        .await
        .expect("exec in registry container failed");
    let pid1 = exec.stdout_to_vec().await.expect("read exec output");
    assert_eq!(String::from_utf8_lossy(&pid1).trim(), "docker-init");
}

/// The sorted IDs of the containers matching the label `filter`.
async fn labeled_ids(docker: &bollard::Docker, filter: &str) -> Vec<String> {
    let mut ids: Vec<String> = common::labeled(docker, filter)
        .await
        .expect("Docker lists containers")
        .into_iter()
        .filter_map(|container| container.id)
        .collect();
    ids.sort();
    ids
}

/// Create an `image` container labeled [`common::LABEL_KEY`]`=value`
/// that sleeps for ten minutes once started, and return its ID. A failed
/// test leaves it to stop on its own, so a later sweep removes it as a
/// stopped container rather than a day later as a running one.
async fn labeled_container(docker: &bollard::Docker, image: &str, value: &str) -> String {
    use bollard::models::{ContainerCreateBody, HostConfig};
    use bollard::query_parameters::CreateContainerOptions;

    docker
        .create_container(
            None::<CreateContainerOptions>,
            ContainerCreateBody {
                image: Some(image.to_owned()),
                entrypoint: Some(vec!["sleep".to_owned(), "600".to_owned()]),
                labels: Some(HashMap::from([(
                    common::LABEL_KEY.to_owned(),
                    value.to_owned(),
                )])),
                host_config: Some(HostConfig {
                    init: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("Docker creates the container")
        .id
}

/// A sweep removes a stopped container only past its stopped age, and a
/// running one only past its any-state age. Breaks if a container another
/// run is still starting is removed, one left behind is kept, a container
/// with another label is touched, or a daemon clock a little ahead of this
/// host's keeps a container. Its labels are its own, so it never removes
/// another test's container; a failure leaves them to a later run's sweep.
#[tokio::test]
async fn sweep_removes_only_containers_past_their_age() {
    use bollard::query_parameters::{CreateImageOptions, StartContainerOptions};

    let docker = testcontainers::core::client::docker_client_instance()
        .await
        .expect("a Docker client");
    let image = format!("{}:{}", common::REGISTRY_NAME, common::REGISTRY_TAG);
    // Pulled only when missing, as testcontainers does, to spare Docker
    // Hub's pull limit.
    match docker.inspect_image(&image).await {
        Ok(_) => {}
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: common::NOT_FOUND,
            ..
        }) => docker
            .create_image(
                Some(CreateImageOptions {
                    from_image: Some(common::REGISTRY_NAME.to_owned()),
                    tag: Some(common::REGISTRY_TAG.to_owned()),
                    ..Default::default()
                }),
                None,
                None,
            )
            .try_collect::<Vec<_>>()
            .await
            .map(drop)
            .expect("Docker pulls the registry image"),
        Err(e) => panic!("Docker inspects the registry image: {e}"),
    }
    let value = format!(
        "sweep-check-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let other_value = format!("{value}-other");
    let created = labeled_container(&docker, &image, &value).await;
    let running = labeled_container(&docker, &image, &value).await;
    docker
        .start_container(&running, None::<StartContainerOptions>)
        .await
        .expect("Docker starts the container");
    let unrelated = labeled_container(&docker, &image, &other_value).await;
    let (label, other) = (common::label(&value), common::label(&other_value));
    // A daemon clock up to a minute ahead of this host still counts these
    // containers as created by now.
    let now = SystemTime::now() + Duration::from_secs(60);

    for (stopped, any_state, removed, left) in [
        (
            common::LONGEST_RUN,
            common::LONGEST_RUN,
            0,
            vec![&created, &running],
        ),
        (Duration::ZERO, common::LONGEST_RUN, 1, vec![&running]),
        (Duration::ZERO, Duration::ZERO, 1, vec![]),
    ] {
        let ages = common::OrphanAge { stopped, any_state };
        let swept = common::sweep(&label, ages, now).await.unwrap();
        assert_eq!(
            (swept.removed, swept.failed.len()),
            (removed, 0),
            "{ages:?}"
        );
        let mut left: Vec<String> = left.into_iter().cloned().collect();
        left.sort();
        assert_eq!(labeled_ids(&docker, &label).await, left, "{ages:?}");
    }
    assert_eq!(
        labeled_ids(&docker, &other).await,
        std::slice::from_ref(&unrelated),
        "another label is never swept"
    );
    docker
        .remove_container(
            &unrelated,
            Some(bollard::query_parameters::RemoveContainerOptions {
                v: true,
                ..Default::default()
            }),
        )
        .await
        .expect("Docker removes the container");
}

/// Which containers count as left behind: a stopped one (created, exited
/// or dead) past the stopped age, any other past the any-state age, never
/// one whose creation time cannot be read or overflows. Breaks if a state
/// takes the wrong age, the boundary is off by one, or a bad creation time
/// counts as old, which would remove a live container.
#[test]
fn orphan_rule() {
    use bollard::models::ContainerSummaryStateEnum as State;

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    let ages = common::OrphanAge {
        stopped: Duration::from_secs(100),
        any_state: Duration::from_secs(1_000),
    };
    for (state, created, orphan) in [
        (Some(State::CREATED), 9_900, true),
        (Some(State::EXITED), 9_900, true),
        (Some(State::DEAD), 9_900, true),
        (Some(State::EXITED), 9_901, false),
        (Some(State::RUNNING), 9_900, false),
        (Some(State::RUNNING), 9_000, true),
        (None, 9_000, true),
        (None, 9_001, false),
        (Some(State::DEAD), -1, false),
        (Some(State::DEAD), i64::MAX, false),
    ] {
        assert_eq!(
            common::is_orphan(state, created, ages, now),
            orphan,
            "{state:?} created at {created}"
        );
    }
}

/// The sweep keeps containers only when testcontainers is told to keep
/// them. Breaks if a developer's kept containers are swept, or if any
/// other value stops the sweep.
#[test]
fn keep_command_skips_the_sweep() {
    for (command, keeps) in [(Some("keep"), true), (Some("remove"), false), (None, false)] {
        assert_eq!(common::keeps(command), keeps, "{command:?}");
    }
}

/// Starting a registry runs the orphan sweep, and the registry is found by
/// the filter that sweep uses. Breaks if leftovers from a timed-out start
/// are never swept.
#[tokio::test]
async fn starting_a_registry_sweeps_and_labels_it() {
    let (container, _url) = start_registry().await;
    assert!(common::swept(), "start runs the orphan sweep");
    let docker = testcontainers::core::client::docker_client_instance()
        .await
        .expect("a Docker client");
    let ids = labeled_ids(&docker, common::LABEL_KEY).await;
    assert!(ids.iter().any(|id| id == container.id()), "{ids:?}");
}

/// Push a small blob via the monolithic (POST+PUT) path and verify it exists.
#[tokio::test]
async fn blob_push_monolithic_and_exists() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/monolithic").unwrap();

    let data = b"hello from ocync integration test";
    let digest = client
        .blob_push(&repo, data)
        .await
        .expect("blob_push failed");

    assert_eq!(digest, test_digest(data));

    assert!(
        client
            .blob_exists(&repo, &digest)
            .await
            .expect("blob_exists failed"),
        "blob should exist after push"
    );
}

/// Push a blob via the streaming upload path.
#[tokio::test]
async fn blob_push_stream_chunked() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/streamed").unwrap();

    let data = vec![0xABu8; 2 * 1024 * 1024];
    let expected_digest = test_digest(&data);
    let data_len = data.len() as u64;

    let body = Bytes::from(data);
    let stream = stream::once(async move { Ok::<_, Error>(body) });
    let slot = client.reserve_upload_slot(Some(data_len)).await;
    let digest = slot
        .push_stream(&repo, &expected_digest, Some(data_len), stream)
        .await
        .expect("blob_push_stream failed");

    assert_eq!(digest, expected_digest);

    assert!(
        client
            .blob_exists(&repo, &digest)
            .await
            .expect("blob_exists failed"),
        "blob should exist after stream push"
    );
}

/// Verify `blob_exists` returns `false` for a nonexistent blob.
#[tokio::test]
async fn blob_exists_returns_false_for_missing() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/missing").unwrap();

    let fake_digest: Digest =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();

    let result = client
        .blob_exists(&repo, &fake_digest)
        .await
        .expect("blob_exists should not error for missing blob");
    assert!(!result);
}

/// Full round-trip: push config blob, push layer blob, push manifest, then
/// HEAD and GET the manifest back. Verifies the entire push/pull protocol
/// against a real registry.
#[tokio::test]
async fn full_image_roundtrip() {
    let (_container, url) = start_registry().await;
    let client = local_client(url);
    let repo = RepositoryName::new("test/roundtrip").unwrap();
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

    // 4. HEAD the manifest by tag: verify the digest.
    let head = client
        .manifest_head(&repo, tag)
        .await
        .expect("manifest HEAD failed")
        .expect("manifest HEAD should return Some for existing tag");
    assert_eq!(head, manifest_digest);

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
    let repo = RepositoryName::new("test/notfound").unwrap();

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
    let repo = RepositoryName::new("test/idempotent").unwrap();

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
    let repo = RepositoryName::new("test/blobpull").unwrap();

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

/// A blob body being read arrives in full while bodies opened before it are
/// left unread, with the client's adaptive HTTP/2 windows. Each blob is far
/// larger than the 65,535 byte window an adaptive HTTP/2 connection starts
/// with. Breaks if blob downloads share an HTTP/2 connection, whose receive
/// window the unread bodies fill.
#[tokio::test]
async fn an_unread_blob_body_does_not_hold_back_another() {
    let (_container, url) = start_registry_tls().await;
    let client = RegistryClientBuilder::new(url)
        .allow_invalid_certs(true)
        .build()
        .unwrap();
    let repo = RepositoryName::new("test/unread").unwrap();
    let mut digests = Vec::new();
    for blob in 0..4u8 {
        let mut data = vec![0u8; 1 << 20];
        data[0] = blob;
        digests.push(client.blob_push(&repo, &data).await.unwrap());
    }
    let path = format!("blobs/{}", digests[0]);
    let head = client
        .head(&repo, &path, None, RegistryAction::BlobHead)
        .await
        .unwrap();
    assert_eq!(
        head.version(),
        Version::HTTP_2,
        "other requests stay on HTTP/2"
    );
    let get = client
        .get(&repo, &path, None, RegistryAction::BlobRead)
        .await
        .unwrap();
    assert_eq!(
        get.version(),
        Version::HTTP_11,
        "blob downloads use HTTP/1.1"
    );
    drop(get);

    let mut unread = Vec::new();
    for digest in &digests[..3] {
        unread.push(client.blob_pull(&repo, digest).await.unwrap());
    }
    let read = client.blob_pull(&repo, &digests[3]).await.unwrap();
    let bytes = read.try_fold(0, |n, chunk| async move { Ok(n + chunk.len()) });
    let bytes = tokio::time::timeout(Duration::from_secs(30), bytes)
        .await
        .expect("the body being read stalled")
        .unwrap();

    assert_eq!(bytes, 1 << 20);
    drop(unread);
}
