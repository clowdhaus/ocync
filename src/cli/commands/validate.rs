//! The `validate` subcommand — offline config validation.

use crate::ValidateArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode};

pub(crate) fn run(args: &ValidateArgs) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;
    let n_mappings = config.mappings.len();
    let n_registries = config.registries.len();
    eprintln!(
        "config valid: {n_registries} registries, {n_mappings} mappings ({})",
        args.config.display(),
    );
    Ok(ExitCode::Success)
}
