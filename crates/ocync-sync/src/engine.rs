//! Sync engine — two-phase orchestrator for image transfers.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use std::time::Duration;

use ocync_distribution::blob::MountResult;
use ocync_distribution::manifest::ManifestPull;
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
///
/// `name` must be unique across targets within a [`ResolvedMapping`] — it is
/// used as the key in the blob deduplication map to track which blobs have
/// already been transferred to each target.
#[derive(Debug)]
pub struct TargetEntry {
    /// Registry identifier used as the blob dedup key.
    ///
    /// Must be unique per mapping. Typically the config-defined registry name
    /// (e.g. `us-ecr`) or the bare hostname for ad-hoc commands.
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

/// A registry endpoint bundling client, repo, and identity for sync operations.
///
/// Used internally to pass source/target pairs through the engine without
/// threading 4+ loose parameters.
struct Endpoint<'a> {
    client: &'a RegistryClient,
    repo: &'a str,
    /// Identity key — used as the blob dedup map key for targets.
    name: &'a str,
}

/// Collect all blob digests with their declared sizes from an image manifest.
fn collect_image_blobs(manifest: &ImageManifest) -> Vec<(Digest, u64)> {
    let mut blobs = Vec::with_capacity(1 + manifest.layers.len());
    blobs.push((manifest.config.digest.clone(), manifest.config.size));
    for layer in &manifest.layers {
        blobs.push((layer.digest.clone(), layer.size));
    }
    blobs
}

/// Source manifest data pre-pulled once per tag.
///
/// Separating the source pull from the target push ensures manifests
/// are fetched once regardless of target count (1:N fan-out).
struct SourceData {
    /// The top-level manifest (image or index).
    pull: ManifestPull,
    /// For index manifests: pre-pulled child image manifests with their blob lists.
    /// Empty for single-image manifests (blobs are derived from `pull.manifest`).
    children: Vec<ChildData>,
}

/// A pre-pulled child manifest within an index.
struct ChildData {
    pull: ManifestPull,
    blobs: Vec<(Digest, u64)>,
}

/// Per-target result of a fan-out blob transfer phase.
#[derive(Default)]
struct BlobTransferResult {
    bytes_transferred: u64,
    stats: BlobTransferStats,
    /// `Some` if a target-specific push failed. Other targets are unaffected.
    error: Option<crate::Error>,
}

