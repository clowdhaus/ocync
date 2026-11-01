//! What `bench-remote` shares across the registries whose repositories it makes
//! and removes on this machine: the names it gives them, the lock that keeps one
//! run at a time, the Terraform outputs it reads, the cloud CLI it calls, and
//! when it may delete.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, TryLockError};
use std::ops::RangeInclusive;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::bench::target::{Kind, Target};
use crate::bench::{Rounds, RunTimestamp, az, captured, gcloud, value_name};

/// What every repository this tool makes is named from. A run removes the
/// repositories with this prefix that an earlier run left, and touches no
/// other.
pub(crate) const REPOSITORY_PREFIX: &str = "ocync-bench-run-";

/// File in the temp directory whose lock keeps a second run from sweeping the
/// repositories of one that makes its own on this machine.
// shortcut: older checkouts lock this name; rename once none predate this
const SWEEP_LOCK_FILE: &str = "ocync-bench-gar.lock";

/// Whether `s` is a lowercase letter, then lowercase letters, digits and
/// hyphens, ending in a letter or digit, within `len` characters.
pub(crate) fn is_lowercase_id(s: &str, len: RangeInclusive<usize>) -> bool {
    let mut chars = s.chars();
    len.contains(&s.len())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.ends_with('-')
}

/// Declares a string newtype that is parsed with `check`, a function of the
/// text, and read back as text.
macro_rules! id {
    ($(#[$doc:meta])* $name:ident, $what:literal, $check:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(crate) struct $name(String);

        impl ::std::str::FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, String> {
                if ($check)(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(format!("{s:?} is not {}", $what))
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
pub(crate) use id;

id!(
    /// What a round's repositories are named from: an Artifact Registry
    /// repository id, or the path prefix of a round's repositories in ACR.
    RoundId,
    "a round id (a lowercase letter, then lowercase letters, digits or hyphens, ending in a letter or digit, at most 63)",
    |s: &str| is_lowercase_id(s, 1..=63)
);

/// The ids of a run's rounds, round 1 first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoundIds(Vec<RoundId>);

impl RoundIds {
    /// The ids of a run that started at `timestamp`: one per round.
    pub(crate) fn for_run(timestamp: &RunTimestamp, rounds: Rounds) -> Result<Self, String> {
        (1..=rounds.get())
            .map(|round| format!("{REPOSITORY_PREFIX}{timestamp}-r{round}").parse())
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// The comma-separated ids in `list`, which must hold one for each of
    /// `rounds`.
    pub(crate) fn parse(list: &str, rounds: Rounds) -> Result<Self, String> {
        let ids = list
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<RoundId>, _>>()?;
        if ids.len() < rounds.get() {
            return Err(format!(
                "{} ids but {} rounds need one each",
                ids.len(),
                rounds.get()
            ));
        }
        Ok(Self(ids))
    }

    /// The ids, round 1 first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &RoundId> {
        self.0.iter()
    }

    /// The id of `round`, counted from 1, if the run has that round.
    pub(crate) fn get(&self, round: usize) -> Option<&RoundId> {
        self.0.get(round.checked_sub(1)?)
    }
}

impl fmt::Display for RoundIds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        f.write_str(&ids.join(","))
    }
}

/// The trimmed, non-empty lines of a `list` answer that an earlier run made:
/// those that start with [`REPOSITORY_PREFIX`].
pub(crate) fn with_prefix(listed: &str) -> impl Iterator<Item = &str> {
    listed
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(REPOSITORY_PREFIX))
}

/// One Terraform output, as `terraform output -json` writes it.
#[derive(Deserialize)]
pub(crate) struct Output<T> {
    pub(crate) value: T,
}

/// The JSON of the applied Terraform stack in `dir`.
fn terraform_output(dir: &str) -> Result<String, String> {
    captured(Command::new("terraform").args([&format!("-chdir={dir}"), "output", "-json"])).map_err(
        |e| format!("terraform output -json failed: {e}. Did you run `terraform apply` in {dir}?"),
    )
}

/// The outputs `T` names in the JSON `terraform output -json` wrote for `dir`,
/// naming the one that is missing or malformed.
#[expect(
    clippy::disallowed_methods,
    reason = "reads Terraform's own output, which holds no credential"
)]
pub(crate) fn parse_terraform_outputs<T: DeserializeOwned>(
    dir: &str,
    json: &str,
) -> Result<T, String> {
    serde_json::from_str(json).map_err(|e| format!("terraform output of {dir}: {e}"))
}

