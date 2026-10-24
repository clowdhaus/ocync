//! The `version` subcommand - shows version and build information.

use ocync_distribution::TlsFips;

use crate::cli::ExitCode;

/// Print version, FIPS compilation status and runtime TLS FIPS status to stdout.
pub(crate) fn run() -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    println!("ocync {version}");

    let fips_compiled = if cfg!(feature = "fips") { "yes" } else { "no" };
    let tls = match ocync_distribution::tls_fips() {
        TlsFips::Approved => "approved",
        TlsFips::NotApproved => "not-approved",
    };
    println!("FIPS 140-3: compiled={fips_compiled} tls={tls}");

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
