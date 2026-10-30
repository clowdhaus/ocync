//! ECR repository creation and deletion for benchmark isolation.

use std::collections::{HashMap, HashSet};
use std::fmt;

use aws_sdk_ecr::Client;
use ocync_distribution::Digest;

use crate::bench::corpus::Corpus;
use crate::bench::lock::{Entry, ImageRef, Lock};
use crate::bench::report::ScenarioKind;
use crate::bench::runner::Tool;

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

/// What a target repository holds, tagged and untagged.
///
/// A multi-arch push stores each child manifest untagged, so the digests of
/// the children are only in the untagged entries.
#[derive(Debug, Default)]
struct RepoListing {
    /// Digest each tag names.
    tags: HashMap<String, Digest>,
    /// Digest of every image in the repo, tagged or not.
    digests: HashSet<Digest>,
}

impl RepoListing {
    /// Record one `ListImages` entry.
    fn insert(&mut self, tag: Option<&str>, digest: &str) -> Result<(), String> {
        let digest: Digest = digest
            .parse()
            .map_err(|e: ocync_distribution::Error| e.to_string())?;
        if let Some(tag) = tag {
            self.tags.insert(tag.to_owned(), digest.clone());
        }
        self.digests.insert(digest);
        Ok(())
    }
}

/// What is wrong with a tag in a target repository.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// The tag is not in the repo.
    TagAbsent { expected: Digest },
    /// The tag names another manifest than the locked one.
    TagDigest { expected: Digest, actual: Digest },
    /// An index child the lock lists is not in the repo.
    ChildAbsent { child: Digest },
}

/// How a tag in a target repository differs from what the lock says it holds.
#[derive(Debug, PartialEq, Eq)]
struct Mismatch {
    /// The target repository.
    repo: String,
    /// The tag.
    tag: String,
    /// What differs.
    kind: Kind,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { repo, tag, kind } = self;
        match kind {
            Kind::TagAbsent { expected } => {
                write!(f, "{repo}:{tag}: expected {expected}, tag absent")
            }
            Kind::TagDigest { expected, actual } => {
                write!(f, "{repo}:{tag}: expected {expected}, found {actual}")
            }
            Kind::ChildAbsent { child } => {
                write!(
                    f,
                    "{repo}:{tag}: expected child {child}, absent from the repo"
                )
            }
        }
    }
}

/// Check one repo against the lock: each expected tag must name the locked
/// digest, and each child of a locked index must be in the repo.
fn check_repo(repo: &str, expected: &[(&str, &Entry)], listing: &RepoListing) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();
    for (tag, locked) in expected {
        let mismatch = |kind| Mismatch {
            repo: repo.to_owned(),
            tag: (*tag).to_owned(),
            kind,
        };
        match listing.tags.get(*tag) {
            None => mismatches.push(mismatch(Kind::TagAbsent {
                expected: locked.digest.clone(),
            })),
            Some(actual) if *actual != locked.digest => {
                mismatches.push(mismatch(Kind::TagDigest {
                    expected: locked.digest.clone(),
                    actual: actual.clone(),
                }))
            }
            Some(_) => {
                for child in &locked.children {
                    if !listing.digests.contains(child) {
                        mismatches.push(mismatch(Kind::ChildAbsent {
                            child: child.clone(),
                        }));
                    }
                }
            }
        }
    }
    mismatches
}

/// List every image in a target repo, tagged or not, across pages.
async fn list_repo(client: &Client, repo: &str) -> Result<RepoListing, String> {
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
        listing
            .insert(id.image_tag(), digest)
            .map_err(|e| format!("{repo}: {e}"))?;
    }
    Ok(listing)
}

/// The target registry and the lock its content is checked against.
pub(crate) struct Target<'a> {
    /// ECR client for the target registry.
    pub(crate) client: &'a Client,
    /// Digests every synced tag must have.
    pub(crate) lock: &'a Lock,
}

