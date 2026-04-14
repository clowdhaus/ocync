//! The `tags` subcommand — lists and filters tags from a repository.

use ocync_distribution::RepositoryName;
use ocync_sync::filter::FilterConfig;

use crate::TagsArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client};

pub(crate) async fn run(args: &TagsArgs) -> Result<ExitCode, CliError> {
    let registry = args.repository.registry();
    let repository = args.repository.repository();

    // Resolve auth settings from config if provided, otherwise use anonymous.
    let reg_config = if let Some(ref config_path) = args.config {
        let config = load_config(config_path)?;
        config
            .registries
            .into_values()
            .find(|r| bare_hostname(&r.url) == registry)
    } else {
        None
    };

    let client = build_registry_client(
        registry,
        reg_config.as_ref().and_then(|r| r.auth_type.as_ref()),
        None,
        reg_config.as_ref().and_then(|r| r.credentials.as_ref()),
        reg_config.as_ref().and_then(|r| r.token.as_deref()),
    )
    .await?;

    let repo_path = RepositoryName::from(repository);
    let all_tags = client.list_tags(&repo_path).await?;

    let filter = FilterConfig {
        glob: args.glob.clone(),
        semver: args.semver.clone(),
        semver_prerelease: None,
        exclude: args.exclude.clone(),
        sort: args.sort.map(|s| s.into()),
        latest: args.latest,
        min_tags: None,
    };

    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    let filtered = filter.apply(&tag_refs)?;

    for tag in &filtered {
        println!("{tag}");
    }

    Ok(ExitCode::Success)
}