/// The outputs `T` names of the applied Terraform stack in `dir`.
pub(crate) fn terraform_outputs<T: DeserializeOwned>(dir: &str) -> Result<T, String> {
    parse_terraform_outputs(dir, &terraform_output(dir)?)
}

/// Takes the lock that keeps a second run that makes its own repositories on
/// this machine out until the returned file is dropped. The sweep of one run
/// would delete the repositories of the other, which may still be provisioning
/// or fetching with no run on the instance for the instance probe to see.
pub(crate) fn lock_local_run() -> Result<File, String> {
    lock_at(&std::env::temp_dir().join(SWEEP_LOCK_FILE))
}

/// Takes an exclusive lock on the file at `path`, which is made if absent.
fn lock_at(path: &Path) -> Result<File, String> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(format!(
            "another bench-remote run that makes its own repositories is going on this machine ({}): \
             this one would sweep its repositories",
            path.display()
        )),
        Err(TryLockError::Error(e)) => Err(format!("lock {}: {e}", path.display())),
    }
}

/// A cloud's command-line program, called with the operator's own sign-in.
pub(crate) struct Cli(OsString);

impl Cli {
    /// The program `program`, which `PATH` resolves unless it is a path.
    pub(crate) fn new(program: impl Into<OsString>) -> Self {
        Self(program.into())
    }

    /// Runs the program with `argv` and returns what it printed, naming the
    /// first words of the command when it fails.
    pub(crate) fn run(&self, argv: &[String]) -> Result<String, String> {
        captured(Command::new(&self.0).args(argv)).map_err(|e| {
            format!(
                "{} {} failed: {e}",
                self.0.to_string_lossy(),
                argv.iter().take(3).cloned().collect::<Vec<_>>().join(" ")
            )
        })
    }
}

/// Whether the instance's run is known to have ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// The script's own exit was read, or the run never reached the instance.
    Settled,
    /// The connection dropped, so the detached benchmark may still be pushing.
    Unknown,
}

/// The cloud stack's outputs, read before the push.
pub(crate) enum Outputs {
    /// `bench/terraform/gcp`.
    ArtifactRegistry(gcloud::Outputs),
    /// `bench/terraform/azure`.
    Acr(az::Outputs),
}

/// What one run made on this machine for its target, removed when it ends.
/// Dropping deletes whatever [`Self::settle`] has not.
pub(crate) enum Provisioned {
    /// Artifact Registry repositories, made with `gcloud`.
    ArtifactRegistry(gcloud::Provisioned),
    /// ACR repositories, which the run creates itself and `az` deletes.
    Acr(az::Provisioned),
}

impl Provisioned {
    /// Sweeps an earlier run's repositories, and makes what the run needs.
    pub(crate) fn create(
        outputs: Outputs,
        timestamp: &RunTimestamp,
        rounds: Rounds,
    ) -> Result<Self, String> {
        match outputs {
            Outputs::ArtifactRegistry(outputs) => {
                gcloud::Provisioned::create(Cli::new("gcloud"), outputs, timestamp, rounds)
                    .map(Self::ArtifactRegistry)
            }
            Outputs::Acr(outputs) => {
                az::Provisioned::create(Cli::new("az"), outputs, timestamp, rounds).map(Self::Acr)
            }
        }
    }

    /// Deletes the run's repositories when its end is [`RunEnd::Settled`];
    /// keeps them, and says so, when it is [`RunEnd::Unknown`], since the
    /// benchmark may still be pushing.
    pub(crate) fn settle(&mut self, end: RunEnd) -> Result<(), String> {
        let kind = self.kind();
        match end {
            RunEnd::Settled => match self {
                Self::ArtifactRegistry(p) => p.delete(),
                Self::Acr(p) => p.delete(),
            },
            RunEnd::Unknown => {
                let rounds = match self {
                    Self::ArtifactRegistry(p) => p.keep(),
                    Self::Acr(p) => p.keep(),
                };
                eprintln!(
                    "bench-remote: kept the {} repositories of {rounds} rounds because the run \
                     may still be live on the instance; the next `--target {}` run deletes \
                     them once the instance is free",
                    kind.name(),
                    value_name(&kind)
                );
                Ok(())
            }
        }
    }

    /// The registry this run made repositories in.
    fn kind(&self) -> Kind {
        match self {
            Self::ArtifactRegistry(_) => Kind::ArtifactRegistry,
            Self::Acr(_) => Kind::Acr,
        }
    }

