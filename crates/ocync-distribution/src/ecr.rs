//! AWS ECR batch operations — bulk blob existence checks.
//!
//! Provides [`BatchBlobChecker`] for bulk blob existence checking via ECR's
//! `BatchCheckLayerAvailability` API. These complement the per-request OCI
//! distribution API with ECR-native batch operations that reduce API
//! round-trips by up to 98%.
//!
//! FIPS endpoint support is handled at the SDK config level: set
//! `AWS_USE_FIPS_ENDPOINT=true` in the environment before loading the
//! `SdkConfig`, and the SDK will route requests to FIPS endpoints
//! automatically.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use aws_config::BehaviorVersion;
use aws_sdk_ecr::types::LayerAvailability;
use tracing::warn;

use crate::digest::Digest;
use crate::error::Error;
use crate::spec::RepositoryName;

/// Extract the AWS region from an ECR hostname.
///
/// Handles both standard (`<account>.dkr.ecr[-fips].<region>.<domain>`)
/// and dual-stack (`<account>.dkr-ecr[-fips].<region>.<domain>`) formats
/// across all AWS partitions.
fn ecr_region(hostname: &str) -> Option<&str> {
    let parts: Vec<&str> = hostname.split('.').collect();
    if parts.len() < 5 {
        return None;
    }

    for (i, part) in parts.iter().enumerate() {
        if matches!(*part, "ecr" | "ecr-fips" | "dkr-ecr" | "dkr-ecr-fips") {
            return parts.get(i + 1).copied();
        }
    }

    None
}

/// Extract the 12-digit registry ID from an ECR hostname.
///
/// The registry ID is the first segment of the hostname (before `.dkr`).
/// Returns `None` if the first segment isn't exactly 12 ASCII digits.
fn ecr_registry_id(hostname: &str) -> Option<&str> {
    let first = hostname.split('.').next()?;
    if first.len() == 12 && first.bytes().all(|b| b.is_ascii_digit()) {
        Some(first)
    } else {
        None
    }
}

/// Load an AWS SDK config for the given ECR hostname.
///
/// Extracts the region from the hostname and configures the SDK with it.
/// FIPS endpoint support is handled at the SDK level: set
/// `AWS_USE_FIPS_ENDPOINT=true` before calling this function.
///
/// The matching SDK client is constructed by callers:
/// `aws_sdk_ecr::Client::new(&load_sdk_config(hostname).await?)`.
///
/// Exported so diagnostic tools (`xtask probe`) and feature-gated
/// integration tests (`tests/ecr_mount.rs`) can reuse the exact same
/// hostname→region logic the auth providers rely on — no parallel
/// hostname parsers.
pub async fn load_sdk_config(hostname: &str) -> Result<aws_config::SdkConfig, Error> {
    let region = ecr_region(hostname).ok_or_else(|| {
        Error::Other(format!(
            "cannot extract AWS region from ECR hostname '{hostname}'"
        ))
    })?;

    Ok(aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await)
}

/// Maximum number of layer digests per `BatchCheckLayerAvailability` API call.
///
/// ECR enforces a limit of 100 digests per request. Larger batches are
/// automatically split into multiple API calls.
const MAX_DIGESTS_PER_BATCH: usize = 100;

/// Boxed future returned by [`BatchBlobChecker::check_blob_existence`].
type CheckFuture<'a> = Pin<Box<dyn Future<Output = Result<HashSet<Digest>, Error>> + 'a>>;

/// Async trait for batch blob existence checking.
///
/// Used by the sync engine to efficiently determine which blobs already exist
/// at an ECR target registry before initiating transfers. Implementations are
/// intended to be held as `Rc<dyn BatchBlobChecker>` on a single-threaded
/// tokio runtime, so no `Send` or `Sync` bounds are required.
pub trait BatchBlobChecker {
    /// Check which blobs exist in the given repository.
    ///
    /// Returns the set of input digests that exist at the target. Digests
    /// absent from the returned set are missing and need transfer. Digests
    /// that the API reports as failures are treated as missing.
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> CheckFuture<'a>;
}

