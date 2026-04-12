//! Sync engine — two-phase orchestrator for image transfers.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use ocync_distribution::blob::MountResult;
use ocync_distribution::spec::{ImageManifest, ManifestKind};
use ocync_distribution::{Digest, RegistryClient};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::plan::BlobDedupMap;
use crate::retry::{self, RetryConfig};
use crate::{BlobTransferStats, ImageResult, ImageStatus, SkipReason, SyncReport, SyncStats};

/// A fully resolved mapping ready for the sync engine.
///
/// All config resolution (registry lookup, tag filtering) is done
/// before constructing this type. The engine operates purely on
/// resolved values.
#[derive(Debug)]
pub struct ResolvedMapping {
    /// Client for the source registry.
    pub source_client: Arc<RegistryClient>,
    /// Repository path at the source (e.g. `library/nginx`).
    pub source_repo: String,
    /// Repository path at the target (e.g. `mirror/nginx`).
    pub target_repo: String,
    /// Target registries to sync to.
    pub targets: Vec<TargetEntry>,
    /// Tag pairs to sync (already filtered).
    pub tags: Vec<TagPair>,
}

/// A single target registry entry.
#[derive(Debug)]
pub struct TargetEntry {
    /// Human-readable name from config (e.g. `us-ecr`).
    pub name: String,
    /// Client for this target registry.
    pub client: Arc<RegistryClient>,
}

/// A source/target tag pair for syncing.
///
/// For most sync operations the source and target tags are identical.
/// For `copy` with retagging they may differ.
#[derive(Debug, Clone)]
pub struct TagPair {
    /// Tag name at the source registry.
    pub source: String,
    /// Tag name at the target registry.
    pub target: String,
}

impl TagPair {
    /// Create a pair where source and target tags are the same.
    pub fn same(tag: impl Into<String>) -> Self {
        let t = tag.into();
        Self {
            source: t.clone(),
            target: t,
        }
    }

    /// Create a pair with different source and target tags.
    pub fn retag(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }
}

/// Collect all blob digests from an image manifest (config + layers).
fn collect_image_blobs(manifest: &ImageManifest) -> Vec<Digest> {
    let mut digests = Vec::with_capacity(1 + manifest.layers.len());
    digests.push(manifest.config.digest.clone());
    for layer in &manifest.layers {
        digests.push(layer.digest.clone());
    }
    digests
}

/// Sync engine — orchestrates image transfers across registries.
///
/// Processes [`ResolvedMapping`]s sequentially, syncing each tag from source
/// to every target. Blob deduplication is tracked globally so that layers
/// shared across images or repositories are transferred at most once per
/// target registry.
#[derive(Debug)]
pub struct SyncEngine {
    retry: RetryConfig,
    dedup: BlobDedupMap,
}

impl SyncEngine {
    /// Create a new sync engine with the given retry configuration.
    pub fn new(retry: RetryConfig) -> Self {
        Self {
            retry,
            dedup: BlobDedupMap::new(),
        }
    }

    /// Run the sync engine across all resolved mappings.
    ///
    /// Processes mappings sequentially. For each mapping, iterates over every
    /// tag and every target, calling [`sync_image`](Self::sync_image) for each
    /// combination. Progress callbacks are invoked before and after each image.
    /// Returns a [`SyncReport`] with per-image results and aggregate statistics.
    pub async fn run(
        &mut self,
        mappings: Vec<ResolvedMapping>,
        progress: &dyn crate::progress::ProgressReporter,
    ) -> SyncReport {
        let run_start = Instant::now();
        let run_id = Uuid::now_v7();
        let mut images = Vec::new();

        for mapping in &mappings {
            for tag_pair in &mapping.tags {
                for target in &mapping.targets {
                    let source_ref = format!(
                        "{}:{} -> {}",
                        mapping.source_repo, tag_pair.source, target.name
                    );
                    let target_ref = format!("{}:{}", mapping.target_repo, tag_pair.target);

                    progress.image_started(&source_ref, &target_ref);

                    let result = self
                        .sync_image(
                            &mapping.source_client,
                            &mapping.source_repo,
                            &target.client,
                            &target.name,
                            &mapping.target_repo,
                            &tag_pair.source,
                            &tag_pair.target,
                        )
                        .await;

                    progress.image_completed(&result);
                    images.push(result);
                }
            }
        }

        let stats = compute_stats(&images);
        let duration = run_start.elapsed();

        let report = SyncReport {
            run_id,
            images,
            stats,
            duration,
        };

        progress.run_completed(&report);
        report
    }

