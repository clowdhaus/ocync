//! Where a run syncs to: the target parsed from the environment, the clients
//! it needs, and the check that what a tool synced is what the lock says.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use aws_sdk_ecr::Client;
use clap::ValueEnum;
use ocync_distribution::auth::AuthProvider;
use ocync_distribution::{Digest, RegistryClient, RepositoryName};

use crate::bench::acr::{self, AcrTarget, ENV_TOKEN_FILE};
use crate::bench::corpus::Corpus;
use crate::bench::gar::{self, ArtifactRegistryTarget, ENV_CREDENTIALS};
use crate::bench::lock::{Entry, Lock};
use crate::bench::provision::RoundIds;
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
    /// Azure Container Registry, the one `bench/terraform/azure` names.
    Acr,
}

impl Kind {
    /// The `auth_type` ocync's config signs in to this registry with.
    pub(crate) fn auth_type(self) -> &'static str {
        match self {
            Self::Ecr => "ecr",
            Self::ArtifactRegistry => "gar",
            Self::Acr => "acr",
        }
    }

    /// The hosts ocync signs in through to reach this registry, which are not
    /// the registry's own and not a source's.
    pub(crate) fn sign_in_hosts(self) -> &'static [&'static str] {
        match self {
            Self::Ecr => &[],
            Self::ArtifactRegistry => gar::SIGN_IN_HOSTS,
            Self::Acr => acr::SIGN_IN_HOSTS,
        }
    }

    /// What a message calls this registry.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Ecr => "ECR",
            Self::ArtifactRegistry => "Artifact Registry",
            Self::Acr => "ACR",
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
    /// Azure Container Registry, one repository path prefix per round.
    Acr(AcrTarget),
}

/// Fails unless a run of `scenario` with `tools` can sync into `kind`. ECR
/// takes any run; the other registries have repositories this machine
/// provisions. Shared by the instance and by `bench-remote`, which refuses
/// before it creates anything.
pub(crate) fn ensure_target_applies(
    kind: Kind,
    scenario: Scenario,
    tools: &[Tool],
) -> Result<(), String> {
    if kind == Kind::Ecr {
        return Ok(());
    }
    let name = kind.name();
    if !matches!(scenario, Scenario::Sync) {
        return Err(format!(
            "the {} scenario cannot sync into {name} yet; only sync can",
            value_name(&scenario)
        ));
    }
    if let Some(tool) = tools
        .iter()
        .find(|tool| !matches!(tool, Tool::Ocync | Tool::OcyncBaseline))
    {
        return Err(format!(
            "{tool} cannot sync into {name}: its config hardcodes ECR credentials"
        ));
    }
    if tools.len() != 1 {
        return Err(format!(
            "{} tools cannot share one {name} repository per round; run one tool",
            tools.len()
        ));
    }
    Ok(())
}

