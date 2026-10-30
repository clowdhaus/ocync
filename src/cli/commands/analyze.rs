//! The `analyze` subcommand: reports blob sharing and cross-repo mount potential
//! without performing any sync.
//!
//! Pulls source manifests only (never blobs), walks index manifests to collect
//! every platform's blob descriptors, and aggregates by digest into unique and
//! shared blob totals plus per-target mount opportunities.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use futures_util::StreamExt;
use ocync_distribution::ecr::BatchBlobChecker;
use ocync_distribution::spec::ManifestKind;
use ocync_distribution::{Digest, RepositoryName};

use ocync_sync::ShutdownSignal;
use ocync_sync::retry::{RetryConfig, with_retry};

use crate::cli::commands::synchronize::{
    ClientMap, DroppedTarget, MappingResolution, PreparePhase, PrepareTracker, Selection,
    UnresolvedMapping, build_clients, log_unresolved_mapping, referenced_registries,
    resolve_mapping, shared_failure_code, with_prepare_progress,
};
use crate::cli::config::{Config, load_config};
use crate::cli::output::format_bytes;
use crate::cli::{CliError, ExitCode, RunCredentials};

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
    /// Target repositories this blob would be pushed to, keyed by target registry alias.
    target_repos: BTreeMap<String, BTreeSet<RepositoryName>>,
}

/// What an analysis produced, including what it could not read.
///
/// `analyzed` is every image that contributed blobs, `partial` the subset of
/// those missing at least one platform, and `failed` contributed nothing.
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
    /// The same shape `sync` reports. Each carries its classification, so a
    /// wholly denied analysis exits 4 like `sync`.
    unresolved: Vec<UnresolvedMapping>,
    /// Targets excluded from the mount-savings estimate.
    dropped: Vec<DroppedTarget>,
    /// Whether shutdown cut the walk short.
    interrupted: bool,
}

/// Mappings resolved, and images pulled, at once.
///
/// Same value as `sync`'s `PREPARE_MAPPING_CONCURRENCY`, kept separate so the
/// two can be tuned apart. A ceiling on open work, not a request rate: each
/// client's AIMD window still governs how hard a registry is hit.
const ANALYZE_CONCURRENCY: usize = 16;

