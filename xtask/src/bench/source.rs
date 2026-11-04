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

/// The AWS region of the controlled source.
pub(crate) const REGION: &str = "us-east-1";

/// The repository prefix of every image in the controlled source:
/// `bench-source/<path of the public source>`.
pub(crate) const REPO_PREFIX: &str = "bench-source";

/// The tag every controlled-source repository carries, whose value is the
/// `locked_at` of the lock it was seeded from.
pub(crate) const LOCK_TAG_KEY: &str = "ocync-bench-lock";

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
    /// `<account>.dkr.ecr.<REGION>.<domain>`.
    host: String,
}

impl Controlled {
    /// The controlled source beside `target_registry`: its account and domain
    /// in [`REGION`]. A target in that region would be the same registry, which
    /// could mount blobs between repositories, so it is refused.
    pub(crate) fn beside(target_registry: &str) -> Result<Self, String> {
        let host = target_registry.to_ascii_lowercase();
        let parts = (RegistryKind::of(&host) == RegistryKind::Ecr)
            .then(|| host.split_once(".dkr.ecr."))
            .flatten()
            .and_then(|(account, rest)| Some((account, rest.split_once('.')?)));
        let Some((account, (region, domain))) = parts else {
            return Err(format!(
                "the target registry {target_registry} is not an ECR host \
                 (<account>.dkr.ecr.<region>.<domain>), so the controlled source cannot sit \
                 beside it"
            ));
        };
        if region == REGION {
            return Err(format!(
                "the target is in {REGION} like the controlled source: the same registry would \
                 let a tool mount blobs from the source and skip the copy the run measures"
            ));
        }
        Ok(Self {
            host: format!("{account}.dkr.ecr.{REGION}.{domain}"),
        })
    }

    /// The registry hostname.
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// The repository holding the copy of `original`, a public source such as
    /// `docker.io/library/alpine`: `bench-source/library/alpine`.
    pub(crate) fn repo(&self, original: &str) -> String {
        format!("{REPO_PREFIX}/{}", source_path(original))
    }
}

impl Source {
    /// The source `kind` names for a run into `target_registry`.
    pub(crate) fn resolve(kind: Kind, target_registry: &str) -> Result<Self, String> {
        match kind {
            Kind::Public => Ok(Self::Public),
            Kind::Controlled => Controlled::beside(target_registry).map(Self::Controlled),
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
    pub(crate) fn region(&self) -> Option<&'static str> {
        match self {
            Self::Public => None,
            Self::Controlled(_) => Some(REGION),
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

    /// What to do when a locked digest is not served.
    pub(crate) fn remedy(&self) -> &'static str {
        match self {
            Self::Public => {
                "Run `cargo xtask bench-lock` to lock the current content. A new lock starts a \
                 new comparison series: do not read its runs against runs under the old one."
            }
            Self::Controlled(_) => {
                "Run `cargo xtask bench-remote --provider aws --seed` to copy the locked digests \
                 into the controlled source."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "111111111111.dkr.ecr.us-east-2.amazonaws.com";

    #[test]
    fn the_controlled_source_is_the_targets_account_in_another_region() {
        let controlled = Controlled::beside(TARGET).unwrap();
        assert_eq!(
            controlled.host(),
            "111111111111.dkr.ecr.us-east-1.amazonaws.com"
        );
        assert_eq!(
            controlled.repo("docker.io/library/alpine"),
            "bench-source/library/alpine"
        );
    }

    #[test]
    fn a_target_the_source_could_not_sit_beside_is_refused() {
        // Catches: a target in the source's region, where a mount from source
        // to target repository would skip the copy, and a target that is no
        // ECR host yielding a made-up source host.
        let same = Controlled::beside("111111111111.dkr.ecr.us-east-1.amazonaws.com");
        assert!(same.unwrap_err().contains("mount"));
        for host in [
            "ghcr.io",
            "example.azurecr.io",
            "111.dkr.ecr-fips.us-east-2.amazonaws.com",
        ] {
            assert!(Controlled::beside(host).is_err(), "{host}");
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
    fn a_source_locates_a_corpus_source_at_its_own_registry() {
        let original = "quay.io/jupyter/base-notebook";
        assert_eq!(Source::Public.locate(original), original);
        let controlled = Source::resolve(Kind::Controlled, TARGET).unwrap();
        assert_eq!(
            controlled.locate(original),
            "111111111111.dkr.ecr.us-east-1.amazonaws.com/bench-source/jupyter/base-notebook"
        );
        assert_eq!(
            Source::resolve(Kind::Public, "anything").unwrap(),
            Source::Public
        );
    }
}
