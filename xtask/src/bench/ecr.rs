//! ECR repository creation and deletion for benchmark isolation.

use aws_sdk_ecr::Client;
use aws_sdk_ecr::types::Tag;

use crate::bench::corpus::Corpus;
use crate::bench::lock::parse_digest;
use crate::bench::target::RepoListing;

/// Create an ECR client from the default AWS config.
pub(crate) async fn client() -> Client {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    Client::new(&config)
}

/// Create an ECR client for `region`, whatever region the environment names.
pub(crate) async fn client_in(region: &'static str) -> Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region))
        .load()
        .await;
    Client::new(&config)
}

/// Validate ECR credentials by calling `GetAuthorizationToken`.
pub(crate) async fn validate_credentials(client: &Client) -> Result<(), String> {
    registry_token(client).await.map(drop)
}

/// The token `GetAuthorizationToken` returns for the client's region: base64 of
/// `AWS:<password>`, which the registry takes as Basic credentials.
pub(crate) async fn registry_token(client: &Client) -> Result<String, String> {
    let response = client
        .get_authorization_token()
        .send()
        .await
        .map_err(|e| format!("ECR credential validation failed: {e}"))?;
    response
        .authorization_data()
        .first()
        .and_then(|data| data.authorization_token())
        .map(str::to_owned)
        .ok_or_else(|| "ECR credential validation failed: no authorization token".to_owned())
}

/// Whether [`create_repo`] made the repository or found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepoState {
    /// It did not exist and now does.
    Created,
    /// It existed already.
    Existed,
}

/// Create the repository `name` with `tags`, or find it already there.
pub(crate) async fn create_repo(
    client: &Client,
    name: &str,
    tags: &[Tag],
) -> Result<RepoState, String> {
    use aws_sdk_ecr::operation::create_repository::CreateRepositoryError;

    match client
        .create_repository()
        .repository_name(name)
        .set_tags((!tags.is_empty()).then(|| tags.to_vec()))
        .send()
        .await
    {
        Ok(_) => Ok(RepoState::Created),
        Err(e) => {
            if let Some(CreateRepositoryError::RepositoryAlreadyExistsException(_)) =
                e.as_service_error()
            {
                Ok(RepoState::Existed)
            } else {
                Err(format!("failed to create repo {name}: {e}"))
            }
        }
    }
}

/// The value of the resource tag `key` on the repository `name`, if it has one.
pub(crate) async fn repo_tag(
    client: &Client,
    name: &str,
    key: &str,
) -> Result<Option<String>, String> {
    let described = client
        .describe_repositories()
        .repository_names(name)
        .send()
        .await
        .map_err(|e| format!("failed to describe repo {name}: {e}"))?;
    let arn = described
        .repositories()
        .first()
        .and_then(|repo| repo.repository_arn())
        .ok_or_else(|| format!("repo {name}: describe returned no ARN"))?;
    let tags = client
        .list_tags_for_resource()
        .resource_arn(arn)
        .send()
        .await
        .map_err(|e| format!("failed to list tags of repo {name}: {e}"))?;
    Ok(tags
        .tags()
        .iter()
        .find(|tag| tag.key() == key)
        .map(|tag| tag.value().to_owned()))
}

/// Create all target repositories for the corpus.
///
/// Each source image maps to a `{prefix}/{name}` repository in ECR.
/// Repositories that already exist are silently skipped.
pub(crate) async fn create_repos(client: &Client, corpus: &Corpus) -> Result<(), String> {
    for img in &corpus.images {
        let repo_name = corpus.target_repo(&img.source);
        match create_repo(client, &repo_name, &[]).await? {
            RepoState::Created => eprintln!("  created repo: {repo_name}"),
            RepoState::Existed => eprintln!("  repo exists: {repo_name}"),
        }
    }

    Ok(())
}

/// List every image in a target repo, tagged or not, across pages.
pub(crate) async fn list_repo(client: &Client, repo: &str) -> Result<RepoListing, String> {
    let mut listing = RepoListing::default();
    let mut ids = client
        .list_images()
        .repository_name(repo)
        .into_paginator()
        .items()
        .send();
    while let Some(id) = ids.next().await {
        let id = id.map_err(|e| format!("{repo}: list failed ({e})"))?;
        let digest = id
            .image_digest()
            .ok_or_else(|| format!("{repo}: list returned an image with no digest"))?;
        let digest = parse_digest(digest).map_err(|e| format!("{repo}: {e}"))?;
        listing.insert(id.image_tag(), digest);
    }
    Ok(listing)
}

/// Delete all target repositories for the corpus (force delete with images).
///
/// Tries every repo, then fails if any could not be deleted for a reason other
/// than not existing: content left behind would let the next tool's digest check
/// pass on what this tool synced.
pub(crate) async fn delete_repos(client: &Client, corpus: &Corpus) -> Result<(), String> {
    use aws_sdk_ecr::operation::delete_repository::DeleteRepositoryError;

    let mut failures = Vec::new();
    for img in &corpus.images {
        let repo_name = corpus.target_repo(&img.source);
        match client
            .delete_repository()
            .repository_name(&repo_name)
            .force(true)
            .send()
            .await
        {
            Ok(_) => eprintln!("  deleted repo: {repo_name}"),
            Err(e) => {
                if let Some(DeleteRepositoryError::RepositoryNotFoundException(_)) =
                    e.as_service_error()
                {
                    // Already gone -- fine.
                } else {
                    failures.push(format!("{repo_name}: {e}"));
                }
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to delete {} repo(s), content left behind: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}