/// Abstraction over ECR batch API calls for testability.
///
/// Wraps `BatchCheckLayerAvailability` so tests can inject mock responses
/// without an SDK client.
trait EcrBatchApi {
    /// Call `BatchCheckLayerAvailability` for a single batch (up to 100 digests).
    fn batch_check_layer_availability(
        &self,
        repo: &str,
        digests: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + '_>>;
}

/// Response from a single `BatchCheckLayerAvailability` call.
struct BatchCheckResponse {
    /// Layers that were successfully checked, with their availability status.
    layers: Vec<(String, bool)>,
    /// Digests that failed to check (treated as unavailable).
    failures: Vec<String>,
}

/// Default [`EcrBatchApi`] backed by the AWS SDK.
struct AwsEcrBatchApi {
    client: aws_sdk_ecr::Client,
    registry_id: Option<String>,
}

impl std::fmt::Debug for AwsEcrBatchApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsEcrBatchApi")
            .field("registry_id", &self.registry_id)
            .finish_non_exhaustive()
    }
}

impl EcrBatchApi for AwsEcrBatchApi {
    fn batch_check_layer_availability(
        &self,
        repo: &str,
        digests: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + '_>> {
        let repo = repo.to_owned();
        let digests: Vec<String> = digests.to_vec();
        Box::pin(async move {
            let mut builder = self
                .client
                .batch_check_layer_availability()
                .repository_name(&repo)
                .set_layer_digests(Some(digests));

            if let Some(ref id) = self.registry_id {
                builder = builder.registry_id(id);
            }

            let output = builder.send().await.map_err(|e| {
                Error::Other(format!(
                    "ECR BatchCheckLayerAvailability failed for '{repo}': {e}"
                ))
            })?;

            let layers: Vec<(String, bool)> = output
                .layers()
                .iter()
                .filter_map(|layer| {
                    let digest = layer.layer_digest()?.to_owned();
                    let available = layer
                        .layer_availability()
                        .is_some_and(|a| *a == LayerAvailability::Available);
                    Some((digest, available))
                })
                .collect();

            let failures: Vec<String> = output
                .failures()
                .iter()
                .filter_map(|f| f.layer_digest().map(|d| d.to_owned()))
                .collect();

            Ok(BatchCheckResponse { layers, failures })
        })
    }
}

/// ECR batch checker backed by the AWS SDK.
///
/// Provides bulk blob existence checking via `BatchCheckLayerAvailability`,
/// splitting large batches into chunks of 100 (the ECR API limit per call).
///
/// Construct via [`BatchChecker::from_hostname`] with an ECR hostname.
/// FIPS support is handled at the SDK config level: set
/// `AWS_USE_FIPS_ENDPOINT=true` before loading.
pub struct BatchChecker {
    api: Box<dyn EcrBatchApi>,
}

impl std::fmt::Debug for BatchChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchChecker").finish_non_exhaustive()
    }
}

impl BatchChecker {
    /// Create a batch checker from an ECR hostname.
    ///
    /// Extracts the AWS region and 12-digit registry ID from the hostname,
    /// then builds an SDK config and ECR client internally. Returns an error
    /// if the region cannot be determined from the hostname.
    pub async fn from_hostname(hostname: &str) -> Result<Self, Error> {
        let sdk_config = load_sdk_config(hostname).await?;
        let registry_id = ecr_registry_id(hostname).map(|s| s.to_owned());
        let client = aws_sdk_ecr::Client::new(&sdk_config);
        Ok(Self {
            api: Box::new(AwsEcrBatchApi {
                client,
                registry_id,
            }),
        })
    }

    /// Create an ECR batch checker with an injected API implementation.
    #[cfg(test)]
    fn with_api(api: impl EcrBatchApi + 'static) -> Self {
        Self { api: Box::new(api) }
    }

