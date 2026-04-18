//! The `version` subcommand - shows version and build information.

use crate::cli::ExitCode;

/// Print version and FIPS compilation status to stdout.
pub(crate) fn run() -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    println!("ocync {version}");

    let fips_compiled = if cfg!(feature = "fips") { "yes" } else { "no" };
    println!("FIPS 140-3: compiled={fips_compiled}");

    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_returns_success() {
        assert_eq!(run(), ExitCode::Success);
    }
}
