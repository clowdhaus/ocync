//! The ACR repositories a `bench-remote` run syncs into, removed on this
//! machine with the operator's `az` sign-in.
//!
//! ACR keeps a blob after the image that held it is deleted, so a repository
//! that has been used is no longer cold. Every round therefore syncs under a
//! path prefix of its own. A first push creates the repositories below it, so
//! nothing is made up front and the instance's identity, which can push but not
//! delete, needs nothing more. The run ends by deleting them here.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

use crate::bench::acr::{Guid, RegistryName, Scope};
use crate::bench::provision::{
    Cli, Output, RoundId, RoundIds, deleted_all, terraform_outputs, with_prefix,
};
use crate::bench::target::Kind;
use crate::bench::{Rounds, RunTimestamp};

/// The most `az` deletions in flight at once.
const DELETE_PARALLELISM: usize = 8;

/// Terraform directory whose outputs name the Azure side of the bench.
const TERRAFORM_DIR: &str = "bench/terraform/azure";

/// The outputs of `bench/terraform/azure` the run reads.
#[derive(Deserialize)]
struct RawOutputs {
    registry: Output<String>,
    tenant_id: Output<String>,
    client_id: Output<String>,
}

/// The Azure side of the bench: where its registry is and who the instance
/// signs in as.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outputs {
    /// The bench's registry.
    registry: RegistryName,
    /// The Entra tenant the instance signs in to.
    tenant: Guid,
    /// The Entra application the instance signs in as.
    client: Guid,
}

impl Outputs {
    /// Reads the outputs of the applied Terraform stack.
    pub(crate) fn read() -> Result<Self, String> {
        Self::from_raw(terraform_outputs(TERRAFORM_DIR)?)
    }

    /// The outputs `raw` holds, each checked.
    fn from_raw(raw: RawOutputs) -> Result<Self, String> {
        let malformed = |name, why| format!("terraform output `{name}`: {why}");
        Ok(Self {
            registry: RegistryName::from_host(&raw.registry.value)
                .map_err(|e| malformed("registry", e))?,
            tenant: raw
                .tenant_id
                .value
                .parse()
                .map_err(|e| malformed("tenant_id", e))?,
            client: raw
                .client_id
                .value
                .parse()
                .map_err(|e| malformed("client_id", e))?,
        })
    }
}

/// The argv of `az acr repository list` for `registry`, one repository path a
/// line.
fn list_argv(registry: &RegistryName) -> Vec<String> {
    [
        "acr",
        "repository",
        "list",
        "--name",
        &registry.to_string(),
        "-o",
        "tsv",
    ]
    .map(str::to_owned)
    .into()
}

/// The argv that deletes `repository` from `registry` without asking.
fn delete_argv(registry: &RegistryName, repository: &str) -> Vec<String> {
    [
        "acr",
        "repository",
        "delete",
        "--name",
        &registry.to_string(),
        "--repository",
        repository,
        "--yes",
    ]
    .map(str::to_owned)
    .into()
}

