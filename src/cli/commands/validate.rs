//! The `validate` subcommand — offline config validation.

use crate::ValidateArgs;
use crate::cli::config::load_config;

pub(crate) fn run(args: &ValidateArgs) -> i32 {
    match load_config(&args.config) {
        Ok(config) => {
            let n_mappings = config.mappings.len();
            let n_registries = config.registries.len();
            eprintln!(
                "config valid: {n_registries} registries, {n_mappings} mappings ({})",
                args.config.display(),
            );
            0
        }
        Err(err) => {
            eprintln!("error: {err}");
            2
        }
    }
}
