//! `xtask probe` — diagnostic tool for observing cross-repo mount
//! behavior against real registries.
//!
//! Currently scoped to AWS ECR. Answers the question:
//! "Given a fully-committed source blob, does ECR fulfill OCI cross-repo
//! mount (returning 201), or does it always fall back to a fresh upload
//! (202)?"
//!
//! Uses [`RegistryClient::blob_mount_unchecked`] to bypass the client-level
//! short-circuit — if we used `blob_mount`, every call would return
//! `MountResult::NotSupported` on ECR by design.
//!
//! Related: `docs/specs/benchmark-design-v2.md` §PR #1, the
//! `project_ecr_mount_behavior` memory, and the `registry2_mount.rs` /
//! `ecr_mount.rs` test suites which pin the resulting design decision.
//!
//! # Example
//!
//! ```text
//! cargo xtask probe \
//!     --registry 123456789012.dkr.ecr.us-east-1.amazonaws.com \
//!     --iterations 5 \
//!     --wait-secs 10
//! ```

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_sdk_ecr::operation::delete_repository::DeleteRepositoryError;
use clap::Args;
use ocync_distribution::auth::detect::{ProviderKind, detect_provider_kind};
use ocync_distribution::auth::ecr::EcrAuth;
use ocync_distribution::blob::MountResult;
use ocync_distribution::client::RegistryClientBuilder;
use ocync_distribution::spec::RepositoryName;
use ocync_distribution::{Digest, RegistryClient};
use url::Url;

/// Arguments for the probe subcommand.
#[derive(Args, Debug)]
pub(crate) struct ProbeArgs {
    /// Registry hostname to probe (e.g. `<account>.dkr.ecr.<region>.amazonaws.com`).
    ///
    /// Only ECR registries are supported today. Support for other providers
    /// can be added by implementing the per-provider repo-create adapter.
    #[arg(long)]
    pub(crate) registry: String,

    /// Number of mount iterations to execute per run.
    ///
    /// A single observation is never sufficient for a protocol claim — one
    /// 202 among 200 attempts is different from 200 of 200. The default
    /// of 3 lets us detect inconsistency cheaply; use more when triaging.
    #[arg(long, default_value = "3")]
    pub(crate) iterations: usize,

    /// Seconds to wait between pushing the source blob and the first mount
    /// attempt. Tests the hypothesis that ECR needs time to commit the
    /// source blob before mount from it can succeed.
    #[arg(long, default_value = "0")]
    pub(crate) wait_secs: u64,

    /// Size of the probe blob in bytes. Default is 1 KiB — small enough
    /// to be cheap, large enough to be a real blob.
    #[arg(long, default_value = "1024")]
    pub(crate) blob_size: usize,

    /// Do not delete the ephemeral test repositories after the probe
    /// completes. Useful for inspecting state after a surprising result.
    #[arg(long)]
    pub(crate) keep_repos: bool,
}

/// Summary of one mount attempt.
#[derive(Debug, Clone, Copy)]
struct AttemptResult {
    /// The status code the probe observed from the `blob_mount_unchecked` call.
    outcome: AttemptOutcome,
    /// Round-trip latency of the mount POST.
    latency: Duration,
}

#[derive(Debug, Clone, Copy)]
enum AttemptOutcome {
    /// Registry returned 201 — mount succeeded on the wire.
    Mounted,
    /// Registry returned 202 — fresh upload URL, mount not fulfilled.
    FallbackUpload,
    /// `blob_mount_unchecked` itself errored (timeout, 4xx/5xx, etc.).
    Error,
}

