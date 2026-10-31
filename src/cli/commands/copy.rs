//! The `copy` subcommand: copies a single image between registries.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ocync_distribution::ecr::{BatchBlobChecker, BatchChecker};
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{
    DEFAULT_MAX_CONCURRENT_TRANSFERS, RegistryAlias, ResolvedArtifacts, ResolvedMapping,
    SyncEngine, TagPair, TargetEntry,
};
use ocync_sync::retry::RetryConfig;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;

use crate::CopyArgs;
use crate::cli::config::load_config;
use crate::cli::{
    CliError, ExitCode, Registry, RunCredentials, bare_hostname, build_registry_client,
    is_ecr_registry,
};

/// Transfer a single image from source to destination; `shutdown`, if given,
/// is forwarded to the engine for a graceful drain.
pub(crate) async fn run(
    args: &CopyArgs,
    progress: &dyn ocync_sync::progress::ProgressReporter,
    shutdown: Option<&ShutdownSignal>,
) -> Result<ExitCode, CliError> {
    let src_tag = args
        .source
        .tag()
        .ok_or_else(|| CliError::Input(format!("source '{}' has no tag", args.source)))?;
    let dst_tag = args.destination.tag().unwrap_or(src_tag);

    let registries = if let Some(ref config_path) = args.config {
        let config = load_config(config_path)?;
        config.registries.into_values().collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let destination = Registry::resolve(&registries, args.destination.registry());
    let dst_reg_config = destination.config();
    let source = Registry::resolve(&registries, args.source.registry());
    let credentials = RunCredentials::default();
    let source_client = Arc::new(build_registry_client(source, &credentials).await?);
    let target_client = Arc::new(build_registry_client(destination, &credentials).await?);

    let source_authority = source_client
        .registry_authority()
        .map_err(|e| CliError::Input(format!("source '{}': {e}", args.source)))?;

    // ECR batch checker for the destination: ECR's per-blob HEAD returns
    // false-positive 200s under concurrency; only `BatchCheckLayerAvailability`
    // is authoritative.
    let dst_hostname = bare_hostname(args.destination.registry());
    let dst_is_ecr = is_ecr_registry(
        dst_reg_config.and_then(|r| r.auth_type.as_ref()),
        dst_hostname,
    );
    let batch_checker: Option<Rc<dyn BatchBlobChecker>> = if dst_is_ecr {
        let profile = dst_reg_config.and_then(|r| r.aws_profile.as_deref());
        let checker = BatchChecker::from_hostname(dst_hostname, profile, &credentials.aws)?;
        Some(Rc::new(checker))
    } else {
        None
    };

    // Read from the source registry's config because it conserves the source's
    // rate-limit budget. False without `--config`: there is no override flag.
    let head_first = source.config().map(|r| r.head_first).unwrap_or(false);

    let mapping = ResolvedMapping {
        source_authority,
        source_client,
        source_repo: args.source.repository().clone(),
        target_repo: args.destination.repository().clone(),
        targets: vec![
            TargetEntry::new(
                RegistryAlias::new(dst_hostname),
                target_client,
                batch_checker,
            )
            .map_err(|e| CliError::Input(format!("destination '{}': {e}", args.destination)))?,
        ],
        tags: vec![TagPair::retag(src_tag.to_owned(), dst_tag.to_owned())],
        platforms: None,
        head_first,
        immutable_glob: None,
        artifacts_config: Rc::new(ResolvedArtifacts {
            enabled: false,
            ..ResolvedArtifacts::default()
        }),
        candidate_count: None,
        filter_report: None,
    };

    let engine = SyncEngine::new(RetryConfig::default(), DEFAULT_MAX_CONCURRENT_TRANSFERS);
    let cache = Rc::new(RefCell::new(TransferStateCache::new()));
    // A single target needs no staging.
    let staging = BlobStage::disabled();
    let report = engine
        .run(vec![mapping], cache, staging, progress, shutdown)
        .await;

    Ok(ExitCode::from_report(report.outcome()))
}