/// Deletes each repository of `registry` that starts with the sweep's prefix
/// and that `to_delete` selects, [`DELETE_PARALLELISM`] at a time, trying all,
/// and names those that stayed.
fn delete_where(
    az: &Cli,
    registry: &RegistryName,
    to_delete: impl Fn(&str) -> bool,
) -> Result<(), String> {
    let listed = az.run(&list_argv(registry))?;
    let repositories: Vec<&str> = with_prefix(&listed)
        .filter(|repository| to_delete(repository))
        .collect();
    let next = AtomicUsize::new(0);
    let mut failures: Vec<(usize, String)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..DELETE_PARALLELISM.min(repositories.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut failed = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&repository) = repositories.get(at) else {
                            break failed;
                        };
                        if let Err(e) = az.run(&delete_argv(registry, repository)) {
                            failed.push((at, format!("{repository}: {e}")));
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    failures.sort();
    let failures: Vec<String> = failures.into_iter().map(|(_, failure)| failure).collect();
    deleted_all(Kind::Acr, &failures)
}

/// The repositories one run made, removed when it ends.
///
/// Dropping deletes whatever [`Provisioned::delete`] has not, so a run that
/// fails or returns early leaves nothing behind. A process that is killed
/// relies on the next run's sweep of
/// [`REPOSITORY_PREFIX`](crate::bench::provision::REPOSITORY_PREFIX) instead.
pub(crate) struct Provisioned {
    az: Cli,
    /// The run's path prefixes.
    pub(crate) target: Scope,
    /// The path prefixes still to delete, round 1 first.
    pending: Vec<RoundId>,
}

impl Provisioned {
    /// Sweeps an earlier run's repositories and names a path prefix for each
    /// round. Nothing else is made: the first push creates the repositories.
    pub(crate) fn create(
        az: Cli,
        outputs: Outputs,
        timestamp: &RunTimestamp,
        rounds: Rounds,
    ) -> Result<Self, String> {
        let Outputs {
            registry,
            tenant,
            client,
        } = outputs;
        let prefixes = RoundIds::for_run(timestamp, rounds)?;
        delete_where(&az, &registry, |_| true)?;
        Ok(Self {
            az,
            pending: prefixes.iter().cloned().collect(),
            target: Scope {
                registry,
                tenant,
                client,
                prefixes,
            },
        })
    }

    /// Gives up the repositories without deleting them, and returns how many
    /// prefixes they are below.
    pub(crate) fn keep(&mut self) -> usize {
        std::mem::take(&mut self.pending).len()
    }

    /// Deletes every repository below this run's prefixes.
    pub(crate) fn delete(&mut self) -> Result<(), String> {
        let prefixes: Vec<String> = std::mem::take(&mut self.pending)
            .iter()
            .map(ToString::to_string)
            .collect();
        if prefixes.is_empty() {
            return Ok(());
        }
        delete_where(&self.az, &self.target.registry, |repository| {
            let first = repository
                .split_once('/')
                .map_or(repository, |(first, _)| first);
            prefixes.iter().any(|prefix| prefix == first)
        })
    }
}

impl Drop for Provisioned {
    fn drop(&mut self) {
        if let Err(e) = self.delete() {
            eprintln!("error: {e}");
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::bench::provision::parse_terraform_outputs;
    use crate::bench::target::{rounds, write_stub};

    const OUTPUTS: &str = r#"{
  "connect": {"sensitive": false, "type": "string", "value": "ssh bench@example.invalid"},
  "client_id": {"sensitive": false, "type": "string", "value": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"},
  "registry": {"sensitive": false, "type": "string", "value": "ocyncbench1a2b3c.azurecr.io"},
  "tenant_id": {"sensitive": false, "type": "string", "value": "11111111-2222-3333-4444-555555555555"}
}"#;

    fn read_outputs(json: &str) -> Result<Outputs, String> {
        Outputs::from_raw(parse_terraform_outputs(TERRAFORM_DIR, json)?)
    }

    fn registry() -> RegistryName {
        RegistryName::from_host("ocyncbench1a2b3c.azurecr.io").unwrap()
    }

    #[test]
    fn terraform_outputs_are_parsed_and_a_bad_one_is_named() {
        let outputs = read_outputs(OUTPUTS).unwrap();
        assert_eq!(outputs.registry, registry());
        assert_eq!(
            outputs.tenant,
            "11111111-2222-3333-4444-555555555555".parse().unwrap()
        );
        assert_eq!(
            outputs.client,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".parse().unwrap()
        );

        let without_client = OUTPUTS.replace("client_id", "other");
        for (name, json, named) in [
            ("missing field", without_client.as_str(), "client_id"),
            (
                "registry on another host",
                &OUTPUTS.replace("ocyncbench1a2b3c.azurecr.io", "ghcr.io"),
                "`registry`",
            ),
            (
                "tenant that is no GUID",
                &OUTPUTS.replace("11111111-2222-3333-4444-555555555555", "contoso"),
                "`tenant_id`",
            ),
            (
                "client that is no GUID",
                &OUTPUTS.replace("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "contoso"),
                "`client_id`",
            ),
        ] {
            let err = read_outputs(json).unwrap_err();
            assert!(err.contains(named), "{name}: {err}");
        }
    }

    #[test]
    fn each_az_command_names_the_registry_and_never_asks() {
        assert_eq!(
            list_argv(&registry()),
            [
                "acr",
                "repository",
                "list",
                "--name",
                "ocyncbench1a2b3c",
                "-o",
                "tsv"
            ]
        );
        assert_eq!(
            delete_argv(
                &registry(),
                "ocync-bench-run-2026-10-30-120000-r1/bench/library-alpine"
            ),
            [
                "acr",
                "repository",
                "delete",
                "--name",
                "ocyncbench1a2b3c",
                "--repository",
                "ocync-bench-run-2026-10-30-120000-r1/bench/library-alpine",
                "--yes"
            ]
        );
    }

    /// An `az` that logs its argv to `log`, fails a `delete` of a repository
    /// whose path holds `bad`, and answers its first `list` with an earlier
    /// run's repositories and later ones with this run's, an earlier run's
    /// `r10` and a newer leftover beside them.
    fn stub_az(dir: &Path, bad: &str) -> (Cli, PathBuf) {
        let log = dir.join("calls.log");
        let program = write_stub(
            dir,
            "az",
            &format!(
                "#!/bin/bash\necho \"$@\" >> '{log}'\n\
                 case \"$*\" in\n\
                 *list*) echo x >> '{dir}/lists'\n\
                   if [ \"$(wc -l < '{dir}/lists')\" = 1 ]; then\n\
                     printf 'ocync-bench-run-old-r1/bench/a\\nocync-bench-run-old-r1/bench/b\\nocync-bench\\nother/x\\n'\n\
                   else\n\
                     printf 'ocync-bench-run-2026-10-30-120000-r1/bench/a\\nocync-bench-run-2026-10-30-120000-r2/bench/a\\nocync-bench-run-2026-10-30-120000-r2/bench/b\\nocync-bench-run-2026-10-30-120000-r3\\nocync-bench-run-2026-10-30-120000-r10/bench/a\\nocync-bench-run-new-r1/bench/a\\nocync-bench\\n'\n\
                   fi ;;\n\
                 *delete*--repository*{bad}*) echo 'denied' >&2; exit 1 ;;\n\
                 esac\n",
                log = log.display(),
                dir = dir.display()
            ),
        );
        (Cli::new(program), log)
    }

    /// A run provisioned against [`stub_az`] for three rounds, and the log of
    /// the calls it made.
    pub(in crate::bench) fn provision(dir: &Path, bad: &str) -> (Provisioned, PathBuf) {
        let (az, log) = stub_az(dir, bad);
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        let outputs = read_outputs(OUTPUTS).unwrap();
        let provisioned = Provisioned::create(az, outputs, &timestamp, rounds("3")).unwrap();
        (provisioned, log)
    }

    /// The repositories the log shows deleted, in order. Deletions run side by
    /// side, so only the order of the sweep before the run and of the run
    /// after it is meaningful, not the order within either.
    fn deleted(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|l| {
                l.strip_prefix("acr repository delete --name ocyncbench1a2b3c --repository ")
            })
            .map(|l| l.strip_suffix(" --yes").unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_sweep_removes_earlier_runs_then_the_run_removes_only_its_own_and_names_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (mut provisioned, log) = provision(dir.path(), "-r2/bench/b");
        // Catches: a sweep that deletes the bench's own repositories or any
        // other in the registry.
        assert_eq!(
            provisioned.target.prefixes.iter().count(),
            3,
            "one prefix per round"
        );
        let mut swept = deleted(&log);
        swept.sort();
        assert_eq!(
            swept,
            [
                "ocync-bench-run-old-r1/bench/a",
                "ocync-bench-run-old-r1/bench/b"
            ]
        );

        let err = provisioned.delete().unwrap_err();
        assert!(
            err.contains("ocync-bench-run-2026-10-30-120000-r2/bench/b: ")
                && err.contains("denied"),
            "{err}"
        );
        assert!(
            !err.contains("r1/bench/a:") && !err.contains("r3:"),
            "{err}"
        );
        drop(provisioned);

        // Catches: `r1` matching `r10`, and a later leftover being taken for
        // the run's: only repositories below the run's own prefixes go, the
        // failure of one did not stop the rest, and the drop deleted nothing
        // twice.
        let mut removed = deleted(&log)[2..].to_vec();
        removed.sort();
        assert_eq!(
            removed,
            [
                "ocync-bench-run-2026-10-30-120000-r1/bench/a",
                "ocync-bench-run-2026-10-30-120000-r2/bench/a",
                "ocync-bench-run-2026-10-30-120000-r2/bench/b",
                "ocync-bench-run-2026-10-30-120000-r3",
            ]
        );
    }

    #[test]
    fn dropping_deletes_what_the_run_made() {
        let dir = tempfile::tempdir().unwrap();
        let (provisioned, log) = provision(dir.path(), "never");
        drop(provisioned);
        assert_eq!(deleted(&log).len(), 2 + 4);
    }

    #[test]
    fn deletions_run_side_by_side_within_the_bound_and_every_failure_is_named() {
        // Catches: deletions one after another, which a registry with the
        // corpus's repositories takes minutes to clear; more at once than the
        // bound; and a failure among them lost.
        let dir = tempfile::tempdir().unwrap();
        let (peak, log) = (dir.path().join("peak"), dir.path().join("calls.log"));
        let repositories: String = (1..=20)
            .map(|n| format!("ocync-bench-run-x-r{n}/bench/a\\n"))
            .collect();
        let program = write_stub(
            dir.path(),
            "az",
            &format!(
                "#!/bin/bash\n\
                 case \"$*\" in\n\
                 *list*) printf '{repositories}' ;;\n\
                 *delete*) echo \"$*\" >> '{log}'\n\
                   mkdir '{dir}'/in.$$\n\
                   ls '{dir}' | grep -c '^in[.]' >> '{peak}'\n\
                   sleep 0.2\n\
                   rmdir '{dir}'/in.$$\n\
                   case \"$*\" in *-r7/*|*-r13/*) echo denied >&2; exit 1 ;; esac ;;\n\
                 esac\n",
                log = log.display(),
                dir = dir.path().display(),
                peak = peak.display(),
            ),
        );

        let err = delete_where(&Cli::new(program), &registry(), |_| true).unwrap_err();

        assert!(
            err.starts_with("failed to delete 2 ACR repositories, content left behind: ")
                && err.contains("ocync-bench-run-x-r7/bench/a: ")
                && err.contains("ocync-bench-run-x-r13/bench/a: "),
            "{err}"
        );
        assert_eq!(deleted(&log).len(), 20, "all tried despite the failures");
        let highest = std::fs::read_to_string(&peak)
            .unwrap()
            .lines()
            .map(|n| n.parse::<usize>().unwrap())
            .max()
            .unwrap();
        assert!((2..=8).contains(&highest), "{highest} at once");
    }

    #[test]
    fn a_sweep_that_cannot_list_fails_the_run_before_it_starts() {
        let err = Provisioned::create(
            Cli::new("/nonexistent/az"),
            read_outputs(OUTPUTS).unwrap(),
            &"2026-10-30-120000".parse().unwrap(),
            "3".parse().unwrap(),
        )
        .map(drop)
        .unwrap_err();
        assert!(err.contains("/nonexistent/az acr repository list"), "{err}");
    }
}
