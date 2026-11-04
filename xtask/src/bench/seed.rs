//! `bench-seed`: copies every digest of the corpus lock from its public source
//! into the controlled source, so a published run reads a registry the bench
//! owns and the public registries' rate limits stay out of the comparison.
//!
//! ocync does the copy, from a config that pins each locked tag to its digest,
//! so the manifests and blobs arrive bit for bit and a second run copies
//! nothing: ocync skips a digest the target holds. Each repository carries the
//! `locked_at` of the lock it was seeded from as a resource tag, so seeding
//! another lock over it is refused rather than mixed in. A run needs no marker
//! of its own: it HEADs every locked digest in the source before it builds.

use std::collections::BTreeMap;
use std::path::Path;

use aws_sdk_ecr::types::Tag;
use clap::Args;

use crate::bench::corpus::{self, Corpus, CorpusSettings, ImageEntry};
use crate::bench::ecr::{self, RepoState};
use crate::bench::lock::{self, Lock, LockedAt};
use crate::bench::runner::{self, Tool};
use crate::bench::source::{self, Controlled, Source};
use crate::bench::target::ENV_REGISTRY;
use crate::bench::{config_gen, docker_hub_auth, registry, write_file};

/// Arguments for the `bench-seed` subcommand.
#[derive(Args)]
pub(crate) struct SeedArgs {
    /// Path to the corpus YAML whose lock is seeded. The lock is the
    /// `.lock.json` beside it and holds the partial scenario's tags too.
    #[arg(long, default_value = corpus::DEFAULT_CORPUS)]
    corpus: String,
}

/// The corpus a seed copies: every source the lock holds, with its locked tags,
/// read from the public registries into `controlled`.
fn seed_corpus(lock: &Lock, controlled: &Controlled) -> Corpus {
    let mut tags: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for image in lock.images.keys() {
        tags.entry(&image.source)
            .or_default()
            .push(image.tag.clone());
    }
    Corpus {
        settings: CorpusSettings {
            target_registry: controlled.host().to_owned(),
            target_prefix: source::REPO_PREFIX.to_owned(),
        },
        images: tags
            .into_iter()
            .map(|(source, tags)| ImageEntry {
                source: source.to_owned(),
                tags,
            })
            .collect(),
        dockerhub_auth: docker_hub_auth(),
        source: Source::Public,
    }
}

/// Refuses a repository seeded from another lock than `lock`, or by nothing
/// that recorded one. `held` is the `locked_at` the repository's tag names.
fn ensure_same_lock(repo: &str, held: Option<&str>, lock: &LockedAt) -> Result<(), String> {
    match held {
        Some(held) if held == lock.to_string() => Ok(()),
        Some(held) => Err(format!(
            "{repo} was seeded from the lock written at {held}, not the one written at {lock}"
        )),
        None => Err(format!(
            "{repo} carries no {} tag, so it was not seeded by bench-seed",
            source::LOCK_TAG_KEY
        )),
    }
}

/// Makes every repository of `corpus` in the controlled source, tagged with the
/// lock it is seeded from, and refuses all that hold another lock's content
/// before anything is copied.
async fn ensure_repos(
    client: &aws_sdk_ecr::Client,
    corpus: &Corpus,
    controlled: &Controlled,
    lock: &LockedAt,
) -> Result<(), String> {
    let tag = Tag::builder()
        .key(source::LOCK_TAG_KEY)
        .value(lock.to_string())
        .build()
        .map_err(|e| format!("build repository tag: {e}"))?;
    let mut refused = Vec::new();
    for image in &corpus.images {
        let repo = controlled.repo(&image.source);
        match ecr::create_repo(client, &repo, std::slice::from_ref(&tag)).await? {
            RepoState::Created => eprintln!("  created repo: {repo}"),
            RepoState::Existed => {
                let held = ecr::repo_tag(client, &repo, source::LOCK_TAG_KEY).await?;
                match ensure_same_lock(&repo, held.as_deref(), lock) {
                    Ok(()) => eprintln!("  repo exists: {repo}"),
                    Err(reason) => refused.push(reason),
                }
            }
        }
    }
    if refused.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the controlled source holds another lock's content, and a run would mix two series:\n  {}\n\
             Delete the {}/ repositories of the controlled source (only copies of public images) \
             and seed again.",
            refused.join("\n  "),
            source::REPO_PREFIX,
        ))
    }
}

