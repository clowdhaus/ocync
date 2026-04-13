//! The `copy` subcommand -- copies a single image between registries.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ocync_sync::ImageStatus;
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{
    DEFAULT_MAX_CONCURRENT_TRANSFERS, ResolvedMapping, SyncEngine, TagPair, TargetEntry,
};
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;
use ocync_sync::staging::BlobStage;

use crate::CopyArgs;
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client};

/// Run the copy command: transfer a single image from source to destination.
pub(crate) async fn run(args: &CopyArgs) -> Result<ExitCode, CliError> {
    let src_tag = args
        .source
        .tag()
        .ok_or_else(|| CliError::Input(format!("source '{}' has no tag", args.source)))?;
    let dst_tag = args.destination.tag().unwrap_or(src_tag);

    let source_client =
        Arc::new(build_registry_client(bare_hostname(args.source.registry()), None, None).await?);
    let target_client = Arc::new(
        build_registry_client(bare_hostname(args.destination.registry()), None, None).await?,
    );

    let mapping = ResolvedMapping {
        source_client,
        source_repo: args.source.repository().to_owned(),
        target_repo: args.destination.repository().to_owned(),
        targets: vec![TargetEntry {
            name: bare_hostname(args.destination.registry()).to_owned(),
            client: target_client,
            batch_checker: None,
        }],
        tags: vec![TagPair::retag(src_tag.to_owned(), dst_tag.to_owned())],
    };

    let engine = SyncEngine::new(RetryConfig::default(), DEFAULT_MAX_CONCURRENT_TRANSFERS);
    let cache = Rc::new(RefCell::new(TransferStateCache::new()));
    // Single-target copy: staging is not needed.
    let staging = BlobStage::disabled();
    let progress = NullProgress;
    let report = engine
        .run(vec![mapping], cache, staging, &progress, None)
        .await;

    let src_display = format!(
        "{}/{}:{}",
        args.source.registry(),
        args.source.repository(),
        src_tag
    );
    let dst_display = format!(
        "{}/{}:{}",
        args.destination.registry(),
        args.destination.repository(),
        dst_tag
    );

    // Exactly one image result expected.
    if let Some(result) = report.images.first() {
        match &result.status {
            ImageStatus::Synced => {
                println!(
                    "Copied {src_display} -> {dst_display} ({:.1}s)",
                    result.duration.as_secs_f64()
                );
            }
            ImageStatus::Skipped { reason } => {
                println!("Skipped {src_display} -> {dst_display} (reason: {reason})");
            }
            ImageStatus::Failed { error, retries } => {
                eprintln!("Failed {src_display} -> {dst_display}: {error} (retries: {retries})");
            }
        }
    }

    match report.exit_code() {
        0 => Ok(ExitCode::Success),
        1 => Ok(ExitCode::Failure),
        _ => Ok(ExitCode::Error),
    }
}
