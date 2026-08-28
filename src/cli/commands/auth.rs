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
    let mut all_ok = true;
    // Tracked apart from `all_ok`: a broken config file is a different problem
    // from a rejected credential, and keeps its own exit code.
    let mut config_error = false;
    // `ping` treats 401 as reachable, so a denial almost always arrives from
    // client construction. Keeping its classification is what lets a wholly
    // denied check exit 4 instead of a generic partial failure.
    let mut auth_error = false;

    for path in configs {
        // `-c` is repeatable and the files are independent, so one unreadable
        // config must not skip the registries defined in the others.
        let config = match load_config(path) {
            Ok(config) => config,
            Err(err) => {
                let _ = writeln!(out, "  FAIL  {} -- {err}", path.display());
                config_error = true;
                all_ok = false;
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
                        all_ok = false;
                    }
                },
                Err(err) => {
                    let _ = writeln!(out, "  FAIL  {name} -- {err}");
                    if matches!(err.exit_code(), ExitCode::AuthError) {
                        auth_error = true;
                    }
                    all_ok = false;
                }
            }
        }
    }

    Ok(classify(config_error, all_ok, auth_error))
}

/// Pick the exit code for a completed check.
///
/// Ordered by which cause the operator has to fix first: an unreadable config
/// outranks a denial, which outranks a generic failure.
fn classify(config_error: bool, all_ok: bool, auth_error: bool) -> ExitCode {
    if config_error {
        ExitCode::ConfigError
    } else if all_ok {
        ExitCode::Success
    } else if auth_error {
        ExitCode::AuthError
    } else {
        ExitCode::PartialFailure
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
                    b"registries:\n  r:\n    url: unreachable.invalid\n    auth_type: static_token\n    token: t\nmappings: []\n",
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
        assert_eq!(
            classify(false, false, true),
            ExitCode::AuthError,
            "a denial reports as one"
        );
        assert_eq!(
            classify(true, false, true),
            ExitCode::ConfigError,
            "a broken config is the cause to fix first"
        );
        assert_eq!(classify(false, false, false), ExitCode::PartialFailure);
        assert_eq!(classify(false, true, false), ExitCode::Success);
    }
}