    /// Sync a single image (one tag) from source to one target.
    ///
    /// Returns an [`ImageResult`] — never panics. All errors are captured
    /// as [`ImageStatus::Failed`].
    #[allow(clippy::too_many_arguments)]
    async fn sync_image(
        &mut self,
        source: &RegistryClient,
        source_repo: &str,
        target: &RegistryClient,
        target_name: &str,
        target_repo: &str,
        source_tag: &str,
        target_tag: &str,
    ) -> ImageResult {
        let image_start = Instant::now();
        let image_id = Uuid::now_v7();
        let source_ref = format!("{source_repo}:{source_tag}");
        let target_ref = format!("{target_repo}:{target_tag}");

        let result = self
            .sync_image_inner(
                source,
                source_repo,
                target,
                target_name,
                target_repo,
                source_tag,
                target_tag,
            )
            .await;

        let duration = image_start.elapsed();

        match result {
            Ok((status, bytes_transferred, blob_stats)) => ImageResult {
                image_id,
                source: source_ref,
                target: target_ref,
                status,
                bytes_transferred,
                blob_stats,
                duration,
            },
            Err(err) => {
                warn!(source = %source_ref, target = %target_ref, error = %err, "image sync failed");
                ImageResult {
                    image_id,
                    source: source_ref,
                    target: target_ref,
                    status: ImageStatus::Failed {
                        error: err.to_string(),
                        retries: self.retry.max_retries,
                    },
                    bytes_transferred: 0,
                    blob_stats: BlobTransferStats::default(),
                    duration,
                }
            }
        }
    }

    /// Inner implementation of image sync that returns Result for ergonomic error handling.
    #[allow(clippy::too_many_arguments)]
    async fn sync_image_inner(
        &mut self,
        source: &RegistryClient,
        source_repo: &str,
        target: &RegistryClient,
        target_name: &str,
        target_repo: &str,
        source_tag: &str,
        target_tag: &str,
    ) -> Result<(ImageStatus, u64, BlobTransferStats), crate::Error> {
        // Step 1: Pull source manifest.
        let pull = with_retry(&self.retry, "manifest pull", || {
            source.manifest_pull(source_repo, source_tag)
        })
        .await
        .map_err(|e| crate::Error::Manifest {
            reference: source_tag.to_owned(),
            source: e,
        })?;

        // Step 2: HEAD target manifest — skip if digest matches.
        match target.manifest_head(target_repo, target_tag).await {
            Ok(Some(head)) if head.digest == pull.digest => {
                info!(
                    source_repo,
                    target_repo,
                    source_tag,
                    target_tag,
                    digest = %pull.digest,
                    "skipping — digest matches at target"
                );
                return Ok((
                    ImageStatus::Skipped {
                        reason: SkipReason::DigestMatch,
                    },
                    0,
                    BlobTransferStats::default(),
                ));
            }
            Ok(_) => {} // manifest missing or digest differs — proceed
            Err(e) => {
                debug!(
                    target_repo,
                    target_tag,
                    error = %e,
                    "target manifest HEAD failed, proceeding with sync"
                );
            }
        }

        // Step 3/4: Handle based on manifest kind.
        let mut blob_stats = BlobTransferStats::default();

        let bytes_transferred = match &pull.manifest {
            ManifestKind::Image(image_manifest) => {
                let blobs = collect_image_blobs(image_manifest);
                let (bytes, stats) = self
                    .transfer_blobs(
                        source,
                        source_repo,
                        target,
                        target_name,
                        target_repo,
                        &blobs,
                    )
                    .await?;
                blob_stats = stats;

                // Push manifest by tag.
                with_retry(&self.retry, "manifest push", || {
                    target.manifest_push(target_repo, target_tag, &pull.media_type, &pull.raw_bytes)
                })
                .await
                .map_err(|e| crate::Error::Manifest {
                    reference: target_tag.to_owned(),
                    source: e,
                })?;

                bytes
            }
            ManifestKind::Index(index) => {
                let mut total_bytes = 0u64;

                // For each child descriptor, pull child manifest, transfer blobs, push child.
                for child_desc in &index.manifests {
                    let child_digest_str = child_desc.digest.to_string();
                    let child_pull = with_retry(&self.retry, "manifest pull", || {
                        source.manifest_pull(source_repo, &child_digest_str)
                    })
                    .await
                    .map_err(|e| crate::Error::Manifest {
                        reference: child_digest_str.clone(),
                        source: e,
                    })?;

                    if let ManifestKind::Image(child_image) = &child_pull.manifest {
                        let blobs = collect_image_blobs(child_image);
                        let (bytes, child_stats) = self
                            .transfer_blobs(
                                source,
                                source_repo,
                                target,
                                target_name,
                                target_repo,
                                &blobs,
                            )
                            .await?;
                        total_bytes = total_bytes.saturating_add(bytes);
                        blob_stats.transferred += child_stats.transferred;
                        blob_stats.skipped += child_stats.skipped;
                        blob_stats.mounted += child_stats.mounted;
                    }

                    // Push child manifest to target by digest.
                    with_retry(&self.retry, "manifest push", || {
                        target.manifest_push(
                            target_repo,
                            &child_digest_str,
                            &child_pull.media_type,
                            &child_pull.raw_bytes,
                        )
                    })
                    .await
                    .map_err(|e| crate::Error::Manifest {
                        reference: child_digest_str.clone(),
                        source: e,
                    })?;
                }

                // Push the index by tag.
                with_retry(&self.retry, "manifest push", || {
                    target.manifest_push(target_repo, target_tag, &pull.media_type, &pull.raw_bytes)
                })
                .await
                .map_err(|e| crate::Error::Manifest {
                    reference: target_tag.to_owned(),
                    source: e,
                })?;

                total_bytes
            }
        };

        info!(
            source_repo,
            target_repo, source_tag, target_tag, bytes_transferred, "image synced"
        );

        Ok((ImageStatus::Synced, bytes_transferred, blob_stats))
    }