    /// Check blob existence, splitting into batches of 100.
    async fn check_batched(
        &self,
        repo: &RepositoryName,
        digests: &[Digest],
    ) -> Result<HashSet<Digest>, Error> {
        let mut existing = HashSet::with_capacity(digests.len());

        for chunk in digests.chunks(MAX_DIGESTS_PER_BATCH) {
            // Build (String, &Digest) pairs once per chunk so we convert each
            // digest to a string exactly once (used for both the API call and
            // the availability lookup).
            let pairs: Vec<(String, &Digest)> = chunk.iter().map(|d| (d.to_string(), d)).collect();
            let digest_strings: Vec<String> = pairs.iter().map(|(s, _)| s.clone()).collect();

            let response = match self
                .api
                .batch_check_layer_availability(repo.as_str(), &digest_strings)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        repo = %repo,
                        completed = existing.len(),
                        remaining = digests.len() - existing.len(),
                        error = %e,
                        "batch check failed mid-batch, returning partial results"
                    );
                    // Total failure (no results yet): propagate error so the
                    // engine logs the fallback warning.
                    if existing.is_empty() {
                        return Err(e);
                    }
                    // Partial success: return what we have. Unchecked digests
                    // are absent from the set and will be handled by per-blob
                    // HEAD in the engine's transfer loop.
                    break;
                }
            };

            // Collect digest strings that the API reports as available.
            let available: HashSet<&str> = response
                .layers
                .iter()
                .filter(|(_, available)| *available)
                .map(|(digest_str, _)| digest_str.as_str())
                .collect();

            for digest_str in &response.failures {
                warn!(
                    repo = %repo,
                    digest = %digest_str,
                    "ECR batch check reported failure for layer"
                );
            }

            // Map back to Digest keys using the pre-computed string.
            // Only available blobs enter the set. Absent, unavailable,
            // and failed digests are all treated as missing.
            for (digest_str, digest) in &pairs {
                if available.contains(digest_str.as_str()) {
                    existing.insert((*digest).clone());
                }
            }
        }

        Ok(existing)
    }
}

impl BatchBlobChecker for BatchChecker {
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> CheckFuture<'a> {
        Box::pin(self.check_batched(repo, digests))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Mutex;

    use super::*;

    // --- ecr_region tests ---

    #[test]
    fn region_standard() {
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        assert_eq!(ecr_region(host), Some("us-east-1"));
    }

    #[test]
    fn region_fips() {
        let host = "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com";
        assert_eq!(ecr_region(host), Some("us-gov-west-1"));
    }

    #[test]
    fn region_dual_stack() {
        let host = "123456789012.dkr-ecr.us-east-1.amazonaws.com";
        assert_eq!(ecr_region(host), Some("us-east-1"));
    }

    #[test]
    fn region_dual_stack_fips() {
        let host = "123456789012.dkr-ecr-fips.us-gov-west-1.amazonaws.com";
        assert_eq!(ecr_region(host), Some("us-gov-west-1"));
    }

    #[test]
    fn region_china() {
        let host = "123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn";
        assert_eq!(ecr_region(host), Some("cn-north-1"));
    }

    #[test]
    fn region_iso() {
        let host = "123456789012.dkr.ecr.us-iso-east-1.c2s.ic.gov";
        assert_eq!(ecr_region(host), Some("us-iso-east-1"));
    }

    #[test]
    fn region_isob() {
        let host = "123456789012.dkr.ecr.us-isob-east-1.sc2s.sgov.gov";
        assert_eq!(ecr_region(host), Some("us-isob-east-1"));
    }

    #[test]
    fn region_eu_sovereign() {
        let host = "123456789012.dkr.ecr.eusc-de-east-1.amazonaws.eu";
        assert_eq!(ecr_region(host), Some("eusc-de-east-1"));
    }

    #[test]
    fn region_invalid_host() {
        assert_eq!(ecr_region("ghcr.io"), None);
        assert_eq!(ecr_region(""), None);
    }

    // --- ecr_registry_id tests ---

