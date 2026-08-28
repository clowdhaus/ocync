//! The `auth check` subcommand - credential validation.

use std::path::PathBuf;

use crate::cli::config::load_config;
use crate::cli::output::redact_url;
use crate::cli::{CliError, ExitCode, build_registry_client};

/// Run credential checks against all registries in config files.
pub(crate) async fn run_check(configs: &[PathBuf]) -> Result<ExitCode, CliError> {
    let mut all_ok = true;
    // Tracked apart from `all_ok`: a broken config file is a different problem
    // from a rejected credential, and keeps its own exit code.
    let mut config_error = false;

    for path in configs {
        // `-c` is repeatable and the files are independent, so one unreadable
        // config must not skip the registries defined in the others.
        let config = match load_config(path) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("  FAIL  {} -- {err}", path.display());
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
                        eprintln!("  OK    {name} ({safe_url})");
                    }
                    Err(err) => {
                        eprintln!("  FAIL  {name} ({safe_url}) -- {err}");
                        all_ok = false;
                    }
                },
                Err(err) => {
                    eprintln!("  FAIL  {name} -- {err}");
                    all_ok = false;
                }
            }
        }
    }

    if config_error {
        // Reported ahead of a credential failure: an unreadable config is the
        // cause the operator has to fix first.
        Ok(ExitCode::ConfigError)
    } else if all_ok {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::PartialFailure)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// `-c` is repeatable and the files are independent, so a broken one must
    /// not skip the rest, and must keep the config exit code rather than
    /// being flattened into a credential failure.
    #[tokio::test]
    async fn unreadable_config_is_reported_without_skipping_the_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.yaml");
        let good = dir.path().join("good.yaml");
        // Not valid config, so `load_config` fails.
        std::fs::File::create(&bad)
            .and_then(|mut f| f.write_all(b"mappings: [[[["))
            .expect("write bad config");
        // Valid, and defines no registries, so the check does no network I/O.
        std::fs::File::create(&good)
            .and_then(|mut f| f.write_all(b"registries: {}\nmappings: []\n"))
            .expect("write good config");

        let code = run_check(&[bad, good.clone()]).await.expect("runs");

        assert_eq!(
            code,
            ExitCode::ConfigError,
            "a broken config keeps the config exit code"
        );

        // The negative half: on its own the good config is clean, proving the
        // run above actually reached it rather than stopping at the bad one.
        let code = run_check(&[good]).await.expect("runs");
        assert_eq!(code, ExitCode::Success);
    }
}
