//! AWS ECR batch operations - bulk blob existence checks.
//!
//! Provides [`BatchBlobChecker`] for bulk blob existence checking via ECR's
//! `BatchCheckLayerAvailability` API. These complement the per-request OCI
//! distribution API with ECR-native batch operations that reduce API
//! round-trips by up to 98%.
//!
//! `AWS_USE_FIPS_ENDPOINT=true` routes the calls to the FIPS endpoint,
//! resolved in `aws/endpoint.rs`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use aws_config::BehaviorVersion;
use serde::{Deserialize, Serialize};
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

/// Load an SDK config for the given ECR hostname.
///
/// Extracts the region from the hostname. The config also carries the
/// credentials and the FIPS, dual-stack and endpoint URL settings.
///
/// When `profile` is `Some`, credential resolution is scoped to that named
/// profile; when `None`, the ambient credential chain is used. Profile-not-found
/// errors surface at the first ECR API call, not here.
pub(crate) async fn load_sdk_config(
    hostname: &str,
    profile: Option<&str>,
) -> Result<aws_config::SdkConfig, Error> {
    let region = ecr_region(hostname).ok_or_else(|| Error::EcrApi {
        reason: format!("cannot extract AWS region from ECR hostname '{hostname}'"),
    })?;

    let mut builder = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()));
    if let Some(p) = profile {
        builder = builder.profile_name(p);
    }
    Ok(builder.load().await)
}

/// Maximum number of layer digests per `BatchCheckLayerAvailability` API call.
///
/// ECR enforces a limit of 100 digests per request. Larger batches are
/// automatically split into multiple API calls.
const MAX_DIGESTS_PER_BATCH: usize = 100;

/// Boxed future returned by [`BatchBlobChecker::check_blob_existence`].
type CheckFuture<'a> = Pin<Box<dyn Future<Output = Result<BlobExistence, Error>> + 'a>>;

/// What a batch check learned about the digests it was asked about.
///
/// A digest is known available, known missing, or unknown. An unknown digest
/// still needs its own check, so a checker that learns less costs requests,
/// never a needless upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobExistence {
    available: HashSet<Digest>,
    missing: HashSet<Digest>,
}

impl BlobExistence {
    /// Records a check of `checked` that found `found` available.
    ///
    /// Each checked digest is available when found and missing otherwise. A
    /// found digest that was not checked is dropped, so a stray answer cannot
    /// skip a transfer.
    pub fn new(checked: &[Digest], found: impl IntoIterator<Item = Digest>) -> Self {
        let found: HashSet<Digest> = found.into_iter().collect();
        let (available, missing) = checked.iter().cloned().partition(|d| found.contains(d));
        Self { available, missing }
    }

    /// Whether the check found `digest` available.
    pub fn is_available(&self, digest: &Digest) -> bool {
        self.available.contains(digest)
    }

    /// Whether the check learned if `digest` is available.
    pub fn is_known(&self, digest: &Digest) -> bool {
        self.available.contains(digest) || self.missing.contains(digest)
    }

    /// The digests the check found available.
    pub fn into_available(self) -> HashSet<Digest> {
        self.available
    }
}

/// Boxed future returned by [`BatchBlobChecker::wait_for_blobs_available`].
type WaitFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Error>> + 'a>>;