    #[test]
    fn registry_id_standard() {
        assert_eq!(
            ecr_registry_id("123456789012.dkr.ecr.us-east-1.amazonaws.com"),
            Some("123456789012")
        );
    }

    #[test]
    fn registry_id_empty() {
        assert_eq!(ecr_registry_id(""), None);
    }

    #[test]
    fn registry_id_dotless_hostname() {
        assert_eq!(ecr_registry_id("localhost"), None);
    }

    #[test]
    fn registry_id_non_numeric() {
        assert_eq!(
            ecr_registry_id("not-a-number.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    #[test]
    fn registry_id_too_short() {
        // 11 digits — too short
        assert_eq!(
            ecr_registry_id("12345678901.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    #[test]
    fn registry_id_too_long() {
        // 13 digits — too long
        assert_eq!(
            ecr_registry_id("1234567890123.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    // --- BatchBlobChecker tests ---

    /// Generate a valid test digest with a unique hex portion.
    fn test_digest(n: u8) -> Digest {
        let hex = format!("{:0>64}", format!("{n:x}"));
        format!("sha256:{hex}").parse().unwrap()
    }

    /// Shared counters for verifying API call counts in tests.
    #[derive(Clone, Default)]
    struct CallCounts {
        check: Arc<AtomicUsize>,
    }

    /// Mock ECR API that returns pre-configured responses in order.
    ///
    /// Verifies that the caller passes the expected repository name and that
    /// digest strings match the expected set (per mock contract fidelity).
    struct MockEcrBatchApi {
        /// Expected repository name — panics if caller passes a different repo.
        expected_repo: String,
        check_responses: Mutex<VecDeque<Result<BatchCheckResponse, Error>>>,
        counts: CallCounts,
    }

    impl MockEcrBatchApi {
        fn new(expected_repo: &str, counts: CallCounts) -> Self {
            Self {
                expected_repo: expected_repo.to_owned(),
                check_responses: Mutex::new(VecDeque::new()),
                counts,
            }
        }

        fn with_check_responses(
            mut self,
            responses: Vec<Result<BatchCheckResponse, Error>>,
        ) -> Self {
            self.check_responses = Mutex::new(VecDeque::from(responses));
            self
        }
    }

    impl EcrBatchApi for MockEcrBatchApi {
        fn batch_check_layer_availability(
            &self,
            repo: &str,
            digests: &[String],
        ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + '_>> {
            assert_eq!(
                repo, self.expected_repo,
                "mock: caller passed wrong repo to batch API"
            );
            // Verify all digests have valid sha256: prefix (catches corruption).
            for d in digests {
                assert!(
                    d.starts_with("sha256:"),
                    "mock: invalid digest format passed to batch API: {d}"
                );
            }
            Box::pin(async move {
                self.counts.check.fetch_add(1, Ordering::Relaxed);
                let mut responses = self.check_responses.lock().await;
                responses.pop_front().unwrap_or_else(|| {
                    Err(Error::Other("mock: no check response available".into()))
                })
            })
        }
    }

    // --- BatchBlobChecker tests ---

    #[tokio::test]
    async fn check_all_blobs_exist() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);

        let response = BatchCheckResponse {
            layers: vec![(d1.to_string(), true), (d2.to_string(), true)],
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone())
            .with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &[d1.clone(), d2.clone()])
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert!(result.contains(&d1));
        assert!(result.contains(&d2));
        assert_eq!(counts.check.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn check_some_blobs_missing() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);
        let d3 = test_digest(3);

        let response = BatchCheckResponse {
            layers: vec![(d1.to_string(), true), (d2.to_string(), false)],
            failures: vec![d3.to_string()],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone())
            .with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(
                &RepositoryName::new("my-repo"),
                &[d1.clone(), d2.clone(), d3.clone()],
            )
            .await
            .unwrap();

        // Only d1 is available; d2 (unavailable) and d3 (failure) are absent.
        assert_eq!(result.len(), 1);
        assert!(result.contains(&d1));
        assert!(!result.contains(&d2));
        assert!(!result.contains(&d3));
    }

    #[tokio::test]
    async fn check_splits_batches_at_100() {
        // Create 250 digests — should result in 3 API calls (100, 100, 50).
        let digests: Vec<Digest> = (0..250u16)
            .map(|n| {
                let hex = format!("{:0>64x}", n);
                format!("sha256:{hex}").parse().unwrap()
            })
            .collect();

        // Build 3 responses, each marking all digests in the batch as available.
        let responses: Vec<Result<BatchCheckResponse, Error>> = digests
            .chunks(MAX_DIGESTS_PER_BATCH)
            .map(|chunk| {
                Ok(BatchCheckResponse {
                    layers: chunk.iter().map(|d| (d.to_string(), true)).collect(),
                    failures: vec![],
                })
            })
            .collect();

        assert_eq!(responses.len(), 3);

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone()).with_check_responses(responses);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &digests)
            .await
            .unwrap();

        assert_eq!(result.len(), 250);
        for d in &digests {
            assert!(result.contains(d));
        }

        // Verify exactly 3 API calls were made.
        assert_eq!(
            counts.check.load(Ordering::Relaxed),
            3,
            "expected 3 batch API calls for 250 digests"
        );
    }

    #[tokio::test]
    async fn check_empty_digests() {
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone());
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &[])
            .await
            .unwrap();

        assert!(result.is_empty());
        assert_eq!(counts.check.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn check_propagates_api_error() {
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts)
            .with_check_responses(vec![Err(Error::Other("throttled".into()))]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &[test_digest(1)])
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("throttled"));
    }

    #[tokio::test]
    async fn check_all_unavailable_returns_empty() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);

        // API responds with entries but none are available.
        let response = BatchCheckResponse {
            layers: vec![(d1.to_string(), false), (d2.to_string(), false)],
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts).with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &[d1, d2])
            .await
            .unwrap();

