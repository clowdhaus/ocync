//! The Artifact Registry repositories a `bench-remote` run syncs into, made and
//! removed on this machine with the operator's `gcloud` sign-in.
//!
//! Artifact Registry keeps a blob after the image that held it is deleted, so
//! a repository that has been used is no longer cold. Every round therefore
//! gets a repository of its own, which the bench service account can write to
//! but not create. The instance only pushes.

use std::ffi::OsString;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::bench::gar::{ArtifactRegistryTarget, Location, Project, RepositoryId};
use crate::bench::{Rounds, RunTimestamp, captured};

/// What every repository this tool makes is named from. A run removes the
/// repositories with this prefix that an earlier run left, and touches no
/// other.
const REPOSITORY_PREFIX: &str = "ocync-bench-run-";

/// What `gcloud` prints, among its error, when the repository it was asked to
/// delete is not there.
const ALREADY_GONE: &str = "NOT_FOUND";

/// File in the temp directory whose lock serializes Artifact Registry runs on
/// this machine.
const LOCAL_LOCK: &str = "ocync-bench-gar.lock";

/// Role that lets the bench service account push.
const WRITER_ROLE: &str = "roles/artifactregistry.writer";

/// Terraform directory whose outputs name the Google side of the bench.
const TERRAFORM_DIR: &str = "bench/terraform/gcp";

/// A service account's email address.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct ServiceAccount(String);

impl TryFrom<String> for ServiceAccount {
    type Error = String;

    fn try_from(email: String) -> Result<Self, String> {
        match email.split_once('@') {
            Some((name, domain))
                if !name.is_empty()
                    && !domain.is_empty()
                    && !email.contains(char::is_whitespace) =>
            {
                Ok(Self(email))
            }
            _ => Err(format!("{email:?} is not an email address")),
        }
    }
}

/// One Terraform output, as `terraform output -json` writes it.
#[derive(Deserialize)]
struct Output<T> {
    value: T,
}

/// The outputs of `bench/terraform/gcp` the run reads.
#[derive(Deserialize)]
struct RawOutputs {
    repository: Output<String>,
    service_account: Output<ServiceAccount>,
    credentials_file: Output<PathBuf>,
}

/// The Google side of the bench: where its repository is and who pushes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outputs {
    /// Location of the bench's repository.
    location: Location,
    /// Project of the bench's repository.
    project: Project,
    /// The identity the instance pushes as.
    service_account: ServiceAccount,
    /// The federation file the instance signs in with. Its contents are never
    /// read or printed here.
    credentials_file: PathBuf,
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
        let path = &raw.repository.value;
        let malformed = |why: String| format!("terraform output `repository` ({path:?}): {why}");
        let mut parts = path.split('/');
        let (Some(host), Some(project), Some(_repository), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed("not <host>/<project>/<repository>".into()));
        };
        Ok(Self {
            location: Location::from_host(host).map_err(&malformed)?,
            project: project.parse().map_err(&malformed)?,
            service_account: raw.service_account.value,
            credentials_file: raw.credentials_file.value,
        })
    }

    /// Reads the outputs of the applied Terraform stack.
    pub(crate) fn read() -> Result<Self, String> {
        let json = captured(Command::new("terraform").args([
            &format!("-chdir={TERRAFORM_DIR}"),
            "output",
            "-json",
        ]))
        .map_err(|e| {
            format!("terraform output -json failed: {e}. Did you run `terraform apply` in {TERRAFORM_DIR}?")
        })?;
        Self::parse(&json)
    }
}

/// Takes the lock that keeps a second Artifact Registry run on this machine out
/// until the returned file is dropped. The sweep of one run would delete the
/// repositories of the other, which may still be provisioning or fetching with
/// no run on the instance for the instance probe to see.
pub(crate) fn lock_local_run() -> Result<File, String> {
    lock_at(&std::env::temp_dir().join(LOCAL_LOCK))
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
            "another Artifact Registry bench-remote run is going on this machine ({}): \
             this one would sweep its repositories",
            path.display()
        )),
        Err(TryLockError::Error(e)) => Err(format!("lock {}: {e}", path.display())),
    }
}

