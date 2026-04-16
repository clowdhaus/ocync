//! Wire-level cross-repo blob mount protocol tests against real AWS ECR.
//!
//! Complements `registry2_mount.rs` — that suite pins the protocol-compliant
//! baseline against the OCI reference implementation; this one asserts what
//! actually happens at ECR. Per Bryant's observation (178/178 OCI mount
//! POSTs returned 202 during the first instrumented benchmark),
//! `ocync_distribution::blob::MountResult::SkippedByClient` is the expected
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

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_sdk_ecr::operation::delete_repository::DeleteRepositoryError;
use futures_util::FutureExt;
use ocync_distribution::auth::ecr::EcrAuth;
use ocync_distribution::blob::MountResult;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::spec::RepositoryName;
use ocync_distribution::{Digest, RegistryClient};
use url::Url;

/// Environment variable carrying the ECR hostname to test against.
const ECR_HOSTNAME_ENV: &str = "OCYNC_TEST_ECR_HOSTNAME";

/// Read the ECR hostname, or return `None` so the test can skip.
///
/// Enabling `--features ecr-integration` compiles these tests. The env
/// var indicates *which* account to test against; when unset (e.g. on a
/// local dev machine without AWS creds) the test skips with a message
/// rather than panicking. Real runs happen on the benchmark instance,
/// which has both the feature and the env var wired up.
fn ecr_hostname_or_skip() -> Option<String> {
    match std::env::var(ECR_HOSTNAME_ENV) {
        Ok(h) => Some(h),
        Err(_) => {
            eprintln!(
                "skipping: {ECR_HOSTNAME_ENV} not set — to run this suite, \
                 export the env var to an ECR hostname like \
                 '123456789012.dkr.ecr.us-east-1.amazonaws.com'"
            );
            None
        }
    }
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

/// Load an ECR SDK client for repo create/delete. Delegates to the
/// distribution crate's `load_sdk_config` so the test path shares the
/// same hostname→region logic as production.
async fn ecr_sdk(hostname: &str) -> aws_sdk_ecr::Client {
    let cfg = ocync_distribution::ecr::load_sdk_config(hostname)
        .await
        .expect("load_sdk_config failed — check ECR hostname format");
    aws_sdk_ecr::Client::new(&cfg)
}

/// Create an empty ECR repository. Returns an error string on failure so
/// callers can run cleanup for any already-created repos before surfacing.
async fn create_repo(sdk: &aws_sdk_ecr::Client, name: &str) -> Result<(), String> {
    sdk.create_repository()
        .repository_name(name)
        .send()
        .await
        .map_err(|e| format!("CreateRepository({name}) failed: {e}"))?;
    eprintln!("  created test repo: {name}");
    Ok(())
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

/// Run `body` against a freshly-created source + target repo pair and
/// guarantee both are deleted regardless of body outcome — including
/// panics from assertions inside the body.
///
/// The body returns `Result<T, String>` for non-panic errors. If the
/// body future panics, the helper catches the panic, runs cleanup, then
/// re-raises so the test failure reporting is unchanged. Without this,
/// a failing `assert!` inside the body would leak paid ECR repos.
async fn with_ephemeral_repos<F, Fut, T>(
    sdk: &aws_sdk_ecr::Client,
    source: &str,
    target: &str,
    body: F,
) -> Result<T, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    create_repo(sdk, source).await?;
    // Source is now live — any subsequent error MUST clean it up.
    if let Err(e) = create_repo(sdk, target).await {
        delete_repo(sdk, source).await;
        return Err(e);
    }

    // Catch panics in body so cleanup always runs. `AssertUnwindSafe`
    // is safe here because the body closure captures only borrowed
    // references that outlive this call.
    let body_fut = AssertUnwindSafe(body()).catch_unwind();
    let outcome = body_fut.await;

    delete_repo(sdk, source).await;
    delete_repo(sdk, target).await;

    match outcome {
        Ok(result) => result,
        Err(panic_payload) => std::panic::resume_unwind(panic_payload),
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

/// **Client-layer pinning:** on ECR, [`RegistryClient::blob_mount`] must
/// return [`MountResult::SkippedByClient`] without issuing any network
/// request. This asserts the short-circuit is wired correctly; paired
/// with [`ecr_mount_unchecked_returns_fallback_upload`] below, which
/// pins what ECR actually returns on the wire.
///
/// If ECR ever starts fulfilling OCI mount (returning 201) this test
/// still passes — the short-circuit is based on provider detection, not
/// live response. When that day comes, flip `ProviderKind::fulfills_cross_repo_mount`
/// and update both this assertion and the unchecked companion.
#[tokio::test]
async fn ecr_mount_returns_skipped_by_client() {
    let Some(hostname) = ecr_hostname_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let source_name = format!("ocync-probe-src-{suffix}");
    let target_name = format!("ocync-probe-tgt-{suffix}");

    let sdk = ecr_sdk(&hostname).await;

    let outcome = with_ephemeral_repos(&sdk, &source_name, &target_name, || async {
        let client = ecr_client(&hostname).await;
        let source_repo = RepositoryName::new(&source_name);
        let target_repo = RepositoryName::new(&target_name);

        // Push to source so that, if the short-circuit WERE broken, the
        // mount POST could in principle succeed and we'd see `Mounted`
        // (and know to update the expectation).
        let digest = client
            .blob_push(&source_repo, tiny_blob_bytes())
            .await
            .map_err(|e| format!("source blob push failed: {e}"))?;
        if digest != tiny_blob_digest() {
            return Err(format!("digest mismatch: got {digest}"));
        }

        client
            .blob_mount(&target_repo, &digest, &source_repo)
            .await
            .map_err(|e| format!("blob_mount call failed: {e}"))
    })
    .await;

    let result = outcome.expect("ecr_mount_returns_skipped_by_client body failed");
    assert!(
        matches!(result, MountResult::SkippedByClient),
        "expected MountResult::SkippedByClient on ECR target, got {result:?}. \
         If ECR now fulfills mount, update `ProviderKind::fulfills_cross_repo_mount` and \
         this test together."
    );
}

/// **Wire-layer pinning:** on ECR, [`RegistryClient::blob_mount_unchecked`]
/// (which bypasses the short-circuit) must return
/// [`MountResult::FallbackUpload`] — i.e. ECR's actual response to the
/// mount POST is still 202, not 201.
///
/// This is the test that the mount optimization should have shipped with
/// originally. It's the signal that tells us if AWS ever changes its
/// behavior: the moment ECR starts returning 201, this test fails and
/// forces a paired update to the short-circuit.
#[tokio::test]
async fn ecr_mount_unchecked_returns_fallback_upload() {
    let Some(hostname) = ecr_hostname_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let source_name = format!("ocync-probe-src-{suffix}");
    let target_name = format!("ocync-probe-tgt-{suffix}");

    let sdk = ecr_sdk(&hostname).await;

    let outcome = with_ephemeral_repos(&sdk, &source_name, &target_name, || async {
        let client = ecr_client(&hostname).await;
        let source_repo = RepositoryName::new(&source_name);
        let target_repo = RepositoryName::new(&target_name);

        let digest = client
            .blob_push(&source_repo, tiny_blob_bytes())
            .await
            .map_err(|e| format!("source blob push failed: {e}"))?;

        client
            .blob_mount_unchecked(&target_repo, &digest, &source_repo)
            .await
            .map_err(|e| format!("blob_mount_unchecked call failed: {e}"))
    })
    .await;

    let result = outcome.expect("ecr_mount_unchecked body failed");
    assert!(
        matches!(result, MountResult::FallbackUpload { .. }),
        "expected FallbackUpload (202) from real ECR, got {result:?}. \
         If AWS is now returning 201 on mount, flip `ProviderKind::fulfills_cross_repo_mount` \
         to allow ECR and update both this test and the `_returns_skipped_by_client` \
         companion."
    );
}

// The doc comment on `ecr_mount_returns_skipped_by_client` is decoupled
// from wire behavior on purpose — the short-circuit is provider-keyed.
// `ecr_mount_unchecked_returns_fallback_upload` is the test that detects
// AWS behavior changes; a change there forces a paired update to both
// tests and to `ProviderKind::fulfills_cross_repo_mount`.
