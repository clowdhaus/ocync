//! The `sync` subcommand — runs all mappings from config.

use crate::SyncArgs;
use crate::cli::{CliError, ExitCode};

pub(crate) async fn run(_args: &SyncArgs) -> Result<ExitCode, CliError> {
    eprintln!("sync: not yet implemented (requires sync engine)");
    Ok(ExitCode::Failure)
}
