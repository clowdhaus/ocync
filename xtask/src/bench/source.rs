//! Where a run's tools read the corpus from: the public registries it names,
//! or the controlled source, an ECR registry the bench owns that holds the
//! locked digests (`bench-seed` copies them there).
//!
//! The source is in the target's account and another region, so it is another
//! registry host and no tool can mount a blob from it into the target: a mount
//! needs both repositories on one registry, and would skip the copy the run
//! measures.

use std::borrow::Cow;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::bench::RegistryKind;
use crate::bench::config_gen::source_path;
use crate::bench::target;

/// The AWS region of the controlled source unless `--source-region` names
/// another.
pub(crate) const REGION: &str = "us-east-1";

/// The repository prefix of every image in the controlled source:
/// `bench-source/<path of the public source>`.
pub(crate) const REPO_PREFIX: &str = "bench-source";

/// An AWS region name such as `us-east-1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Region(String);

impl Region {
    /// The region as the registry host and the AWS API spell it.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Region {
    fn default() -> Self {
        Self(REGION.to_owned())
    }
}

impl std::str::FromStr for Region {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, String> {
        let shaped = raw.starts_with(|c: char| c.is_ascii_lowercase())
            && raw.ends_with(|c: char| c.is_ascii_digit())
            && raw
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if shaped {
            Ok(Self(raw.to_owned()))
        } else {
            Err(format!(
                "{raw:?} is not a region name such as {REGION}: lowercase letters, digits and \
                 dashes, ending in a digit"
            ))
        }
    }
}

impl std::fmt::Display for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which source a run reads, as `--source` names it and a run record states it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// The registries the corpus names. Where rate-limit behavior is measured.
    #[default]
    Public,
    /// The registry the bench owns. Where speed is measured.
    Controlled,
}

impl Kind {
    /// Whether this is [`Self::Public`], which a record leaves unwritten.
    pub(crate) fn is_public(&self) -> bool {
        *self == Self::Public
    }

    /// Fails unless a run into `target` can read this source.
    pub(crate) fn ensure_applies(self, target: target::Kind) -> Result<(), String> {
        if self == Self::Controlled && target != target::Kind::Ecr {
            return Err(format!(
                "--source controlled needs the ECR target, not {}: the controlled source is ECR \
                 in the target's account",
                target.name()
            ));
        }
        Ok(())
    }
}

/// The source a run's tools read, with the registry it names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Source {
    /// Each image from the registry the corpus names.
    #[default]
    Public,
    /// Each image from the bench's own registry.
    Controlled(Controlled),
}

/// The controlled source's registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Controlled {
    /// `<account>.dkr.ecr.<region>.<domain>`, in the target's form.
    host: String,
    /// The region the registry is in.
    region: Region,
}

impl Controlled {
    /// The controlled source beside `target_registry`: its account, domain and
    /// FIPS or standard form, in `region`. The same region would be the same
    /// registry, which could mount blobs between repositories, so it is
    /// refused.
    pub(crate) fn beside(target_registry: &str, region: &Region) -> Result<Self, String> {
        let host = target_registry.to_ascii_lowercase();
        let parts = (RegistryKind::of(&host) == RegistryKind::Ecr)
            .then(|| {
                ECR_INFIXES
                    .iter()
                    .find_map(|infix| Some((infix, host.split_once(infix)?)))
            })
            .flatten()
            .and_then(|(infix, (account, rest))| Some((infix, account, rest.split_once('.')?)));
        let Some((infix, account, (target_region, domain))) = parts else {
            return Err(format!(
                "the target registry {target_registry} is not an ECR host \
                 (<account>.dkr.ecr.<region>.<domain>), so the controlled source cannot sit \
                 beside it"
            ));
        };
        Self::ensure_other_region(target_region, region)?;
        Ok(Self {
            host: format!("{account}{infix}{region}.{domain}"),
            region: region.clone(),
        })
    }

    /// Fails when the target is in the source's region.
    pub(crate) fn ensure_other_region(
        target_region: &str,
        source_region: &Region,
    ) -> Result<(), String> {
        if target_region == source_region.as_str() {
            return Err(format!(
                "the target is in {source_region} like the controlled source: the same registry \
                 would let a tool mount blobs from the source and skip the copy the run \
                 measures; name another region with --source-region"
            ));
        }
        Ok(())
    }

    /// The registry hostname.
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// The region the registry is in.
    pub(crate) fn region(&self) -> &Region {
        &self.region
    }

    /// The repository holding the copy of `original`, a public source such as
    /// `docker.io/library/alpine`: `bench-source/library/alpine`.
    pub(crate) fn repo(&self, original: &str) -> String {
        format!("{REPO_PREFIX}/{}", source_path(original))
    }
}

/// The two forms an ECR host takes, which the source keeps from the target.
const ECR_INFIXES: [&str; 2] = [".dkr.ecr.", ".dkr.ecr-fips."];

impl Source {
    /// The source `kind` names for a run into `target_registry`.
    pub(crate) fn resolve(
        kind: Kind,
        target_registry: &str,
        region: &Region,
    ) -> Result<Self, String> {
        match kind {
            Kind::Public => Ok(Self::Public),
            Kind::Controlled => Controlled::beside(target_registry, region).map(Self::Controlled),
        }
    }