/// The program that talks to Google Cloud.
pub(crate) struct Gcloud(OsString);

impl Gcloud {
    /// The `gcloud` on `PATH`.
    pub(crate) fn installed() -> Self {
        Self("gcloud".into())
    }

    fn run(&self, argv: &[String]) -> Result<String, String> {
        captured(Command::new(&self.0).args(argv)).map_err(|e| {
            format!(
                "{} {} failed: {e}",
                self.0.to_string_lossy(),
                argv.iter().take(3).cloned().collect::<Vec<_>>().join(" ")
            )
        })
    }
}

/// The argv of a `gcloud artifacts repositories` command, with `id` and
/// `flags` after the verb, and the project and location always explicit.
fn repositories_argv(
    verb: &str,
    id: Option<&RepositoryId>,
    flags: &[String],
    target: &ArtifactRegistryTarget,
) -> Vec<String> {
    let mut argv = vec!["artifacts".to_owned(), "repositories".into(), verb.into()];
    argv.extend(id.map(ToString::to_string));
    argv.extend_from_slice(flags);
    argv.extend([
        "--project".to_owned(),
        target.project.to_string(),
        "--location".into(),
        target.location.to_string(),
        "--quiet".into(),
    ]);
    argv
}

/// Lists the ids of the repositories in the target's project and location.
fn list_argv(target: &ArtifactRegistryTarget) -> Vec<String> {
    repositories_argv(
        "list",
        None,
        &["--format=value(name.basename())".into()],
        target,
    )
}

/// Creates the Docker repository `id`.
fn create_argv(target: &ArtifactRegistryTarget, id: &RepositoryId) -> Vec<String> {
    repositories_argv(
        "create",
        Some(id),
        &["--repository-format=docker".into()],
        target,
    )
}

/// Lets `account` push to the repository `id`.
fn bind_argv(
    target: &ArtifactRegistryTarget,
    id: &RepositoryId,
    account: &ServiceAccount,
) -> Vec<String> {
    repositories_argv(
        "add-iam-policy-binding",
        Some(id),
        &[
            format!("--member=serviceAccount:{}", account.0),
            format!("--role={WRITER_ROLE}"),
        ],
        target,
    )
}

/// Deletes the repository `id`, waiting for the deletion to finish.
fn delete_argv(target: &ArtifactRegistryTarget, id: &RepositoryId) -> Vec<String> {
    repositories_argv("delete", Some(id), &[], target)
}

/// The id of round `round`'s repository in the run that started at `timestamp`.
fn repository_id(timestamp: &RunTimestamp, round: usize) -> Result<RepositoryId, String> {
    format!("{REPOSITORY_PREFIX}{timestamp}-r{round}").parse()
}

/// The repositories in a `list` answer that an earlier run made: those whose
/// id starts with [`REPOSITORY_PREFIX`].
fn leftovers(listed: &str) -> Vec<RepositoryId> {
    listed
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(REPOSITORY_PREFIX))
        .filter_map(|line| line.parse().ok())
        .collect()
}

/// Whether the instance's run is known to have ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// The script's own exit was read, or the run never reached the instance.
    Settled,
    /// The connection dropped, so the detached benchmark may still be pushing.
    Unknown,
}

/// The repositories one run made, removed when it ends.
///
/// Dropping deletes whatever [`Self::settle`] has not, so a run that fails or
/// returns early leaves nothing behind. A process that is killed relies on the
/// next run's sweep of [`REPOSITORY_PREFIX`] instead.
pub(crate) struct Provisioned {
    gcloud: Gcloud,
    /// The repositories made so far, round 1 first. Deleting empties it.
    pub(crate) target: ArtifactRegistryTarget,
    service_account: ServiceAccount,
    /// The federation file the instance signs in with.
    pub(crate) credentials_file: PathBuf,
}

