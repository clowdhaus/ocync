//! The `expand` subcommand — shows resolved config with env var expansion.

use crate::ExpandArgs;
use crate::cli::config::{expand_config, load_config};

pub(crate) fn run(args: &ExpandArgs) -> i32 {
    let config = match load_config(&args.config) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err}");
            return 2;
        }
    };

    match expand_config(&config, args.show_secrets) {
        Ok(yaml) => {
            print!("{yaml}");
            0
        }
        Err(err) => {
            eprintln!("error: {err}");
            2
        }
    }
}
