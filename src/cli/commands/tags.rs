//! The `tags` subcommand — lists and filters tags from a repository.

use ocync_distribution::auth::anonymous::AnonymousAuth;
use ocync_distribution::{Reference, RegistryClient};
use ocync_sync::filter::FilterConfig;
use url::Url;

use crate::TagsArgs;

pub(crate) async fn run(args: &TagsArgs) -> i32 {
    let reference: Reference = match args.repository.parse() {
        Ok(r) => r,
        Err(err) => {
            eprintln!("error: invalid repository reference: {err}");
            return 2;
        }
    };

    let registry = reference.registry();
    let repository = reference.repository();

    let base_url = format!("https://{registry}");
    let url = match Url::parse(&base_url) {
        Ok(u) => u,
        Err(err) => {
            eprintln!("error: invalid registry URL '{base_url}': {err}");
            return 2;
        }
    };

    let http = reqwest::Client::new();
    let auth = AnonymousAuth::new(registry, http);
    let client = match RegistryClient::builder(url).auth(auth).build() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: failed to build registry client: {err}");
            return 2;
        }
    };

    let all_tags = match client.list_tags(repository).await {
        Ok(t) => t,
        Err(err) => {
            eprintln!("error: failed to list tags for {registry}/{repository}: {err}");
            return 1;
        }
    };

    // Build filter from CLI flags.
    let filter = FilterConfig {
        glob: args.glob.iter().cloned().collect(),
        semver: args.semver.clone(),
        semver_prerelease: None,
        exclude: args.exclude.iter().cloned().collect(),
        sort: args.sort.map(|s| s.into()),
        latest: args.latest,
        min_tags: None,
    };

    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    let filtered = match filter.apply(&tag_refs) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("error: tag filter failed: {err}");
            return 2;
        }
    };

    for tag in &filtered {
        println!("{tag}");
    }

    0
}