/// The value of a variable that must be set and not empty.
pub(crate) fn require(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    env(name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

/// The ids in the variable `name`, comma-separated in round order, which must
/// hold one for each of `rounds`.
pub(crate) fn round_ids(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    rounds: Rounds,
) -> Result<RoundIds, String> {
    RoundIds::parse(&require(env, name)?, rounds).map_err(|e| format!("{name}: {e}"))
}

/// Refuses a run that `kind`, a registry other than ECR, cannot serve. A
/// `--regression` run is among them: the one stored baseline is ECR's.
fn refuse_unservable(
    kind: Kind,
    scenario: Scenario,
    tools: &[Tool],
    regression: bool,
) -> Result<(), String> {
    if regression {
        return Err(format!(
            "--regression cannot run against {}: the one stored baseline is ECR's",
            kind.name()
        ));
    }
    ensure_target_applies(kind, scenario, tools)
}

impl Target {
    /// Reads the `kind` of target from `env`, which an ECR run does not read.
    /// Refuses a run that Artifact Registry or ACR cannot serve before
    /// anything is built or pulled.
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
                refuse_unservable(kind, scenario, tools, regression)?;
                require(env, ENV_CREDENTIALS)?;
                ArtifactRegistryTarget::parse(env, rounds).map(Self::ArtifactRegistry)
            }
            Kind::Acr => {
                refuse_unservable(kind, scenario, tools, regression)?;
                require(env, ENV_TOKEN_FILE)?;
                AcrTarget::parse(env, rounds).map(Self::Acr)
            }
        }
    }

    /// The registry this target is.
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Ecr => Kind::Ecr,
            Self::ArtifactRegistry(_) => Kind::ArtifactRegistry,
            Self::Acr(_) => Kind::Acr,
        }
    }

    /// The target prefix of `round` (from 1): the round's own repository or
    /// path prefix, then the corpus's `prefix` level, so repository names match
    /// ECR's below it. `None` for ECR, which gives no round a prefix of its
    /// own, and for a round the run does not have.
    pub(crate) fn target_prefix(&self, round: usize, prefix: &str) -> Option<String> {
        match self {
            Self::Ecr => None,
            Self::ArtifactRegistry(target) => {
                let repository = target.repositories.get(round)?;
                Some(format!("{}/{repository}/{prefix}", target.project))
            }
            Self::Acr(target) => {
                let round_prefix = target.prefixes.get(round)?;
                Some(format!("{round_prefix}/{prefix}"))
            }
        }
    }

    /// The repository the sign-in check asks in: round 1's, `None` for ECR,
    /// which has no such check.
    pub(crate) fn sign_in_repo(&self) -> Option<String> {
        self.target_prefix(1, SIGN_IN_REPO)
    }

    /// `corpus` as `round` (from 1) of the sync scenario syncs it: Artifact
    /// Registry and ACR give each round repositories of its own, so no round
    /// meets blobs an earlier one left behind.
    pub(crate) fn round_corpus<'a>(
        &self,
        corpus: &'a Corpus,
        round: usize,
    ) -> Result<Cow<'a, Corpus>, String> {
        let Some(target_prefix) = self.target_prefix(round, &corpus.settings.target_prefix) else {
            return match self {
                Self::Ecr => Ok(Cow::Borrowed(corpus)),
                _ => Err(format!(
                    "round {round} has no {} repository: the run has fewer rounds",
                    self.kind().name()
                )),
            };
        };
        let mut corpus = corpus.clone();
        corpus.settings.target_prefix = target_prefix;
        Ok(Cow::Owned(corpus))
    }
}

/// The client a run reads its target through.
pub(crate) enum Store {
    /// ECR, which also creates and deletes the target repositories.
    Ecr(Client),
    /// Artifact Registry or ACR, whose repositories `bench-remote` makes or
    /// deletes, read through ocync's own sign-in.
    Distribution(Box<RegistryClient>),
}

impl Store {
    /// Opens the client `target` needs and checks its credentials.
    pub(crate) async fn open(target: &Target) -> Result<Self, String> {
        let client = match target {
            Target::Ecr => {
                let client = ecr::client().await;
                ecr::validate_credentials(&client).await?;
                return Ok(Self::Ecr(client));
            }
            Target::ArtifactRegistry(target) => gar::client(&target.location.host()).await?,
            Target::Acr(target) => acr::client(&target.registry.host()).await?,
        };
        let kind = target.kind();
        let repo = target.sign_in_repo().ok_or_else(|| {
            format!(
                "{} sign-in check: the run has no round 1 repository",
                kind.name()
            )
        })?;
        check_sign_in(&client, kind, &repo).await?;
        Ok(Self::Distribution(Box::new(client)))
    }
}

/// A client for `host` that signs in with the auth `sign_in` makes for the
/// registry's URL. The registries differ only in that auth.
pub(crate) async fn registry_client<A: AuthProvider + 'static>(
    host: &str,
    sign_in: impl AsyncFnOnce(&reqwest::Url) -> Result<A, String>,
) -> Result<RegistryClient, String> {
    ocync_distribution::install_crypto_provider();
    let url = reqwest::Url::parse(&format!("https://{host}"))
        .map_err(|e| format!("registry host {host}: {e}"))?;
    let auth = sign_in(&url).await?;
    RegistryClient::builder(url)
        .auth(auth)
        .build()
        .map_err(|e| format!("registry client for {host}: {e}"))
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
    /// Artifact Registry's and ACR's are new, made for the round.
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
            Store::Distribution(_) => Ok(()),
        }
    }

    /// What `repo` holds of the `expected` tags.
    async fn list(&self, repo: &str, expected: &[(&str, &Entry)]) -> Result<RepoListing, String> {
        match self.store {
            Store::Ecr(client) => ecr::list_repo(client, repo).await,
            Store::Distribution(client) => list_by_head(client, repo, expected).await,
        }
    }
}

