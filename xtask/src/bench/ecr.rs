//! ECR repository creation and deletion for benchmark isolation.

use aws_sdk_ecr::Client;

use crate::bench::corpus::Corpus;
use crate::bench::lock::parse_digest;
use crate::bench::target::RepoListing;

/// Create an ECR client from the default AWS config.
pub(crate) async fn client() -> Client {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    Client::new(&config)
}

/// Validate ECR credentials by calling `GetAuthorizationToken`.
pub(crate) async fn validate_credentials(client: &Client) -> Result<(), String> {
    client
        .get_authorization_token()
        .send()
        .await
        .map_err(|e| format!("ECR credential validation failed: {e}"))?;
    Ok(())
}

/// Create all target repositories for the corpus.
///
/// Each source image maps to a `{prefix}/{name}` repository in ECR.
/// Repositories that already exist are silently skipped.
pub(crate) async fn create_repos(client: &Client, corpus: &Corpus) -> Result<(), String> {
    use aws_sdk_ecr::operation::create_repository::CreateRepositoryError;

    for img in &corpus.images {
        let repo_name = corpus.target_repo(&img.source);
        match client
            .create_repository()
            .repository_name(&repo_name)
            .send()
            .await
        {
            Ok(_) => eprintln!("  created repo: {repo_name}"),
            Err(e) => {
                if let Some(CreateRepositoryError::RepositoryAlreadyExistsException(_)) =
                    e.as_service_error()
                {
                    eprintln!("  repo exists: {repo_name}");
                } else {
                    return Err(format!("failed to create repo {repo_name}: {e}"));
                }
            }
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
