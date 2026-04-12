//! The `copy` subcommand — copies a single image between registries.

use crate::CopyArgs;
use crate::cli::{CliError, ExitCode};

pub(crate) async fn run(_args: &CopyArgs) -> Result<ExitCode, CliError> {
    eprintln!("copy: not yet implemented (requires sync engine)");
    Ok(ExitCode::Failure)
}