/// The `bench-seed` command.
pub(crate) async fn run(args: SeedArgs) -> Result<(), Box<dyn std::error::Error>> {
    let lock = Lock::load(&corpus::lock_path(Path::new(&args.corpus)))?;
    let target_registry = std::env::var(ENV_REGISTRY).map_err(|_| {
        format!("{ENV_REGISTRY} is not set: it names the target registry the controlled source sits beside")
    })?;
    let controlled = Controlled::beside(&target_registry)?;

    let client = ecr::client_in(source::REGION).await;
    ecr::validate_credentials(&client).await?;
    let corpus = seed_corpus(&lock, &controlled);
    eprintln!(
        "bench-seed: {} refs of {} images, lock written at {}, into {}",
        lock.images.len(),
        corpus.images.len(),
        lock.locked_at,
        source::REGION,
    );
    ensure_repos(&client, &corpus, &controlled, &lock.locked_at).await?;

    let workspace_root = Path::new(".");
    let version = runner::check_tool(Tool::Ocync, workspace_root).await?;
    eprintln!("  ocync: {version}");
    let config_dir = tempfile::tempdir()?;
    let config = config_dir.path().join("seed.yaml");
    write_file(
        &config,
        &config_gen::seed_config(&corpus, &lock, &controlled),
        0o600,
    )?;
    let copied =
        runner::run_tool(Tool::Ocync, "seed", &config, None, workspace_root, false).await?;
    if !copied.exit_status.success() {
        return Err(format!("ocync did not finish the copy ({})", copied.exit_status).into());
    }

    // The copy exits clean only for what it was asked to copy; this is the
    // check a run makes, so a seed is done when a run would start.
    let refs = lock.images.keys().cloned().collect();
    lock::ensure_served(
        &registry::http_client()?,
        &lock,
        &refs,
        None,
        &Source::Controlled(controlled),
    )
    .await?;
    eprintln!(
        "bench-seed: the controlled source holds every digest of the lock written at {}",
        lock.locked_at
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::lock::fixtures::{entry, lock_of, locked_at};

    fn controlled() -> Controlled {
        Controlled::beside("111111111111.dkr.ecr.us-east-2.amazonaws.com").unwrap()
    }

    fn lock() -> Lock {
        lock_of(&[
            ("docker.io/library/alpine:3.20", entry('1', &[])),
            ("docker.io/library/alpine:latest", entry('2', &[])),
            ("cgr.dev/chainguard/static:latest", entry('3', &[])),
        ])
    }

    #[test]
    fn the_seed_config_pins_every_locked_digest_into_the_controlled_source() {
        // Catches: a tag copied from its moving tag instead of its digest, a
        // locked ref left out, and an image written to a repository the runs
        // do not read.
        let lock = lock();
        let controlled = controlled();
        let config = config_gen::seed_config(&seed_corpus(&lock, &controlled), &lock, &controlled);
        for entry in lock.images.values() {
            assert!(
                config.contains(&format!("digest: \"{}\"", entry.digest)),
                "{config}"
            );
        }
        assert_eq!(config.matches("digest: ").count(), lock.images.len());
        for (from, to) in [
            ("library/alpine", "bench-source/library/alpine"),
            ("chainguard/static", "bench-source/chainguard/static"),
        ] {
            assert!(
                config.contains(&format!("from: \"{from}\"\n    to: \"{to}\"")),
                "{config}"
            );
        }
        assert!(
            config
                .contains("url: 111111111111.dkr.ecr.us-east-1.amazonaws.com\n    auth_type: ecr"),
            "{config}"
        );
        assert!(config.contains("url: docker.io") && config.contains("url: cgr.dev"));
    }

    #[test]
    fn a_repository_seeded_from_another_lock_is_refused() {
        // Catches: seeding a new lock over an old one, which leaves a registry
        // holding two series, and a repository nobody tagged being adopted.
        let now = locked_at("2026-11-01-000000");
        assert!(ensure_same_lock("bench-source/a", Some("2026-11-01-000000"), &now).is_ok());
        let other = ensure_same_lock("bench-source/a", Some("2026-10-30-120000"), &now);
        let other = other.unwrap_err();
        assert!(
            other.contains("2026-10-30-120000") && other.contains("2026-11-01-000000"),
            "{other}"
        );
        let untagged = ensure_same_lock("bench-source/a", None, &now).unwrap_err();
        assert!(untagged.contains("not seeded by bench-seed"), "{untagged}");
    }
}