/// Verify that every (image, tag) in the corpus is at the target with the
/// locked digest, and that every locked index child is there too.
///
/// This runs after each sync pass, so a tool that converted a format, dropped
/// a platform or transferred nothing fails the run instead of being credited
/// with a fast time.
pub(crate) async fn verify_sync(
    target: &Target<'_>,
    corpus: &Corpus,
    tool: Tool,
    kind: ScenarioKind,
) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    let mut total_tags = 0usize;

    for img in &corpus.images {
        let repo_name = corpus.target_repo(&img.source);
        total_tags += img.tags.len();

        let mut expected: Vec<(&str, &Entry)> = Vec::new();
        for tag in &img.tags {
            let key = ImageRef {
                source: img.source.clone(),
                tag: tag.clone(),
            };
            match target.lock.images.get(&key) {
                Some(entry) => expected.push((tag, entry)),
                None => problems.push(format!("{key}: not in the lock")),
            }
        }

        let listing = match list_repo(target.client, &repo_name).await {
            Ok(listing) => listing,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        let mismatches = check_repo(&repo_name, &expected, &listing);
        if mismatches.is_empty() {
            eprintln!(
                "  verify: {repo_name} OK ({} tags, {} images)",
                expected.len(),
                listing.digests.len()
            );
        }
        problems.extend(mismatches.iter().map(Mismatch::to_string));
    }

    if problems.is_empty() {
        eprintln!(
            "  verify: {tool} {} OK ({total_tags} tags across {} repos)",
            kind.slug(),
            corpus.images.len()
        );
        Ok(())
    } else {
        Err(format!(
            "{tool} {} verification failed -- {} problems: {}",
            kind.slug(),
            problems.len(),
            problems.join("; ")
        ))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::lock::fixtures::{digest as d, entry as locked};

    fn listing(entries: &[(Option<&str>, char)]) -> RepoListing {
        let mut listing = RepoListing::default();
        for (tag, digest) in entries {
            listing.insert(*tag, &d(*digest).to_string()).unwrap();
        }
        listing
    }

    fn mismatch(tag: &str, kind: Kind) -> Mismatch {
        Mismatch {
            repo: "bench/r".into(),
            tag: tag.into(),
            kind,
        }
    }

    #[test]
    fn check_repo_reports_each_way_content_differs_from_the_lock() {
        let index = locked('1', &['2', '3']);
        let single = locked('4', &[]);
        let expected = [("latest", &index), ("v1", &single)];

        // Index and both children present, single manifest at its digest.
        let good = listing(&[
            (Some("latest"), '1'),
            (None, '2'),
            (None, '3'),
            (Some("v1"), '4'),
        ]);
        assert_eq!(check_repo("bench/r", &expected, &good), vec![]);

        let cases: [(&str, RepoListing, Vec<Mismatch>); 3] = [
            (
                "tag at a different digest",
                listing(&[
                    (Some("latest"), '1'),
                    (None, '2'),
                    (None, '3'),
                    (Some("v1"), '9'),
                ]),
                vec![mismatch(
                    "v1",
                    Kind::TagDigest {
                        expected: d('4'),
                        actual: d('9'),
                    },
                )],
            ),
            (
                "index missing a child",
                listing(&[(Some("latest"), '1'), (None, '2'), (Some("v1"), '4')]),
                vec![mismatch("latest", Kind::ChildAbsent { child: d('3') })],
            ),
            (
                "tag absent",
                listing(&[(Some("latest"), '1'), (None, '2'), (None, '3')]),
                vec![mismatch("v1", Kind::TagAbsent { expected: d('4') })],
            ),
        ];
        for (name, listing, want) in cases {
            assert_eq!(check_repo("bench/r", &expected, &listing), want, "{name}");
        }
    }

    #[test]
    fn a_child_present_only_as_a_tag_still_counts_as_in_the_repo() {
        let index = locked('1', &['2']);
        let listing = listing(&[(Some("latest"), '1'), (Some("other"), '2')]);
        assert_eq!(
            check_repo("bench/r", &[("latest", &index)], &listing),
            vec![]
        );
    }

    #[test]
    fn mismatch_text_names_repo_tag_expected_and_actual() {
        let text = mismatch(
            "v1",
            Kind::TagDigest {
                expected: d('4'),
                actual: d('9'),
            },
        )
        .to_string();
        assert_eq!(
            text,
            format!("bench/r:v1: expected {}, found {}", d('4'), d('9'))
        );
    }
}
