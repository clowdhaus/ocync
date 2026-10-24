//! The `version` subcommand - shows version and build information.

use ocync_distribution::TlsFips;

use crate::cli::ExitCode;

/// Print version and FIPS 140-3 status to stdout.
pub(crate) fn run() -> ExitCode {
    println!("{}", report());
    ExitCode::Success
}

/// The version report. A FIPS build names the linked module, so the
/// matching validation certificate can be looked up, and the runtime TLS
/// state, in the shape `openssl list -providers` uses.
fn report() -> String {
    let version = env!("CARGO_PKG_VERSION");
    if !cfg!(feature = "fips") {
        return format!("ocync {version}\nFIPS 140-3: disabled");
    }
    let tls = match ocync_distribution::tls_fips() {
        TlsFips::Approved => "approved",
        TlsFips::NotApproved => "not-approved",
    };
    format!(
        "ocync {version}\nFIPS 140-3: enabled\n  module: {}\n  tls: {tls}",
        ocync_distribution::crypto_module()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_returns_success() {
        assert_eq!(run(), ExitCode::Success);
    }

    #[cfg(feature = "fips")]
    #[test]
    fn fips_report_names_the_module_and_tls_state() {
        ocync_distribution::install_crypto_provider();
        let report = report();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[1], "FIPS 140-3: enabled");
        assert!(lines[2].starts_with("  module: AWS-LC FIPS "), "{report}");
        assert_eq!(lines[3], "  tls: approved");
    }

    #[cfg(not(feature = "fips"))]
    #[test]
    fn non_fips_report_says_disabled() {
        assert!(report().ends_with("\nFIPS 140-3: disabled"));
    }
}
