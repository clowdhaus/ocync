//! Azure Container Registry as a benchmark target: the ids that name it, the
//! target the instance parses from its environment, and the client that signs
//! in to it.

use ocync_distribution::RegistryClient;
use ocync_distribution::auth::acr::AcrAuth;

use crate::bench::Rounds;
use crate::bench::provision::{RoundId, RoundIds, id};
use crate::bench::target::{ENV_REGISTRY, registry_client, require, round_ids};

/// Environment variable holding the path prefixes of the rounds' repositories,
/// comma-separated, in round order.
pub(crate) const ENV_PREFIXES: &str = "BENCH_ACR_PREFIXES";
/// Environment variable ocync reads the Entra tenant from.
pub(crate) const ENV_TENANT: &str = "AZURE_TENANT_ID";
/// Environment variable ocync reads the Entra application from.
pub(crate) const ENV_CLIENT: &str = "AZURE_CLIENT_ID";
/// Environment variable naming the file that holds the token ocync signs in
/// with, which it rereads on every sign-in.
pub(crate) const ENV_TOKEN_FILE: &str = "AZURE_FEDERATED_TOKEN_FILE";

/// The Entra authority host ocync gets its token from for a `.azurecr.io`
/// registry.
pub(crate) const SIGN_IN_HOSTS: &[&str] = &["login.microsoftonline.com"];

/// What follows the registry's name in its host.
const HOST_SUFFIX: &str = ".azurecr.io";

/// Whether `name` is 5 to 50 lowercase letters and digits.
fn is_registry_name(name: &str) -> bool {
    (5..=50).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

id!(
    /// The name of a registry.
    RegistryName,
    "a registry name (5 to 50 lowercase letters or digits)",
    is_registry_name
);

impl RegistryName {
    /// The name a registry host (`<name>.azurecr.io`) carries.
    pub(crate) fn from_host(host: &str) -> Result<Self, String> {
        host.strip_suffix(HOST_SUFFIX)
            .ok_or_else(|| format!("{host:?} is not a <name>{HOST_SUFFIX} host"))?
            .parse()
    }

    /// The registry's host.
    pub(crate) fn host(&self) -> String {
        format!("{}{HOST_SUFFIX}", self.0)
    }
}

/// Whether `s` is a GUID: hexadecimal digits in groups of 8, 4, 4, 4 and 12
/// divided by hyphens.
fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

id!(
    /// A GUID, as Entra writes tenant and application ids.
    Guid,
    "a GUID",
    is_guid
);

/// The repositories in an ACR a run syncs into, one path prefix per round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The registry.
    pub(crate) registry: RegistryName,
    /// The Entra tenant ocync signs in to.
    pub(crate) tenant: Guid,
    /// The Entra application ocync signs in as.
    pub(crate) client: Guid,
    /// The path prefix of each round's repositories, round 1 first. A first
    /// push creates the repositories below it.
    pub(crate) prefixes: RoundIds,
}

impl Scope {
    /// The target prefix of the round whose prefix is `round`, then the
    /// corpus's `prefix` level.
    pub(crate) fn round_prefix(&self, round: &RoundId, prefix: &str) -> String {
        format!("{round}/{prefix}")
    }

