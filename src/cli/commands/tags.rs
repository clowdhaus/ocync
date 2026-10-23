//! The `tags` subcommand - lists and filters tags from a repository.

use ocync_distribution::RepositoryName;
use ocync_sync::filter::FilterConfig;

use crate::TagsArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode, Registry, build_registry_client};

pub(crate) async fn run(args: &TagsArgs) -> Result<ExitCode, CliError> {
    let registry = args.repository.registry();
    let repository = args.repository.repository();

    // Resolve auth settings from config if provided, otherwise auto-detect.
    let registries = match &args.config {
        Some(config_path) => load_config(config_path)?.registries.into_values().collect(),
        None => Vec::new(),
    };
    let client = build_registry_client(Registry::resolve(&registries, registry)).await?;

    let repo_path = RepositoryName::new(repository)?;
    let all_tags = client.list_tags(&repo_path).await?;

    let filter = FilterConfig {
        glob: args.glob.clone(),
        semver: args.semver.clone(),
        exclude: args.exclude.clone(),
        sort: args.sort.map(|s| s.into()),
        latest: args.latest,
        ..FilterConfig::default()
    };

    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    let filtered = filter.apply(&tag_refs)?;

    for tag in &filtered {
        println!("{tag}");
    }

    Ok(ExitCode::Success)
}
