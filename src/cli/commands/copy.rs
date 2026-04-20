//! The `copy` subcommand - copies a single image between registries.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use ocync_distribution::RepositoryName;
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{
    DEFAULT_MAX_CONCURRENT_TRANSFERS, RegistryAlias, ResolvedMapping, SyncEngine, TagPair,
    TargetEntry,
};
use ocync_sync::retry::RetryConfig;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;

use crate::CopyArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client, endpoint_host};

/// Run the copy command: transfer a single image from source to destination.
///
/// The `shutdown` signal, if provided, will be forwarded to the engine for
/// graceful drain on SIGINT/SIGTERM.
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

    // Resolve auth settings from config if provided, otherwise use auto-detection.
    let registries = if let Some(ref config_path) = args.config {
        let config = load_config(config_path)?;
        config.registries.into_values().collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let normalized_src = endpoint_host(bare_hostname(args.source.registry()));
    let src_reg_config = registries
        .iter()
        .find(|r| endpoint_host(bare_hostname(&r.url)) == normalized_src);

    let normalized_dst = endpoint_host(bare_hostname(args.destination.registry()));
    let dst_reg_config = registries
        .iter()
        .find(|r| endpoint_host(bare_hostname(&r.url)) == normalized_dst);

    let source_client = Arc::new(
        build_registry_client(bare_hostname(args.source.registry()), src_reg_config).await?,
    );
    let target_client = Arc::new(
        build_registry_client(bare_hostname(args.destination.registry()), dst_reg_config).await?,
    );

    let source_authority = source_client
        .registry_authority()
        .map_err(|e| CliError::Input(format!("source '{}': {e}", args.source)))?;

    let mapping = ResolvedMapping {
        source_authority,
        source_client,
        source_repo: RepositoryName::new(args.source.repository())?,
        target_repo: RepositoryName::new(args.destination.repository())?,
        targets: vec![TargetEntry {
            name: RegistryAlias::new(bare_hostname(args.destination.registry())),
            client: target_client,
            batch_checker: None,
            existing_tags: HashSet::new(),
        }],
        tags: vec![TagPair::retag(src_tag.to_owned(), dst_tag.to_owned())],
        platforms: None,
        head_first: false,
        immutable_glob: None,
    };

    let engine = SyncEngine::new(RetryConfig::default(), DEFAULT_MAX_CONCURRENT_TRANSFERS);
    let cache = Rc::new(RefCell::new(TransferStateCache::new()));
    // Single-target copy: staging is not needed.
    let staging = BlobStage::disabled();
    let report = engine
        .run(vec![mapping], cache, staging, progress, shutdown)
        .await;

    Ok(ExitCode::from_report(report.exit_code()))
}
