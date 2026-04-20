//! The `copy` subcommand - copies a single image between registries.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{
    DEFAULT_MAX_CONCURRENT_TRANSFERS, RegistryAlias, ResolvedMapping, SyncEngine, TagPair,
    TargetEntry,
};
use ocync_sync::retry::RetryConfig;
use ocync_sync::staging::BlobStage;

use crate::CopyArgs;
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client};

/// Run the copy command: transfer a single image from source to destination.
pub(crate) async fn run(
    args: &CopyArgs,
    progress: &dyn ocync_sync::progress::ProgressReporter,
) -> Result<ExitCode, CliError> {
    let src_tag = args
        .source
        .tag()
        .ok_or_else(|| CliError::Input(format!("source '{}' has no tag", args.source)))?;
    let dst_tag = args.destination.tag().unwrap_or(src_tag);

    let source_client =
        Arc::new(build_registry_client(bare_hostname(args.source.registry()), None).await?);
    let target_client =
        Arc::new(build_registry_client(bare_hostname(args.destination.registry()), None).await?);

    let source_authority = source_client
        .registry_authority()
        .map_err(|e| CliError::Input(format!("source '{}': {e}", args.source)))?;

    let mapping = ResolvedMapping {
        source_authority,
        source_client,
        source_repo: args.source.repository().into(),
        target_repo: args.destination.repository().into(),
        targets: vec![TargetEntry {
            name: RegistryAlias::new(bare_hostname(args.destination.registry())),
            client: target_client,
            batch_checker: None,
        }],
        tags: vec![TagPair::retag(src_tag.to_owned(), dst_tag.to_owned())],
        platforms: None,
    };

    let engine = SyncEngine::new(RetryConfig::default(), DEFAULT_MAX_CONCURRENT_TRANSFERS);
    let cache = Rc::new(RefCell::new(TransferStateCache::new()));
    // Single-target copy: staging is not needed.
    let staging = BlobStage::disabled();
    let report = engine
        .run(vec![mapping], cache, staging, progress, None)
        .await;

    match report.exit_code() {
        0 => Ok(ExitCode::Success),
        1 => Ok(ExitCode::PartialFailure),
        _ => Ok(ExitCode::Failure),
    }
}
