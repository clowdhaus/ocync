//! Wire-level cross-repo blob mount protocol tests against real AWS ECR.
//!
//! Complements `registry2_mount.rs` — that suite pins the protocol-compliant
//! baseline against the OCI reference implementation; this one asserts what
//! actually happens at ECR. Per Bryant's observation (178/178 OCI mount
//! POSTs returned 202 during the first instrumented benchmark),
//! `ocync_distribution::blob::MountResult::NotSupported` is the expected
//! outcome — the client short-circuits the POST entirely on ECR targets.
//!
//! # Running
//!
//! Feature-gated so CI without AWS credentials skips compilation:
//!
//! ```text
//! export OCYNC_TEST_ECR_HOSTNAME=123456789012.dkr.ecr.us-east-1.amazonaws.com
//! cargo test --package ocync-distribution \
//!     --features ecr-integration --test ecr_mount
//! ```
//!
//! # IAM permissions required
//!
//! - `ecr:GetAuthorizationToken`
//! - `ecr:CreateRepository` / `ecr:DeleteRepository` (for ephemeral test repos)
//! - `ecr:BatchCheckLayerAvailability`
//! - `ecr:InitiateLayerUpload` / `ecr:UploadLayerPart` / `ecr:CompleteLayerUpload`
//! - `ecr:PutImage` (not needed here — no manifest push)
//!
//! # Cost
//!
//! Each test run creates 2 repos, pushes 1 tiny blob, issues 1 mount POST,
//! deletes 2 repos. Effectively free; rounded to ~$0.01 per run.

#![cfg(feature = "ecr-integration")]

use std::time::{SystemTime, UNIX_EPOCH};

use aws_sdk_ecr::operation::delete_repository::DeleteRepositoryError;
use ocync_distribution::auth::ecr::EcrAuth;
use ocync_distribution::blob::MountResult;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::spec::RepositoryName;
use ocync_distribution::{Digest, RegistryClient};
use url::Url;

/// Environment variable carrying the ECR hostname to test against.
const ECR_HOSTNAME_ENV: &str = "OCYNC_TEST_ECR_HOSTNAME";

/// Read the ECR hostname or panic with a helpful message.
fn ecr_hostname() -> String {
    std::env::var(ECR_HOSTNAME_ENV).unwrap_or_else(|_| {
        panic!(
            "{ECR_HOSTNAME_ENV} must be set to an ECR hostname like \
             '123456789012.dkr.ecr.us-east-1.amazonaws.com' to run \
             ecr_mount tests"
        )
    })
}

/// Build a unique repo-name suffix for test isolation (no collisions across
/// parallel test runs). Uses nanos-since-epoch to avoid any dep on uuid.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:x}")
}

/// Create a `RegistryClient` authenticated against ECR via the AWS SDK.
async fn ecr_client(hostname: &str) -> RegistryClient {
    let auth = EcrAuth::new(hostname)
        .await
        .expect("EcrAuth::new failed — are AWS credentials configured?");
    let base_url = Url::parse(&format!("https://{hostname}")).unwrap();
    RegistryClientBuilder::new(base_url)
        .auth(auth)
        .build()
        .expect("RegistryClientBuilder::build failed")
}

/// Load an ECR SDK client for repo create/delete. Region is derived by the
/// SDK from the same AWS config chain (env/profile/instance role).
async fn ecr_sdk(hostname: &str) -> aws_sdk_ecr::Client {
    let region = hostname
        .split('.')
        .nth(3)
        .expect("ECR hostname must contain region segment");
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await;
    aws_sdk_ecr::Client::new(&cfg)
}

/// Create an empty ECR repository. Logs and panics on failure.
async fn create_repo(sdk: &aws_sdk_ecr::Client, name: &str) {
    sdk.create_repository()
        .repository_name(name)
        .send()
        .await
        .unwrap_or_else(|e| panic!("CreateRepository({name}) failed: {e}"));
    eprintln!("  created test repo: {name}");
}

/// Force-delete an ECR repository (ignores NotFound). Logs on failure —
/// callers should continue cleanup even if one delete fails.
async fn delete_repo(sdk: &aws_sdk_ecr::Client, name: &str) {
    match sdk
        .delete_repository()
        .repository_name(name)
        .force(true)
        .send()
        .await
    {
        Ok(_) => eprintln!("  deleted test repo: {name}"),
        Err(e) => {
            if let Some(DeleteRepositoryError::RepositoryNotFoundException(_)) =
                e.as_service_error()
            {
                // Already gone — fine.
            } else {
                eprintln!("  warning: DeleteRepository({name}) failed: {e}");
            }
        }
    }
}

/// Deterministic tiny blob used by the mount tests.
fn tiny_blob_bytes() -> &'static [u8] {
    // Small, constant payload — the mount assertion is behavioral, not
    // bytewise, so the content doesn't need to vary.
    b"ocync ecr_mount.rs probe blob"
}

fn tiny_blob_digest() -> Digest {
    let hash = ocync_distribution::sha256::Sha256::digest(tiny_blob_bytes());
    Digest::from_sha256(hash)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// **Protocol pinning:** on ECR, `blob_mount` must return
/// `MountResult::NotSupported` without issuing any network request.
///
/// This is the Layer-1 assertion that the mount short-circuit stays active
/// against the real registry. If ECR ever starts fulfilling OCI mount
/// (returning 201), this test will still pass — the short-circuit is based
/// on provider detection, not live response. When that day comes, flip the
/// `supports_cross_repo_mount` branch and update this assertion to
/// `MountResult::Mounted`.
#[tokio::test]
async fn ecr_mount_returns_not_supported() {
    let hostname = ecr_hostname();
    let suffix = unique_suffix();
    let source_name = format!("ocync-probe-src-{suffix}");
    let target_name = format!("ocync-probe-tgt-{suffix}");

    let sdk = ecr_sdk(&hostname).await;
    create_repo(&sdk, &source_name).await;
    create_repo(&sdk, &target_name).await;

    let client = ecr_client(&hostname).await;
    let source_repo = RepositoryName::new(&source_name);
    let target_repo = RepositoryName::new(&target_name);

    // Push a blob to the source so that, if the short-circuit WERE broken,
    // the mount POST could in principle succeed and we'd see `Mounted` and
    // know to update the expectation.
    let digest = client
        .blob_push(&source_repo, tiny_blob_bytes())
        .await
        .expect("source blob push failed");
    assert_eq!(digest, tiny_blob_digest());

    // Attempt mount — client must short-circuit without hitting the wire.
    let result = client
        .blob_mount(&target_repo, &digest, &source_repo)
        .await
        .expect("blob_mount call failed");

    // Cleanup regardless of assertion outcome.
    delete_repo(&sdk, &source_name).await;
    delete_repo(&sdk, &target_name).await;

    assert!(
        matches!(result, MountResult::NotSupported),
        "expected MountResult::NotSupported on ECR target, got {result:?}. \
         If ECR now fulfills mount, update `supports_cross_repo_mount` and \
         this test together."
    );
}