/// Initial poll interval when waiting for an ECR consistency view to
/// converge. Doubles on each subsequent poll up to [`MAX_POLL_INTERVAL`].
const INITIAL_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Upper bound on the poll interval used while waiting on the ECR
/// consistency view. Keeps the worst-case stale period bounded so a
/// blob that becomes available right after a long sleep is picked up
/// quickly.
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Async trait for batch blob existence checking.
///
/// Used by the sync engine to efficiently determine which blobs already exist
/// at an ECR target registry before initiating transfers, and to gate manifest
/// commits on blob visibility (see [`Self::wait_for_blobs_available`]).
/// Implementations are intended to be held as `Rc<dyn BatchBlobChecker>` on a
/// single-threaded tokio runtime, so no `Send` or `Sync` bounds are required.
pub trait BatchBlobChecker {
    /// Check which blobs exist in the given repository.
    ///
    /// Returns what the check learned about the input digests. A failed batch
    /// leaves its digests unknown. Digests that the API reports as failures
    /// are treated as missing. A check that learned nothing is an error.
    fn check_blob_existence<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
    ) -> CheckFuture<'a>;

    /// Wait until all `digests` are reported available by the target.
    ///
    /// Polls [`Self::check_blob_existence`] with exponential backoff
    /// (starting at [`INITIAL_POLL_INTERVAL`], doubling up to
    /// [`MAX_POLL_INTERVAL`]) until either every digest is reported
    /// available or `deadline` is reached.
    ///
    /// # Why this exists
    ///
    /// ECR's manifest-validation index is eventually consistent with
    /// blob upload state. A blob `PUT /v2/.../blobs/...` returns
    /// 201 Created while the validator's view can lag for hundreds of
    /// milliseconds to several seconds at high concurrency. A manifest
    /// `PUT` issued during this window fails with HTTP 404 carrying
    /// `BLOB_UPLOAD_UNKNOWN`. Calling this method before manifest push
    /// gates the commit on the consistency view directly rather than
    /// relying on retry budget alone.
    ///
    /// # Error semantics
    ///
    /// Returns `Ok(())` once every digest is available. Returns an
    /// `Err` with [`Error::EcrApi`] carrying the count of still-missing
    /// digests when `deadline` expires; the caller is expected to log
    /// and proceed with the standard retry path. Individual poll
    /// failures (transient ECR API errors) are logged as warnings and
    /// treated as "no progress this iteration" -- the loop continues
    /// until `deadline`.
    ///
    /// # Default implementation
    ///
    /// The default polls `check_blob_existence` and returns early on
    /// full availability. Tests may override to avoid real sleeping.
    fn wait_for_blobs_available<'a>(
        &'a self,
        repo: &'a RepositoryName,
        digests: &'a [Digest],
        deadline: Duration,
    ) -> WaitFuture<'a> {
        Box::pin(default_wait_for_blobs_available(
            self, repo, digests, deadline,
        ))
    }
}

/// Default polling loop shared by every [`BatchBlobChecker`] impl.
///
/// Free function so the trait remains object-safe under `Rc<dyn ...>`
/// while still factoring out the polling logic.
async fn default_wait_for_blobs_available<T>(
    checker: &T,
    repo: &RepositoryName,
    digests: &[Digest],
    deadline: Duration,
) -> Result<(), Error>
where
    T: BatchBlobChecker + ?Sized,
{
    if digests.is_empty() {
        return Ok(());
    }

    let start = tokio::time::Instant::now();
    let mut interval = INITIAL_POLL_INTERVAL;
    // Track the surface of "still missing" so we only re-query the
    // shrinking remainder, not the full input on every poll. ECR
    // BatchCheckLayerAvailability counts toward an account-level quota
    // (10 TPS), so trimming the request size reduces blast radius when
    // the wait spans several seconds.
    let mut remaining: Vec<Digest> = digests.to_vec();

    loop {
        match checker.check_blob_existence(repo, &remaining).await {
            Ok(found) => {
                remaining.retain(|d| !found.is_available(d));
                if remaining.is_empty() {
                    return Ok(());
                }
            }
            Err(e) => {
                warn!(
                    repo = %repo,
                    error = %e,
                    "BatchCheckLayerAvailability poll failed; retrying until deadline"
                );
            }
        }

        let elapsed = start.elapsed();
        if elapsed >= deadline {
            return Err(Error::EcrApi {
                reason: format!(
                    "BatchCheckLayerAvailability timed out for {repo} after {:?} with {} digest(s) still missing",
                    elapsed,
                    remaining.len()
                ),
            });
        }

        // Cap sleep so we never overshoot the deadline by more than one
        // interval, which would be observable as a wait noticeably
        // longer than requested.
        let sleep_for = interval.min(deadline.saturating_sub(elapsed));
        tokio::time::sleep(sleep_for).await;
        interval = interval.saturating_mul(2).min(MAX_POLL_INTERVAL);
    }
}

/// Abstraction over ECR batch API calls for testability.
///
/// Wraps `BatchCheckLayerAvailability` so tests can inject mock responses
/// without a network.
trait EcrBatchApi {
    /// Call `BatchCheckLayerAvailability` for a single batch (up to 100 digests).
    fn batch_check_layer_availability<'a>(
        &'a self,
        repo: &'a str,
        digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + 'a>>;
}