/// A tag no image carries, so a signed-in client is answered "absent".
const SIGN_IN_PROBE_TAG: &str = "ocync-bench-sign-in-probe";

/// The corpus level under a round's first prefix that the sign-in check asks
/// in.
const SIGN_IN_REPO: &str = "sign-in-probe";

/// Sends one authenticated request, so a broken federation or a missing grant
/// fails before the build and the pre-warm rather than after them. An absent
/// tag in `repo` answers 404: Artifact Registry's repository exists by now,
/// and ACR's is made by a first push, so it may not.
async fn check_sign_in(client: &RegistryClient, kind: Kind, repo: &str) -> Result<(), String> {
    let name = RepositoryName::new(repo).map_err(|e| format!("{repo}: {e}"))?;
    client
        .manifest_head(&name, SIGN_IN_PROBE_TAG)
        .await
        .map(drop)
        .map_err(|e| format!("{} sign-in check against {repo} failed: {e}", kind.name()))
}

/// What `repo` holds of the `expected` tags and, for a tag at its locked
/// digest, of the index children the lock lists. The registry API lists tags
/// only, so each child is asked for by digest.
async fn list_by_head(
    client: &RegistryClient,
    repo: &str,
    expected: &[(&str, &Entry)],
) -> Result<RepoListing, String> {
    let name = &RepositoryName::new(repo).map_err(|e| format!("{repo}: {e}"))?;
    let mut listing = RepoListing::default();
    for (tag, locked) in expected {
        let head = |reference: String| async move {
            client
                .manifest_head(name, &reference)
                .await
                .map_err(|e| format!("{repo}: HEAD {reference} failed ({e})"))
        };
        let Some(digest) = head((*tag).to_owned()).await? else {
            continue;
        };
        let locked_digest = digest == locked.digest;
        listing.insert(Some(*tag), digest);
        if locked_digest {
            for child in &locked.children {
                if head(child.to_string()).await?.is_some() {
                    listing.insert(None, child.clone());
                }
            }
        }
    }
    Ok(listing)
}

/// A lookup over `pairs`, standing in for the process environment.
#[cfg(test)]
pub(super) fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