impl Provisioned {
    /// Sweeps an earlier run's repositories, then makes one per round and
    /// lets the bench service account push to each. All exist before the
    /// instance is reached, so the grants have time to take effect.
    pub(crate) fn create(
        gcloud: Gcloud,
        outputs: Outputs,
        timestamp: &RunTimestamp,
        rounds: Rounds,
    ) -> Result<Self, String> {
        let Outputs {
            location,
            project,
            service_account,
            credentials_file,
        } = outputs;
        let mut this = Self {
            gcloud,
            target: ArtifactRegistryTarget {
                location,
                project,
                repositories: Vec::new(),
            },
            service_account,
            credentials_file,
        };
        let listed = this.gcloud.run(&list_argv(&this.target))?;
        this.delete_ids(&leftovers(&listed))?;
        for round in 1..=rounds.get() {
            let id = repository_id(timestamp, round)?;
            this.gcloud.run(&create_argv(&this.target, &id))?;
            this.target.repositories.push(id.clone());
            this.gcloud
                .run(&bind_argv(&this.target, &id, &this.service_account))?;
        }
        Ok(this)
    }

    /// Deletes the repositories when the run's end is [`RunEnd::Settled`]; when
    /// it is [`RunEnd::Unknown`], keeps them, since the benchmark may still be
    /// pushing, and says so.
    pub(crate) fn settle(&mut self, end: RunEnd) -> Result<(), String> {
        match end {
            RunEnd::Settled => self.delete(),
            RunEnd::Unknown => {
                let kept = std::mem::take(&mut self.target.repositories);
                eprintln!(
                    "bench-remote: kept {} Artifact Registry repositories because the run may                      still be live on the instance; the next `--target artifact-registry` run                      deletes them once the instance is free",
                    kept.len()
                );
                Ok(())
            }
        }
    }

    /// Deletes every repository this run made.
    fn delete(&mut self) -> Result<(), String> {
        let ids = std::mem::take(&mut self.target.repositories);
        self.delete_ids(&ids)
    }