/// Entry point for `xtask probe`.
pub(crate) async fn run(args: ProbeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ProbeArgs {
        registry,
        iterations,
        wait_secs,
        blob_size,
        keep_repos,
    } = args;

    // Provider gate — the current implementation only knows how to create
    // ephemeral repos on ECR. Fail early with a helpful message.
    let Some(ProviderKind::Ecr) = detect_provider_kind(&registry) else {
        return Err(format!(
            "probe is currently ECR-only: '{registry}' is not an ECR hostname. \
             Contribute a per-provider adapter in xtask/src/probe.rs to extend support."
        )
        .into());
    };

    eprintln!(
        "probe: registry={registry} iterations={iterations} wait={wait_secs}s blob={blob_size}B"
    );

    let suffix = unique_suffix();
    let source_name = format!("ocync-probe-src-{suffix}");
    let target_name = format!("ocync-probe-tgt-{suffix}");
    eprintln!("probe: creating ephemeral repos '{source_name}' and '{target_name}'");

    let sdk = ecr_sdk_client(&registry).await?;

    // Source created first; if target creation fails, source is cleaned
    // before propagating the error so we don't leak an ECR repo.
    create_repo(&sdk, &source_name).await?;
    if let Err(e) = create_repo(&sdk, &target_name).await {
        delete_repo(&sdk, &source_name).await;
        return Err(e);
    }

    let run_result = probe_body(
        &registry,
        &source_name,
        &target_name,
        iterations,
        wait_secs,
        blob_size,
    )
    .await;

    // Cleanup regardless of body outcome (unless --keep-repos).
    if keep_repos {
        eprintln!("probe: --keep-repos set; leaving '{source_name}' and '{target_name}' in place");
    } else {
        delete_repo(&sdk, &source_name).await;
        delete_repo(&sdk, &target_name).await;
    }

    run_result
}

/// The work of the probe, factored out so cleanup runs regardless of
/// per-attempt errors.
async fn probe_body(
    registry: &str,
    source_name: &str,
    target_name: &str,
    iterations: usize,
    wait_secs: u64,
    blob_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = build_client(registry).await?;
    let source_repo = RepositoryName::new(source_name);
    let target_repo = RepositoryName::new(target_name);

    // Deterministic but host-unique blob content so we can re-run against
    // the same account without digest collisions.
    let blob = probe_blob(registry, blob_size);
    eprintln!(
        "probe: pushing {} B source blob to '{source_name}'",
        blob.len()
    );
    let digest = client.blob_push(&source_repo, &blob).await?;
    eprintln!("probe: source digest = {digest}");

    if wait_secs > 0 {
        eprintln!("probe: waiting {wait_secs}s before first mount attempt");
        tokio::time::sleep(Duration::from_secs(wait_secs)).await;
    }

    let mut attempts = Vec::with_capacity(iterations);
    for i in 1..=iterations {
        let attempt = probe_one_mount(&client, &target_repo, &digest, &source_repo).await;
        eprintln!(
            "probe: iter {i}/{iterations}: outcome={:?} latency={:?}",
            attempt.outcome, attempt.latency
        );
        attempts.push(attempt);
    }

    print_summary(&attempts);
    Ok(())
}

/// Issue one mount POST and classify the outcome.
async fn probe_one_mount(
    client: &RegistryClient,
    target: &RepositoryName,
    digest: &Digest,
    source: &RepositoryName,
) -> AttemptResult {
    let start = Instant::now();
    let res = client.blob_mount_unchecked(target, digest, source).await;
    let latency = start.elapsed();
    let outcome = match res {
        Ok(MountResult::Mounted) => AttemptOutcome::Mounted,
        Ok(MountResult::FallbackUpload { .. }) => AttemptOutcome::FallbackUpload,
        Ok(MountResult::NotSupported) => {
            // `blob_mount_unchecked` bypasses the short-circuit, so this branch
            // should be unreachable — treat as an error so the summary
            // surfaces the anomaly rather than silently succeeding.
            eprintln!("probe: warning: blob_mount_unchecked returned NotSupported (unreachable)");
            AttemptOutcome::Error
        }
        Err(e) => {
            eprintln!("probe: mount error: {e}");
            AttemptOutcome::Error
        }
    };
    AttemptResult { outcome, latency }
}

