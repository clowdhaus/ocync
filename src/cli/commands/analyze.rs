//! The `analyze` subcommand - reports blob sharing and cross-repo mount potential
//! without performing any sync.
//!
//! Pulls source manifests only (never blobs), walks index manifests to collect
//! all platform-specific blob descriptors, and aggregates by digest to show:
//! - Total unique blobs and total bytes
//! - Shared blobs (same digest across 2+ images) and deduplicated bytes saved
//! - Per-target-registry mount opportunities (how many pushes cross-repo mount
//!   would replace)

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use ocync_distribution::ecr::BatchBlobChecker;
use ocync_distribution::spec::ManifestKind;
use ocync_distribution::{Digest, RepositoryName};

use ocync_sync::ShutdownSignal;

use crate::cli::commands::synchronize::{
    ClientMap, DroppedTarget, MappingResolution, PreparePhase, PrepareTracker, UnresolvedMapping,
    build_clients, log_unresolved_mapping, resolve_mapping, shared_failure_code,
    with_prepare_progress,
};
use crate::cli::config::{Config, load_config};
use crate::cli::output::format_bytes;
use crate::cli::{CliError, ExitCode};

/// Arguments for the `analyze` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct AnalyzeArgs {
    /// Path to the sync config file.
    #[arg(short, long)]
    pub(crate) config: PathBuf,
    /// Emit a JSON report instead of the text summary.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Per-blob aggregate across all mappings.
#[derive(Debug)]
struct BlobAggregate {
    size: u64,
    /// Image references (`source/repo:tag`) that include this blob.
    images: BTreeSet<String>,
    /// Target registry aliases this blob would be pushed to.
    targets: BTreeSet<String>,
    /// Target repositories this blob would be pushed to, per target registry.
    /// `target_alias → {target_repo}`.
    target_repos: BTreeMap<String, BTreeSet<RepositoryName>>,
}

/// What an analysis produced, including what it could not read.
///
/// The three image counts are disjoint views of the same attempts: `analyzed`
/// is every image that contributed blobs, `partial` is the subset of those
/// missing at least one platform, and `failed` contributed nothing at all.
#[derive(Debug, Default)]
struct Analysis {
    blobs: HashMap<Digest, BlobAggregate>,
    /// Images that contributed blobs, complete or not.
    analyzed: usize,
    /// Images recorded with at least one platform missing.
    partial: usize,
    /// Images that could not be read at all.
    failed: usize,
    /// Mappings that produced no images to read.
    ///
    /// The same shape `sync` reports, so one consumer can read both. Each
    /// carries its classification, so a wholly denied analysis exits 4 the way
    /// `sync` does rather than collapsing to the generic failure.
    unresolved: Vec<UnresolvedMapping>,
    /// Targets excluded from the mount-savings estimate.
    dropped: Vec<DroppedTarget>,
    /// Whether shutdown cut the walk short.
    interrupted: bool,
}

/// Run the analyze command.
pub(crate) async fn run(
    args: &AnalyzeArgs,
    shutdown: &ShutdownSignal,
) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;
    // Analyze doesn't push anything, so no batch checkers needed.
    let no_checkers: HashMap<String, Rc<dyn BatchBlobChecker>> = HashMap::new();

    // Client construction mints a token per registry and every mapping lists a
    // repository's tags, all before the first `analyzing` line. The ticker has
    // to span the whole loop, not just the client build, or the tag-listing
    // stretch it exists for stays silent.
    let tracker = PrepareTracker::default();
    let analysis = with_prepare_progress(&tracker, async {
        let clients = build_clients(&config, &tracker).await;
        tracker.begin(PreparePhase::Mappings, config.mappings.len());
        collect_analysis(&config, &clients, &no_checkers, shutdown, &tracker).await
    })
    .await;

    if analysis.failed > 0 || analysis.partial > 0 || !analysis.unresolved.is_empty() {
        tracing::warn!(
            failed_images = analysis.failed,
            partial_images = analysis.partial,
            unresolved_mappings = analysis.unresolved.len(),
            "the analysis is incomplete; the reported totals are short by whatever it could not read"
        );
    }

    if args.json {
        print_json(&analysis)?;
    } else {
        print_text(&analysis);
    }

    Ok(analyze_exit_code(&analysis))
}