/// Response from a single `BatchCheckLayerAvailability` call.
#[derive(Debug, PartialEq, Eq)]
struct BatchCheckResponse {
    /// Layers the repository holds.
    available: Vec<Digest>,
    /// Layers that were not checked. They are transferred.
    failures: Vec<LayerIssue>,
}

/// Why a layer in a batch answer was not checked.
#[derive(Debug, PartialEq, Eq)]
enum LayerIssue {
    /// ECR reported a failure for the digest, as it named it.
    Failed(String),
    /// ECR reported a digest available that ocync cannot parse.
    Unparsed(String),
}

/// Default [`EcrBatchApi`], calling ECR through [`crate::aws::Client`].
#[derive(Debug)]
struct AwsEcrBatchApi {
    client: crate::aws::Client,
    registry_id: Option<String>,
}

/// ECR's `BatchCheckLayerAvailability` for one batch.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchCheckLayerAvailability<'a> {
    layer_digests: &'a [Digest],
    /// The account, when the hostname names one. Without it ECR checks the
    /// caller's own account.
    #[serde(skip_serializing_if = "Option::is_none")]
    registry_id: Option<&'a str>,
    repository_name: &'a str,
}

impl crate::aws::Operation for BatchCheckLayerAvailability<'_> {
    const NAME: &'static str = "BatchCheckLayerAvailability";
    type Output = BatchCheckOutput;
}

/// The `BatchCheckLayerAvailability` output fields ocync reads.
#[derive(Debug, Deserialize)]
struct BatchCheckOutput {
    #[serde(default, deserialize_with = "crate::aws::null_as_default")]
    layers: Vec<Layer>,
    #[serde(default, deserialize_with = "crate::aws::null_as_default")]
    failures: Vec<LayerFailure>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Layer {
    /// Kept as text so one digest ocync cannot parse costs only that layer
    /// its skip, not the whole batch.
    layer_digest: Option<String>,
    #[serde(default, deserialize_with = "crate::aws::null_as_default")]
    layer_availability: LayerAvailability,
}

/// ECR's `layerAvailability` values.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum LayerAvailability {
    /// The repository holds the layer.
    Available,
    /// The layer is missing, absent from the answer, or a value newer than
    /// this list: either way it is transferred.
    #[default]
    #[serde(other)]
    Unavailable,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayerFailure {
    layer_digest: Option<String>,
}

impl From<BatchCheckOutput> for BatchCheckResponse {
    /// An available digest ocync cannot parse joins the failures, so it is
    /// logged and transferred.
    fn from(output: BatchCheckOutput) -> Self {
        let mut failures: Vec<LayerIssue> = output
            .failures
            .into_iter()
            .filter_map(|f| f.layer_digest.map(LayerIssue::Failed))
            .collect();
        let mut available = Vec::new();
        for layer in output.layers {
            if layer.layer_availability != LayerAvailability::Available {
                continue;
            }
            let Some(text) = layer.layer_digest else {
                continue;
            };
            match text.parse() {
                Ok(digest) => available.push(digest),
                Err(_) => failures.push(LayerIssue::Unparsed(text)),
            }
        }
        Self {
            available,
            failures,
        }
    }
}

impl EcrBatchApi for AwsEcrBatchApi {
    fn batch_check_layer_availability<'a>(
        &'a self,
        repo: &'a str,
        digests: &'a [Digest],
    ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + 'a>> {
        Box::pin(async move {
            let input = BatchCheckLayerAvailability {
                layer_digests: digests,
                registry_id: self.registry_id.as_deref(),
                repository_name: repo,
            };
            let output = self.client.call(&input).await.map_err(|e| Error::EcrApi {
                reason: format!("{e} for '{repo}'"),
            })?;
            Ok(output.into())
        })
    }
}

