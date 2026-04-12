//! The `watch` subcommand — daemon mode for continuous sync.

use crate::WatchArgs;

pub(crate) async fn run(_args: &WatchArgs) -> i32 {
    eprintln!("watch: not yet implemented");
    1
}
