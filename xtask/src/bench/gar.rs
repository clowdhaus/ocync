//! Google Artifact Registry as a benchmark target: the ids that name it, the
//! target the instance parses from its environment, and listing a repository
//! by manifest HEADs.

use std::fmt;
use std::ops::RangeInclusive;
use std::str::FromStr;

use ocync_distribution::auth::gcp::{GcpAuth, Residency, SharedGoogleToken};
use ocync_distribution::{AwsCredentialProviders, RegistryClient, RepositoryName};

use crate::bench::Rounds;
use crate::bench::lock::Entry;
use crate::bench::registry::http_client;
use crate::bench::target::{ENV_REGISTRY, RepoListing};

/// Environment variable holding the project the repositories are in.
pub(crate) const ENV_PROJECT: &str = "BENCH_GAR_PROJECT";
/// Environment variable holding the repository ids, comma-separated, in round
/// order.
pub(crate) const ENV_REPOSITORIES: &str = "BENCH_GAR_REPOSITORIES";
/// Environment variable naming the Google federation file ocync signs in with.
pub(crate) const ENV_CREDENTIALS: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// What follows the location in an Artifact Registry host.
const HOST_SUFFIX: &str = "-docker.pkg.dev";

/// Whether `s` is a lowercase letter, then lowercase letters, digits and
/// hyphens, ending in a letter or digit, within `len` characters.
fn is_lowercase_id(s: &str, len: RangeInclusive<usize>) -> bool {
    let mut chars = s.chars();
    len.contains(&s.len())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.ends_with('-')
}

