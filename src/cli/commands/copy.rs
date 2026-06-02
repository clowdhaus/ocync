//! The `copy` subcommand - copies a single image between registries.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use ocync_distribution::RepositoryName;
use ocync_distribution::auth::detect::{ProviderKind, detect_provider_kind};
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
use crate::cli::config::{AuthType, load_config};
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

    // Build an ECR batch checker for the destination when applicable. Without
    // this, single-image copy falls back to per-blob HEAD against ECR, whose
    // HEAD returns false-positive 200s under concurrency; only
    // `BatchCheckLayerAvailability` is authoritative.
    //
    // Resolution: an explicit non-Ecr `auth_type` in registry config is a
    // hard opt-out -- the user has told us this destination is not ECR even
    // though the hostname pattern matches. Only fall through to hostname
    // detection when no `auth_type` is set.
    let dst_hostname = bare_hostname(args.destination.registry());
    let dst_is_ecr = match dst_reg_config.and_then(|r| r.auth_type.as_ref()) {
        Some(AuthType::Ecr) => true,
        Some(_) => false,
        None => detect_provider_kind(dst_hostname) == Some(ProviderKind::Ecr),
    };
    let batch_checker: Option<Rc<dyn BatchBlobChecker>> = if dst_is_ecr {
        let profile = dst_reg_config.and_then(|r| r.aws_profile.as_deref());
        let checker = BatchChecker::from_hostname(dst_hostname, profile)
            .await
            .map_err(|e| CliError::Input(format!("ECR batch checker for '{dst_hostname}': {e}")))?;
        Some(Rc::new(checker))
    } else {
        None
    };

    // head_first is a source-side optimization: HEAD-check the target
    // before pulling the source manifest, skipping the source GET when
    // the destination already holds the same digest. Defined on the
    // source registry's config because the optimization conserves the
    // source registry's rate-limit budget; applies to both `sync` and
    // `copy` because the savings shape is the same.
    //
    // When `--config` is not supplied (one-shot copy by image ref) the
    // default is false -- no per-invocation override flag yet; see
    // RegistryConfig::head_first for the field doc.
    let head_first = src_reg_config.map(|r| r.head_first).unwrap_or(false);

    let mapping = ResolvedMapping {
        source_authority,
        source_client,
        source_repo: RepositoryName::new(args.source.repository())?,
        target_repo: RepositoryName::new(args.destination.repository())?,
        targets: vec![TargetEntry {
            name: RegistryAlias::new(dst_hostname),
            client: target_client,
            batch_checker,
            existing_tags: HashSet::new(),
        }],
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
    // Single-target copy: staging is not needed.
    let staging = BlobStage::disabled();
    let report = engine
        .run(vec![mapping], cache, staging, progress, shutdown)
        .await;

    Ok(ExitCode::from_report(report.exit_code()))
}