/// `n` rounds, as the command line spells them.
#[cfg(test)]
pub(super) fn rounds(n: &str) -> Rounds {
    n.parse().unwrap()
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
            match target.lock.entry(&img.source, tag) {
                Ok(entry) => expected.push((tag, entry)),
                Err(problem) => problems.push(problem),
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
    use crate::bench::acr::{self, ENV_TENANT, FULL_ENV};
    use crate::bench::gar::{ENV_PROJECT, ENV_REPOSITORIES};
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
    fn an_ecr_run_reads_none_of_the_artifact_registry_or_acr_names() {
        // Catches: Artifact Registry or ACR chosen, or a plain ECR run
        // refused, for the sake of a variable it does not use.
        let target = parse(
            Kind::Ecr,
            &[(ENV_PROJECT, "Not A Project"), (ENV_TENANT, "contoso")],
            Scenario::All,
            &[Tool::Ocync, Tool::Dregsy],
            "3",
        );
        assert_eq!(target, Ok(Target::Ecr));
        // Catches: `bench-remote` refusing an ECR run for the sake of the
        // checks that only the other registries need.
        assert_eq!(
            ensure_target_applies(Kind::Ecr, Scenario::All, &[Tool::Ocync, Tool::Dregsy]),
            Ok(())
        );
    }

    #[test]
    fn a_provisioned_registry_run_is_refused_by_name_before_anything_is_built() {
        let without = |env: &[(&'static str, &'static str)], name: &str| -> Vec<(&str, &str)> {
            env.iter().filter(|(k, _)| *k != name).copied().collect()
        };
        for (name, kind, pairs, scenario, tools, error) in [
            (
                "no credentials file",
                Kind::ArtifactRegistry,
                without(&GAR_ENV, ENV_CREDENTIALS),
                Scenario::Sync,
                vec![Tool::Ocync],
                "GOOGLE_APPLICATION_CREDENTIALS is not set",
            ),
            (
                "no project",
                Kind::ArtifactRegistry,
                without(&GAR_ENV, ENV_PROJECT),
                Scenario::Sync,
                vec![Tool::Ocync],
                "BENCH_GAR_PROJECT is not set",
            ),
            (
                "partial scenario",
                Kind::ArtifactRegistry,
                GAR_ENV.to_vec(),
                Scenario::Partial,
                vec![Tool::Ocync],
                "the partial scenario cannot sync into Artifact Registry yet; only sync can",
            ),
            (
                "all scenarios",
                Kind::ArtifactRegistry,
                GAR_ENV.to_vec(),
                Scenario::All,
                vec![Tool::Ocync],
                "the all scenario cannot sync into Artifact Registry yet; only sync can",
            ),
            (
                "competitor tool",
                Kind::ArtifactRegistry,
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Regsync],
                "regsync cannot sync into Artifact Registry: its config hardcodes ECR credentials",
            ),
            (
                "ocync beside a competitor",
                Kind::ArtifactRegistry,
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::Dregsy],
                "dregsy cannot sync into Artifact Registry",
            ),
            (
                "ocync beside its baseline",
                Kind::ArtifactRegistry,
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::OcyncBaseline],
                "2 tools cannot share one Artifact Registry repository per round; run one tool",
            ),
            (
                "ACR without a token file",
                Kind::Acr,
                without(&FULL_ENV, ENV_TOKEN_FILE),
                Scenario::Sync,
                vec![Tool::Ocync],
                "AZURE_FEDERATED_TOKEN_FILE is not set",
            ),
            (
                "ACR without a tenant",
                Kind::Acr,
                without(&FULL_ENV, ENV_TENANT),
                Scenario::Sync,
                vec![Tool::Ocync],
                "AZURE_TENANT_ID is not set",
            ),
            (
                "ACR without a client",
                Kind::Acr,
                without(&FULL_ENV, acr::ENV_CLIENT),
                Scenario::Sync,
                vec![Tool::Ocync],
                "AZURE_CLIENT_ID is not set",
            ),
            (
                "ACR without prefixes",
                Kind::Acr,
                without(&FULL_ENV, acr::ENV_PREFIXES),
                Scenario::Sync,
                vec![Tool::Ocync],
                "BENCH_ACR_PREFIXES is not set",
            ),
            (
                "ACR with Artifact Registry's variables only",
                Kind::Acr,
                GAR_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync],
                "AZURE_FEDERATED_TOKEN_FILE is not set",
            ),
            (
                "ACR partial scenario",
                Kind::Acr,
                FULL_ENV.to_vec(),
                Scenario::Partial,
                vec![Tool::Ocync],
                "the partial scenario cannot sync into ACR yet; only sync can",
            ),
            (
                "ACR competitor tool",
                Kind::Acr,
                FULL_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Regsync],
                "regsync cannot sync into ACR: its config hardcodes ECR credentials",
            ),
            (
                "ACR ocync beside its baseline",
                Kind::Acr,
                FULL_ENV.to_vec(),
                Scenario::Sync,
                vec![Tool::Ocync, Tool::OcyncBaseline],
                "2 tools cannot share one ACR repository per round; run one tool",
            ),
        ] {
            let err = parse(kind, &pairs, scenario, &tools, "3").unwrap_err();
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
        assert!(matches!(
            parse(Kind::Acr, &FULL_ENV, Scenario::Sync, &[Tool::Ocync], "3"),
            Ok(Target::Acr(_))
        ));
    }

    #[test]
    fn regression_is_refused_with_artifact_registry_and_acr_and_still_allowed_with_ecr() {
        // Catches: a Google or Azure run compared with, or saved as, ECR's one
        // baseline.
        let from_env = |kind, pairs: &[(&str, &str)], regression| {
            Target::from_env(
                kind,
                &env_of(pairs),
                Scenario::Sync,
                &[Tool::Ocync],
                regression,
                rounds("3"),
            )
        };
        for (kind, pairs, name) in [
            (Kind::ArtifactRegistry, &GAR_ENV[..], "Artifact Registry"),
            (Kind::Acr, &FULL_ENV[..], "ACR"),
        ] {
            let err = from_env(kind, pairs, true).unwrap_err();
            assert_eq!(
                err,
                format!("--regression cannot run against {name}: the one stored baseline is ECR's")
            );
        }
        assert_eq!(from_env(Kind::Ecr, &GAR_ENV, true), Ok(Target::Ecr));
    }

    #[test]
    fn round_n_syncs_into_the_prefix_of_round_n_and_ecr_keeps_one_corpus() {
        let corpus = |host: &str| -> Corpus {
            serde_yaml::from_str(&format!(
                r#"
corpus: {{ target_registry: "{host}", target_prefix: "bench" }}
images:
  - source: "docker.io/library/alpine"
    tags: ["3.20"]
"#
            ))
            .unwrap()
        };
        let gar = parse(
            Kind::ArtifactRegistry,
            &GAR_ENV,
            Scenario::Sync,
            &[Tool::Ocync],
            "3",
        )
        .unwrap();
        let acr = parse(Kind::Acr, &FULL_ENV, Scenario::Sync, &[Tool::Ocync], "3").unwrap();
        for (target, host, below) in [
            (&gar, "europe-west9-docker.pkg.dev", "example-project/"),
            (&acr, "ocyncbench1a2b3c.azurecr.io", ""),
        ] {
            let corpus = corpus(host);
            for round in 1..=3 {
                let round_corpus = target.round_corpus(&corpus, round).unwrap();
                assert_eq!(
                    round_corpus.target_repo("docker.io/library/alpine"),
                    format!("{below}run-r{round}/bench/library-alpine"),
                    "{host} round {round}"
                );
                // The host the proxy and the source-error check compare
                // against stays bare.
                assert_eq!(round_corpus.settings.target_registry, host);
            }
        }
        let one = corpus("x.example");
        let ecr = Target::Ecr.round_corpus(&one, 2).unwrap();
        assert_eq!(
            ecr.target_repo("docker.io/library/alpine"),
            "bench/library-alpine"
        );

        // Catches: a round the run does not have syncing into another's
        // repositories, or into ECR's layout, and a panic for round 0.
        for (target, name) in [(&gar, "Artifact Registry"), (&acr, "ACR")] {
            for round in [0, 4] {
                let err = target.round_corpus(&one, round).unwrap_err();
                assert_eq!(
                    err,
                    format!("round {round} has no {name} repository: the run has fewer rounds")
                );
            }
        }
    }

    /// Answers one request with `status`, and returns the request line it saw.
    fn serve_once(status: &str) -> (reqwest::Url, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let status = status.to_owned();
        let seen = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request_line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request_line)
                .unwrap();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            request_line
        });
        (url, seen)
    }

    #[test]
    fn the_sign_in_check_is_one_head_in_the_first_round_and_a_refusal_fails_it() {
        // Catches: a broken federation or a missing grant surfacing only after
        // the build, a check that sends nothing or the wrong request, and a
        // check made in a later round's repository.
        let gar = parse(
            Kind::ArtifactRegistry,
            &GAR_ENV,
            Scenario::Sync,
            &[Tool::Ocync],
            "3",
        )
        .unwrap();
        let acr = parse(Kind::Acr, &FULL_ENV, Scenario::Sync, &[Tool::Ocync], "3").unwrap();
        assert_eq!(Target::Ecr.sign_in_repo(), None, "ECR has no sign-in check");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let check = |target: &Target, status: &str| {
            let (url, seen) = serve_once(status);
            let client = RegistryClient::builder(url).build().unwrap();
            let repo = target.sign_in_repo().unwrap();
            let result = runtime.block_on(check_sign_in(&client, target.kind(), &repo));
            (result, seen.join().unwrap())
        };

        for (target, name, head) in [
            (
                &gar,
                "Artifact Registry",
                "HEAD /v2/example-project/run-r1/sign-in-probe/manifests/ocync-bench-sign-in-probe HTTP/1.1\r\n",
            ),
            (
                &acr,
                "ACR",
                "HEAD /v2/run-r1/sign-in-probe/manifests/ocync-bench-sign-in-probe HTTP/1.1\r\n",
            ),
        ] {
            let (result, request) = check(target, "404 Not Found");
            assert_eq!(result, Ok(()), "{name}");
            assert_eq!(request, head, "{name}");
            let (result, _) = check(target, "403 Forbidden");
            let err = result.unwrap_err();
            assert!(
                err.starts_with(&format!("{name} sign-in check against ")),
                "{err}"
            );
        }
    }
}
