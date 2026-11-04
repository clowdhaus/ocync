//! `bench-seed`: copies every digest of the corpus lock from its public source
//! into the controlled source, so a published run reads a registry the bench
//! owns and the public registries' rate limits stay out of the comparison.
//!
//! ocync does the copy, from a config that pins each locked tag to its digest,
//! so the manifests and blobs arrive bit for bit and a second run copies
//! nothing: ocync skips a digest the target holds. A run needs no marker of its
//! own: it HEADs every locked digest in the source before it builds, and so
//! does the end of a seed.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::process::ExitStatus;

use clap::Args;

use crate::bench::corpus::{CorpusPaths, DockerHubAuth, ImageEntry};
use crate::bench::ecr::{self, RepoState};
use crate::bench::lock::{self, Lock, NotServed};
use crate::bench::runner::{self, Tool};
use crate::bench::source::{self, Controlled, Region, Source};
use crate::bench::target::ENV_REGISTRY;
use crate::bench::{config_gen, docker_hub_auth, registry, write_file};

/// Arguments for the `bench-seed` subcommand.
#[derive(Args)]
pub(crate) struct SeedArgs {
    /// The corpus whose lock is seeded, as `bench` takes it. The lock holds the
    /// partial scenario's tags too.
    #[command(flatten)]
    paths: CorpusPaths,

    /// The AWS region of the controlled source, which must not be the
    /// target's.
    #[arg(long, default_value = source::REGION)]
    source_region: Region,
}

/// Every source the lock holds, with its locked tags.
fn seed_images(lock: &Lock) -> Vec<ImageEntry> {
    let mut tags: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for image in lock.images.keys() {
        tags.entry(&image.source)
            .or_default()
            .push(image.tag.clone());
    }
    tags.into_iter()
        .map(|(source, tags)| ImageEntry {
            source: source.to_owned(),
            tags,
        })
        .collect()
}

/// Seeds `seed`: makes every repository, copies, and checks that the
/// controlled source serves every locked digest. The steps that touch the
/// outside are `ensure_repo`, `copy` (given the ocync config, returns how ocync
/// exited) and `verify`.
async fn seed_with(
    seed: &config_gen::Seed<'_>,
    lock: &Lock,
    ensure_repo: impl AsyncFn(&str) -> Result<RepoState, String>,
    copy: impl AsyncFn(&str) -> Result<ExitStatus, String>,
    verify: impl AsyncFn() -> Result<(), NotServed>,
) -> Result<(), Box<dyn Error>> {
    for image in seed.images {
        let repo = seed.controlled.repo(&image.source);
        match ensure_repo(&repo).await? {
            RepoState::Created => eprintln!("  created repo: {repo}"),
            RepoState::Existed => eprintln!("  repo exists: {repo}"),
        }
    }

    let copied = copy(&config_gen::seed_config(seed, lock)).await?;
    if !copied.success() {
        return Err(format!("ocync did not finish the copy ({copied})").into());
    }

    // The copy exits clean only for what it was asked to copy; this is the
    // check a run makes, so a seed is done when a run would start.
    verify().await?;
    Ok(())
}

