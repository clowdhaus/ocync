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

/// Verify that every (image, tag) in the corpus exists at the target registry.
///
/// Lists tags in each target repo and checks that every expected tag is present.
/// Returns `Err` with the list of missing tags if any are absent. This runs
/// after each cold/warm sync to confirm the tool actually transferred all content.
pub(crate) async fn verify_sync(
    client: &Client,
    corpus: &Corpus,
    tool: &str,
    phase: &str,
) -> Result<(), String> {
    let mut missing: Vec<String> = Vec::new();
    let mut total_tags = 0u64;

    for img in &corpus.images {
        let repo_name = corpus.target_repo(&img.source);
        let expected_tags: std::collections::HashSet<&str> =
            img.tags.iter().map(|s| s.as_str()).collect();
        total_tags += expected_tags.len() as u64;

        // List all tags at target.
        let mut actual_tags: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut req = client
                .list_images()
                .repository_name(&repo_name)
                .filter(
                    aws_sdk_ecr::types::ListImagesFilter::builder()
                        .tag_status(aws_sdk_ecr::types::TagStatus::Tagged)
                        .build(),
                );
            if let Some(token) = &next_token {
                req = req.next_token(token);
            }
            match req.send().await {
                Ok(output) => {
                    for id in output.image_ids() {
                        if let Some(tag) = id.image_tag() {
                            actual_tags.insert(tag.to_string());
                        }
                    }
                    next_token = output.next_token().map(|s| s.to_string());
                    if next_token.is_none() {
                        break;
                    }
                }
                Err(e) => {
                    missing.push(format!("{repo_name}: list failed ({e})"));
                    break;
                }
            }
        }

        for tag in &expected_tags {
            if !actual_tags.contains(*tag) {
                missing.push(format!("{repo_name}:{tag}"));
            }
        }
    }

    if missing.is_empty() {
        eprintln!(
            "  verify: {tool} {phase} OK ({total_tags} tags across {} repos)",
            corpus.images.len()
        );
        Ok(())
    } else {
        Err(format!(
            "{tool} {phase} verification failed -- {} missing tags: {}",
            missing.len(),
            missing.join(", ")
        ))
    }
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