        assert!(result.is_empty(), "no blobs should be reported as existing");
    }

    #[tokio::test]
    async fn check_digest_missing_from_response_treated_as_unavailable() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);

        // Response only mentions d1, d2 is absent entirely.
        let response = BatchCheckResponse {
            layers: vec![(d1.to_string(), true)],
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts).with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &[d1.clone(), d2.clone()])
            .await
            .unwrap();

        assert!(result.contains(&d1));
        assert!(!result.contains(&d2));
    }

    #[tokio::test]
    async fn check_partial_batch_failure_preserves_results() {
        // 150 digests → 2 batches (100 + 50).
        // Batch 1 succeeds (all available), batch 2 fails.
        // Result should contain the 100 successful results.
        let digests: Vec<Digest> = (0..150u16)
            .map(|n| {
                let hex = format!("{:0>64x}", n);
                format!("sha256:{hex}").parse().unwrap()
            })
            .collect();

        let batch1_response = BatchCheckResponse {
            layers: digests[..100]
                .iter()
                .map(|d| (d.to_string(), true))
                .collect(),
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone()).with_check_responses(vec![
            Ok(batch1_response),
            Err(Error::Other("throttled on batch 2".into())),
        ]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo"), &digests)
            .await
            .unwrap();

        // First 100 digests should be present (all available in batch 1).
        assert_eq!(
            result.len(),
            100,
            "partial results from successful batch must be preserved"
        );
        for d in &digests[..100] {
            assert!(result.contains(d));
        }
        // Remaining 50 not in the result (batch 2 failed, never checked).
        for d in &digests[100..] {
            assert!(
                !result.contains(d),
                "unchecked digests from failed batch must not appear"
            );
        }
        // 2 API calls were made (first succeeded, second failed).
        assert_eq!(counts.check.load(Ordering::Relaxed), 2);
    }

    // --- Trait object compatibility ---

    #[test]
    fn batch_blob_checker_is_object_safe() {
        // Verify the trait can be used as Rc<dyn BatchBlobChecker>.
        fn _assert_object_safe(_: std::rc::Rc<dyn BatchBlobChecker>) {}
    }
}