    /// The environment that describes this target to the instance. The token
    /// file's path is the instance's own to choose.
    pub(crate) fn env(&self) -> [(&'static str, String); 4] {
        [
            (ENV_REGISTRY, self.registry.host()),
            (ENV_PREFIXES, self.prefixes.to_string()),
            (ENV_TENANT, self.tenant.to_string()),
            (ENV_CLIENT, self.client.to_string()),
        ]
    }

    /// Reads the target from `env`, which must hold a prefix for each of
    /// `rounds`.
    pub(crate) fn parse(
        env: &dyn Fn(&str) -> Option<String>,
        rounds: Rounds,
    ) -> Result<Self, String> {
        let guid = |name| {
            require(env, name)?
                .parse()
                .map_err(|e| format!("{name}: {e}"))
        };
        Ok(Self {
            registry: RegistryName::from_host(&require(env, ENV_REGISTRY)?)?,
            tenant: guid(ENV_TENANT)?,
            client: guid(ENV_CLIENT)?,
            prefixes: round_ids(env, ENV_PREFIXES, rounds)?,
        })
    }
}

/// A client for `host` signed in the way ocync signs in to ACR.
pub(crate) async fn client(host: &str) -> Result<RegistryClient, String> {
    registry_client(host, async |url| {
        AcrAuth::new(url)
            .await
            .map_err(|e| format!("Azure sign-in for {host}: {e}"))
    })
    .await
}

/// Environment for an ACR target in tests.
#[cfg(test)]
pub(super) const FULL_ENV: [(&str, &str); 5] = [
    (ENV_REGISTRY, "ocyncbench1a2b3c.azurecr.io"),
    (ENV_PREFIXES, "run-r1,run-r2,run-r3"),
    (ENV_TENANT, "11111111-2222-3333-4444-555555555555"),
    (ENV_CLIENT, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
    (ENV_TOKEN_FILE, "/home/bench/.bench-azure-token"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::target::{env_of, rounds};

    #[test]
    fn registry_and_tenant_ids_are_parsed_against_their_rules() {
        // Catches: a host or id that would reach the remote shell, an `az`
        // argument or a URL unchecked.
        let long = format!("{}.azurecr.io", "a".repeat(51));
        let longest = format!("{}.azurecr.io", "a".repeat(50));
        for (host, ok) in [
            ("ocyncbench1a2b3c.azurecr.io", true),
            ("abcde.azurecr.io", true),
            ("abcd.azurecr.io", false),
            (longest.as_str(), true),
            (long.as_str(), false),
            ("OcyncBench.azurecr.io", false),
            ("ocync-bench.azurecr.io", false),
            ("ocyncbench.azurecr.us", false),
            ("ocyncbench.azurecr.io.evil.example", false),
            ("a;b$(x).azurecr.io", false),
            (".azurecr.io", false),
            ("ocyncbench", false),
        ] {
            assert_eq!(RegistryName::from_host(host).is_ok(), ok, "{host:?}");
        }
        for (guid, ok) in [
            ("11111111-2222-3333-4444-555555555555", true),
            ("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE", true),
            ("11111111222233334444555555555555", false),
            ("11111111-2222-3333-4444-55555555555g", false),
            ("11111111-2222-3333-4444-5555555555555", false),
            ("11111111-2222-3333-4444-555555555555; id", false),
            ("", false),
        ] {
            assert_eq!(guid.parse::<Guid>().is_ok(), ok, "{guid:?}");
        }
    }

    #[test]
    fn the_target_is_read_from_the_environment_it_describes() {
        let target = Scope::parse(&env_of(&FULL_ENV), rounds("3")).unwrap();
        assert_eq!(target.registry.host(), "ocyncbench1a2b3c.azurecr.io");
        // Catches: the names the local side writes drifting from the ones
        // the instance reads.
        let written = target.env();
        let pairs: Vec<(&str, &str)> = written.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(Scope::parse(&env_of(&pairs), rounds("3")).unwrap(), target);
    }

    #[test]
    fn a_target_that_cannot_serve_its_rounds_is_refused_by_name() {
        let without = |name: &str| -> Vec<(&str, &str)> {
            FULL_ENV
                .iter()
                .filter(|(k, _)| *k != name)
                .copied()
                .collect()
        };
        let with = |name: &str, value: &'static str| -> Vec<(&str, &str)> {
            FULL_ENV
                .iter()
                .map(|&(k, v)| (k, if k == name { value } else { v }))
                .collect()
        };
        for (name, pairs, asked, error) in [
            (
                "no host",
                without(ENV_REGISTRY),
                "3",
                "BENCH_TARGET_REGISTRY is not set",
            ),
            (
                "host of another registry",
                with(ENV_REGISTRY, "123.dkr.ecr.us-east-1.amazonaws.com"),
                "3",
                "\"123.dkr.ecr.us-east-1.amazonaws.com\" is not a <name>.azurecr.io host",
            ),
            (
                "no tenant",
                without(ENV_TENANT),
                "3",
                "AZURE_TENANT_ID is not set",
            ),
            (
                "empty client",
                with(ENV_CLIENT, ""),
                "3",
                "AZURE_CLIENT_ID is not set",
            ),
            (
                "tenant that is no GUID",
                with(ENV_TENANT, "contoso.onmicrosoft.com"),
                "3",
                "AZURE_TENANT_ID: \"contoso.onmicrosoft.com\" is not a GUID",
            ),
            (
                "no prefixes",
                without(ENV_PREFIXES),
                "3",
                "BENCH_ACR_PREFIXES is not set",
            ),
            (
                "malformed prefix",
                with(ENV_PREFIXES, "run-r1,Run_2"),
                "3",
                "BENCH_ACR_PREFIXES: \"Run_2\" is not a round id",
            ),
            (
                "fewer prefixes than rounds",
                FULL_ENV.to_vec(),
                "5",
                "BENCH_ACR_PREFIXES: 3 ids but 5 rounds need one each",
            ),
        ] {
            let err = Scope::parse(&env_of(&pairs), rounds(asked)).unwrap_err();
            assert!(err.starts_with(error), "{name}: {err}");
        }
    }
}