    /// Transfer blobs from source to target, using dedup and cross-repo mount.
    ///
    /// For each digest: checks the dedup map, does a HEAD check at the target,
    /// attempts a cross-repo mount, and falls back to pull+push. Returns the
    /// total bytes transferred and per-call blob statistics.
    async fn transfer_blobs(
        &mut self,
        source: &RegistryClient,
        source_repo: &str,
        target: &RegistryClient,
        target_name: &str,
        target_repo: &str,
        digests: &[Digest],
    ) -> Result<(u64, BlobTransferStats), crate::Error> {
        let mut total_bytes = 0u64;
        let mut stats = BlobTransferStats::default();

        for digest in digests {
            // Check dedup map — skip if already handled.
            if let Some(status) = self.dedup.status(target_name, digest) {
                use crate::plan::BlobStatus;
                match status {
                    BlobStatus::ExistsAtTarget | BlobStatus::Completed | BlobStatus::InProgress => {
                        debug!(%digest, status = ?status, "blob already handled, skipping");
                        stats.skipped += 1;
                        continue;
                    }
                    BlobStatus::Unknown | BlobStatus::Failed(_) => {}
                }
            }

            // HEAD check at target — if exists, mark in dedup map and skip.
            match target.blob_exists(target_repo, digest).await {
                Ok(Some(_size)) => {
                    debug!(%digest, "blob exists at target, skipping");
                    self.dedup.set_exists(target_name, digest, target_repo);
                    stats.skipped += 1;
                    continue;
                }
                Ok(None) => {} // not found, need to transfer
                Err(e) => {
                    debug!(%digest, error = %e, "blob HEAD check failed, will attempt transfer");
                }
            }

            // Mark in-progress.
            self.dedup.set_in_progress(target_name, digest);

            // Try cross-repo mount.
            let mount_source = self
                .dedup
                .mount_source(target_name, digest, target_repo)
                .map(|s| s.to_owned());

            if let Some(from_repo) = mount_source {
                debug!(%digest, %from_repo, "attempting cross-repo mount");
                match target.blob_mount(target_repo, digest, &from_repo).await {
                    Ok(MountResult::Mounted) => {
                        debug!(%digest, %from_repo, "blob mounted");
                        self.dedup.set_completed(target_name, digest, target_repo);
                        stats.mounted += 1;
                        continue;
                    }
                    Ok(MountResult::FallbackUpload { .. }) => {
                        debug!(%digest, "mount fallback, proceeding with pull+push");
                    }
                    Err(e) => {
                        debug!(%digest, error = %e, "mount failed, proceeding with pull+push");
                    }
                }
            }

            // Pull from source with retry.
            let data = match with_retry(&self.retry, "blob pull", || {
                source.blob_pull_all(source_repo, digest)
            })
            .await
            {
                Ok(data) => data,
                Err(e) => {
                    let err = crate::Error::BlobPull {
                        digest: digest.to_string(),
                        source: e,
                    };
                    self.dedup.set_failed(target_name, digest, err.to_string());
                    return Err(err);
                }
            };
            let blob_size = data.len() as u64;

            // Push to target with retry.
            if let Err(e) = with_retry(&self.retry, "blob push", || {
                target.blob_push(target_repo, &data)
            })
            .await
            {
                let err = crate::Error::BlobPush {
                    digest: digest.to_string(),
                    source: e,
                };
                self.dedup.set_failed(target_name, digest, err.to_string());
                return Err(err);
            }

            total_bytes = total_bytes.saturating_add(blob_size);
            stats.transferred += 1;

            // Mark completed.
            self.dedup.set_completed(target_name, digest, target_repo);
        }

        Ok((total_bytes, stats))
    }
}

