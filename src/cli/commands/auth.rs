//! The `auth check` subcommand - credential validation.

use std::io::{self, Write};
use std::path::PathBuf;

use crate::cli::config::load_config;
use crate::cli::output::redact_url;
use crate::cli::{CliError, ExitCode, build_registry_client};

/// Run credential checks against all registries in config files.
pub(crate) async fn run_check(configs: &[PathBuf]) -> Result<ExitCode, CliError> {
    run_check_to(configs, &mut io::stderr()).await
}

/// [`run_check`] with the status sink injected, so a test can assert which
/// files and registries were actually reached.
async fn run_check_to<W: Write>(configs: &[PathBuf], out: &mut W) -> Result<ExitCode, CliError> {
    // `ping` treats 401 as reachable, so a denial almost always arrives from
    // client construction. Keeping the classification is what lets a wholly
    // denied check exit 4 instead of a generic partial failure.
    let mut worst = Worst::Clean;

    for path in configs {
        // `-c` is repeatable and the files are independent, so one unreadable
        // config must not skip the registries defined in the others.
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
            match build_registry_client(&reg.url, Some(reg)).await {
                Ok(client) => match client.ping().await {
                    Ok(()) => {
                        let _ = writeln!(out, "  OK    {name} ({safe_url})");
                    }
                    Err(err) => {
                        let _ = writeln!(out, "  FAIL  {name} ({safe_url}) -- {err}");
                        // `ping` accepts 401 as reachable but returns Err on
                        // 403, which is a denial and keeps the auth code.
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

/// Pick the exit code for a completed check.
///
/// Ordered by which cause the operator has to fix first: an unreadable config
/// outranks a denial, which outranks a generic failure.
/// The worst thing a credential check ran into.
///
/// Ordered by which cause the operator has to fix first, so folding is
/// `max` and the precedence lives in the type rather than an if-chain.
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

    /// `-c` is repeatable and the files are independent, so a broken one must
    /// not skip the rest, and must keep the config exit code rather than
    /// being flattened into a credential failure.
    ///
    /// Both files are named in the output, which is what distinguishes
    /// continuing from returning early: an early return would report only the
    /// first and still produce the same exit code.
    #[tokio::test]
    async fn unreadable_config_is_reported_without_skipping_the_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.yaml");
        let good = dir.path().join("good.yaml");
        // Not valid config, so `load_config` fails.
        std::fs::File::create(&bad)
            .and_then(|mut f| f.write_all(b"mappings: [[[["))
            .expect("write bad config");
        // Valid, and names one registry that resolves offline.
        std::fs::File::create(&good)
            .and_then(|mut f| {
                f.write_all(
                    // A URL that fails to parse, so the check reports the
                    // registry without issuing a request: the test must not
                    // depend on name resolution or a network timeout.
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
        // The half an early return would fail: the file *after* the broken one
        // was still checked.
        assert!(
            out.contains("  r "),
            "the registry from the later config was still checked: {out}"
        );
    }

    /// A denied credential keeps the auth exit code rather than collapsing
    /// into the generic partial failure.
    #[test]
    fn auth_failures_outrank_a_generic_partial_failure() {
        assert_eq!(Worst::Denied.exit_code(), ExitCode::AuthError);
        assert_eq!(Worst::Unreadable.exit_code(), ExitCode::ConfigError);
        assert_eq!(Worst::Failed.exit_code(), ExitCode::PartialFailure);
        assert_eq!(Worst::Clean.exit_code(), ExitCode::Success);

        // The ordering is the precedence: a broken config is the cause to fix
        // before a denial, and a denial before a generic failure.
        assert!(Worst::Unreadable > Worst::Denied);
        assert!(Worst::Denied > Worst::Failed);
        assert!(Worst::Failed > Worst::Clean);
    }
}