/// ECR batch checker.
///
/// Provides bulk blob existence checking via `BatchCheckLayerAvailability`,
/// splitting large batches into chunks of 100 (the ECR API limit per call).
///
/// Construct via [`BatchChecker::from_hostname`] with an ECR hostname.
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
    /// then loads the AWS config and builds the ECR client. Returns an error
    /// if the region cannot be determined from the hostname.
    ///
    /// `profile` must match the value passed to the corresponding
    /// [`crate::auth::EcrAuth::new`]; otherwise the batch checker authenticates
    /// as a different identity than the auth provider, which silently breaks
    /// `BatchCheckLayerAvailability` for registries reachable only via the
    /// named profile.
    pub async fn from_hostname(hostname: &str, profile: Option<&str>) -> Result<Self, Error> {
        let sdk_config = load_sdk_config(hostname, profile).await?;
        let registry_id = ecr_registry_id(hostname).map(|s| s.to_owned());
        let client =
            crate::aws::Client::new(&sdk_config, crate::aws::Service::Ecr).map_err(|e| {
                Error::EcrApi {
                    reason: e.to_string(),
                }
            })?;
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
    ) -> Result<BlobExistence, Error> {
        let mut found = Vec::with_capacity(digests.len());
        let mut checked = 0;

        for chunk in digests.chunks(MAX_DIGESTS_PER_BATCH) {
            let response = match self
                .api
                .batch_check_layer_availability(repo.as_str(), chunk)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        repo = %repo,
                        checked,
                        available = found.len(),
                        unchecked = digests.len() - checked,
                        error = %e,
                        "batch check failed mid-batch, returning partial results"
                    );
                    // Nothing checked: propagate the error so the engine logs
                    // the fallback warning.
                    if checked == 0 {
                        return Err(e);
                    }
                    // Keep what the earlier batches learned. The engine sends
                    // a per-blob HEAD for each digest left unknown.
                    break;
                }
            };

            for issue in &response.failures {
                match issue {
                    LayerIssue::Failed(digest) => warn!(
                        repo = %repo,
                        digest = %digest,
                        "ECR batch check reported failure for layer"
                    ),
                    LayerIssue::Unparsed(digest) => warn!(
                        repo = %repo,
                        digest = %digest,
                        "ECR batch check reported a layer digest ocync cannot parse"
                    ),
                }
            }

            // Absent, unavailable and failed digests are all treated as
            // missing.
            checked += chunk.len();
            found.extend(response.available);
        }

        Ok(BlobExistence::new(&digests[..checked], found))
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
        // 11 digits - too short
        assert_eq!(
            ecr_registry_id("12345678901.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    #[test]
    fn registry_id_too_long() {
        // 13 digits - too long
        assert_eq!(
            ecr_registry_id("1234567890123.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    /// Only `AVAILABLE` skips a transfer. A value ECR adds later, a missing
    /// or null availability or a digest ocync cannot parse must count as
    /// missing, not fail the whole batch.
    #[test]
    fn reads_layer_availability() {
        let (a, b, c, d, e) = (
            test_digest(1),
            test_digest(2),
            test_digest(3),
            test_digest(4),
            test_digest(5),
        );
        let body = format!(
            r#"{{"layers":[
                {{"layerDigest":"{a}","layerAvailability":"AVAILABLE"}},
                {{"layerDigest":"{b}","layerAvailability":"UNAVAILABLE"}},
                {{"layerDigest":"{c}","layerAvailability":"ARCHIVED"}},
                {{"layerDigest":"{d}"}},
                {{"layerDigest":"{e}","layerAvailability":null}},
                {{"layerDigest":"md5:nope","layerAvailability":"AVAILABLE"}}
            ],"failures":[{{"layerDigest":"sha256:bad","failureCode":"InvalidLayerDigest"}}]}}"#
        );
        let output: BatchCheckOutput = serde_json::from_str(&body).unwrap();
        assert_eq!(
            BatchCheckResponse::from(output),
            BatchCheckResponse {
                available: vec![a],
                failures: vec![
                    LayerIssue::Failed("sha256:bad".to_owned()),
                    LayerIssue::Unparsed("md5:nope".to_owned()),
                ],
            }
        );
    }

    /// A null list reads as empty, as the SDK reads it, so the batch is not
    /// lost to a decode failure.
    #[test]
    fn reads_null_lists_as_empty() {
        let output: BatchCheckOutput =
            serde_json::from_str(r#"{"layers":null,"failures":null}"#).unwrap();
        assert_eq!(
            BatchCheckResponse::from(output),
            BatchCheckResponse {
                available: vec![],
                failures: vec![],
            }
        );
    }

    /// The request names the account from the hostname, and leaves it out
    /// when the hostname names none, as the SDK does.
    #[test]
    fn writes_the_batch_check_request() {
        let digests = [test_digest(1)];
        for (registry_id, expected) in [
            (
                Some("123456789012"),
                serde_json::json!({"layerDigests": [digests[0].to_string()], "registryId": "123456789012", "repositoryName": "app"}),
            ),
            (
                None,
                serde_json::json!({"layerDigests": [digests[0].to_string()], "repositoryName": "app"}),
            ),
        ] {
            let input = BatchCheckLayerAvailability {
                layer_digests: &digests,
                registry_id,
                repository_name: "app",
            };
            assert_eq!(serde_json::to_value(&input).unwrap(), expected);
        }
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
        /// The digests each call asked about, in call order.
        asked: Arc<std::sync::Mutex<Vec<Vec<Digest>>>>,
    }

    impl CallCounts {
        fn calls(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    /// Mock ECR API that returns pre-configured responses in order.
    ///
    /// Verifies that the caller passes the expected repository name, and
    /// records the digests each call asks about for the test to check.
    struct MockEcrBatchApi {
        /// Expected repository name - panics if caller passes a different repo.
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
        fn batch_check_layer_availability<'a>(
            &'a self,
            repo: &'a str,
            digests: &'a [Digest],
        ) -> Pin<Box<dyn Future<Output = Result<BatchCheckResponse, Error>> + 'a>> {
            assert_eq!(
                repo, self.expected_repo,
                "mock: caller passed wrong repo to batch API"
            );
            self.counts.asked.lock().unwrap().push(digests.to_vec());
            Box::pin(async move {
                let mut responses = self.check_responses.lock().await;
                responses.pop_front().unwrap_or_else(|| {
                    Err(Error::EcrApi {
                        reason: "mock: no check response available".into(),
                    })
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
            available: vec![d1.clone(), d2.clone()],
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone())
            .with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(
                &RepositoryName::new("my-repo").unwrap(),
                &[d1.clone(), d2.clone()],
            )
            .await
            .unwrap();

        assert_eq!(result.into_available(), HashSet::from([d1, d2]));
        assert_eq!(counts.calls(), 1);
    }

    /// A digest in the answer that the request did not ask about is not
    /// taken as present, so it cannot skip a transfer of a different blob.
    #[tokio::test]
    async fn ignores_an_available_digest_it_did_not_ask_about() {
        let (asked, stray) = (test_digest(1), test_digest(2));
        let mock = MockEcrBatchApi::new("my-repo", CallCounts::default()).with_check_responses(
            vec![Ok(BatchCheckResponse {
                available: vec![stray.clone()],
                failures: vec![],
            })],
        );
        let result = BatchChecker::with_api(mock)
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &[asked])
            .await
            .unwrap();
        assert!(result.into_available().is_empty());
    }

    #[tokio::test]
    async fn check_some_blobs_missing() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);
        let d3 = test_digest(3);

        let response = BatchCheckResponse {
            available: vec![d1.clone()],
            failures: vec![LayerIssue::Failed(d3.to_string())],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone())
            .with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(
                &RepositoryName::new("my-repo").unwrap(),
                &[d1.clone(), d2.clone(), d3.clone()],
            )
            .await
            .unwrap();

        // Only d1 is available; d2 (unavailable) and d3 (failure) are known
        // missing.
        assert!(result.is_known(&d2) && result.is_known(&d3));
        assert_eq!(result.into_available(), HashSet::from([d1]));
    }

    #[tokio::test]
    async fn check_splits_batches_at_100() {
        // Create 250 digests - should result in 3 API calls (100, 100, 50).
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
                    available: chunk.to_vec(),
                    failures: vec![],
                })
            })
            .collect();

        assert_eq!(responses.len(), 3);

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone()).with_check_responses(responses);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &digests)
            .await
            .unwrap();

        assert_eq!(result.into_available(), digests.iter().cloned().collect());

        // Verify exactly 3 API calls were made.
        assert_eq!(
            counts.calls(),
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
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &[])
            .await
            .unwrap();

        assert!(result.into_available().is_empty());
        assert_eq!(counts.calls(), 0);
    }

    #[tokio::test]
    async fn check_propagates_api_error() {
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts).with_check_responses(vec![Err(
            Error::EcrApi {
                reason: "throttled".into(),
            },
        )]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &[test_digest(1)])
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
            available: vec![],
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts).with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &[d1, d2])
            .await
            .unwrap();

        assert!(
            result.into_available().is_empty(),
            "no blobs should be reported as existing"
        );
    }

    #[tokio::test]
    async fn check_digest_missing_from_response_treated_as_unavailable() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);

        // Response only mentions d1, d2 is absent entirely.
        let response = BatchCheckResponse {
            available: vec![d1.clone()],
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts).with_check_responses(vec![Ok(response)]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(
                &RepositoryName::new("my-repo").unwrap(),
                &[d1.clone(), d2.clone()],
            )
            .await
            .unwrap();

        assert!(result.is_available(&d1));
        assert!(!result.is_available(&d2));
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
            available: digests[..100].to_vec(),
            failures: vec![],
        };

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("my-repo", counts.clone()).with_check_responses(vec![
            Ok(batch1_response),
            Err(Error::EcrApi {
                reason: "throttled on batch 2".into(),
            }),
        ]);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &digests)
            .await
            .unwrap();

        // Remaining 50 are unknown (batch 2 failed, never checked).
        for d in &digests[100..] {
            assert!(
                !result.is_known(d),
                "digests from the failed batch must stay unknown"
            );
        }
        // First 100 digests should be present (all available in batch 1).
        assert_eq!(
            result.into_available(),
            digests[..100].iter().cloned().collect(),
            "partial results from successful batch must be preserved"
        );
        // 2 API calls were made (first succeeded, second failed).
        assert_eq!(counts.calls(), 2);
    }

    /// A batch that finds nothing still checked its digests: a later
    /// failure leaves them known missing and only the rest unknown. A
    /// failure before any batch answers learns nothing and is an error.
    #[tokio::test]
    async fn check_keeps_missing_digests_from_before_a_failed_batch() {
        let digests: Vec<Digest> = (0..150u16)
            .map(|n| format!("sha256:{n:0>64x}").parse().unwrap())
            .collect();
        let throttled = || {
            Err(Error::EcrApi {
                reason: "throttled".into(),
            })
        };
        let mock =
            MockEcrBatchApi::new("my-repo", CallCounts::default()).with_check_responses(vec![
                Ok(BatchCheckResponse {
                    available: vec![],
                    failures: vec![],
                }),
                throttled(),
            ]);
        let result = BatchChecker::with_api(mock)
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &digests)
            .await
            .unwrap();
        for d in &digests[..100] {
            assert!(result.is_known(d) && !result.is_available(d));
        }
        for d in &digests[100..] {
            assert!(!result.is_known(d));
        }

        let mock = MockEcrBatchApi::new("my-repo", CallCounts::default())
            .with_check_responses(vec![throttled()]);
        BatchChecker::with_api(mock)
            .check_blob_existence(&RepositoryName::new("my-repo").unwrap(), &digests)
            .await
            .unwrap_err();
    }

    // --- Trait object compatibility ---

    #[test]
    fn batch_blob_checker_is_object_safe() {
        // Verify the trait can be used as Rc<dyn BatchBlobChecker>.
        fn _assert_object_safe(_: std::rc::Rc<dyn BatchBlobChecker>) {}
    }

    // --- wait_for_blobs_available tests ---

    /// Empty input must short-circuit without any API call.
    #[tokio::test]
    async fn wait_empty_digests_short_circuits() {
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("repo", counts.clone());
        let checker = BatchChecker::with_api(mock);
        let result = checker
            .wait_for_blobs_available(
                &RepositoryName::new("repo").unwrap(),
                &[],
                Duration::from_secs(1),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(
            counts.calls(),
            0,
            "empty input must NOT trigger an ECR API call"
        );
    }

    /// First poll reports all blobs available -- single call, no polling.
    #[tokio::test]
    async fn wait_all_available_on_first_poll() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("repo", counts.clone()).with_check_responses(vec![Ok(
            BatchCheckResponse {
                available: vec![d1.clone(), d2.clone()],
                failures: vec![],
            },
        )]);
        let checker = BatchChecker::with_api(mock);
        let result = checker
            .wait_for_blobs_available(
                &RepositoryName::new("repo").unwrap(),
                &[d1, d2],
                Duration::from_secs(10),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(
            counts.calls(),
            1,
            "all-available on first poll must NOT loop"
        );
    }

    /// First poll reports partial availability, second reports the rest.
    /// The second call must request only the previously-missing digest
    /// (request trimming optimisation -- otherwise we re-query the
    /// already-available digest on every poll).
    #[tokio::test(start_paused = true)]
    async fn wait_polls_until_all_available_and_trims_requests() {
        let d1 = test_digest(1);
        let d2 = test_digest(2);

        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("repo", counts.clone()).with_check_responses(vec![
            // Poll 1: only d1 is available.
            Ok(BatchCheckResponse {
                available: vec![d1.clone()],
                failures: vec![],
            }),
            // Poll 2: d2 now available.
            Ok(BatchCheckResponse {
                available: vec![d2.clone()],
                failures: vec![],
            }),
        ]);
        let checker = BatchChecker::with_api(mock);
        let result = checker
            .wait_for_blobs_available(
                &RepositoryName::new("repo").unwrap(),
                &[d1.clone(), d2.clone()],
                Duration::from_secs(10),
            )
            .await;
        assert!(result.is_ok(), "{:?}", result.err());
        assert_eq!(counts.calls(), 2);
        assert_eq!(
            *counts.asked.lock().unwrap(),
            [vec![d1, d2.clone()], vec![d2]]
        );
    }

    /// Polls keep reporting blob missing until deadline expires.
    /// Returns `Err(EcrApi)` carrying a useful diagnostic.
    #[tokio::test(start_paused = true)]
    async fn wait_times_out_when_blob_never_appears() {
        let d1 = test_digest(1);
        // Mock returns "missing" forever. Use a long response queue so
        // the test isn't flaky on number of polls.
        let counts = CallCounts::default();
        let responses: Vec<Result<BatchCheckResponse, Error>> = (0..50)
            .map(|_| {
                Ok(BatchCheckResponse {
                    available: vec![],
                    failures: vec![],
                })
            })
            .collect();
        let mock = MockEcrBatchApi::new("repo", counts.clone()).with_check_responses(responses);
        let checker = BatchChecker::with_api(mock);

        let result = checker
            .wait_for_blobs_available(
                &RepositoryName::new("repo").unwrap(),
                &[d1],
                Duration::from_secs(2),
            )
            .await;
        match result {
            Err(Error::EcrApi { reason }) => {
                assert!(
                    reason.contains("timed out") && reason.contains("1 digest"),
                    "unexpected timeout reason: {reason}"
                );
            }
            other => panic!("expected EcrApi timeout, got {other:?}"),
        }
    }

    /// Poll failure does NOT abort the wait -- the loop continues until
    /// the deadline expires.
    #[tokio::test(start_paused = true)]
    async fn wait_continues_through_transient_api_errors() {
        let d1 = test_digest(1);
        let counts = CallCounts::default();
        let mock = MockEcrBatchApi::new("repo", counts.clone()).with_check_responses(vec![
            // Transient API failure on first poll.
            Err(Error::EcrApi {
                reason: "throttled".into(),
            }),
            // Second poll succeeds with blob now available.
            Ok(BatchCheckResponse {
                available: vec![d1.clone()],
                failures: vec![],
            }),
        ]);
        let checker = BatchChecker::with_api(mock);
        let result = checker
            .wait_for_blobs_available(
                &RepositoryName::new("repo").unwrap(),
                &[d1],
                Duration::from_secs(10),
            )
            .await;
        assert!(
            result.is_ok(),
            "transient errors must not abort the wait: {:?}",
            result.err()
        );
        assert_eq!(counts.calls(), 2);
    }

    #[tokio::test]
    async fn batch_checker_from_hostname_accepts_named_profile() {
        // Pins the BatchChecker / EcrAuth symmetry: both must thread the same
        // profile, otherwise sync runs against profile-scoped registries get
        // split-brain auth.
        let checker = BatchChecker::from_hostname(
            "123456789012.dkr.ecr.us-east-1.amazonaws.com",
            Some("nonexistent-profile-for-test"),
        )
        .await;
        assert!(checker.is_ok(), "{:?}", checker.err());
    }

    #[tokio::test]
    async fn load_sdk_config_accepts_named_profile() {
        // SdkConfig has no public profile accessor, so this only verifies the
        // load path itself does not reject an unknown profile name (resolution
        // is deferred to the first ECR API call).
        let cfg = load_sdk_config(
            "123456789012.dkr.ecr.us-east-1.amazonaws.com",
            Some("nonexistent-profile-for-test"),
        )
        .await
        .unwrap();
        assert_eq!(cfg.region().map(|r| r.as_ref()), Some("us-east-1"));
    }
}