/// Retry an async operation with exponential backoff on transient HTTP errors.
///
/// Calls `f()` in a loop. If the result is `Err` with a retryable HTTP status
/// (408, 429, 5xx), waits with exponential backoff and tries again up to
/// `config.max_retries` times. Returns the first `Ok` or the final `Err`.
async fn with_retry<T, F, Fut>(
    config: &RetryConfig,
    operation: &str,
    f: F,
) -> Result<T, ocync_distribution::Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, ocync_distribution::Error>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if let Some(status) = e.status_code() {
                    if retry::should_retry(status, attempt, config.max_retries) {
                        let backoff = config.backoff_for(attempt);
                        warn!(
                            operation,
                            attempt,
                            status = %status,
                            backoff_ms = backoff.as_millis(),
                            "retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }
                }
                return Err(e);
            }
        }
    }
}

/// Compute aggregate statistics from a list of image results.
fn compute_stats(images: &[ImageResult]) -> SyncStats {
    let mut stats = SyncStats::default();
    for image in images {
        match &image.status {
            ImageStatus::Synced => {
                stats.images_synced += 1;
                stats.bytes_transferred += image.bytes_transferred;
            }
            ImageStatus::Skipped { .. } => {
                stats.images_skipped += 1;
            }
            ImageStatus::Failed { .. } => {
                stats.images_failed += 1;
            }
        }
        stats.blobs_transferred += image.blob_stats.transferred;
        stats.blobs_skipped += image.blob_stats.skipped;
        stats.blobs_mounted += image.blob_stats.mounted;
    }
    stats
}

#[cfg(test)]
mod tests {
    use ocync_distribution::spec::{Descriptor, MediaType};

    use super::*;

    /// Build a valid sha256 digest from a short suffix, zero-padded to 64 hex chars.
    fn test_digest(suffix: &str) -> Digest {
        format!("sha256:{suffix:0>64}").parse().unwrap()
    }

    /// Build a minimal descriptor with the given digest.
    fn test_descriptor(digest: Digest, media_type: MediaType) -> Descriptor {
        Descriptor {
            media_type,
            digest,
            size: 0,
            platform: None,
            artifact_type: None,
            annotations: None,
        }
    }

    #[test]
    fn collect_blobs_config_and_layers() {
        let config_digest = test_digest("c0");
        let layer1_digest = test_digest("a1");
        let layer2_digest = test_digest("b2");

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: test_descriptor(config_digest.clone(), MediaType::OciConfig),
            layers: vec![
                test_descriptor(layer1_digest.clone(), MediaType::OciLayerGzip),
                test_descriptor(layer2_digest.clone(), MediaType::OciLayerGzip),
            ],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0], config_digest);
        assert_eq!(blobs[1], layer1_digest);
        assert_eq!(blobs[2], layer2_digest);
    }

    #[test]
    fn collect_blobs_no_layers() {
        let config_digest = test_digest("cc");

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: test_descriptor(config_digest.clone(), MediaType::OciConfig),
            layers: vec![],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0], config_digest);
    }

    fn make_image_result(status: ImageStatus, bytes: u64) -> ImageResult {
        ImageResult {
            image_id: Uuid::now_v7(),
            source: "source/repo:tag".into(),
            target: "target/repo:tag".into(),
            status,
            bytes_transferred: bytes,
            blob_stats: BlobTransferStats::default(),
            duration: std::time::Duration::from_millis(100),
        }
    }

    #[test]
    fn compute_stats_mixed_results() {
        let images = vec![
            make_image_result(ImageStatus::Synced, 1024),
            make_image_result(
                ImageStatus::Skipped {
                    reason: SkipReason::DigestMatch,
                },
                0,
            ),
            make_image_result(
                ImageStatus::Failed {
                    error: "timeout".into(),
                    retries: 3,
                },
                0,
            ),
        ];

        let stats = compute_stats(&images);
        assert_eq!(stats.images_synced, 1);
        assert_eq!(stats.images_skipped, 1);
        assert_eq!(stats.images_failed, 1);
        assert_eq!(stats.bytes_transferred, 1024);
    }

    #[test]
    fn compute_stats_empty() {
        let stats = compute_stats(&[]);
        assert_eq!(stats.images_synced, 0);
        assert_eq!(stats.images_skipped, 0);
        assert_eq!(stats.images_failed, 0);
        assert_eq!(stats.bytes_transferred, 0);
    }
}