/// Walk every mapping and tag, recording blobs and what could not be read.
async fn collect_analysis(
    config: &Config,
    clients: &ClientMap,
    no_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    shutdown: &ShutdownSignal,
    tracker: &PrepareTracker,
) -> Analysis {
    let mut analysis = Analysis::default();

    for mapping in &config.mappings {
        if shutdown.is_triggered() {
            tracing::info!("shutdown signal received, stopping analysis early");
            analysis.interrupted = true;
            break;
        }

        // Advanced up front so every exit from the match below still counts
        // the mapping: a `continue` that skips it strands the progress line
        // short of its total.
        tracker.advance();

        // One unresolvable mapping must not cost the analysis every mapping
        // behind it, same as `sync`.
        // Dropped targets arrive whatever the outcome: analyze reports what a
        // sync would move, so a target it cannot reach changes the answer even
        // when the mapping itself then fails.
        let (outcome, dropped) =
            resolve_mapping(mapping, config, clients, no_checkers, false).await;
        for target in &dropped {
            tracing::warn!(
                from = %target.from,
                registry = %target.registry,
                error = %target.error,
                "target registry unavailable; excluded from the mount-savings estimate"
            );
        }
        analysis.dropped.extend(dropped);

        let resolved = match outcome {
            Ok(MappingResolution::Resolved(r)) => r,
            Ok(MappingResolution::NoMatchingTags(_)) => continue,
            Err(err) => {
                log_unresolved_mapping(&mapping.from, &err);
                // A mapping the analysis could not resolve is a hole in the
                // estimate exactly like an image it could not read. Its
                // classification is kept so a wholly denied analysis reports
                // as a denial rather than a generic failure.
                analysis.unresolved.push(UnresolvedMapping {
                    from: mapping.from.clone(),
                    code: err.exit_code(),
                    error: err.to_string(),
                });
                continue;
            }
        };

        for tag_pair in &resolved.tags {
            if shutdown.is_triggered() {
                tracing::info!("shutdown signal received, stopping analysis early");
                analysis.interrupted = true;
                break;
            }

            let image_ref = format!("{}:{}", resolved.source_repo, tag_pair.source);
            tracing::info!(image = %image_ref, "analyzing");
            // One unreadable image must not end the analysis: the remaining
            // tags and mappings are independent of it.
            match collect_blobs(
                &resolved.source_client,
                &resolved.source_repo,
                &tag_pair.source,
                &image_ref,
                &resolved.targets,
                &resolved.target_repo,
                &mut analysis.blobs,
            )
            .await
            {
                Ok(Completeness::Full) => analysis.analyzed += 1,
                // A skipped index child still leaves this image's other
                // platforms recorded, so it counts as analyzed. Conflating it
                // with a total failure reported a mostly-complete run as zero
                // images and exited 2.
                Ok(Completeness::Partial) => {
                    analysis.analyzed += 1;
                    analysis.partial += 1;
                }
                Err(err) => {
                    tracing::error!(
                        image = %image_ref,
                        error = %err,
                        "image could not be analyzed; skipping"
                    );
                    analysis.failed += 1;
                }
            }
        }
    }

    analysis
}

/// Exit code for an analysis that could not read everything it was asked to.
///
/// An incomplete estimate must not look like a clean one: the totals are short
/// by whatever the unread images and unreachable targets hold.
fn analyze_exit_code(analysis: &Analysis) -> ExitCode {
    // An interrupted walk is a truncated report, which is the same kind of
    // incompleteness as an unreadable image and must not read as clean.
    if analysis.failed == 0
        && analysis.partial == 0
        && analysis.unresolved.is_empty()
        && analysis.dropped.is_empty()
        && !analysis.interrupted
    {
        return ExitCode::Success;
    }
    if analysis.interrupted && analysis.analyzed > 0 {
        return ExitCode::PartialFailure;
    }
    // Something was read, or the only defect was an unreachable target: the
    // report exists, it is just short. `sync` reports the same case as partial.
    if analysis.analyzed > 0 || (analysis.failed == 0 && analysis.unresolved.is_empty()) {
        return ExitCode::PartialFailure;
    }
    // Nothing readable at all. Keep the specific cause when every failure
    // agrees on one, so a wholly denied analysis exits 4 like `sync`.
    let codes = analysis
        .unresolved
        .iter()
        .map(|u| u.code)
        .chain((analysis.failed > 0).then_some(ExitCode::Failure));
    shared_failure_code(codes)
}

/// Whether every blob referenced by an image was recorded.
#[derive(Debug, Clone, Copy)]
enum Completeness {
    /// Every referenced manifest was pulled.
    Full,
    /// At least one index child could not be pulled, so blobs are missing.
    Partial,
}

