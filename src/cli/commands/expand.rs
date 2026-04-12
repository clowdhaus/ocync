//! The `expand` subcommand — shows resolved config with env var expansion.

use crate::ExpandArgs;
use crate::cli::config::expand_config_for_display;
use crate::cli::{CliError, ExitCode};

pub(crate) fn run(args: &ExpandArgs) -> Result<ExitCode, CliError> {
    let yaml = expand_config_for_display(&args.config, args.show_secrets)?;
    print!("{yaml}");
    Ok(ExitCode::Success)
}
