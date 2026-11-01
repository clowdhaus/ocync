//! The ACR repositories a `bench-remote` run syncs into, removed on this
//! machine with the operator's `az` sign-in.
//!
//! ACR keeps a blob after the image that held it is deleted, so a repository
//! that has been used is no longer cold. Every round therefore syncs under a
//! path prefix of its own. A first push creates the repositories below it, so
//! nothing is made up front and the instance's identity, which can push but not
//! delete, needs nothing more. The run ends by deleting them here.

use serde::Deserialize;

use crate::bench::acr::{AcrTarget, Guid, RegistryName};
use crate::bench::provision::{Cli, Output, RunEnd, repository_id, terraform_output, with_prefix};
use crate::bench::{Rounds, RunTimestamp};

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
    /// Reads the outputs from `terraform output -json`, naming the one that
    /// is missing or malformed.
    #[expect(
        clippy::disallowed_methods,
        reason = "reads Terraform's own output, which holds no credential"
    )]
    pub(crate) fn parse(json: &str) -> Result<Self, String> {
        let raw: RawOutputs = serde_json::from_str(json)
            .map_err(|e| format!("terraform output of {TERRAFORM_DIR}: {e}"))?;
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

    /// Reads the outputs of the applied Terraform stack.
    pub(crate) fn read() -> Result<Self, String> {
        Self::parse(&terraform_output(TERRAFORM_DIR)?)
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
/// and that `keep` selects, trying all, and names those that stayed.
fn delete_where(
    az: &Cli,
    registry: &RegistryName,
    keep: impl Fn(&str) -> bool,
) -> Result<(), String> {
    let listed = az.run(&list_argv(registry))?;
    let failures: Vec<String> = with_prefix(&listed)
        .filter(|repository| keep(repository))
        .filter_map(|repository| {
            az.run(&delete_argv(registry, repository))
                .err()
                .map(|e| format!("{repository}: {e}"))
        })
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to delete {} ACR repositories, content left behind: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

/// The repositories one run made, removed when it ends.
///
/// Dropping deletes whatever [`Self::settle`] has not, so a run that fails or
/// returns early leaves nothing behind. A process that is killed relies on the
/// next run's sweep of [`crate::bench::provision::REPOSITORY_PREFIX`] instead.
pub(crate) struct Provisioned {
    az: Cli,
    /// The run's path prefixes, round 1 first. Deleting empties them.
    pub(crate) target: AcrTarget,
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
        let prefixes = (1..=rounds.get())
            .map(|round| repository_id(timestamp, round))
            .collect::<Result<_, _>>()?;
        delete_where(&az, &registry, |_| true)?;
        Ok(Self {
            az,
            target: AcrTarget {
                registry,
                tenant,
                client,
                prefixes,
            },
        })
    }

    /// Deletes the repositories when the run's end is [`RunEnd::Settled`]; when
    /// it is [`RunEnd::Unknown`], keeps them, since the benchmark may still be
    /// pushing, and says so.
    pub(crate) fn settle(&mut self, end: RunEnd) -> Result<(), String> {
        match end {
            RunEnd::Settled => self.delete(),
            RunEnd::Unknown => {
                let kept = std::mem::take(&mut self.target.prefixes);
                eprintln!(
                    "bench-remote: kept the ACR repositories under {} prefixes because the run \
                     may still be live on the instance; the next `--target acr` run deletes \
                     them once the instance is free",
                    kept.len()
                );
                Ok(())
            }
        }
    }

    /// Deletes every repository below this run's prefixes.
    fn delete(&mut self) -> Result<(), String> {
        let prefixes: Vec<String> = std::mem::take(&mut self.target.prefixes)
            .iter()
            .map(ToString::to_string)
            .collect();
        if prefixes.is_empty() {
            return Ok(());
        }
        delete_where(&self.az, &self.target.registry, |repository| {
            let first = repository.split('/').next().unwrap_or(repository);
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
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const OUTPUTS: &str = r#"{
  "connect": {"sensitive": false, "type": "string", "value": "ssh bench@example.invalid"},
  "client_id": {"sensitive": false, "type": "string", "value": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"},
  "registry": {"sensitive": false, "type": "string", "value": "ocyncbench1a2b3c.azurecr.io"},
  "tenant_id": {"sensitive": false, "type": "string", "value": "11111111-2222-3333-4444-555555555555"}
}"#;

    fn registry() -> RegistryName {
        RegistryName::from_host("ocyncbench1a2b3c.azurecr.io").unwrap()
    }

    #[test]
    fn terraform_outputs_are_parsed_and_a_bad_one_is_named() {
        let outputs = Outputs::parse(OUTPUTS).unwrap();
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
            let err = Outputs::parse(json).unwrap_err();
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
        let program = dir.join("az");
        std::fs::write(
            &program,
            format!(
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
        )
        .unwrap();
        std::fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        (Cli::new(program), log)
    }

    fn provision(dir: &Path, bad: &str) -> (Provisioned, PathBuf) {
        let (az, log) = stub_az(dir, bad);
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        let outputs = Outputs::parse(OUTPUTS).unwrap();
        let provisioned =
            Provisioned::create(az, outputs, &timestamp, "3".parse().unwrap()).unwrap();
        (provisioned, log)
    }

    /// The repositories the log shows deleted, in order.
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
        assert_eq!(provisioned.target.prefixes.len(), 3, "one prefix per round");
        assert_eq!(
            deleted(&log),
            [
                "ocync-bench-run-old-r1/bench/a",
                "ocync-bench-run-old-r1/bench/b"
            ]
        );

        let err = provisioned.settle(RunEnd::Settled).unwrap_err();
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
        assert_eq!(
            deleted(&log)[2..],
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
    fn repositories_are_kept_when_the_end_of_the_run_is_unknown() {
        // Catches: deleting repositories a detached benchmark is still
        // pushing into, by `settle` or by the drop that follows it.
        let dir = tempfile::tempdir().unwrap();
        let (mut provisioned, log) = provision(dir.path(), "never");
        let before = std::fs::read_to_string(&log).unwrap().lines().count();

        assert_eq!(provisioned.settle(RunEnd::Unknown), Ok(()));
        drop(provisioned);

        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), before, "{calls}");
    }

    #[test]
    fn a_sweep_that_cannot_list_fails_the_run_before_it_starts() {
        let err = Provisioned::create(
            Cli::new("/nonexistent/az"),
            Outputs::parse(OUTPUTS).unwrap(),
            &"2026-10-30-120000".parse().unwrap(),
            "3".parse().unwrap(),
        )
        .map(drop)
        .unwrap_err();
        assert!(err.contains("/nonexistent/az acr repository list"), "{err}");
    }
}