/// Pull a manifest (recursively for indexes) and record every blob's
/// descriptor against the source image reference and target set.
async fn collect_blobs(
    source_client: &ocync_distribution::RegistryClient,
    source_repo: &RepositoryName,
    tag: &str,
    image_ref: &str,
    targets: &[ocync_sync::engine::TargetEntry],
    target_repo: &RepositoryName,
    blobs: &mut HashMap<Digest, BlobAggregate>,
) -> Result<Completeness, CliError> {
    let pulled = source_client
        .manifest_pull(source_repo, tag)
        .await
        .map_err(|e| CliError::Input(format!("manifest_pull {image_ref}: {e}")))?;

    let descriptors = descriptors_of(&pulled.manifest);
    for descriptor in descriptors {
        record_blob(
            descriptor.digest,
            descriptor.size,
            image_ref,
            targets,
            target_repo,
            blobs,
        );
    }

    let mut completeness = Completeness::Full;

    // Recurse into index children to collect per-platform manifest blobs.
    if let ManifestKind::Index(index) = &pulled.manifest {
        for child in &index.manifests {
            // Platforms are independent: a missing arm64 manifest should not
            // discard the amd64 blobs already recorded for this image.
            let child_pulled = match source_client
                .manifest_pull(source_repo, &child.digest.to_string())
                .await
            {
                Ok(pulled) => pulled,
                Err(e) => {
                    tracing::warn!(
                        image = %image_ref,
                        child = %child.digest,
                        error = %e,
                        "index child could not be pulled; skipping this platform"
                    );
                    completeness = Completeness::Partial;
                    continue;
                }
            };
            for descriptor in descriptors_of(&child_pulled.manifest) {
                record_blob(
                    descriptor.digest,
                    descriptor.size,
                    image_ref,
                    targets,
                    target_repo,
                    blobs,
                );
            }
        }
    }

    Ok(completeness)
}

/// Descriptor data extracted from a manifest.
struct BlobDescriptor {
    digest: Digest,
    size: u64,
}

/// Return the (digest, size) of every blob referenced by a manifest.
fn descriptors_of(manifest: &ManifestKind) -> Vec<BlobDescriptor> {
    match manifest {
        ManifestKind::Image(image) => {
            let mut out = Vec::with_capacity(1 + image.layers.len());
            out.push(BlobDescriptor {
                digest: image.config.digest.clone(),
                size: image.config.size,
            });
            for layer in &image.layers {
                out.push(BlobDescriptor {
                    digest: layer.digest.clone(),
                    size: layer.size,
                });
            }
            out
        }
        // Index descriptors themselves aren't blobs we push; children handle that.
        ManifestKind::Index(_) => Vec::new(),
    }
}