/// Run the analyze command.
pub(crate) async fn run(
    args: &AnalyzeArgs,
    shutdown: &ShutdownSignal,
) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;
    // Nothing is pushed, so no batch checkers.
    let no_checkers: HashMap<String, Rc<dyn BatchBlobChecker>> = HashMap::new();

    // The ticker spans client construction and tag listing as well as the
    // pulls: both run before the first `analyzing` line.
    let tracker = PrepareTracker::default();
    let analysis = with_prepare_progress(&tracker, "analysis", async {
        let credentials = RunCredentials::default();
        let clients = build_clients(
            &config,
            &referenced_registries(&config),
            &tracker,
            &credentials,
        )
        .await;
        collect_analysis(
            &config,
            &clients,
            &no_checkers,
            shutdown,
            &tracker,
            ANALYZE_CONCURRENCY,
        )
        .await
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
///
/// Mappings resolve concurrently, then every image across every mapping is
/// pulled from one flattened list; per-mapping walks are only a request or two
/// long and would otherwise stay serial. Both walks are unordered so a slow
/// mapping (a repository with tens of thousands of tags) does not hold slots
/// that finished work behind it could use.
///
/// Aggregation stays sequential with one owner of `analysis.blobs`. Its order
/// does not matter (every printed figure is a count, a sum or a sorted
/// container); mapping bookkeeping does, so those outcomes are re-sorted into
/// config order.
async fn collect_analysis(
    config: &Config,
    clients: &ClientMap,
    no_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    shutdown: &ShutdownSignal,
    tracker: &PrepareTracker,
    concurrency: usize,
) -> Analysis {
    let mut analysis = Analysis::default();
    tracker.begin(PreparePhase::Mappings, config.mappings.len());

    /// One mapping's resolution, or `None` if shutdown reached it first.
    type MappingOutcome = Option<(Result<MappingResolution, CliError>, Vec<DroppedTarget>)>;

    // `None` marks a mapping shutdown reached before it started. Checked
    // inside the task so a signal part way through still stops work that has
    // not begun.
    let mut indexed: Vec<(usize, MappingOutcome)> =
        futures_util::stream::iter(config.mappings.iter().enumerate().map(|(idx, mapping)| {
            async move {
                if shutdown.is_triggered() {
                    return (idx, None);
                }
                // Held for the whole task so every exit counts the mapping and
                // the progress line reaches its total.
                let _item = tracker.track(&mapping.from);
                (
                    idx,
                    Some(
                        resolve_mapping(mapping, config, clients, no_checkers, Selection::Sync)
                            .await,
                    ),
                )
            }
        }))
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    indexed.sort_by_key(|(idx, _)| *idx);
    let outcomes: Vec<MappingOutcome> = indexed.into_iter().map(|(_, o)| o).collect();

    let mut resolved_mappings = Vec::new();
    for (mapping, outcome) in config.mappings.iter().zip(outcomes) {
        let Some((outcome, dropped)) = outcome else {
            analysis.interrupted = true;
            continue;
        };

        // Dropped targets arrive whatever the outcome: an unreachable target
        // changes the estimate even when the mapping then fails.
        for target in &dropped {
            tracing::warn!(
                from = %target.from,
                registry = %target.registry,
                error = %target.error,
                "target registry unavailable; excluded from the mount-savings estimate"
            );
        }
        analysis.dropped.extend(dropped);

        match outcome {
            Ok(MappingResolution::Resolved(r)) => resolved_mappings.push(r),
            Ok(MappingResolution::NoMatchingTags(_)) => {}
            // One unresolvable mapping must not cost the mappings behind it.
            // Its classification is kept so a wholly denied analysis exits as
            // a denial, not a generic failure.
            Err(err) => {
                log_unresolved_mapping(&mapping.from, &err);
                analysis.unresolved.push(UnresolvedMapping {
                    from: mapping.from.clone(),
                    code: err.exit_code(),
                    error: err.to_string(),
                });
            }
        }
    }

    let images: Vec<(usize, String)> = resolved_mappings
        .iter()
        .enumerate()
        .flat_map(|(idx, m)| m.tags.iter().map(move |t| (idx, t.source.clone())))
        .collect();

    // Its own step: resolution finishes long before the walk, and the ticker
    // would otherwise report a completed mapping count for the slower half.
    tracker.begin(PreparePhase::Images, images.len());

    // This walk runs before the engine exists, so it carries the engine's
    // default retry policy itself.
    let retry = RetryConfig::default();
    let retry = &retry;

    let mut pulls = futures_util::stream::iter(images.iter().map(|(idx, tag)| {
        let resolved = &resolved_mappings[*idx];
        async move {
            let image_ref = format!("{}:{}", resolved.source_repo, tag);
            if shutdown.is_triggered() {
                return (*idx, image_ref, None);
            }
            let _item = tracker.track(&image_ref);
            tracing::info!(image = %image_ref, "analyzing");
            // One unreadable image must not end the analysis.
            let pulled = pull_image_descriptors(
                &resolved.source_client,
                &resolved.source_repo,
                tag,
                &image_ref,
                retry,
            )
            .await;
            (*idx, image_ref, Some(pulled))
        }
    }))
    .buffer_unordered(concurrency.max(1));

    while let Some((idx, image_ref, pulled)) = pulls.next().await {
        let Some(pulled) = pulled else {
            analysis.interrupted = true;
            continue;
        };
        let resolved = &resolved_mappings[idx];
        match pulled {
            Ok((descriptors, completeness)) => {
                for descriptor in descriptors {
                    record_blob(
                        descriptor.digest,
                        descriptor.size,
                        &image_ref,
                        &resolved.targets,
                        &resolved.target_repo,
                        &mut analysis.blobs,
                    );
                }
                analysis.analyzed += 1;
                // A skipped index child still leaves the image's other
                // platforms recorded, so it counts as analyzed, not failed.
                if matches!(completeness, Completeness::Partial) {
                    analysis.partial += 1;
                }
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

    if analysis.interrupted {
        tracing::info!("shutdown signal received, stopping analysis early");
    }

    analysis
}

/// An incomplete estimate must not look clean: the totals are short by
/// whatever unread images, unreachable targets and an interrupted walk hold.
fn analyze_exit_code(analysis: &Analysis) -> ExitCode {
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
    // report exists but is short, which `sync` also reports as partial.
    if analysis.analyzed > 0 || (analysis.failed == 0 && analysis.unresolved.is_empty()) {
        return ExitCode::PartialFailure;
    }
    // Nothing readable: keep the specific cause when every failure agrees.
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

/// Pull a manifest, recursing into index children, and return every blob
/// descriptor it references.
///
/// Recording is the caller's job: this half borrows nothing mutable, so images
/// pull concurrently while the aggregate keeps a single owner.
///
/// Index children stay sequential within an image: images already overlap and
/// the AIMD window saturates well below the number in flight.
///
/// Every pull is retried here, not inside `manifest_pull`: the engine wraps its
/// own `manifest_pull` calls in `with_retry`, so a lower retry would multiply
/// the two. This walk is the highest-volume request path of the prepare phase
/// against one source registry, the shape that provokes a throttle.
async fn pull_image_descriptors(
    source_client: &ocync_distribution::RegistryClient,
    source_repo: &RepositoryName,
    tag: &str,
    image_ref: &str,
    retry: &RetryConfig,
) -> Result<(Vec<BlobDescriptor>, Completeness), CliError> {
    let pulled = with_retry(retry, "analyze manifest pull", || {
        source_client.manifest_pull(source_repo, tag)
    })
    .await
    .map_err(|e| CliError::Input(format!("manifest_pull {image_ref}: {e}")))?;

    let mut descriptors = descriptors_of(&pulled.manifest);
    let mut completeness = Completeness::Full;

    if let ManifestKind::Index(index) = &pulled.manifest {
        for child in &index.manifests {
            // Platforms are independent: a missing arm64 manifest must not
            // discard the amd64 blobs.
            let child_ref = child.digest.to_string();
            match with_retry(retry, "analyze child manifest pull", || {
                source_client.manifest_pull(source_repo, &child_ref)
            })
            .await
            {
                Ok(child_pulled) => descriptors.extend(descriptors_of(&child_pulled.manifest)),
                Err(e) => {
                    tracing::warn!(
                        image = %image_ref,
                        child = %child.digest,
                        error = %e,
                        "index child could not be pulled; skipping this platform"
                    );
                    completeness = Completeness::Partial;
                }
            }
        }
    }

    Ok((descriptors, completeness))
}

/// Digest and size of one blob a manifest references.
struct BlobDescriptor {
    digest: Digest,
    size: u64,
}

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
        // Index children carry the blobs.
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
        target_repos: BTreeMap::new(),
    });
    entry.images.insert(image_ref.to_owned());
    for target in targets {
        entry
            .target_repos
            .entry(target.name.to_string())
            .or_default()
            .insert(target_repo.clone());
    }
}

/// Per-target-registry count of redundant pushes and bytes cross-repo mount
/// would avoid.
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
        // An unreachable target changes the estimate, so it is in the document.
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

    fn unresolved_mapping() -> UnresolvedMapping {
        UnresolvedMapping {
            from: "repo/one".into(),
            error: "403 Forbidden".into(),
            code: ExitCode::AuthError,
        }
    }

    #[test]
    fn exit_code_reflects_how_complete_the_analysis_is() {
        let cases = [
            ("complete", analysis(3, 0, 0, 0), ExitCode::Success),
            ("empty and clean", analysis(0, 0, 0, 0), ExitCode::Success),
            // A missing platform still leaves the others recorded: partial, not failed.
            (
                "partial image",
                analysis(1, 1, 0, 0),
                ExitCode::PartialFailure,
            ),
            ("nothing readable", analysis(0, 0, 3, 0), ExitCode::Failure),
            (
                "some readable",
                analysis(2, 0, 1, 0),
                ExitCode::PartialFailure,
            ),
            // A run cut short by SIGINT is a truncated report, not a clean one.
            (
                "interrupted",
                Analysis {
                    interrupted: true,
                    ..analysis(3, 0, 0, 0)
                },
                ExitCode::PartialFailure,
            ),
            // As much a hole in the estimate as an image it could not read.
            (
                "unresolved mapping alone",
                Analysis {
                    unresolved: vec![unresolved_mapping()],
                    ..analysis(3, 0, 0, 0)
                },
                ExitCode::PartialFailure,
            ),
            // An unreachable target changes the mount-savings answer.
            (
                "unreachable target alone",
                analysis(3, 0, 0, 1),
                ExitCode::PartialFailure,
            ),
        ];
        for (name, analysis, want) in cases {
            assert_eq!(analyze_exit_code(&analysis), want, "{name}");
        }
    }

    use crate::cli::commands::test_support::{max_in_flight, repo_names, slow_registry};

    /// Many mappings with few tags each: a per-mapping walk would stay serial.
    #[tokio::test]
    async fn collect_analysis_overlaps_tag_listings_and_manifest_pulls() {
        let delay = std::time::Duration::from_millis(250);
        let repos = repo_names(8);
        let (_server, config, clients, tag_arrivals, manifest_arrivals) =
            slow_registry(&repos, &["v1"], delay).await;

        let analysis = collect_analysis(
            &config,
            &clients,
            &HashMap::new(),
            &ShutdownSignal::new(),
            &PrepareTracker::default(),
            ANALYZE_CONCURRENCY,
        )
        .await;

        assert_eq!(analysis.analyzed, 8, "every image must be analyzed");
        let observed = max_in_flight(&tag_arrivals, delay);
        assert!(
            observed > 1,
            "tag listings must overlap; saw at most {observed} in flight"
        );
        let observed = max_in_flight(&manifest_arrivals, delay);
        assert!(
            observed > 1,
            "manifest pulls must overlap across mappings; saw at most {observed} in flight"
        );
    }

    /// The registry arrivals are the negative half: a check after the fan-out
    /// would still have listed every mapping's tags.
    #[tokio::test]
    async fn collect_analysis_starts_nothing_once_shutdown_is_triggered() {
        let delay = std::time::Duration::from_millis(1);
        let repos = repo_names(8);
        let (_server, config, clients, tag_arrivals, manifest_arrivals) =
            slow_registry(&repos, &["v1"], delay).await;

        let shutdown = ShutdownSignal::new();
        shutdown.trigger();

        let analysis = collect_analysis(
            &config,
            &clients,
            &HashMap::new(),
            &shutdown,
            &PrepareTracker::default(),
            ANALYZE_CONCURRENCY,
        )
        .await;

        assert!(analysis.interrupted, "the report must say it is truncated");
        assert_eq!(analysis.analyzed, 0);
        assert_eq!(
            tag_arrivals.lock().expect("no panic in a stub").len(),
            0,
            "no mapping should have been resolved"
        );
        assert_eq!(
            manifest_arrivals.lock().expect("no panic in a stub").len(),
            0,
            "and no image should have been pulled"
        );
    }

    /// More images than the concurrency bound, so those past the first batch
    /// have not started when the signal lands and must not start.
    #[tokio::test]
    async fn collect_analysis_stops_pulling_images_after_shutdown() {
        let delay = std::time::Duration::from_millis(1);
        let images = ANALYZE_CONCURRENCY * 3;
        let repos = repo_names(images);
        let (_server, config, clients, _, manifest_arrivals) =
            slow_registry(&repos, &["v1"], delay).await;

        let shutdown = ShutdownSignal::new();
        let no_checkers = HashMap::new();
        let tracker = PrepareTracker::default();
        let analysis = {
            // Triggered once the image walk is under way.
            let walk = collect_analysis(
                &config,
                &clients,
                &no_checkers,
                &shutdown,
                &tracker,
                ANALYZE_CONCURRENCY,
            );
            let trip = async {
                while manifest_arrivals
                    .lock()
                    .expect("no panic in a stub")
                    .is_empty()
                {
                    tokio::task::yield_now().await;
                }
                shutdown.trigger();
            };
            let (analysis, ()) = futures_util::future::join(walk, trip).await;
            analysis
        };

        assert!(analysis.interrupted, "the report must say it is truncated");
        assert!(
            analysis.analyzed < images,
            "the walk must stop short; analyzed {} of {images}",
            analysis.analyzed
        );
    }

    /// Elapsed time as the two walks are widened, over many one-tag mappings
    /// (the shape that exposes the flattened image walk).
    #[tokio::test]
    #[ignore = "wall-clock benchmark against a latency-injected mock registry"]
    async fn analyze_benchmark_walk_concurrency() {
        let mappings = 95;
        let delay = std::time::Duration::from_millis(30);
        let repos = repo_names(mappings);

        println!(
            "\n{mappings} mappings, 1 tag each, {}ms per request (1 tag list + 1 manifest each)\n",
            delay.as_millis()
        );
        println!(
            "{:>11}  {:>10}  {:>10}",
            "concurrency", "elapsed", "speedup"
        );

        let mut baseline: Option<std::time::Duration> = None;
        for concurrency in [1usize, 4, 8, 16, 32] {
            // Fresh server and clients per row: AIMD widens with reuse.
            let (_server, config, clients, _, _) = slow_registry(&repos, &["v1"], delay).await;

            let started = std::time::Instant::now();
            let analysis = collect_analysis(
                &config,
                &clients,
                &HashMap::new(),
                &ShutdownSignal::new(),
                &PrepareTracker::default(),
                concurrency,
            )
            .await;
            let elapsed = started.elapsed();
            assert_eq!(analysis.analyzed, mappings);

            let base = *baseline.get_or_insert(elapsed);
            println!(
                "{concurrency:>11}  {:>9.2}s  {:>9.1}x",
                elapsed.as_secs_f64(),
                base.as_secs_f64() / elapsed.as_secs_f64()
            );
        }
        println!(
            "\nboth walks run at most {ANALYZE_CONCURRENCY} at once; past that each \
             registry's AIMD window is the binding constraint\n"
        );
    }

    /// Left on the mapping step, the ticker would report `done=N total=N` for
    /// the whole image walk.
    #[tokio::test]
    async fn collect_analysis_reports_the_image_walk_as_its_own_step() {
        let delay = std::time::Duration::from_millis(1);
        let repos = repo_names(3);
        let (_server, config, clients, _, _) = slow_registry(&repos, &["v1", "v2"], delay).await;

        let tracker = PrepareTracker::default();
        let analysis = collect_analysis(
            &config,
            &clients,
            &HashMap::new(),
            &ShutdownSignal::new(),
            &tracker,
            ANALYZE_CONCURRENCY,
        )
        .await;

        assert_eq!(analysis.analyzed, 6, "three mappings, two tags each");
        let p = tracker.snapshot();
        assert!(
            matches!(p.phase, PreparePhase::Images),
            "the walk must leave the tracker on the image step, got {:?}",
            p.phase
        );
        assert_eq!(
            (p.done, p.total),
            (6, 6),
            "and count every image, not every mapping"
        );
    }
}