/// Emit a human-readable summary + a recommendation.
fn print_summary(attempts: &[AttemptResult]) {
    let total = attempts.len();
    let mounted = attempts
        .iter()
        .filter(|a| matches!(a.outcome, AttemptOutcome::Mounted))
        .count();
    let fallback = attempts
        .iter()
        .filter(|a| matches!(a.outcome, AttemptOutcome::FallbackUpload))
        .count();
    let errored = attempts
        .iter()
        .filter(|a| matches!(a.outcome, AttemptOutcome::Error))
        .count();

    eprintln!();
    eprintln!("probe summary:");
    eprintln!("  iterations:   {total}");
    eprintln!("  mounted:      {mounted} (201)");
    eprintln!("  fallback:     {fallback} (202)");
    eprintln!("  errored:      {errored}");

    let recommendation = if mounted == 0 && errored == 0 {
        "recommend: keep ECR short-circuit enabled (no mount successes observed)"
    } else if mounted == total {
        "recommend: DISABLE ECR short-circuit — every attempt succeeded"
    } else if mounted > 0 {
        "recommend: investigate mixed results before acting; some mounts succeeded"
    } else {
        "recommend: re-run with fewer iterations or investigate errors"
    };
    eprintln!("  {recommendation}");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a unique repo-name suffix so repeated probe runs don't collide
/// with stale repos left over from a previous invocation, and successive
/// calls within the same process produce distinct suffixes (guarantee
/// via an atomic counter, independent of clock resolution).
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{counter:x}")
}

/// Deterministic blob payload of the requested size, tagged with the
/// registry hostname so concurrent probes against different hosts cannot
/// produce digest collisions.
fn probe_blob(registry: &str, size: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size);
    let seed = format!("ocync-probe:{registry}:");
    bytes.extend_from_slice(seed.as_bytes());
    while bytes.len() < size {
        bytes.push((bytes.len() % 256) as u8);
    }
    bytes.truncate(size);
    bytes
}

/// Build an authenticated `RegistryClient` for an ECR hostname.
async fn build_client(registry: &str) -> Result<RegistryClient, Box<dyn std::error::Error>> {
    let auth = EcrAuth::new(registry).await?;
    let base_url = Url::parse(&format!("https://{registry}"))?;
    Ok(RegistryClientBuilder::new(base_url).auth(auth).build()?)
}

/// Construct an ECR SDK client keyed to the given ECR hostname.
///
/// Uses `ocync_distribution::ecr::load_sdk_config` so the region-parsing
/// logic is identical to what the auth provider uses — avoids the class
/// of bugs where a diagnostic tool and the production code disagree about
/// which region an ECR hostname belongs to.
async fn ecr_sdk_client(registry: &str) -> Result<aws_sdk_ecr::Client, Box<dyn std::error::Error>> {
    let cfg = ocync_distribution::ecr::load_sdk_config(registry).await?;
    Ok(aws_sdk_ecr::Client::new(&cfg))
}

/// Create an ECR repository. Fails the probe if the API rejects the call.
async fn create_repo(
    sdk: &aws_sdk_ecr::Client,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sdk.create_repository()
        .repository_name(name)
        .send()
        .await
        .map_err(|e| format!("CreateRepository({name}) failed: {e}"))?;
    Ok(())
}

/// Force-delete an ECR repository. Logs and continues on failure — a stuck
/// repo must never prevent the rest of cleanup.
async fn delete_repo(sdk: &aws_sdk_ecr::Client, name: &str) {
    match sdk
        .delete_repository()
        .repository_name(name)
        .force(true)
        .send()
        .await
    {
        Ok(_) => eprintln!("probe: deleted ephemeral repo '{name}'"),
        Err(e) => {
            if let Some(DeleteRepositoryError::RepositoryNotFoundException(_)) =
                e.as_service_error()
            {
                // Already gone — fine.
            } else {
                eprintln!("probe: warning: failed to delete repo '{name}': {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_blob_has_requested_size() {
        let blob = probe_blob("registry.test", 1024);
        assert_eq!(blob.len(), 1024);
    }

    #[test]
    fn probe_blob_includes_registry_seed() {
        let blob = probe_blob("registry.test", 64);
        assert!(
            blob.starts_with(b"ocync-probe:registry.test:"),
            "probe blob must be seeded with registry hostname so parallel \
             probes against different registries produce different digests"
        );
    }

    #[test]
    fn probe_blob_zero_size() {
        let blob = probe_blob("r", 0);
        assert!(blob.is_empty());
    }

    #[test]
    fn probe_blob_smaller_than_seed_truncates() {
        // Seed is 17+ bytes; size=4 must still produce a 4-byte blob.
        let blob = probe_blob("host.example", 4);
        assert_eq!(blob.len(), 4);
    }

    #[test]
    fn unique_suffix_is_hex_and_varies() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two successive suffixes should differ");
    }
}
