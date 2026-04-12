//! The `watch` subcommand — daemon mode for continuous sync.

use crate::WatchArgs;
use crate::cli::{CliError, ExitCode};

pub(crate) async fn run(_args: &WatchArgs) -> Result<ExitCode, CliError> {
    eprintln!("watch: not yet implemented (requires sync engine)");
    Ok(ExitCode::Failure)
}
