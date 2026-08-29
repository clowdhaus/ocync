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

use futures_util::StreamExt;
use ocync_distribution::ecr::BatchBlobChecker;
use ocync_distribution::spec::ManifestKind;
use ocync_distribution::{Digest, RepositoryName};

use ocync_sync::ShutdownSignal;
use ocync_sync::retry::{RetryConfig, with_retry};

use crate::cli::commands::synchronize::{
    ClientMap, DroppedTarget, MappingResolution, PreparePhase, PrepareTracker, UnresolvedMapping,
    build_clients, log_unresolved_mapping, referenced_registries, resolve_mapping,
    shared_failure_code, with_prepare_progress,
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

/// Mappings resolved, and images pulled, at once.
///
/// Analysis is pure read traffic against the source registry: a tag listing
/// per mapping and a manifest pull per image, all independent. Same value and
/// same reasoning as `sync`'s
/// [`PREPARE_MAPPING_CONCURRENCY`](super::synchronize::PREPARE_MAPPING_CONCURRENCY),
/// kept separate because the two commands can be tuned apart: a ceiling on
/// open work, not a request rate, with each client's AIMD window still
/// governing how hard any one registry is hit.
const ANALYZE_CONCURRENCY: usize = 16;

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
    let analysis = with_prepare_progress(&tracker, "analysis", async {
        let clients = build_clients(&config, &referenced_registries(&config), &tracker).await;
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
/// Both walks overlap their network waits: mappings resolve concurrently, then
/// every image across every mapping is pulled from one flattened list. The
/// flattening is what makes the second walk worth anything -- a config of many
/// mappings holding a handful of tags each would otherwise be as serial as
/// before, because each mapping's walk is only a request or two long.
///
/// Both walks free each slot as its own work finishes rather than when the
/// oldest one does. An ordered queue would let a single mapping listing a
/// repository with tens of thousands of tags hold its slot while everything
/// that finished behind it holds theirs, which leaves the window slack and
/// gives back most of the overlap.
///
/// Aggregation stays sequential: `analysis.blobs` has one owner and one
/// consumer loop. It no longer runs in config order, which the report does not
/// need -- every figure it prints is a count, a sum, or a sorted container, so
/// it reads the same whichever registry answered first. Mapping bookkeeping
/// does need config order, so those outcomes are put back in order once they
/// are all in.
async fn collect_analysis(
    config: &Config,
    clients: &ClientMap,
    no_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    shutdown: &ShutdownSignal,
    tracker: &PrepareTracker,
    concurrency: usize,
) -> Analysis {
    let mut analysis = Analysis::default();
    // Begun here rather than by the caller so this function owns both of the
    // steps it runs, the way `sync`'s `resolve_all` does.
    tracker.begin(PreparePhase::Mappings, config.mappings.len());

    /// One mapping's resolution, or `None` if shutdown reached it first.
    type MappingOutcome = Option<(Result<MappingResolution, CliError>, Vec<DroppedTarget>)>;

    // --- Resolve every mapping ---
    //
    // `None` marks a mapping shutdown reached before it started. Checked
    // inside the task rather than once before the fan-out so a signal part way
    // through still stops the work that has not begun, which is what the
    // sequential loop's `break` did.
    let mut indexed: Vec<(usize, MappingOutcome)> =
        futures_util::stream::iter(config.mappings.iter().enumerate().map(|(idx, mapping)| {
            async move {
                if shutdown.is_triggered() {
                    return (idx, None);
                }
                // Held for the whole task so every exit still counts the mapping:
                // one that skipped it would strand the progress line short of its
                // total.
                let _item = tracker.track(&mapping.from);
                (
                    idx,
                    Some(resolve_mapping(mapping, config, clients, no_checkers, false).await),
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

        // Dropped targets arrive whatever the outcome: analyze reports what a
        // sync would move, so a target it cannot reach changes the answer even
        // when the mapping itself then fails.
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
            // One unresolvable mapping must not cost the analysis every
            // mapping behind it, same as `sync`.
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
            }
        }
    }

    // --- Walk every image, flattened across mappings ---
    let images: Vec<(usize, String)> = resolved_mappings
        .iter()
        .enumerate()
        .flat_map(|(idx, m)| m.tags.iter().map(move |t| (idx, t.source.clone())))
        .collect();

    // Its own step, so the progress line keeps meaning something. Resolution
    // finishes long before the walk does, and without this the ticker would
    // report a completed mapping count for the whole of the slower half.
    tracker.begin(PreparePhase::Images, images.len());

    // The engine owns the retry policy for the transfer phase; this walk runs
    // before the engine exists, so it carries the same defaults itself.
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
            // One unreadable image must not end the analysis: the remaining
            // tags and mappings are independent of it.
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
                // A skipped index child still leaves this image's other
                // platforms recorded, so it counts as analyzed. Conflating it
                // with a total failure reported a mostly-complete run as zero
                // images and exited 2.
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

/// Pull a manifest, recursing into index children, and return every blob
/// descriptor it references.
///
/// Recording is the caller's job. Splitting it out is what lets images be
/// pulled concurrently: the network half borrows nothing mutable, so the
/// aggregate map keeps a single owner and one deterministic write order.
///
/// Index children stay sequential within an image. The images themselves
/// already overlap, and the client's AIMD window saturates well below the
/// number of images in flight, so nesting a second level of concurrency here
/// would add no throughput.
///
/// Every pull is retried here. This walk is the highest-volume request path in
/// the prepare phase, one manifest read per tag plus one per platform, all
/// against the same source registry at `ANALYZE_CONCURRENCY`, so it is exactly
/// the shape that provokes a throttle. The retry lives at this call site
/// rather than inside `manifest_pull` because the engine wraps its own
/// `manifest_pull` calls in `with_retry`; putting one further down would
/// multiply the two. `analyze` runs before the engine exists, so it brings its
/// own.
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

    // Recurse into index children to collect per-platform manifest blobs.
    if let ManifestKind::Index(index) = &pulled.manifest {
        for child in &index.manifests {
            // Platforms are independent: a missing arm64 manifest should not
            // discard the amd64 blobs already recorded for this image.
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

    // -----------------------------------------------------------------------
    // Prepare-phase concurrency
    // -----------------------------------------------------------------------

    use crate::cli::commands::test_support::{
        Arrivals, SlowRecorder, arrivals, config_yaml, max_in_flight, repo_names, test_client,
    };

    /// A registry that answers tag listings and manifest pulls after `delay`,
    /// recording each separately.
    async fn slow_registry(
        repos: &[String],
        tags: &[&str],
        delay: std::time::Duration,
    ) -> (wiremock::MockServer, Config, ClientMap, Arrivals, Arrivals) {
        let server = wiremock::MockServer::start().await;
        let tag_arrivals = arrivals();
        let manifest_arrivals = arrivals();

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v2/repo/img\d+/tags/list$",
            ))
            .respond_with(SlowRecorder::tag_list(&tag_arrivals, delay, tags))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v2/repo/img\d+/manifests/.+$",
            ))
            .respond_with(SlowRecorder::image_manifest(&manifest_arrivals, delay))
            .mount(&server)
            .await;

        let yaml = config_yaml(&server, repos, "      glob: [\"*\"]\n");
        let config: Config = serde_yaml::from_str(&yaml).expect("generated config parses");
        let clients = ClientMap::from([
            ("src".to_string(), Ok(test_client(&server.uri()))),
            ("dst".to_string(), Ok(test_client(&server.uri()))),
        ]);
        (server, config, clients, tag_arrivals, manifest_arrivals)
    }

    /// Analysis must resolve its mappings concurrently, like `sync` does.
    #[tokio::test]
    async fn collect_analysis_resolves_mappings_concurrently() {
        let delay = std::time::Duration::from_millis(250);
        let repos = repo_names(8);
        let (_server, config, clients, tag_arrivals, _) =
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
    }

    /// The image walk must overlap across mappings, not just within one.
    ///
    /// The shape that matters is many mappings holding few tags each: walking
    /// one mapping's tags at a time leaves that config as slow as it ever was,
    /// because each mapping's walk is one request long.
    #[tokio::test]
    async fn collect_analysis_pulls_images_concurrently_across_mappings() {
        let delay = std::time::Duration::from_millis(250);
        let repos = repo_names(8);
        let (_server, config, clients, _, manifest_arrivals) =
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

        assert_eq!(analysis.analyzed, 8);
        let observed = max_in_flight(&manifest_arrivals, delay);
        assert!(
            observed > 1,
            "manifest pulls must overlap across mappings; saw at most {observed} in flight"
        );
    }

    /// Shutdown before the walk starts must stop it, not just flag it.
    ///
    /// The negative half is the registry: a check that ran after the fan-out
    /// would still have listed every mapping's tags before noticing.
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

    /// Shutdown part way through leaves a truncated report, not a clean one.
    ///
    /// More images than the concurrency bound, so the ones past the first
    /// batch have not started when the signal lands and must not start.
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
            // Triggered once resolution is done and the image walk is under
            // way, so the walk is what gets cut short.
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

    /// How long an analysis takes as its two walks are widened.
    ///
    /// The config shape is the one that exposes the flattened image walk: many
    /// mappings holding a single tag each, so a per-mapping walk would still
    /// be one request long and gain nothing. Concurrency 1 is the behaviour
    /// this replaced.
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
            // Fresh server and clients per row: AIMD widens as a registry keeps
            // answering, so reuse would hand later rows a head start.
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

    /// The image walk has to drive the progress line too.
    ///
    /// Resolution finishes long before the walk does. Left on the mapping
    /// step, the ticker would report `done=N total=N` for the whole of the
    /// slower half, which reads as a finished run that never ends.
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
