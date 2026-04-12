//! The `copy` subcommand — copies a single image between registries.

use std::sync::Arc;

use ocync_sync::engine::{ResolvedMapping, SyncEngine, TagPair, TargetEntry};
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;
use ocync_sync::{ImageStatus, SkipReason};

use crate::CopyArgs;
use crate::cli::{CliError, ExitCode, build_registry_client};

/// Run the copy command: transfer a single image from source to destination.
pub(crate) async fn run(args: &CopyArgs) -> Result<ExitCode, CliError> {
    let src_tag = args
        .source
        .tag()
        .ok_or_else(|| CliError::Input(format!("source '{}' has no tag", args.source)))?;
    let dst_tag = args.destination.tag().unwrap_or(src_tag);

    let source_client = Arc::new(build_registry_client(args.source.registry(), None).await?);
    let target_client = Arc::new(build_registry_client(args.destination.registry(), None).await?);

    let mapping = ResolvedMapping {
        source_client,
        source_repo: args.source.repository().to_owned(),
        target_repo: args.destination.repository().to_owned(),
        targets: vec![TargetEntry {
            name: args.destination.registry().to_owned(),
            client: target_client,
        }],
        tags: vec![TagPair::retag(src_tag.to_owned(), dst_tag.to_owned())],
    };

    let engine = SyncEngine::new(RetryConfig::default());
    let progress = NullProgress;
    let report = engine.run(vec![mapping], &progress).await;

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
                let reason_str = skip_reason_label(reason);
                println!("Skipped {src_display} -> {dst_display} (reason: {reason_str})");
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

/// Human-readable label for a skip reason.
fn skip_reason_label(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::DigestMatch => "DigestMatch",
        SkipReason::SkipExisting => "SkipExisting",
        SkipReason::ImmutableTag => "ImmutableTag",
    }
}