/// Sync engine — orchestrates image transfers across registries.
///
/// For each tag, the engine pulls from the source once and fans out to all
/// targets. Blob deduplication is tracked globally so that layers shared
/// across images or repositories are transferred at most once per target
/// registry.
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
    /// For each mapping and tag: pulls from source once, HEAD-checks all
    /// targets, fans out blob transfers (each blob pulled once from source,
    /// pushed to all targets that need it), then pushes manifests.
    pub async fn run(
        &mut self,
        mappings: Vec<ResolvedMapping>,
        progress: &dyn crate::progress::ProgressReporter,
    ) -> SyncReport {
        let run_start = Instant::now();
        let run_id = Uuid::now_v7();
        let mut images = Vec::new();

        for mapping in &mappings {
            let source = Endpoint {
                client: &mapping.source_client,
                repo: &mapping.source_repo,
                name: &mapping.source_repo,
            };

            for tag_pair in &mapping.tags {
                self.sync_tag(&source, mapping, tag_pair, progress, &mut images)
                    .await;
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

    /// Sync one tag across all targets in a mapping.
    ///
    /// Three phases:
    /// 1. Pull source manifest + children once
    /// 2. HEAD-check each target, fan-out blob transfer (pull each blob once)
    /// 3. Push manifests to each target that succeeded
    async fn sync_tag(
        &mut self,
        source: &Endpoint<'_>,
        mapping: &ResolvedMapping,
        tag_pair: &TagPair,
        progress: &dyn crate::progress::ProgressReporter,
        images: &mut Vec<ImageResult>,
    ) {
        let source_tag = &tag_pair.source;
        let target_tag = &tag_pair.target;

        // Phase 0: Pull source manifest once.
        let source_data = match self.pull_source(source, source_tag).await {
            Ok(data) => data,
            Err(err) => {
                let error_str = err.to_string();
                warn!(
                    source_repo = source.repo,
                    tag = %source_tag,
                    error = %error_str,
                    "source pull failed, skipping all targets"
                );
                for te in &mapping.targets {
                    let result = ImageResult {
                        image_id: Uuid::now_v7(),
                        source: format!("{}:{source_tag}", source.repo),
                        target: format!("{}:{target_tag}", mapping.target_repo),
                        status: ImageStatus::Failed {
                            error: error_str.clone(),
                            retries: self.retry.max_retries,
                        },
                        bytes_transferred: 0,
                        blob_stats: BlobTransferStats::default(),
                        duration: Duration::ZERO,
                    };
                    progress.image_completed(&result);
                    images.push(result);
                    let _ = te; // all targets get the same failure
                }
                return;
            }
        };

        // Build target endpoints.
        let targets: Vec<Endpoint<'_>> = mapping
            .targets
            .iter()
            .map(|te| Endpoint {
                client: &te.client,
                repo: &mapping.target_repo,
                name: &te.name,
            })
            .collect();

        // Phase 1: HEAD-check each target — partition into active/skipped.
        let mut active: Vec<(usize, Instant)> = Vec::new(); // (index, start_time)

        for (i, target) in targets.iter().enumerate() {
            let source_display = format!("{}:{source_tag} -> {}", source.repo, target.name);
            let target_display = format!("{}:{target_tag}", target.repo);
            progress.image_started(&source_display, &target_display);

            match target.client.manifest_head(target.repo, target_tag).await {
                Ok(Some(head)) if head.digest == source_data.pull.digest => {
                    info!(
                        source_repo = source.repo,
                        target_repo = target.repo,
                        tag = source_tag,
                        digest = %source_data.pull.digest,
                        "skipping — digest matches at target"
                    );
                    let result = ImageResult {
                        image_id: Uuid::now_v7(),
                        source: format!("{}:{source_tag}", source.repo),
                        target: target_display,
                        status: ImageStatus::Skipped {
                            reason: SkipReason::DigestMatch,
                        },
                        bytes_transferred: 0,
                        blob_stats: BlobTransferStats::default(),
                        duration: Duration::ZERO,
                    };
                    progress.image_completed(&result);
                    images.push(result);
                }
                Ok(_) => active.push((i, Instant::now())),
                Err(e) => {
                    warn!(
                        target_repo = target.repo,
                        target_tag,
                        error = %e,
                        "target manifest HEAD failed, proceeding with sync"
                    );
                    active.push((i, Instant::now()));
                }
            }
        }

        if active.is_empty() {
            return;
        }

        // Collect all blob digests (with sizes) from the source data.
        let owned_blobs;
        let all_blobs: Vec<(&Digest, u64)> = match &source_data.pull.manifest {
            ManifestKind::Image(m) => {
                owned_blobs = collect_image_blobs(m);
                owned_blobs.iter().map(|(d, s)| (d, *s)).collect()
            }
            ManifestKind::Index(_) => source_data
                .children
                .iter()
                .flat_map(|c| c.blobs.iter().map(|(d, s)| (d, *s)))
                .collect(),
        };

        // Phase 2: Fan-out blob transfer — pull each blob once, push to all active targets.
        let active_targets: Vec<&Endpoint<'_>> = active.iter().map(|&(i, _)| &targets[i]).collect();

        let blob_results = match self
            .transfer_blobs_fanout(source, &active_targets, &all_blobs)
            .await
        {
            Ok(results) => results,
            Err(err) => {
                // Source blob pull failed — all active targets fail.
                let error_str = err.to_string();
                for &(i, start) in &active {
                    let target = &targets[i];
                    let result = ImageResult {
                        image_id: Uuid::now_v7(),
                        source: format!("{}:{source_tag}", source.repo),
                        target: format!("{}:{target_tag}", target.repo),
                        status: ImageStatus::Failed {
                            error: error_str.clone(),
                            retries: self.retry.max_retries,
                        },
                        bytes_transferred: 0,
                        blob_stats: BlobTransferStats::default(),
                        duration: start.elapsed(),
                    };
                    progress.image_completed(&result);
                    images.push(result);
                }
                return;
            }
        };

        // Phase 3: Push manifests to targets whose blobs all succeeded.
        for (br, &(i, start)) in blob_results.into_iter().zip(active.iter()) {
            let target = &targets[i];
            let source_str = format!("{}:{source_tag}", source.repo);
            let target_str = format!("{}:{target_tag}", target.repo);

            if let Some(err) = br.error {
                warn!(target_name = target.name, error = %err, "blob transfer failed");
                let result = ImageResult {
                    image_id: Uuid::now_v7(),
                    source: source_str,
                    target: target_str,
                    status: ImageStatus::Failed {
                        error: err.to_string(),
                        retries: self.retry.max_retries,
                    },
                    bytes_transferred: br.bytes_transferred,
                    blob_stats: br.stats,
                    duration: start.elapsed(),
                };
                progress.image_completed(&result);
                images.push(result);
                continue;
            }

            // Push manifests.
            match self.push_manifests(target, target_tag, &source_data).await {
                Ok(()) => {
                    info!(
                        source_repo = source.repo,
                        target_repo = target.repo,
                        source_tag,
                        target_tag,
                        bytes = br.bytes_transferred,
                        "image synced"
                    );
                    let result = ImageResult {
                        image_id: Uuid::now_v7(),
                        source: source_str,
                        target: target_str,
                        status: ImageStatus::Synced,
                        bytes_transferred: br.bytes_transferred,
                        blob_stats: br.stats,
                        duration: start.elapsed(),
                    };
                    progress.image_completed(&result);
                    images.push(result);
                }
                Err(err) => {
                    warn!(target_name = target.name, error = %err, "manifest push failed");
                    let result = ImageResult {
                        image_id: Uuid::now_v7(),
                        source: source_str,
                        target: target_str,
                        status: ImageStatus::Failed {
                            error: err.to_string(),
                            retries: self.retry.max_retries,
                        },
                        bytes_transferred: br.bytes_transferred,
                        blob_stats: br.stats,
                        duration: start.elapsed(),
                    };
                    progress.image_completed(&result);
                    images.push(result);
                }
            }
        }
    }

    /// Pull all source manifest data for a single tag.
    ///
    /// For image manifests, returns just the manifest. For index manifests,
    /// also pulls all child manifests and computes their blob lists.
    async fn pull_source(
        &self,
        source: &Endpoint<'_>,
        source_tag: &str,
    ) -> Result<SourceData, crate::Error> {
        let pull = with_retry(&self.retry, "manifest pull", || {
            source.client.manifest_pull(source.repo, source_tag)
        })
        .await
        .map_err(|e| crate::Error::Manifest {
            reference: source_tag.to_owned(),
            source: e,
        })?;

        let children = match &pull.manifest {
            ManifestKind::Image(_) => Vec::new(),
            ManifestKind::Index(index) => {
                let mut children = Vec::with_capacity(index.manifests.len());
                for child_desc in &index.manifests {
                    let child_digest_str = child_desc.digest.to_string();
                    let child_pull = with_retry(&self.retry, "manifest pull", || {
                        source.client.manifest_pull(source.repo, &child_digest_str)
                    })
                    .await
                    .map_err(|e| crate::Error::Manifest {
                        reference: child_digest_str.clone(),
                        source: e,
                    })?;

                    match &child_pull.manifest {
                        ManifestKind::Image(child_image) => {
                            let blobs = collect_image_blobs(child_image);
                            children.push(ChildData {
                                pull: child_pull,
                                blobs,
                            });
                        }
                        ManifestKind::Index(_) => {
                            return Err(crate::Error::Manifest {
                                reference: child_digest_str,
                                source: ocync_distribution::Error::Other(
                                    "nested index manifests are not supported".into(),
                                ),
                            });
                        }
                    }
                }
                children
            }
        };

        Ok(SourceData { pull, children })
    }

    /// Push all manifests (children for indexes, then top-level) to one target.
    async fn push_manifests(
        &self,
        target: &Endpoint<'_>,
        target_tag: &str,
        source_data: &SourceData,
    ) -> Result<(), crate::Error> {
        // For index manifests, push each child by digest first.
        for child in &source_data.children {
            let child_digest_str = child.pull.digest.to_string();
            with_retry(&self.retry, "manifest push", || {
                target.client.manifest_push(
                    target.repo,
                    &child_digest_str,
                    &child.pull.media_type,
                    &child.pull.raw_bytes,
                )
            })
            .await
            .map_err(|e| crate::Error::Manifest {
                reference: child_digest_str.clone(),
                source: e,
            })?;
        }

        // Push top-level manifest by tag.
        with_retry(&self.retry, "manifest push", || {
            target.client.manifest_push(
                target.repo,
                target_tag,
                &source_data.pull.media_type,
                &source_data.pull.raw_bytes,
            )
        })
        .await
        .map_err(|e| crate::Error::Manifest {
            reference: target_tag.to_owned(),
            source: e,
        })?;

        Ok(())
    }

    /// Fan-out blob transfer: stream each blob from source to each target.
    ///
    /// For each blob, checks dedup/HEAD/mount per target. If any target
    /// needs a pull, streams from source directly to that target (one stream
    /// per target since streams are consumed). Transfer failures are per-target
    /// (recorded in the returned results, other targets continue).
    async fn transfer_blobs_fanout(
        &mut self,
        source: &Endpoint<'_>,
        targets: &[&Endpoint<'_>],
        blobs: &[(&Digest, u64)],
    ) -> Result<Vec<BlobTransferResult>, crate::Error> {
        let mut results: Vec<BlobTransferResult> = targets
            .iter()
            .map(|_| BlobTransferResult::default())
            .collect();

        for &(digest, size) in blobs {
            // Determine which targets need this blob pulled+pushed.
            let mut needs_pull: Vec<usize> = Vec::new();

            for (i, target) in targets.iter().enumerate() {
                if results[i].error.is_some() {
                    continue; // already failed, skip
                }

                // Dedup: skip if this repo already has it.
                let has_blob = self
                    .dedup
                    .known_repos(target.name, digest)
                    .is_some_and(|repos| repos.contains(target.repo));
                if has_blob {
                    results[i].stats.skipped += 1;
                    continue;
                }

                // HEAD check at target.
                match target.client.blob_exists(target.repo, digest).await {
                    Ok(Some(_)) => {
                        self.dedup.set_exists(target.name, digest, target.repo);
                        results[i].stats.skipped += 1;
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(%digest, target = target.name, error = %e, "blob HEAD failed");
                    }
                }

                // Try cross-repo mount.
                if let Some(from_repo) = self
                    .dedup
                    .mount_source(target.name, digest, target.repo)
                    .map(|s| s.to_owned())
                {
                    debug!(%digest, %from_repo, target = target.name, "attempting mount");
                    match target
                        .client
                        .blob_mount(target.repo, digest, &from_repo)
                        .await
                    {
                        Ok(MountResult::Mounted) => {
                            self.dedup.set_completed(target.name, digest, target.repo);
                            results[i].stats.mounted += 1;
                            continue;
                        }
                        Ok(MountResult::FallbackUpload { .. }) | Err(_) => {
                            debug!(%digest, target = target.name, "mount failed, needs pull");
                        }
                    }
                }

                needs_pull.push(i);
            }

            if needs_pull.is_empty() {
                continue;
            }

            // Stream from source to each target independently.
            // Each target gets its own pull stream since streams are consumed.
            // On retry, both pull and push are re-initiated (stream is not resumable).
            for &i in &needs_pull {
                let target = targets[i];
                self.dedup.set_in_progress(target.name, digest);

                let transfer_result = with_retry(&self.retry, "blob transfer", || async {
                    let stream = source.client.blob_pull(source.repo, digest).await?;
                    target
                        .client
                        .blob_push_stream(target.repo, digest, size, stream)
                        .await
                })
                .await;

                match transfer_result {
                    Ok(_digest) => {
                        results[i].bytes_transferred += size;
                        results[i].stats.transferred += 1;
                        self.dedup.set_completed(target.name, digest, target.repo);
                    }
                    Err(e) => {
                        let err = crate::Error::BlobPush {
                            digest: digest.to_string(),
                            source: e,
                        };
                        self.dedup.set_failed(target.name, digest, err.to_string());
                        results[i].error = Some(err);
                        // other targets may still succeed
                    }
                }
            }
        }

        Ok(results)
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

        let config_desc = Descriptor {
            size: 50,
            ..test_descriptor(config_digest.clone(), MediaType::OciConfig)
        };
        let layer1_desc = Descriptor {
            size: 200,
            ..test_descriptor(layer1_digest.clone(), MediaType::OciLayerGzip)
        };
        let layer2_desc = Descriptor {
            size: 300,
            ..test_descriptor(layer2_digest.clone(), MediaType::OciLayerGzip)
        };

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: config_desc,
            layers: vec![layer1_desc, layer2_desc],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0], (config_digest, 50));
        assert_eq!(blobs[1], (layer1_digest, 200));
        assert_eq!(blobs[2], (layer2_digest, 300));
    }

    #[test]
    fn collect_blobs_no_layers() {
        let config_digest = test_digest("cc");

        let config_desc = Descriptor {
            size: 42,
            ..test_descriptor(config_digest.clone(), MediaType::OciConfig)
        };

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: config_desc,
            layers: vec![],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0], (config_digest, 42));
    }

    fn make_image_result(status: ImageStatus, bytes: u64) -> ImageResult {
        ImageResult {
            image_id: Uuid::now_v7(),
            source: "source/repo:tag".into(),
            target: "target/repo:tag".into(),
            status,
            bytes_transferred: bytes,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::from_millis(100),
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
