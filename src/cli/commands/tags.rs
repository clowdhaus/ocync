//! The `tags` subcommand — lists and filters tags from a repository.

use ocync_sync::filter::FilterConfig;

use crate::TagsArgs;
use crate::cli::{CliError, ExitCode, build_registry_client};

pub(crate) async fn run(args: &TagsArgs) -> Result<ExitCode, CliError> {
    let registry = args.repository.registry();
    let repository = args.repository.repository();

    let client = build_registry_client(registry, None).await?;

    let all_tags = client.list_tags(repository).await?;

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