    /// The target the instance is told to sync into.
    pub(crate) fn target(&self) -> Target {
        match self {
            Self::ArtifactRegistry(p) => Target::ArtifactRegistry(p.target.clone()),
            Self::Acr(p) => Target::Acr(p.target.clone()),
        }
    }

    /// The Google federation file the instance signs in with, if the target
    /// signs in with one.
    pub(crate) fn credentials_file(&self) -> Option<&Path> {
        match self {
            Self::ArtifactRegistry(p) => Some(p.credentials_file.as_path()),
            Self::Acr(_) => None,
        }
    }
}

/// Ok when `failures` is empty; otherwise an error that names each repository
/// of `kind` that stayed.
pub(crate) fn deleted_all(kind: Kind, failures: &[String]) -> Result<(), String> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to delete {} {} repositories, content left behind: {}",
            failures.len(),
            kind.name(),
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::target::rounds;

    #[test]
    fn ids_are_parsed_against_their_rules() {
        // Catches: an id that would reach a cloud CLI or a URL unchecked.
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
            assert_eq!(id.parse::<RoundId>().is_ok(), ok, "{id:?}");
        }
    }

    #[test]
    fn a_round_names_its_repositories_from_the_run_start_and_the_sweep_keeps_to_that_prefix() {
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        assert_eq!(
            RoundIds::for_run(&timestamp, rounds("3"))
                .unwrap()
                .to_string(),
            "ocync-bench-run-2026-10-30-120000-r1,ocync-bench-run-2026-10-30-120000-r2,\
             ocync-bench-run-2026-10-30-120000-r3"
        );
        // Catches: a sweep that deletes the bench's own repositories or any
        // other that happens to be in the registry.
        let listed = "ocync-bench\nocync-bench-mirror\nocync-bench-run-2026-10-29-090000-r1\n\
                      other-ocync-bench-run-x\n  ocync-bench-run-2026-10-29-090000-r2/bench/a\n\n";
        assert_eq!(
            with_prefix(listed).collect::<Vec<_>>(),
            [
                "ocync-bench-run-2026-10-29-090000-r1",
                "ocync-bench-run-2026-10-29-090000-r2/bench/a",
            ]
        );
    }

    #[test]
    fn a_round_has_the_id_at_its_place_and_none_outside_the_run() {
        // Catches: an index that wraps or panics for round 0 or a round past
        // the last, and a list shorter than the rounds taken for enough.
        let ids = RoundIds::parse("run-a,run-b,run-c,run-d", rounds("3")).unwrap();
        let at = |round| ids.get(round).map(ToString::to_string);
        assert_eq!(at(0), None);
        assert_eq!(at(1).as_deref(), Some("run-a"));
        assert_eq!(at(4).as_deref(), Some("run-d"));
        assert_eq!(at(5), None);

        assert_eq!(
            RoundIds::parse("run-a,run-b", rounds("3")),
            Err("2 ids but 3 rounds need one each".to_owned())
        );
        assert_eq!(
            RoundIds::parse("run-a,Run_b,run-c", rounds("3")),
            Err(
                "\"Run_b\" is not a round id (a lowercase letter, then lowercase letters, \
                  digits or hyphens, ending in a letter or digit, at most 63)"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_second_local_run_is_refused_until_the_first_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let first = lock_at(&path).unwrap();
        let err = lock_at(&path).unwrap_err();
        assert!(err.contains("another bench-remote run"), "{err}");
        drop(first);
        assert!(lock_at(&path).is_ok(), "released with the first run");
    }

    #[test]
    fn repositories_are_kept_when_the_end_of_the_run_is_unknown() {
        // Catches: deleting repositories a detached benchmark is still
        // pushing into, by `settle` or by the drop that follows it, for
        // either registry.
        let (gcloud_dir, az_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (gcloud_run, gcloud_log) = gcloud::tests::provision(gcloud_dir.path(), "never");
        let (az_run, az_log) = az::tests::provision(az_dir.path(), "never");
        for (name, mut provisioned, log) in [
            (
                "gcloud",
                Provisioned::ArtifactRegistry(gcloud_run),
                gcloud_log,
            ),
            ("az", Provisioned::Acr(az_run), az_log),
        ] {
            let before = std::fs::read_to_string(&log).unwrap().lines().count();

            assert_eq!(provisioned.settle(RunEnd::Unknown), Ok(()), "{name}");
            drop(provisioned);

            let calls = std::fs::read_to_string(&log).unwrap();
            assert_eq!(calls.lines().count(), before, "{name}: {calls}");
        }
    }
}
