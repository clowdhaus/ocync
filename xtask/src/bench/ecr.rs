//! ECR repository creation and deletion for benchmark isolation.

use aws_sdk_ecr::Client;

use crate::bench::corpus::Corpus;

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

/// Delete all target repositories for the corpus (force delete with images).
pub(crate) async fn delete_repos(client: &Client, corpus: &Corpus) -> Result<(), String> {
    use aws_sdk_ecr::operation::delete_repository::DeleteRepositoryError;

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
                    eprintln!("  warning: failed to delete repo {repo_name}: {e}");
                    // Log and continue -- don't abort cleanup for one failure.
                }
            }
        }
    }

    Ok(())
}
