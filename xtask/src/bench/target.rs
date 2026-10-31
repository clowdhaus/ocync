//! Where a run syncs to: the target parsed from the environment, the clients
//! it needs, and the check that what a tool synced is what the lock says.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use aws_sdk_ecr::Client;
use clap::ValueEnum;
use ocync_distribution::{Digest, RegistryClient};

use crate::bench::corpus::Corpus;
use crate::bench::gar::{self, ArtifactRegistryTarget, ENV_CREDENTIALS};
use crate::bench::lock::{Entry, ImageRef, Lock};
use crate::bench::report::ScenarioKind;
use crate::bench::runner::Tool;
use crate::bench::{Rounds, Scenario, ecr, value_name};

/// Environment variable holding the registry host, for every target.
pub(crate) const ENV_REGISTRY: &str = "BENCH_TARGET_REGISTRY";

/// The registry a run syncs into, as `--target` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Kind {
    /// Amazon ECR, in the account the instance runs in.
    Ecr,
    /// Google Artifact Registry, in the project `bench/terraform/gcp` names.
    ArtifactRegistry,
}

impl Kind {
    /// The `auth_type` ocync's config signs in to this registry with.
    pub(crate) fn auth_type(self) -> &'static str {
        match self {
            Self::Ecr => "ecr",
            Self::ArtifactRegistry => "gar",
        }
    }
}

/// The registry a run syncs into, with what the instance's environment says
/// about it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Amazon ECR, in the account the instance runs in.
    Ecr,
    /// Google Artifact Registry, one repository per round.
    ArtifactRegistry(ArtifactRegistryTarget),
}

/// Fails unless a run of `scenario` with `tools` can sync into Artifact
/// Registry. Shared by the instance and by `bench-remote`, which refuses
/// before it creates anything. `regression` is whether the run compares with
/// the stored baseline, which `bench-remote` never asks for.
pub(crate) fn ensure_artifact_registry_applies(
    scenario: Scenario,
    tools: &[Tool],
    regression: bool,
) -> Result<(), String> {
    if regression {
        return Err(
            "--regression cannot run against Artifact Registry: the one stored baseline is ECR's"
                .to_owned(),
        );
    }
    if !matches!(scenario, Scenario::Sync) {
        return Err(format!(
            "the {} scenario cannot sync into Artifact Registry yet; only sync can",
            value_name(&scenario)
        ));
    }
    if let Some(tool) = tools
        .iter()
        .find(|tool| !matches!(tool, Tool::Ocync | Tool::OcyncBaseline))
    {
        return Err(format!(
            "{tool} cannot sync into Artifact Registry: its config hardcodes ECR credentials"
        ));
    }
    if tools.len() != 1 {
        return Err(format!(
            "{} tools cannot share one Artifact Registry repository per round; run one tool",
            tools.len()
        ));
    }
    Ok(())
}

impl Target {
    /// Reads the `kind` of target from `env`, which an ECR run does not read.
    /// Refuses a run that Artifact Registry cannot serve before anything is
    /// built or pulled.
    pub(crate) fn from_env(
        kind: Kind,
        env: &dyn Fn(&str) -> Option<String>,
        scenario: Scenario,
        tools: &[Tool],
        regression: bool,
        rounds: Rounds,
    ) -> Result<Self, String> {
        match kind {
            Kind::Ecr => Ok(Self::Ecr),
            Kind::ArtifactRegistry => {
                ensure_artifact_registry_applies(scenario, tools, regression)?;
                gar::require(env, ENV_CREDENTIALS)?;
                ArtifactRegistryTarget::parse(env, rounds).map(Self::ArtifactRegistry)
            }
        }
    }

    /// `corpus` as `round` (from 1) of the sync scenario syncs it: Artifact
    /// Registry gives each round a repository of its own, so no round meets
    /// blobs an earlier one left behind.
    pub(crate) fn round_corpus<'a>(&self, corpus: &'a Corpus, round: usize) -> Cow<'a, Corpus> {
        match self {
            Self::Ecr => Cow::Borrowed(corpus),
            Self::ArtifactRegistry(target) => {
                let mut corpus = corpus.clone();
                corpus.settings.target_prefix =
                    target.target_prefix(round, &corpus.settings.target_prefix);
                Cow::Owned(corpus)
            }
        }
    }
}

/// The client a run reads its target through.
pub(crate) enum Store {
    /// ECR, which also creates and deletes the target repositories.
    Ecr(Client),
    /// Artifact Registry, whose repositories `bench-remote` creates.
    ArtifactRegistry(Box<RegistryClient>),
}

impl Store {
    /// Opens the client `target` needs and checks its credentials.
    pub(crate) async fn open(target: &Target) -> Result<Self, String> {
        match target {
            Target::Ecr => {
                let client = ecr::client().await;
                ecr::validate_credentials(&client).await?;
                Ok(Self::Ecr(client))
            }
            Target::ArtifactRegistry(target) => {
                let client = gar::client(&target.location.host()).await?;
                gar::check_sign_in(&client, target).await?;
                Ok(Self::ArtifactRegistry(Box::new(client)))
            }
        }
    }
}