fn record_blob(
    digest: Digest,
    size: u64,
    image_ref: &str,
    targets: &[ocync_sync::engine::TargetEntry],
    target_repo: &RepositoryName,
    blobs: &mut HashMap<Digest, BlobAggregate>,
) {
    let entry = blobs.entry(digest).or_insert_with(|| BlobAggregate {
        size,
        images: BTreeSet::new(),
        targets: BTreeSet::new(),
        target_repos: BTreeMap::new(),
    });
    entry.images.insert(image_ref.to_owned());
    for target in targets {
        let alias = target.name.to_string();
        entry.targets.insert(alias.clone());
        entry
            .target_repos
            .entry(alias)
            .or_default()
            .insert(target_repo.clone());
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Compute per-target-registry mount savings: count of redundant pushes and
/// total bytes that cross-repo mount would avoid.
fn compute_mount_savings(blobs: &HashMap<Digest, BlobAggregate>) -> BTreeMap<String, (usize, u64)> {
    let mut savings: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for blob in blobs.values() {
        for (target, repos) in &blob.target_repos {
            if repos.len() > 1 {
                let count = repos.len() - 1;
                let bytes = blob.size * count as u64;
                let entry = savings.entry(target.clone()).or_default();
                entry.0 += count;
                entry.1 += bytes;
            }
        }
    }
    savings
}

fn print_text(analysis: &Analysis) {
    let blobs = &analysis.blobs;
    let total_blobs = blobs.len();
    let total_bytes: u64 = blobs.values().map(|b| b.size).sum();

    let shared: Vec<&BlobAggregate> = blobs.values().filter(|b| b.images.len() > 1).collect();
    let shared_bytes: u64 = shared.iter().map(|b| b.size).sum();

    let mount_savings_by_target = compute_mount_savings(blobs);

    let attempted = analysis.analyzed + analysis.failed;
    if !analysis.unresolved.is_empty() {
        println!(
            "  WARNING: {} mapping(s) could not be resolved; excluded entirely",
            analysis.unresolved.len()
        );
    }
    if analysis.failed > 0 || analysis.partial > 0 {
        println!(
            "Analyzed {} of {attempted} image mappings ({} incomplete, {} failed)",
            analysis.analyzed, analysis.partial, analysis.failed,
        );
    } else {
        println!("Analyzed {} image mappings", analysis.analyzed);
    }
    for target in &analysis.dropped {
        println!(
            "  WARNING: target {} unreachable; excluded from the estimate",
            target.registry
        );
    }
    println!();
    println!(
        "Unique blobs: {total_blobs} ({})",
        format_bytes(total_bytes)
    );
    println!(
        "Shared blobs: {} ({}) across 2+ images",
        shared.len(),
        format_bytes(shared_bytes)
    );
    if !mount_savings_by_target.is_empty() {
        println!();
        println!("Cross-repo mount opportunities (per target registry):");
        for (target, (count, bytes)) in &mount_savings_by_target {
            println!(
                "  {target}: {count} redundant pushes avoidable, {} savings",
                format_bytes(*bytes)
            );
        }
    }
}

fn print_json(analysis: &Analysis) -> Result<(), CliError> {
    let blobs = &analysis.blobs;
    let mount_savings_by_target = compute_mount_savings(blobs);

    let report = serde_json::json!({
        "images_analyzed": analysis.analyzed,
        "images_partial": analysis.partial,
        "images_failed": analysis.failed,
        "unresolved_mappings": analysis.unresolved,
        // A target the estimate could not reach changes the answer, so it is
        // part of the document rather than a log line only.
        "dropped_targets": analysis.dropped,
        "total_blobs": blobs.len(),
        "total_bytes": blobs.values().map(|b| b.size).sum::<u64>(),
        "shared_blobs": blobs.values().filter(|b| b.images.len() > 1).count(),
        "shared_bytes": blobs.values().filter(|b| b.images.len() > 1).map(|b| b.size).sum::<u64>(),
        "mount_savings_by_target": mount_savings_by_target
            .iter()
            .map(|(k, (c, b))| (k.clone(), serde_json::json!({"redundant_pushes": c, "bytes": b})))
            .collect::<BTreeMap<_, _>>(),
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Input(format!("serialize report: {e}")))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(analyzed: usize, partial: usize, failed: usize, dropped: usize) -> Analysis {
        Analysis {
            blobs: HashMap::new(),
            analyzed,
            partial,
            failed,
            unresolved: Vec::new(),
            interrupted: false,
            dropped: (0..dropped)
                .map(|i| DroppedTarget {
                    from: "repo/one".into(),
                    registry: format!("mirror-{i}"),
                    error: "403 Forbidden".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_complete_analysis_is_a_clean_exit() {
        assert_eq!(analyze_exit_code(&analysis(3, 0, 0, 0)), ExitCode::Success);
        assert_eq!(analyze_exit_code(&analysis(0, 0, 0, 0)), ExitCode::Success);
    }

    /// An image missing one platform still contributed the others, so the
    /// report is partial, not empty. Counting it as a total failure reported a
    /// mostly-complete run as zero images analyzed and exited 2.
    #[test]
    fn a_partial_image_is_partial_not_failed() {
        assert_eq!(
            analyze_exit_code(&analysis(1, 1, 0, 0)),
            ExitCode::PartialFailure
        );
    }

    #[test]
    fn nothing_readable_is_a_failure() {
        assert_eq!(analyze_exit_code(&analysis(0, 0, 3, 0)), ExitCode::Failure);
    }

    #[test]
    fn some_readable_is_partial() {
        assert_eq!(
            analyze_exit_code(&analysis(2, 0, 1, 0)),
            ExitCode::PartialFailure
        );
    }

    /// A run cut short by SIGINT is a truncated report, not a clean one.
    #[test]
    fn an_interrupted_analysis_is_partial() {
        let mut a = analysis(3, 0, 0, 0);
        a.interrupted = true;
        assert_eq!(analyze_exit_code(&a), ExitCode::PartialFailure);
    }

    /// A mapping the analysis could not resolve is as much a hole in the
    /// estimate as an image it could not read.
    #[test]
    fn an_unresolved_mapping_alone_is_partial() {
        let mut a = analysis(3, 0, 0, 0);
        a.unresolved = vec![UnresolvedMapping {
            from: "repo/one".into(),
            error: "403 Forbidden".into(),
            code: ExitCode::AuthError,
        }];
        assert_eq!(analyze_exit_code(&a), ExitCode::PartialFailure);
    }

    /// An unreachable target changes the mount-savings answer even when every
    /// image read cleanly, so it cannot report as a clean run.
    #[test]
    fn an_unreachable_target_alone_is_partial() {
        assert_eq!(
            analyze_exit_code(&analysis(3, 0, 0, 1)),
            ExitCode::PartialFailure
        );
    }
}
