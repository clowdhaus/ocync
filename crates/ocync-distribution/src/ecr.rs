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

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use aws_sdk_ecr::types::LayerAvailability;
use tracing::warn;

use crate::digest::Digest;
use crate::error::Error;

/// Maximum number of layer digests per `BatchCheckLayerAvailability` API call.
///
/// ECR enforces a limit of 100 digests per request. Larger batches are
/// automatically split into multiple API calls.
const MAX_DIGESTS_PER_BATCH: usize = 100;

/// Boxed future returned by [`BatchBlobChecker::check_blob_existence`].
type CheckFuture<'a> = Pin<Box<dyn Future<Output = Result<HashMap<Digest, bool>, Error>> + 'a>>;

/// Async trait for batch blob existence checking.
///
/// Used by the sync engine to efficiently determine which blobs already exist
/// at an ECR target registry before initiating transfers. Implementations are
/// intended to be held as `Rc<dyn BatchBlobChecker>` on a single-threaded
/// tokio runtime, so no `Send` or `Sync` bounds are required.
pub trait BatchBlobChecker {
    /// Check which blobs exist in the given repository.
    ///
    /// Returns a map from each input digest to `true` (exists) or `false`
    /// (missing). Digests that the API reports as failures are mapped to
    /// `false`.
    fn check_blob_existence<'a>(&'a self, repo: &'a str, digests: &'a [Digest]) -> CheckFuture<'a>;
}

/// Abstraction over ECR batch API calls for testability.
///
/// Wraps `BatchCheckLayerAvailability` and `CreateRepository` so tests can
/// inject mock responses without an SDK client.
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
/// Construct via [`EcrBatchChecker::new`] with an already-loaded
/// [`aws_config::SdkConfig`]. FIPS support is handled at the config level.
pub struct EcrBatchChecker {
    api: Box<dyn EcrBatchApi>,
}

impl std::fmt::Debug for EcrBatchChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcrBatchChecker").finish_non_exhaustive()
    }
}

impl EcrBatchChecker {
    /// Create a new batch checker from an AWS SDK config.
    ///
    /// The `registry_id` is the 12-digit AWS account ID that owns the ECR
    /// registry. Pass `None` to use the default registry for the caller's
    /// account.
    pub fn new(config: &aws_config::SdkConfig, registry_id: Option<String>) -> Self {
        let client = aws_sdk_ecr::Client::new(config);
        Self {
            api: Box::new(AwsEcrBatchApi {
                client,
                registry_id,
            }),
        }
    }

    /// Create an ECR batch checker with an injected API implementation.
    #[cfg(test)]
    fn with_api(api: impl EcrBatchApi + 'static) -> Self {
        Self { api: Box::new(api) }
    }

    /// Check blob existence, splitting into batches of 100.
    async fn check_batched(
        &self,
        repo: &str,
        digests: &[Digest],
    ) -> Result<HashMap<Digest, bool>, Error> {
        let mut result = HashMap::with_capacity(digests.len());

        for chunk in digests.chunks(MAX_DIGESTS_PER_BATCH) {
            let digest_strings: Vec<String> = chunk.iter().map(|d| d.to_string()).collect();
            let response = self
                .api
                .batch_check_layer_availability(repo, &digest_strings)
                .await?;

            // Build a lookup from digest string to availability.
            let mut availability: HashMap<&str, bool> =
                HashMap::with_capacity(response.layers.len() + response.failures.len());

            for (digest_str, available) in &response.layers {
                availability.insert(digest_str.as_str(), *available);
            }

            for digest_str in &response.failures {
                warn!(
                    repo = %repo,
                    digest = %digest_str,
                    "ECR batch check reported failure for layer"
                );
                availability.insert(digest_str.as_str(), false);
            }

            // Map back to Digest keys. Digests not in the response are
            // treated as unavailable (defensive against partial responses).
            for digest in chunk {
                let exists = availability
                    .get(digest.to_string().as_str())
                    .copied()
                    .unwrap_or(false);
                result.insert(digest.clone(), exists);
            }
        }

        Ok(result)
    }
}

impl BatchBlobChecker for EcrBatchChecker {
    fn check_blob_existence<'a>(&'a self, repo: &'a str, digests: &'a [Digest]) -> CheckFuture<'a> {
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
    struct MockEcrBatchApi {
        check_responses: Mutex<VecDeque<Result<BatchCheckResponse, Error>>>,
        counts: CallCounts,
    }

    impl MockEcrBatchApi {
        fn new(counts: CallCounts) -> Self {
            Self {
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
            _repo: &str,
            _digests: &[String],
        ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + '_>> {
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
        let mock = MockEcrBatchApi::new(counts.clone()).with_check_responses(vec![Ok(response)]);
        let checker = EcrBatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence("my-repo", &[d1.clone(), d2.clone()])
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[&d1], true);
        assert_eq!(result[&d2], true);
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
        let mock = MockEcrBatchApi::new(counts.clone()).with_check_responses(vec![Ok(response)]);
        let checker = EcrBatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence("my-repo", &[d1.clone(), d2.clone(), d3.clone()])
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[&d1], true);
        assert_eq!(result[&d2], false);
        assert_eq!(result[&d3], false);
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
        let mock = MockEcrBatchApi::new(counts.clone()).with_check_responses(responses);
        let checker = EcrBatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence("my-repo", &digests)
            .await
            .unwrap();

        assert_eq!(result.len(), 250);
        for d in &digests {
            assert_eq!(result[d], true);
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
        let mock = MockEcrBatchApi::new(counts.clone());
        let checker = EcrBatchChecker::with_api(mock);

        let result = checker.check_blob_existence("my-repo", &[]).await.unwrap();

        assert!(result.is_empty());
        assert_eq!(counts.check.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn check_propagates_api_error() {
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new(counts)
            .with_check_responses(vec![Err(Error::Other("throttled".into()))]);
        let checker = EcrBatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence("my-repo", &[test_digest(1)])
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("throttled"));
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
        let mock = MockEcrBatchApi::new(counts).with_check_responses(vec![Ok(response)]);
        let checker = EcrBatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence("my-repo", &[d1.clone(), d2.clone()])
            .await
            .unwrap();

        assert_eq!(result[&d1], true);
        assert_eq!(result[&d2], false);
    }

    // --- Trait object compatibility ---

    #[test]
    fn batch_blob_checker_is_object_safe() {
        // Verify the trait can be used as Rc<dyn BatchBlobChecker>.
        fn _assert_object_safe(_: std::rc::Rc<dyn BatchBlobChecker>) {}
    }
}