/// The target registry and the lock its content is checked against.
pub(crate) struct Destination<'a> {
    /// The target registry's client.
    pub(crate) store: &'a Store,
    /// Digests every synced tag must have.
    pub(crate) lock: &'a Lock,
}

impl Destination<'_> {
    /// Leaves the target empty for `corpus`. ECR repositories are deleted and
    /// recreated, since one that exists may hold an aborted run's data;
    /// Artifact Registry's are new, made for the round.
    pub(crate) async fn reset(&self, corpus: &Corpus) -> Result<(), String> {
        if let Store::Ecr(client) = self.store {
            ecr::delete_repos(client, corpus).await?;
            ecr::create_repos(client, corpus).await?;
        }
        Ok(())
    }

    /// Removes what a pass synced, where the run removes it itself. A
    /// repository that cannot be deleted fails the pass, so its content cannot
    /// satisfy the next tool's digest check.
    pub(crate) async fn release(&self, corpus: &Corpus) -> Result<(), String> {
        match self.store {
            Store::Ecr(client) => ecr::delete_repos(client, corpus).await,
            Store::ArtifactRegistry(_) => Ok(()),
        }
    }

    /// What `repo` holds of the `expected` tags.
    async fn list(&self, repo: &str, expected: &[(&str, &Entry)]) -> Result<RepoListing, String> {
        match self.store {
            Store::Ecr(client) => ecr::list_repo(client, repo).await,
            Store::ArtifactRegistry(client) => gar::list_repo(client, repo, expected).await,
        }
    }
}

/// What a target repository holds, tagged and untagged.
///
/// A multi-arch push stores each child manifest untagged, so the digests of
/// the children are only in the untagged entries.
#[derive(Debug, Default)]
pub(crate) struct RepoListing {
    /// Digest each tag names.
    tags: HashMap<String, Digest>,
    /// Digest of every image in the repo, tagged or not.
    digests: HashSet<Digest>,
}

impl RepoListing {
    /// Record one image of the repo.
    pub(crate) fn insert(&mut self, tag: Option<&str>, digest: Digest) {
        if let Some(tag) = tag {
            self.tags.insert(tag.to_owned(), digest.clone());
        }
        self.digests.insert(digest);
    }
}

/// What is wrong with a tag in a target repository.
#[derive(Debug, PartialEq, Eq)]
enum Fault {
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
    kind: Fault,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { repo, tag, kind } = self;
        match kind {
            Fault::TagAbsent { expected } => {
                write!(f, "{repo}:{tag}: expected {expected}, tag absent")
            }
            Fault::TagDigest { expected, actual } => {
                write!(f, "{repo}:{tag}: expected {expected}, found {actual}")
            }
            Fault::ChildAbsent { child } => {
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
            None => mismatches.push(mismatch(Fault::TagAbsent {
                expected: locked.digest.clone(),
            })),
            Some(actual) if *actual != locked.digest => {
                mismatches.push(mismatch(Fault::TagDigest {
                    expected: locked.digest.clone(),
                    actual: actual.clone(),
                }))
            }
            Some(_) => {
                for child in &locked.children {
                    if !listing.digests.contains(child) {
                        mismatches.push(mismatch(Fault::ChildAbsent {
                            child: child.clone(),
                        }));
                    }
                }
            }
        }
    }
    mismatches
}