    /// Deletes each of `ids`, trying all, and names those that stayed. One
    /// that is already gone is deleted.
    fn delete_ids(&self, ids: &[RepositoryId]) -> Result<(), String> {
        let failures: Vec<String> = ids
            .iter()
            .filter_map(|id| {
                self.gcloud
                    .run(&delete_argv(&self.target, id))
                    .err()
                    .filter(|e| !e.contains(ALREADY_GONE))
                    .map(|e| format!("{id}: {e}"))
            })
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "failed to delete {} Artifact Registry repositories, content left behind: {}",
                failures.len(),
                failures.join("; ")
            ))
        }
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
    use super::*;

    const OUTPUTS: &str = r#"{
  "connect": {"sensitive": false, "type": "string", "value": "ssh bench@example.invalid"},
  "credentials_file": {"sensitive": false, "type": "string", "value": "/work/gcp-credentials.json"},
  "repository": {"sensitive": false, "type": "string", "value": "europe-west9-docker.pkg.dev/example-project/ocync-bench"},
  "service_account": {"sensitive": false, "type": "string", "value": "bench@example-project.iam.gserviceaccount.com"}
}"#;

    fn target(repositories: &[&str]) -> ArtifactRegistryTarget {
        ArtifactRegistryTarget {
            location: "europe-west9".parse().unwrap(),
            project: "example-project".parse().unwrap(),
            repositories: repositories.iter().map(|id| id.parse().unwrap()).collect(),
        }
    }

    fn id(raw: &str) -> RepositoryId {
        raw.parse().unwrap()
    }

    fn account() -> ServiceAccount {
        "bench@example-project.iam.gserviceaccount.com"
            .to_owned()
            .try_into()
            .unwrap()
    }

    #[test]
    fn terraform_outputs_are_parsed_and_a_bad_one_is_named() {
        let outputs = Outputs::parse(OUTPUTS).unwrap();
        assert_eq!(outputs.location, "europe-west9".parse().unwrap());
        assert_eq!(outputs.project, "example-project".parse().unwrap());
        assert_eq!(outputs.service_account, account());
        assert_eq!(
            outputs.credentials_file,
            PathBuf::from("/work/gcp-credentials.json")
        );

        let without_account = OUTPUTS.replace("service_account", "other");
        for (name, json, named) in [
            ("missing field", without_account.as_str(), "service_account"),
            (
                "repository without a repository",
                &OUTPUTS.replace("/ocync-bench\"", "\""),
                "`repository`",
            ),
            (
                "repository on another host",
                &OUTPUTS.replace("europe-west9-docker.pkg.dev", "ghcr.io"),
                "`repository`",
            ),
            (
                "account that is no email",
                &OUTPUTS.replace("bench@example-project.iam.gserviceaccount.com", "bench"),
                "not an email address",
            ),
        ] {
            let err = Outputs::parse(json).unwrap_err();
            assert!(err.contains(named), "{name}: {err}");
        }
    }

    #[test]
    fn each_gcloud_command_names_its_project_and_location_and_never_asks() {
        let t = target(&[]);
        let scope = [
            "--project",
            "example-project",
            "--location",
            "europe-west9",
            "--quiet",
        ];
        let id = id("ocync-bench-run-2026-10-30-120000-r1");
        let with_scope = |head: &[&str]| -> Vec<String> {
            head.iter().chain(&scope).map(|s| (*s).to_owned()).collect()
        };
        assert_eq!(
            list_argv(&t),
            with_scope(&[
                "artifacts",
                "repositories",
                "list",
                "--format=value(name.basename())"
            ])
        );
        assert_eq!(
            create_argv(&t, &id),
            with_scope(&[
                "artifacts",
                "repositories",
                "create",
                "ocync-bench-run-2026-10-30-120000-r1",
                "--repository-format=docker",
            ])
        );
        assert_eq!(
            bind_argv(&t, &id, &account()),
            with_scope(&[
                "artifacts",
                "repositories",
                "add-iam-policy-binding",
                "ocync-bench-run-2026-10-30-120000-r1",
                "--member=serviceAccount:bench@example-project.iam.gserviceaccount.com",
                "--role=roles/artifactregistry.writer",
            ])
        );
        assert_eq!(
            delete_argv(&t, &id),
            with_scope(&[
                "artifacts",
                "repositories",
                "delete",
                "ocync-bench-run-2026-10-30-120000-r1",
            ])
        );
    }

    #[test]
    fn a_round_names_its_repository_from_the_run_start_and_the_sweep_keeps_to_that_prefix() {
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        assert_eq!(
            repository_id(&timestamp, 3).unwrap(),
            id("ocync-bench-run-2026-10-30-120000-r3")
        );
        // Catches: a sweep that deletes the bench's own repositories or any
        // other that happens to be in the project.
        let listed = "ocync-bench\nocync-bench-mirror\nocync-bench-run-2026-10-29-090000-r1\n\
                      other-ocync-bench-run-x\nocync-bench-run-2026-10-29-090000-r2\n\n";
        assert_eq!(
            leftovers(listed),
            vec![
                id("ocync-bench-run-2026-10-29-090000-r1"),
                id("ocync-bench-run-2026-10-29-090000-r2"),
            ]
        );
    }

    /// A `gcloud` that logs its argv to `log`, fails a `delete` of `bad`, and
    /// answers a `delete` of a repository named `gone` as Google does when it
    /// is already deleted.
    fn stub_gcloud(dir: &Path, bad: &str) -> (Gcloud, PathBuf) {
        let log = dir.join("calls.log");
        let program = dir.join("gcloud");
        std::fs::write(
            &program,
            format!(
                "#!/bin/bash\necho \"$@\" >> '{}'\n\
                 case \"$*\" in\n\
                 *list*) printf 'ocync-bench-run-old-r1\\nocync-bench-run-old-gone\\nocync-bench\\n' ;;\n\
                 *delete*gone*) echo 'ERROR: NOT_FOUND: Requested entity was not found.' >&2; exit 1 ;;\n\
                 *delete*{bad}*) echo 'denied' >&2; exit 1 ;;\n\
                 esac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        (Gcloud(program.into()), log)
    }

    #[test]
    fn every_repository_is_deleted_when_the_run_ends_and_a_failed_delete_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let (gcloud, log) = stub_gcloud(dir.path(), "-r2");
        let outputs = Outputs::parse(OUTPUTS).unwrap();
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        let rounds: Rounds = "3".parse().unwrap();

        let mut provisioned = Provisioned::create(gcloud, outputs, &timestamp, rounds).unwrap();
        assert_eq!(provisioned.target.repositories.len(), 3);
        let err = provisioned.settle(RunEnd::Settled).unwrap_err();
        assert!(
            err.contains("ocync-bench-run-2026-10-30-120000-r2: ") && err.contains("denied"),
            "{err}"
        );
        assert!(!err.contains("-r1:") && !err.contains("-r3:"), "{err}");
        drop(provisioned);

        let calls = std::fs::read_to_string(&log).unwrap();
        let deleted = |id: &str| {
            calls
                .lines()
                .filter(|l| l.contains("repositories delete") && l.contains(id))
                .count()
        };
        // The sweep removed the leftovers, one of them already gone, and
        // spared the bench's own; the failure of round 2 did not stop rounds 1
        // and 3, and the drop that followed deleted nothing twice.
        assert_eq!(deleted("ocync-bench-run-old-r1"), 1);
        assert_eq!(deleted("ocync-bench-run-old-gone"), 1);
        assert_eq!(deleted("ocync-bench "), 0);
        for round in 1..=3 {
            assert_eq!(deleted(&format!("120000-r{round} ")), 1, "round {round}");
        }
    }

    #[test]
    fn dropping_deletes_what_the_run_made() {
        let dir = tempfile::tempdir().unwrap();
        let (gcloud, log) = stub_gcloud(dir.path(), "never");
        let outputs = Outputs::parse(OUTPUTS).unwrap();
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        drop(Provisioned::create(gcloud, outputs, &timestamp, "3".parse().unwrap()).unwrap());
        let calls = std::fs::read_to_string(&log).unwrap();
        let deleted = calls
            .lines()
            .filter(|l| l.contains("repositories delete ocync-bench-run-2026-10-30-120000-r"))
            .count();
        assert_eq!(deleted, 3, "{calls}");
    }

    #[test]
    fn repositories_are_kept_when_the_end_of_the_run_is_unknown() {
        // Catches: deleting repositories a detached benchmark is still
        // pushing into, by `settle` or by the drop that follows it.
        let dir = tempfile::tempdir().unwrap();
        let (gcloud, log) = stub_gcloud(dir.path(), "never");
        let outputs = Outputs::parse(OUTPUTS).unwrap();
        let timestamp: RunTimestamp = "2026-10-30-120000".parse().unwrap();
        let mut provisioned =
            Provisioned::create(gcloud, outputs, &timestamp, "3".parse().unwrap()).unwrap();
        let before = std::fs::read_to_string(&log).unwrap().lines().count();

        assert_eq!(provisioned.settle(RunEnd::Unknown), Ok(()));
        drop(provisioned);

        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), before, "{calls}");
    }

    #[test]
    fn a_second_local_run_is_refused_until_the_first_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let first = lock_at(&path).unwrap();
        let err = lock_at(&path).unwrap_err();
        assert!(
            err.contains("another Artifact Registry bench-remote run"),
            "{err}"
        );
        drop(first);
        assert!(lock_at(&path).is_ok(), "released with the first run");
    }
}
