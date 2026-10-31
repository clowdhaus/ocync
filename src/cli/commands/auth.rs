//! The `auth check` subcommand: credential validation.

use std::io::{self, Write};
use std::path::PathBuf;

use ocync_distribution::auth::Presents;

use crate::cli::config::load_config;
use crate::cli::output::redact_url;
use crate::cli::{CliError, ExitCode, Registry, RunCredentials, build_registry_client};

/// Run credential checks against all registries in config files.
pub(crate) async fn run_check(configs: &[PathBuf]) -> Result<ExitCode, CliError> {
    run_check_to(configs, &mut io::stderr()).await
}

/// [`run_check`] with the status sink injected, so a test can assert which
/// files and registries were reached.
async fn run_check_to<W: Write>(configs: &[PathBuf], out: &mut W) -> Result<ExitCode, CliError> {
    // `ping` treats 401 as reachable, so a denial almost always arrives from
    // client construction; keeping its classification lets a wholly denied
    // check exit 4.
    let mut worst = Worst::Clean;
    let credentials = RunCredentials::default();

    for path in configs {
        // Files are independent: one unreadable config must not skip the others.
        let config = match load_config(path) {
            Ok(config) => config,
            Err(err) => {
                let _ = writeln!(out, "  FAIL  {} -- {err}", path.display());
                worst = worst.max(Worst::Unreadable);
                continue;
            }
        };

        for (name, reg) in &config.registries {
            let safe_url = redact_url(&reg.url);
            match build_registry_client(Registry::Configured(reg), &credentials).await {
                Ok(client) => match client.ping().await {
                    // `ping` accepts 401, so a registry read without credentials
                    // answers it too: say so rather than imply they work.
                    Ok(()) if client.auth_presents() == Presents::Nothing => {
                        let _ = writeln!(
                            out,
                            "  OK    {name} ({safe_url}) -- anonymous, no credentials checked"
                        );
                    }
                    Ok(()) => {
                        let _ = writeln!(out, "  OK    {name} ({safe_url})");
                    }
                    Err(err) => {
                        let _ = writeln!(out, "  FAIL  {name} ({safe_url}) -- {err}");
                        // `ping` accepts 401 but errs on 403, a denial.
                        worst = worst.max(if err.is_auth_error() {
                            Worst::Denied
                        } else {
                            Worst::Failed
                        });
                    }
                },
                Err(err) => {
                    let _ = writeln!(out, "  FAIL  {name} -- {err}");
                    worst = worst.max(if matches!(err.exit_code(), ExitCode::AuthError) {
                        Worst::Denied
                    } else {
                        Worst::Failed
                    });
                }
            }
        }
    }

    Ok(worst.exit_code())
}

/// The worst thing a credential check ran into.
///
/// Ordered by which cause the operator must fix first (an unreadable config,
/// then a denial, then a generic failure), so folding is `max`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Worst {
    /// Every registry answered.
    #[default]
    Clean,
    /// A registry failed for a reason that is not a denial.
    Failed,
    /// Credentials were rejected.
    Denied,
    /// A config file could not be read at all.
    Unreadable,
}

impl Worst {
    fn exit_code(self) -> ExitCode {
        match self {
            Self::Clean => ExitCode::Success,
            Self::Failed => ExitCode::PartialFailure,
            Self::Denied => ExitCode::AuthError,
            Self::Unreadable => ExitCode::ConfigError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A broken config must not skip the rest, and keeps the config exit code.
    /// Both files appearing in the output distinguishes continuing from an
    /// early return, which would give the same exit code.
    #[tokio::test]
    async fn unreadable_config_is_reported_without_skipping_the_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.yaml");
        let good = dir.path().join("good.yaml");
        std::fs::File::create(&bad)
            .and_then(|mut f| f.write_all(b"mappings: [[[["))
            .expect("write bad config");
        std::fs::File::create(&good)
            .and_then(|mut f| {
                f.write_all(
                    // A URL that fails to parse, so no request is issued.
                    b"registries:\n  r:\n    url: \"exa mple\"\n    auth_type: static_token\n    token: t\nmappings: []\n",
                )
            })
            .expect("write good config");

        let mut out = Vec::new();
        let code = run_check_to(&[bad.clone(), good], &mut out)
            .await
            .expect("runs");
        let out = String::from_utf8(out).expect("utf8");

        assert_eq!(
            code,
            ExitCode::ConfigError,
            "a broken config keeps the config exit code"
        );
        assert!(
            out.contains("bad.yaml"),
            "the broken file is reported: {out}"
        );
        // The file after the broken one was still checked.
        assert!(
            out.contains("  r "),
            "the registry from the later config was still checked: {out}"
        );
    }

    /// A denial keeps the auth exit code instead of collapsing to partial failure.
    #[test]
    fn auth_failures_outrank_a_generic_partial_failure() {
        assert_eq!(Worst::Denied.exit_code(), ExitCode::AuthError);
        assert_eq!(Worst::Unreadable.exit_code(), ExitCode::ConfigError);
        assert_eq!(Worst::Failed.exit_code(), ExitCode::PartialFailure);
        assert_eq!(Worst::Clean.exit_code(), ExitCode::Success);

        // The ordering is the precedence.
        assert!(Worst::Unreadable > Worst::Denied);
        assert!(Worst::Denied > Worst::Failed);
        assert!(Worst::Failed > Worst::Clean);
    }

    /// A registry read anonymously says so, and one with credentials does
    /// not: `ping` accepts 401, so a bare OK would imply credentials were
    /// checked. Breaks if an anonymous registry prints a plain OK, or one
    /// with credentials is called anonymous.
    #[tokio::test]
    async fn a_registry_read_without_credentials_is_not_reported_as_a_plain_ok() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.yaml");
        std::fs::write(
            &config,
            format!(
                "registries:\n  open:\n    url: {uri}\n    auth_type: anonymous\n  \
                 keyless:\n    url: {uri}\n    auth_type: docker_config\n  \
                 signed:\n    url: {uri}\n    auth_type: static_token\n    token: t\nmappings: []\n",
                uri = server.uri()
            ),
        )
        .expect("write config");
        // A docker config with no entry for the registry: named
        // `docker-config`, but nothing is presented.
        let docker = tempfile::tempdir().expect("tempdir");
        std::fs::write(docker.path().join("config.json"), "{}").expect("write config.json");

        let _docker_config = crate::cli::test_support::EnvGuard::set_docker_config(docker.path());
        let mut out = Vec::new();
        let code = run_check_to(&[config], &mut out).await.expect("runs");
        let out = String::from_utf8(out).expect("utf8");

        assert_eq!(code, ExitCode::Success, "{out}");
        let line = |name: &str| {
            out.lines()
                .find(|line| line.contains(&format!("  {name} ")))
                .unwrap_or_else(|| panic!("no line for {name}: {out}"))
        };
        assert!(line("open").contains("anonymous"), "{out}");
        assert!(line("keyless").contains("anonymous"), "{out}");
        assert!(!line("signed").contains("anonymous"), "{out}");
    }
}