/// Verify that every (image, tag) in the corpus is at the target with the
/// locked digest, and that every locked index child is there too.
///
/// This runs after each sync pass, so a tool that converted a format, dropped
/// a platform or transferred nothing fails the run instead of being credited
/// with a fast time.
pub(crate) async fn verify_sync(
    target: &Destination<'_>,
    corpus: &Corpus,
    tool: &str,
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

        let listing = match target.list(&repo_name, &expected).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::gar::{ENV_PROJECT, ENV_REPOSITORIES, env_of, rounds};
    use crate::bench::lock::fixtures::{digest as d, entry as locked};

    fn listing(entries: &[(Option<&str>, char)]) -> RepoListing {
        let mut listing = RepoListing::default();
        for (tag, digest) in entries {
            listing.insert(*tag, d(*digest));
        }
        listing
    }

    fn mismatch(tag: &str, kind: Fault) -> Mismatch {
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
                    Fault::TagDigest {
                        expected: d('4'),
                        actual: d('9'),
                    },
                )],
            ),
            (
                "index missing a child",
                listing(&[(Some("latest"), '1'), (None, '2'), (Some("v1"), '4')]),
                vec![mismatch("latest", Fault::ChildAbsent { child: d('3') })],
            ),
            (
                "tag absent",
                listing(&[(Some("latest"), '1'), (None, '2'), (None, '3')]),
                vec![mismatch("v1", Fault::TagAbsent { expected: d('4') })],
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
            Fault::TagDigest {
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

    const GAR_ENV: [(&str, &str); 4] = [
        (ENV_REGISTRY, "europe-west9-docker.pkg.dev"),
        (ENV_PROJECT, "example-project"),
        (ENV_REPOSITORIES, "run-r1,run-r2,run-r3"),
        (ENV_CREDENTIALS, "/home/bench/creds.json"),
    ];

    fn parse(
        kind: Kind,
        pairs: &[(&str, &str)],
        scenario: Scenario,
        tools: &[Tool],
        asked: &str,
    ) -> Result<Target, String> {
        Target::from_env(kind, &env_of(pairs), scenario, tools, false, rounds(asked))
    }

    #[test]
    fn an_ecr_run_reads_none_of_the_artifact_registry_names() {
        // Catches: Artifact Registry chosen, or a plain ECR run refused, for
        // the sake of a variable it does not use.
        let target = parse(
            Kind::Ecr,
            &[(ENV_PROJECT, "Not A Project")],
            Scenario::All,
            &[Tool::Ocync, Tool::Dregsy],
            "3",
        );
        assert_eq!(target, Ok(Target::Ecr));
    }

    #[test]
    fn an_artifact_registry_run_is_refused_by_name_before_anything_is_built() {
        let without = |name: &str| -> Vec<(&str, &str)> {
            GAR_ENV
                .iter()
                .filter(|(k, _)| *k != name)
                .copied()
                .collect()
        };
        for (name, pairs, scenario, tools, error) in [
            (
                "no credentials file",
                without(ENV_CREDENTIALS),
                Scenario::Sync,
                vec![Tool::Ocync],
                "GOOGLE_APPLICATION_CREDENTIALS is not set",
            ),
            (
                "no project",
                without(ENV_PROJECT),
                Scenario::Sync,
                vec![Tool::Ocync],
                "BENCH_GAR_PROJECT is not set",
            ),
            (
                "partial scenario",
                GAR_ENV.to_vec(),
                Scenario::Partial,
                vec![Tool::Ocync],
                "the partial scenario cannot sync into Artifact Registry yet; only sync can",
            ),
            (
                "all scenarios",
                GAR_ENV.to_vec(),
                Scenario::All,
                vec![Tool::Ocync],
                "the all scenario cannot sync into Artifact Registry yet; only sync can",
            ),
            (
                "competitor tool",
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Regsync],
                "regsync cannot sync into Artifact Registry: its config hardcodes ECR credentials",
            ),
            (
                "ocync beside a competitor",
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::Dregsy],
                "dregsy cannot sync into Artifact Registry",
            ),
            (
                "ocync beside its baseline",
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::OcyncBaseline],
                "2 tools cannot share one Artifact Registry repository per round; run one tool",
            ),
        ] {
            let err = parse(Kind::ArtifactRegistry, &pairs, scenario, &tools, "3").unwrap_err();
            assert!(err.contains(error), "{name}: {err}");
        }
        assert!(matches!(
            parse(
                Kind::ArtifactRegistry,
                &GAR_ENV,
                Scenario::Sync,
                &[Tool::Ocync],
                "3"
            ),
            Ok(Target::ArtifactRegistry(_))
        ));
    }

    #[test]
    fn regression_is_refused_with_artifact_registry_and_still_allowed_with_ecr() {
        // Catches: a Google run compared with, or saved as, ECR's one baseline.
        let from_env = |kind, regression| {
            Target::from_env(
                kind,
                &env_of(&GAR_ENV),
                Scenario::Sync,
                &[Tool::Ocync],
                regression,
                rounds("3"),
            )
        };
        let err = from_env(Kind::ArtifactRegistry, true).unwrap_err();
        assert!(
            err.starts_with("--regression cannot run against Artifact Registry"),
            "{err}"
        );
        assert_eq!(from_env(Kind::Ecr, true), Ok(Target::Ecr));
    }

    #[test]
    fn round_n_syncs_into_repository_n_and_ecr_keeps_one_corpus() {
        let corpus: Corpus = serde_yaml::from_str(
            r#"
corpus: { target_registry: "europe-west9-docker.pkg.dev", target_prefix: "bench" }
images:
  - source: "docker.io/library/alpine"
    tags: ["3.20"]
"#,
        )
        .unwrap();
        let gar = parse(
            Kind::ArtifactRegistry,
            &GAR_ENV,
            Scenario::Sync,
            &[Tool::Ocync],
            "3",
        )
        .unwrap();
        for (round, repo) in [(1, "run-r1"), (2, "run-r2"), (3, "run-r3")] {
            let round_corpus = gar.round_corpus(&corpus, round);
            assert_eq!(
                round_corpus.target_repo("docker.io/library/alpine"),
                format!("example-project/{repo}/bench/library-alpine"),
                "round {round}"
            );
            // The host the proxy and the source-error check compare against
            // stays bare.
            assert_eq!(
                round_corpus.settings.target_registry,
                "europe-west9-docker.pkg.dev"
            );
        }
        let ecr = Target::Ecr.round_corpus(&corpus, 2);
        assert_eq!(
            ecr.target_repo("docker.io/library/alpine"),
            "bench/library-alpine"
        );
    }
}