    /// Which kind of source this is.
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Public => Kind::Public,
            Self::Controlled(_) => Kind::Controlled,
        }
    }

    /// The region of the source, when the bench owns it.
    pub(crate) fn region(&self) -> Option<&str> {
        match self {
            Self::Public => None,
            Self::Controlled(controlled) => Some(controlled.region.as_str()),
        }
    }

    /// Where the tools read `original`, a corpus source such as
    /// `docker.io/library/alpine`, as `host/repository`.
    pub(crate) fn locate<'a>(&self, original: &'a str) -> Cow<'a, str> {
        match self {
            Self::Public => Cow::Borrowed(original),
            Self::Controlled(controlled) => {
                Cow::Owned(format!("{}/{}", controlled.host, controlled.repo(original)))
            }
        }
    }

    /// What to do when a locked digest of `corpus` is not served.
    pub(crate) fn remedy(&self, corpus: &str) -> String {
        match self {
            Self::Public => "Run `cargo xtask bench-lock` to lock the current content. A new lock \
                 starts a new comparison series: do not read its runs against runs under the \
                 old one."
                .to_owned(),
            Self::Controlled(controlled) => format!(
                "Run `cargo xtask bench-remote --provider aws --seed --corpus {corpus} \
                 --source-region {}` to copy the locked digests into the controlled source.",
                controlled.region
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "111111111111.dkr.ecr.us-east-2.amazonaws.com";

    fn region(raw: &str) -> Region {
        raw.parse().unwrap()
    }

    #[test]
    fn the_controlled_source_is_the_targets_account_in_another_region() {
        let controlled = Controlled::beside(TARGET, &Region::default()).unwrap();
        assert_eq!(
            controlled.host(),
            "111111111111.dkr.ecr.us-east-1.amazonaws.com"
        );
        assert_eq!(
            controlled.repo("docker.io/library/alpine"),
            "bench-source/library/alpine"
        );
        let west = Controlled::beside(TARGET, &region("us-west-2")).unwrap();
        assert_eq!(west.host(), "111111111111.dkr.ecr.us-west-2.amazonaws.com");
        assert_eq!(west.region().as_str(), "us-west-2");
    }

    #[test]
    fn a_fips_target_gets_a_fips_source() {
        // Catches: a FIPS target refused, or given a standard-endpoint source
        // that a FIPS run would not be allowed to read.
        let fips = Controlled::beside(
            "111111111111.dkr.ecr-fips.us-east-2.amazonaws.com",
            &Region::default(),
        )
        .unwrap();
        assert_eq!(
            fips.host(),
            "111111111111.dkr.ecr-fips.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn a_target_the_source_could_not_sit_beside_is_refused() {
        // Catches: a target in the source's region, where a mount from source
        // to target repository would skip the copy, and a target that is no
        // ECR host yielding a made-up source host.
        let same = Controlled::beside(
            "111111111111.dkr.ecr.us-east-1.amazonaws.com",
            &Region::default(),
        );
        assert!(same.unwrap_err().contains("--source-region"));
        assert!(Controlled::beside(TARGET, &region("us-east-2")).is_err());
        assert!(Controlled::ensure_other_region("us-east-2", &region("us-east-2")).is_err());
        assert!(Controlled::ensure_other_region("us-east-2", &region("us-east-1")).is_ok());
        for host in ["ghcr.io", "example.azurecr.io", "111.dkr.ecr.example.com"] {
            assert!(
                Controlled::beside(host, &Region::default()).is_err(),
                "{host}"
            );
        }
        assert!(
            Kind::Controlled
                .ensure_applies(target::Kind::Acr)
                .unwrap_err()
                .contains("ECR target")
        );
        assert!(Kind::Controlled.ensure_applies(target::Kind::Ecr).is_ok());
        assert!(Kind::Public.ensure_applies(target::Kind::Acr).is_ok());
    }

    #[test]
    fn a_region_is_a_name_and_nothing_a_host_could_be_built_wrong_from() {
        for good in ["us-east-1", "eu-central-2", "us-gov-west-1"] {
            assert_eq!(
                good.parse::<Region>().map(|r| r.to_string()),
                Ok(good.into())
            );
        }
        for bad in [
            "",
            "US-EAST-1",
            "us-east",
            "us.east-1",
            "us-east-1/x",
            "1-east-1",
        ] {
            assert!(bad.parse::<Region>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_source_locates_a_corpus_source_at_its_own_registry() {
        let original = "quay.io/jupyter/base-notebook";
        assert_eq!(Source::Public.locate(original), original);
        let controlled = Source::resolve(Kind::Controlled, TARGET, &Region::default()).unwrap();
        assert_eq!(
            controlled.locate(original),
            "111111111111.dkr.ecr.us-east-1.amazonaws.com/bench-source/jupyter/base-notebook"
        );
        assert_eq!(controlled.region(), Some("us-east-1"));
        assert_eq!(
            Source::resolve(Kind::Public, "anything", &Region::default()).unwrap(),
            Source::Public
        );
    }

    #[test]
    fn the_remedy_names_the_corpus_and_region_the_run_used() {
        // Catches: advice that seeds the default corpus or region when the run
        // read another.
        let controlled = Source::resolve(Kind::Controlled, TARGET, &region("us-west-2")).unwrap();
        let advice = controlled.remedy("bench/corpus-small.yaml");
        assert!(
            advice.contains("--corpus bench/corpus-small.yaml")
                && advice.contains("--source-region us-west-2"),
            "{advice}"
        );
        assert!(
            Source::Public
                .remedy("bench/corpus.yaml")
                .contains("bench-lock")
        );
    }
}