/// Declares a string newtype that is parsed against a rule and read back as
/// text.
macro_rules! id {
    ($(#[$doc:meta])* $name:ident, $what:literal, $len:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(crate) struct $name(String);

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, String> {
                if is_lowercase_id(s, $len) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(format!("{s:?} is not {}", $what))
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id!(
    /// An Artifact Registry repository id.
    RepositoryId,
    "a repository id (a lowercase letter, then lowercase letters, digits or hyphens, ending in a letter or digit, at most 63)",
    1..=63
);
id!(
    /// A Google Cloud project id.
    Project,
    "a project id (a lowercase letter, then lowercase letters, digits or hyphens, ending in a letter or digit, 6 to 30)",
    6..=30
);
id!(
    /// A Google Cloud location, such as a region.
    Location,
    "a location (a lowercase letter, then lowercase letters, digits or hyphens, ending in a letter or digit)",
    1..=63
);

impl Location {
    /// The registry host serving this location.
    pub(crate) fn host(&self) -> String {
        format!("{}{HOST_SUFFIX}", self.0)
    }

    /// The location a registry host serves.
    pub(crate) fn from_host(host: &str) -> Result<Self, String> {
        host.strip_suffix(HOST_SUFFIX)
            .ok_or_else(|| format!("{host:?} is not a <location>{HOST_SUFFIX} host"))?
            .parse()
    }
}

/// Artifact Registry repositories a run syncs into, one per round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactRegistryTarget {
    /// Where the repositories are.
    pub(crate) location: Location,
    /// The project that holds them.
    pub(crate) project: Project,
    /// The repositories, round 1 first.
    pub(crate) repositories: Vec<RepositoryId>,
}

impl ArtifactRegistryTarget {
    /// The target prefix of `round` (from 1): the round's own repository, then
    /// the corpus's `prefix` level, so repository names match ECR's below it.
    pub(crate) fn target_prefix(&self, round: usize, prefix: &str) -> String {
        let repository = self
            .repositories
            .get(round - 1)
            .expect("parsing leaves one repository for every round");
        format!("{}/{repository}/{prefix}", self.project)
    }

    /// The environment that describes this target to the instance.
    pub(crate) fn env(&self) -> [(&'static str, String); 3] {
        let ids: Vec<String> = self.repositories.iter().map(ToString::to_string).collect();
        [
            (ENV_REGISTRY, self.location.host()),
            (ENV_PROJECT, self.project.to_string()),
            (ENV_REPOSITORIES, ids.join(",")),
        ]
    }

    /// Reads the target from `env`, which must hold a repository for each of
    /// `rounds`.
    pub(crate) fn parse(
        env: &dyn Fn(&str) -> Option<String>,
        rounds: Rounds,
    ) -> Result<Self, String> {
        let location = Location::from_host(&require(env, ENV_REGISTRY)?)?;
        let project = require(env, ENV_PROJECT)?.parse()?;
        let repositories = require(env, ENV_REPOSITORIES)?
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<RepositoryId>, _>>()
            .map_err(|e| format!("{ENV_REPOSITORIES}: {e}"))?;
        if repositories.len() < rounds.get() {
            return Err(format!(
                "{ENV_REPOSITORIES} names {} repositories but {} rounds need one each",
                repositories.len(),
                rounds.get()
            ));
        }
        Ok(Self {
            location,
            project,
            repositories,
        })
    }
}

/// The value of a variable that must be set and not empty.
pub(crate) fn require(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    env(name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

/// A client for `host` signed in the way ocync signs in to Artifact Registry.
pub(crate) async fn client(host: &str) -> Result<RegistryClient, String> {
    ocync_distribution::install_crypto_provider();
    let url = reqwest::Url::parse(&format!("https://{host}"))
        .map_err(|e| format!("registry host {host}: {e}"))?;
    let auth = GcpAuth::new(
        &url,
        http_client()?,
        &AwsCredentialProviders::default(),
        &SharedGoogleToken::with_residency(Residency::process()),
    )
    .await
    .map_err(|e| format!("Google sign-in for {host}: {e}"))?;
    RegistryClient::builder(url)
        .auth(auth)
        .build()
        .map_err(|e| format!("registry client for {host}: {e}"))
}

/// A tag no image carries, so a signed-in client is answered "absent".
const SIGN_IN_PROBE_TAG: &str = "ocync-bench-sign-in-probe";

/// Sends one authenticated request, so a broken federation file or a missing
/// grant fails before the build and the pre-warm rather than after them. The
/// first repository exists by now, and an absent tag in it answers 404.
pub(crate) async fn check_sign_in(
    client: &RegistryClient,
    target: &ArtifactRegistryTarget,
) -> Result<(), String> {
    let repo = target.target_prefix(1, "sign-in-probe");
    let name = RepositoryName::new(&repo).map_err(|e| format!("{repo}: {e}"))?;
    client
        .manifest_head(&name, SIGN_IN_PROBE_TAG)
        .await
        .map(drop)
        .map_err(|e| format!("Google sign-in check against {repo} failed: {e}"))
}

/// What `repo` holds of the `expected` tags and, for a tag at its locked
/// digest, of the index children the lock lists. Artifact Registry has no
/// listing that names untagged images, so each is asked for by digest.
pub(crate) async fn list_repo(
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

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: [(&str, &str); 3] = [
        (ENV_REGISTRY, "europe-west9-docker.pkg.dev"),
        (ENV_PROJECT, "example-project"),
        (ENV_REPOSITORIES, "run-r1,run-r2,run-r3"),
    ];

    #[test]
    fn ids_are_parsed_against_their_rules() {
        // Catches: an id that would reach gcloud or a URL unchecked.
        let long = format!("a{}", "b".repeat(63));
        for (id, ok) in [
            ("ocync-bench-run-2026-10-30-120000-r1", true),
            ("a", true),
            ("1abc", false),
            ("Abc", false),
            ("a_b", false),
            ("run-", false),
            ("", false),
            (long.as_str(), false),
            (&long[..63], true),
        ] {
            assert_eq!(id.parse::<RepositoryId>().is_ok(), ok, "{id:?}");
        }
        assert!("example-project".parse::<Project>().is_ok());
        assert!("short".parse::<Project>().is_err(), "under 6 characters");
        assert!(
            "example-project-".parse::<Project>().is_err(),
            "ends in a hyphen"
        );
        assert_eq!(
            Location::from_host("europe-west9-docker.pkg.dev")
                .unwrap()
                .host(),
            "europe-west9-docker.pkg.dev"
        );
        assert!(Location::from_host("123.dkr.ecr.us-east-1.amazonaws.com").is_err());
        assert!(Location::from_host("-docker.pkg.dev").is_err());
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
    fn the_sign_in_check_is_one_head_in_the_first_repository_and_a_refusal_fails_it() {
        // Catches: a broken federation file or a missing grant surfacing only
        // after the build, and a check that sends nothing or the wrong request.
        let target = ArtifactRegistryTarget::parse(&env_of(&FULL), rounds("3")).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let check = |status: &str| {
            let (url, seen) = serve_once(status);
            let client = RegistryClient::builder(url).build().unwrap();
            let result = runtime.block_on(check_sign_in(&client, &target));
            (result, seen.join().unwrap())
        };

        let (result, request) = check("404 Not Found");
        assert_eq!(result, Ok(()));
        assert_eq!(
            request,
            "HEAD /v2/example-project/run-r1/sign-in-probe/manifests/ocync-bench-sign-in-probe HTTP/1.1\r\n"
        );
        let (result, _) = check("403 Forbidden");
        assert!(result.unwrap_err().contains("sign-in check"), "refused");
    }

    #[test]
    fn the_target_is_read_from_the_environment_it_describes() {
        let target = ArtifactRegistryTarget::parse(&env_of(&FULL), rounds("3")).unwrap();
        assert_eq!(target.location.host(), "europe-west9-docker.pkg.dev");
        // Catches: the names the local side writes drifting from the ones
        // the instance reads.
        let written = target.env();
        let pairs: Vec<(&str, &str)> = written.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(
            ArtifactRegistryTarget::parse(&env_of(&pairs), rounds("3")).unwrap(),
            target
        );
    }

    #[test]
    fn a_target_that_cannot_serve_its_rounds_is_refused_by_name() {
        let without = |name: &str| -> Vec<(&str, &str)> {
            FULL.iter().filter(|(k, _)| *k != name).copied().collect()
        };
        for (name, pairs, asked, error) in [
            (
                "no host",
                without(ENV_REGISTRY),
                "3",
                "BENCH_TARGET_REGISTRY is not set",
            ),
            (
                "no project",
                without(ENV_PROJECT),
                "3",
                "BENCH_GAR_PROJECT is not set",
            ),
            (
                "empty project",
                vec![
                    (ENV_REGISTRY, "europe-west9-docker.pkg.dev"),
                    (ENV_PROJECT, ""),
                    (ENV_REPOSITORIES, "run-r1,run-r2,run-r3"),
                ],
                "3",
                "BENCH_GAR_PROJECT is not set",
            ),
            (
                "no repositories",
                without(ENV_REPOSITORIES),
                "3",
                "BENCH_GAR_REPOSITORIES is not set",
            ),
            (
                "malformed id",
                vec![
                    (ENV_REGISTRY, "europe-west9-docker.pkg.dev"),
                    (ENV_PROJECT, "example-project"),
                    (ENV_REPOSITORIES, "run-r1,Run_2"),
                ],
                "3",
                "BENCH_GAR_REPOSITORIES: \"Run_2\" is not a repository id",
            ),
            (
                "fewer repositories than rounds",
                FULL.to_vec(),
                "5",
                "BENCH_GAR_REPOSITORIES names 3 repositories but 5 rounds need one each",
            ),
        ] {
            let err = ArtifactRegistryTarget::parse(&env_of(&pairs), rounds(asked)).unwrap_err();
            assert!(err.starts_with(error), "{name}: {err}");
        }
    }
}
