//! Google Artifact Registry as a benchmark target: the ids that name it, the
//! target the instance parses from its environment, and the client that signs
//! in to it.

use ocync_distribution::auth::gcp::{GcpAuth, Residency, SharedGoogleToken};
use ocync_distribution::{AwsCredentialProviders, RegistryClient};

use crate::bench::Rounds;
use crate::bench::provision::{RoundId, RoundIds, id, is_lowercase_id};
use crate::bench::registry::http_client;
use crate::bench::target::{ENV_REGISTRY, registry_client, require, round_ids};

/// Environment variable holding the project the repositories are in.
pub(crate) const ENV_PROJECT: &str = "BENCH_GAR_PROJECT";
/// Environment variable holding the repository ids, comma-separated, in round
/// order.
pub(crate) const ENV_REPOSITORIES: &str = "BENCH_GAR_REPOSITORIES";
/// Environment variable naming the Google federation file ocync signs in with.
pub(crate) const ENV_CREDENTIALS: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// The hosts the federation file signs in through: the token exchange and the
/// service account impersonation.
pub(crate) const SIGN_IN_HOSTS: &[&str] = &["sts.googleapis.com", "iamcredentials.googleapis.com"];

/// What follows the location in an Artifact Registry host.
const HOST_SUFFIX: &str = "-docker.pkg.dev";

id!(
    /// A Google Cloud project id.
    Project,
    "a project id (a lowercase letter, then lowercase letters, digits or hyphens, ending in a letter or digit, 6 to 30)",
    |s: &str| is_lowercase_id(s, 6..=30)
);
id!(
    /// A Google Cloud location, such as a region.
    Location,
    "a location (a lowercase letter, then lowercase letters, digits or hyphens, ending in a letter or digit)",
    |s: &str| is_lowercase_id(s, 1..=63)
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
    pub(crate) repositories: RoundIds,
}

impl ArtifactRegistryTarget {
    /// The environment that describes this target to the instance.
    pub(crate) fn env(&self) -> [(&'static str, String); 3] {
        [
            (ENV_REGISTRY, self.location.host()),
            (ENV_PROJECT, self.project.to_string()),
            (ENV_REPOSITORIES, self.repositories.to_string()),
        ]
    }

    /// The target prefix of the round whose repository is `repository`, then
    /// the corpus's `prefix` level.
    pub(crate) fn round_prefix(&self, repository: &RoundId, prefix: &str) -> String {
        format!("{}/{repository}/{prefix}", self.project)
    }

    /// Reads the target from `env`, which must hold a repository for each of
    /// `rounds`.
    pub(crate) fn parse(
        env: &dyn Fn(&str) -> Option<String>,
        rounds: Rounds,
    ) -> Result<Self, String> {
        let location = Location::from_host(&require(env, ENV_REGISTRY)?)?;
        let project = require(env, ENV_PROJECT)?.parse()?;
        let repositories = round_ids(env, ENV_REPOSITORIES, rounds)?;
        Ok(Self {
            location,
            project,
            repositories,
        })
    }
}

/// A client for `host` signed in the way ocync signs in to Artifact Registry.
pub(crate) async fn client(host: &str) -> Result<RegistryClient, String> {
    registry_client(host, async |url| {
        GcpAuth::new(
            url,
            http_client()?,
            &AwsCredentialProviders::default(),
            &SharedGoogleToken::with_residency(Residency::process()),
        )
        .await
        .map_err(|e| format!("Google sign-in for {host}: {e}"))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::target::{env_of, rounds};

    const FULL: [(&str, &str); 3] = [
        (ENV_REGISTRY, "europe-west9-docker.pkg.dev"),
        (ENV_PROJECT, "example-project"),
        (ENV_REPOSITORIES, "run-r1,run-r2,run-r3"),
    ];

    #[test]
    fn project_and_location_ids_are_parsed_against_their_rules() {
        // Catches: an id that would reach gcloud or a URL unchecked.
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
                "BENCH_GAR_REPOSITORIES: \"Run_2\" is not a round id",
            ),
            (
                "fewer repositories than rounds",
                FULL.to_vec(),
                "5",
                "BENCH_GAR_REPOSITORIES: 3 ids but 5 rounds need one each",
            ),
        ] {
            let err = ArtifactRegistryTarget::parse(&env_of(&pairs), rounds(asked)).unwrap_err();
            assert!(err.starts_with(error), "{name}: {err}");
        }
    }
}