/// The `bench-seed` command.
pub(crate) async fn run(args: SeedArgs) -> Result<(), Box<dyn Error>> {
    let lock = Lock::load(&args.paths.lock_path())?;
    let target_registry = std::env::var(ENV_REGISTRY).map_err(|_| {
        format!("{ENV_REGISTRY} is not set: it names the target registry the controlled source sits beside")
    })?;
    let controlled = Controlled::beside(&target_registry, &args.source_region)?;

    let client = ecr::client_in(controlled.region().as_str()).await;
    ecr::validate_credentials(&client).await?;
    let images = seed_images(&lock);
    eprintln!(
        "bench-seed: {} refs of {} images, lock written at {}, into {}",
        lock.images.len(),
        images.len(),
        lock.locked_at,
        controlled.region(),
    );

    let workspace_root = Path::new(".");
    eprintln!("bench-seed: building ocync in release mode...");
    runner::build_ocync(workspace_root).await?;
    let version = runner::check_tool(Tool::Ocync, workspace_root).await?;
    eprintln!("  ocync: {version}");

    let dockerhub_auth: Option<DockerHubAuth> = docker_hub_auth();
    let seed = config_gen::Seed {
        controlled: &controlled,
        images: &images,
        dockerhub_auth: dockerhub_auth.as_ref(),
    };
    let http = registry::http_client()?;
    let served = Source::Controlled(controlled.clone());
    let check = lock::SourceCheck::new(&http, None, &served, args.paths.corpus()).await?;
    let refs = lock.images.keys().cloned().collect();
    let config_dir = tempfile::tempdir()?;
    seed_with(
        &seed,
        &lock,
        async |repo| ecr::create_repo(&client, repo).await,
        async |config| {
            let path = config_dir.path().join("seed.yaml");
            write_file(&path, config, 0o600).map_err(|e| e.to_string())?;
            let run =
                runner::run_tool(Tool::Ocync, "seed", &path, None, workspace_root, false).await?;
            Ok(run.exit_status)
        },
        async || check.ensure_served(&lock, &refs).await,
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
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    use futures_util::FutureExt;

    use super::*;
    use crate::bench::lock::fixtures::{entry, lock_of};

    fn controlled() -> Controlled {
        Controlled::beside(
            "111111111111.dkr.ecr.us-east-2.amazonaws.com",
            &Region::default(),
        )
        .unwrap()
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
        let images = seed_images(&lock);
        let config = config_gen::seed_config(
            &config_gen::Seed {
                controlled: &controlled,
                images: &images,
                dockerhub_auth: None,
            },
            &lock,
        );
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

    /// What a seed did, in order.
    type Log = RefCell<Vec<String>>;

    /// A seed whose steps record themselves in `log`, with ocync exiting as
    /// `exit` and the check finding `missing` unserved.
    fn seeded(log: &Log, exit: i32, missing: Option<&str>) -> Result<(), String> {
        let (lock, controlled) = (lock(), controlled());
        let images = seed_images(&lock);
        seed_with(
            &config_gen::Seed {
                controlled: &controlled,
                images: &images,
                dockerhub_auth: None,
            },
            &lock,
            async |repo| {
                log.borrow_mut().push(format!("repo {repo}"));
                Ok(RepoState::Created)
            },
            async |_config| {
                log.borrow_mut().push("copy".into());
                Ok(ExitStatus::from_raw(exit << 8))
            },
            async || {
                log.borrow_mut().push("verify".into());
                missing.map_or(Ok(()), |text| Err(NotServed::Incomplete(text.into())))
            },
        )
        .now_or_never()
        .unwrap()
        .map_err(|e| e.to_string())
    }

    #[test]
    fn repositories_are_made_before_the_copy_and_the_digests_checked_after_it() {
        // Catches: a copy into repositories that are not there yet, and a seed
        // that is done without a run's own check.
        let log = Log::default();
        seeded(&log, 0, None).unwrap();
        assert_eq!(
            *log.borrow(),
            [
                "repo bench-source/chainguard/static",
                "repo bench-source/library/alpine",
                "copy",
                "verify"
            ]
        );
    }

    #[test]
    fn a_copy_that_fails_is_reported_and_not_checked() {
        // Catches: a failed ocync exit passing because the digests that
        // happened to be there were served, and the check's answer replacing
        // the copy's.
        let log = Log::default();
        let error = seeded(&log, 1, None).unwrap_err();
        assert!(error.contains("did not finish the copy"), "{error}");
        assert_eq!(log.borrow().last().map(String::as_str), Some("copy"));
        assert!(!log.borrow().iter().any(|step| step == "verify"));
    }

    #[test]
    fn a_digest_the_source_does_not_serve_after_the_copy_fails_the_seed() {
        // Catches: a copy that exited clean without delivering a locked digest.
        let log = Log::default();
        let error = seeded(&log, 0, Some("gone")).unwrap_err();
        assert!(error.contains("gone"), "{error}");
        assert_eq!(log.borrow().last().map(String::as_str), Some("verify"));
    }
}
