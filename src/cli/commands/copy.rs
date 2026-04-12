//! The `copy` subcommand — copies a single image between registries.

use crate::CopyArgs;
use crate::cli::{CliError, ExitCode};

pub(crate) async fn run(_args: &CopyArgs) -> Result<ExitCode, CliError> {
    Err(CliError::Input(
        "copy command not yet implemented".to_string(),
    ))
}
