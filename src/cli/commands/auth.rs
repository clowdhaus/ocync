//! The `auth check` subcommand - credential validation.

use std::path::PathBuf;

use crate::cli::config::load_config;
use crate::cli::output::redact_url;
use crate::cli::{CliError, ExitCode, build_registry_client};

/// Run credential checks against all registries in config files.
pub(crate) async fn run_check(configs: &[PathBuf]) -> Result<ExitCode, CliError> {
    let mut all_ok = true;

    for path in configs {
        // `-c` is repeatable and the files are independent, so one unreadable
        // config must not skip the registries defined in the others.
        let config = match load_config(path) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("  FAIL  {} -- {err}", path.display());
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

    if all_ok {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::PartialFailure)
    }
}
