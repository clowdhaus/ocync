//! Sync engine — two-phase orchestrator for image transfers.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use ocync_distribution::blob::MountResult;
use ocync_distribution::spec::{ImageManifest, ManifestKind};
use ocync_distribution::{Digest, RegistryClient};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::plan::BlobDedupMap;
use crate::retry::{self, RetryConfig};
use crate::{ImageResult, ImageStatus, SkipReason};

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
    /// Tags to sync (already filtered).
    pub tags: Vec<String>,
}

/// A single target registry entry.
#[derive(Debug)]
pub struct TargetEntry {
    /// Human-readable name from config (e.g. `us-ecr`).
    pub name: String,
    /// Client for this target registry.
    pub client: Arc<RegistryClient>,
}

/// Collect all blob digests from an image manifest (config + layers).
#[allow(dead_code)] // Called by sync_image_inner; public entry point `run()` added next.
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
pub struct SyncEngine {
    retry: RetryConfig,
    #[allow(dead_code)] // Used by transfer_blobs; public entry point `run()` added next.
    dedup: std::sync::Mutex<BlobDedupMap>,
}

impl fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncEngine")
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl SyncEngine {
    /// Create a new sync engine with the given retry configuration.
    pub fn new(retry: RetryConfig) -> Self {
        Self {
            retry,
            dedup: std::sync::Mutex::new(BlobDedupMap::new()),
        }
    }

    /// Sync a single image (one tag) from source to one target.
    ///
    /// Returns an [`ImageResult`] — never panics. All errors are captured
    /// as [`ImageStatus::Failed`].
    #[allow(dead_code)] // Public entry point `run()` added next.
    async fn sync_image(
        &self,
        source: &RegistryClient,
        source_repo: &str,
        target: &RegistryClient,
        target_name: &str,
        target_repo: &str,
        tag: &str,
    ) -> ImageResult {
        let image_start = Instant::now();
        let image_id = Uuid::now_v7();
        let source_ref = format!("{source_repo}:{tag}");
        let target_ref = format!("{target_repo}:{tag}");

        let result = self
            .sync_image_inner(source, source_repo, target, target_name, target_repo, tag)
            .await;

        let duration = image_start.elapsed();

        match result {
            Ok((status, bytes_transferred)) => ImageResult {
                image_id,
                source: source_ref,
                target: target_ref,
                status,
                bytes_transferred,
                duration,
            },
            Err(err) => {
                warn!(source = %source_ref, target = %target_ref, error = %err, "image sync failed");
                ImageResult {
                    image_id,
                    source: source_ref,
                    target: target_ref,
                    status: ImageStatus::Failed {
                        error: err,
                        retries: self.retry.max_retries,
                    },
                    bytes_transferred: 0,
                    duration,
                }
            }
        }
    }

    /// Inner implementation of image sync that returns Result for ergonomic error handling.
    async fn sync_image_inner(
        &self,
        source: &RegistryClient,
        source_repo: &str,
        target: &RegistryClient,
        target_name: &str,
        target_repo: &str,
        tag: &str,
    ) -> Result<(ImageStatus, u64), String> {
        // Step 1: Pull source manifest.
        let pull = source
            .manifest_pull(source_repo, tag)
            .await
            .map_err(|e| format!("failed to pull source manifest: {e}"))?;

        // Step 2: HEAD target manifest — skip if digest matches.
        match target.manifest_head(target_repo, tag).await {
            Ok(Some(head)) if head.digest == pull.digest => {
                info!(
                    source_repo,
                    target_repo,
                    tag,
                    digest = %pull.digest,
                    "skipping — digest matches at target"
                );
                return Ok((
                    ImageStatus::Skipped {
                        reason: SkipReason::DigestMatch,
                    },
                    0,
                ));
            }
            Ok(_) => {} // manifest missing or digest differs — proceed
            Err(e) => {
                debug!(
                    target_repo,
                    tag,
                    error = %e,
                    "target manifest HEAD failed, proceeding with sync"
                );
            }
        }

        // Step 3/4: Handle based on manifest kind.
        let bytes_transferred = match &pull.manifest {
            ManifestKind::Image(image_manifest) => {
                let blobs = collect_image_blobs(image_manifest);
                let bytes = self
                    .transfer_blobs(
                        source,
                        source_repo,
                        target,
                        target_name,
                        target_repo,
                        &blobs,
                    )
                    .await?;

                // Push manifest by tag.
                target
                    .manifest_push(target_repo, tag, &pull.media_type, &pull.raw_bytes)
                    .await
                    .map_err(|e| format!("failed to push manifest: {e}"))?;

                bytes
            }
            ManifestKind::Index(index) => {
                let mut total_bytes = 0u64;

                // For each child descriptor, pull child manifest, transfer blobs, push child.
                for child_desc in &index.manifests {
                    let child_digest_str = child_desc.digest.to_string();
                    let child_pull = source
                        .manifest_pull(source_repo, &child_digest_str)
                        .await
                        .map_err(|e| {
                            format!("failed to pull child manifest {}: {e}", child_desc.digest)
                        })?;

                    if let ManifestKind::Image(child_image) = &child_pull.manifest {
                        let blobs = collect_image_blobs(child_image);
                        let bytes = self
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
                    }

                    // Push child manifest to target by digest.
                    target
                        .manifest_push(
                            target_repo,
                            &child_digest_str,
                            &child_pull.media_type,
                            &child_pull.raw_bytes,
                        )
                        .await
                        .map_err(|e| {
                            format!("failed to push child manifest {}: {e}", child_desc.digest)
                        })?;
                }

                // Push the index by tag.
                target
                    .manifest_push(target_repo, tag, &pull.media_type, &pull.raw_bytes)
                    .await
                    .map_err(|e| format!("failed to push index manifest: {e}"))?;

                total_bytes
            }
        };

        info!(
            source_repo,
            target_repo, tag, bytes_transferred, "image synced"
        );

        Ok((ImageStatus::Synced, bytes_transferred))
    }

    /// Transfer blobs from source to target, using dedup and cross-repo mount.
    ///
    /// For each digest: checks the dedup map, does a HEAD check at the target,
    /// attempts a cross-repo mount, and falls back to pull+push. Returns the
    /// total bytes transferred.
    async fn transfer_blobs(
        &self,
        source: &RegistryClient,
        source_repo: &str,
        target: &RegistryClient,
        target_name: &str,
        target_repo: &str,
        digests: &[Digest],
    ) -> Result<u64, String> {
        let mut total_bytes = 0u64;

        for digest in digests {
            // Check dedup map — skip if already handled.
            {
                let dedup = self.dedup.lock().expect("dedup lock poisoned");
                if let Some(status) = dedup.status(target_name, digest) {
                    use crate::plan::BlobStatus;
                    match status {
                        BlobStatus::ExistsAtTarget
                        | BlobStatus::Completed
                        | BlobStatus::InProgress => {
                            debug!(%digest, status = ?status, "blob already handled, skipping");
                            continue;
                        }
                        BlobStatus::Unknown | BlobStatus::Failed(_) => {}
                    }
                }
            }

            // HEAD check at target — if exists, mark in dedup map and skip.
            match target.blob_exists(target_repo, digest).await {
                Ok(Some(_size)) => {
                    debug!(%digest, "blob exists at target, skipping");
                    let mut dedup = self.dedup.lock().expect("dedup lock poisoned");
                    dedup.set_exists(target_name, digest, target_repo);
                    continue;
                }
                Ok(None) => {} // not found, need to transfer
                Err(e) => {
                    debug!(%digest, error = %e, "blob HEAD check failed, will attempt transfer");
                }
            }

            // Mark in-progress.
            {
                let mut dedup = self.dedup.lock().expect("dedup lock poisoned");
                dedup.set_in_progress(target_name, digest);
            }

            // Try cross-repo mount.
            let mount_source = {
                let dedup = self.dedup.lock().expect("dedup lock poisoned");
                dedup
                    .mount_source(target_name, digest, target_repo)
                    .map(|s| s.to_owned())
            };

            if let Some(from_repo) = mount_source {
                debug!(%digest, from_repo, "attempting cross-repo mount");
                match target.blob_mount(target_repo, digest, &from_repo).await {
                    Ok(MountResult::Mounted) => {
                        debug!(%digest, "blob mounted from {from_repo}");
                        let mut dedup = self.dedup.lock().expect("dedup lock poisoned");
                        dedup.set_completed(target_name, digest, target_repo);
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
            let data = self
                .pull_blob_with_retry(source, source_repo, digest)
                .await?;
            let blob_size = data.len() as u64;

            // Push to target with retry.
            self.push_blob_with_retry(target, target_repo, &data)
                .await?;

            total_bytes = total_bytes.saturating_add(blob_size);

            // Mark completed.
            let mut dedup = self.dedup.lock().expect("dedup lock poisoned");
            dedup.set_completed(target_name, digest, target_repo);
        }

        Ok(total_bytes)
    }

    /// Pull a blob with retry logic for transient errors.
    async fn pull_blob_with_retry(
        &self,
        source: &RegistryClient,
        source_repo: &str,
        digest: &Digest,
    ) -> Result<Vec<u8>, String> {
        let mut attempt = 0;
        loop {
            match source.blob_pull_all(source_repo, digest).await {
                Ok(data) => return Ok(data),
                Err(e) => {
                    if let Some(status) = e.status_code() {
                        if retry::should_retry(status, attempt, self.retry.max_retries) {
                            let backoff = self.retry.backoff_for(attempt);
                            warn!(
                                %digest,
                                attempt,
                                status = %status,
                                backoff_ms = backoff.as_millis(),
                                "retrying blob pull"
                            );
                            tokio::time::sleep(backoff).await;
                            attempt += 1;
                            continue;
                        }
                    }
                    return Err(format!("failed to pull blob {digest}: {e}"));
                }
            }
        }
    }

    /// Push a blob with retry logic for transient errors.
    async fn push_blob_with_retry(
        &self,
        target: &RegistryClient,
        target_repo: &str,
        data: &[u8],
    ) -> Result<Digest, String> {
        let mut attempt = 0;
        loop {
            match target.blob_push(target_repo, data).await {
                Ok(digest) => return Ok(digest),
                Err(e) => {
                    if let Some(status) = e.status_code() {
                        if retry::should_retry(status, attempt, self.retry.max_retries) {
                            let backoff = self.retry.backoff_for(attempt);
                            warn!(
                                attempt,
                                status = %status,
                                backoff_ms = backoff.as_millis(),
                                "retrying blob push"
                            );
                            tokio::time::sleep(backoff).await;
                            attempt += 1;
                            continue;
                        }
                    }
                    return Err(format!("failed to push blob: {e}"));
                }
            }
        }
    }
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
}
