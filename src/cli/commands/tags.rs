//! The `tags` subcommand - lists and filters tags from a repository.

use ocync_distribution::RepositoryName;
use ocync_sync::filter::FilterConfig;

use crate::TagsArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client, endpoint_host};

pub(crate) async fn run(args: &TagsArgs) -> Result<ExitCode, CliError> {
    let registry = args.repository.registry();
    let repository = args.repository.repository();

    // Resolve auth settings from config if provided, otherwise use anonymous.
    // Normalize both sides through `endpoint_host` so that `docker.io` matches
    // `registry-1.docker.io` (and vice versa) -- the same rewrite applied by
    // the sync command's client builder.
    let normalized_registry = endpoint_host(bare_hostname(registry));
    let reg_config = if let Some(ref config_path) = args.config {
        let config = load_config(config_path)?;
        config
            .registries
            .into_values()
            .find(|r| endpoint_host(bare_hostname(&r.url)) == normalized_registry)
    } else {
        None
    };

    let client = build_registry_client(registry, reg_config.as_ref()).await?;

    let repo_path = RepositoryName::new(repository)?;
    let all_tags = client.list_tags(&repo_path).await?;

    let filter = FilterConfig {
        glob: args.glob.clone(),
        semver: args.semver.clone(),
        exclude: args.exclude.clone(),
        sort: args.sort.map(|s| s.into()),
        latest: args.latest,
        min_tags: None,
        ..FilterConfig::default()
    };

    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    let filtered = filter.apply(&tag_refs)?;

    for tag in &filtered {
        println!("{tag}");
    }

    Ok(ExitCode::Success)
}
