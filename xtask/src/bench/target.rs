//! Where a run syncs to: the target parsed from the environment, the clients
//! it needs, and the check that what a tool synced is what the lock says.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use aws_sdk_ecr::Client;
use ocync_distribution::{Digest, RegistryClient};

use crate::bench::corpus::{Corpus, TargetAuth};
use crate::bench::gar::{self, ArtifactRegistryTarget, ENV_CREDENTIALS, ENV_PROJECT};
use crate::bench::lock::{Entry, ImageRef, Lock};
use crate::bench::report::ScenarioKind;
use crate::bench::runner::Tool;
use crate::bench::{Rounds, Scenario, ecr, value_name};

/// The registry a run syncs into, as the instance's environment names it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BenchTarget {
    /// Amazon ECR, in the account the instance runs in.
    Ecr,
    /// Google Artifact Registry, one repository per round.
    ArtifactRegistry(ArtifactRegistryTarget),
}

/// Fails unless a run of `scenario` with `tools` can sync into Artifact
/// Registry. Shared by the instance and by `bench-remote`, which refuses
/// before it creates anything.
pub(crate) fn ensure_artifact_registry_applies(
    scenario: Scenario,
    tools: &[Tool],
) -> Result<(), String> {
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

impl BenchTarget {
    /// Reads the target from `env`: Artifact Registry when the run names its
    /// project or repositories, ECR otherwise. Refuses a run that target
    /// cannot serve before anything is built or pulled.
    pub(crate) fn from_env(
        env: &dyn Fn(&str) -> Option<String>,
        scenario: Scenario,
        tools: &[Tool],
        rounds: Rounds,
    ) -> Result<Self, String> {
        if env(ENV_PROJECT).is_none() && env(gar::ENV_REPOSITORIES).is_none() {
            return Ok(Self::Ecr);
        }
        ensure_artifact_registry_applies(scenario, tools)?;
        gar::require(env, ENV_CREDENTIALS)?;
        ArtifactRegistryTarget::parse(env, rounds.get()).map(Self::ArtifactRegistry)
    }

    /// How ocync signs in to the target.
    pub(crate) fn auth(&self) -> TargetAuth {
        match self {
            Self::Ecr => TargetAuth::Ecr,
            Self::ArtifactRegistry(_) => TargetAuth::Gar,
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
    pub(crate) async fn open(target: &BenchTarget) -> Result<Self, String> {
        match target {
            BenchTarget::Ecr => {
                let client = ecr::client().await;
                ecr::validate_credentials(&client).await?;
                Ok(Self::Ecr(client))
            }
            BenchTarget::ArtifactRegistry(target) => gar::client(&target.host())
                .await
                .map(|client| Self::ArtifactRegistry(Box::new(client))),
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
    /// recreated; Artifact Registry's are new, made for the round.
    pub(crate) async fn reset(&self, corpus: &Corpus) -> Result<(), String> {
        if let Store::Ecr(client) = self.store {
            ecr::delete_repos(client, corpus).await?;
            ecr::create_repos(client, corpus).await?;
        }
        Ok(())
    }

    /// Removes what a pass synced, where the run removes it itself.
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
    use crate::bench::lock::fixtures::{digest as d, entry as locked};

    fn listing(entries: &[(Option<&str>, char)]) -> RepoListing {
        let mut listing = RepoListing::default();
        for (tag, digest) in entries {
            listing.insert(*tag, d(*digest));
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

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    const GAR_ENV: [(&str, &str); 4] = [
        ("BENCH_TARGET_REGISTRY", "europe-west9-docker.pkg.dev"),
        ("BENCH_GAR_PROJECT", "example-project"),
        ("BENCH_GAR_REPOSITORIES", "run-r1,run-r2,run-r3"),
        ("GOOGLE_APPLICATION_CREDENTIALS", "/home/bench/creds.json"),
    ];

    fn rounds(n: &str) -> Rounds {
        n.parse().unwrap()
    }

    fn parse(
        pairs: &[(&str, &str)],
        scenario: Scenario,
        tools: &[Tool],
        rounds: Rounds,
    ) -> Result<BenchTarget, String> {
        BenchTarget::from_env(&env_of(pairs), scenario, tools, rounds)
    }

    #[test]
    fn a_run_without_artifact_registry_names_is_an_ecr_run() {
        // Catches: Artifact Registry chosen, or refused, for a plain ECR run.
        let target = parse(
            &[(
                "BENCH_TARGET_REGISTRY",
                "111111111111.dkr.ecr.us-east-1.amazonaws.com",
            )],
            Scenario::All,
            &[Tool::Ocync, Tool::Dregsy],
            rounds("3"),
        );
        assert_eq!(target, Ok(BenchTarget::Ecr));
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
        for (name, pairs, scenario, tools, rounds_asked, error) in [
            (
                "no credentials file",
                without("GOOGLE_APPLICATION_CREDENTIALS"),
                Scenario::Sync,
                vec![Tool::Ocync],
                "3",
                "GOOGLE_APPLICATION_CREDENTIALS is not set",
            ),
            (
                "partial scenario",
                GAR_ENV.to_vec(),
                Scenario::Partial,
                vec![Tool::Ocync],
                "3",
                "the partial scenario cannot sync into Artifact Registry yet; only sync can",
            ),
            (
                "all scenarios",
                GAR_ENV.to_vec(),
                Scenario::All,
                vec![Tool::Ocync],
                "3",
                "the all scenario cannot sync into Artifact Registry yet; only sync can",
            ),
            (
                "competitor tool",
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Regsync],
                "3",
                "regsync cannot sync into Artifact Registry: its config hardcodes ECR credentials",
            ),
            (
                "ocync beside a competitor",
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::Dregsy],
                "3",
                "dregsy cannot sync into Artifact Registry",
            ),
            (
                "ocync beside its baseline",
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::OcyncBaseline],
                "3",
                "2 tools cannot share one Artifact Registry repository per round; run one tool",
            ),
        ] {
            let err = parse(&pairs, scenario, &tools, rounds(rounds_asked)).unwrap_err();
            assert!(err.contains(error), "{name}: {err}");
        }
        assert!(matches!(
            parse(&GAR_ENV, Scenario::Sync, &[Tool::Ocync], rounds("3")),
            Ok(BenchTarget::ArtifactRegistry(_))
        ));
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
        let gar = parse(&GAR_ENV, Scenario::Sync, &[Tool::Ocync], rounds("3")).unwrap();
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
        let ecr = BenchTarget::Ecr.round_corpus(&corpus, 2);
        assert_eq!(
            ecr.target_repo("docker.io/library/alpine"),
            "bench/library-alpine"
        );
        assert_eq!(gar.auth(), TargetAuth::Gar);
        assert_eq!(BenchTarget::Ecr.auth(), TargetAuth::Ecr);
    }
}
