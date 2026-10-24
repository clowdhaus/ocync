//! The `version` subcommand - shows version and build information.

use ocync_distribution::{
    CRYPTO_BUILD, Certification, CryptoBuild, MODULES_IN_PROCESS_URL, TlsFips,
};

use crate::cli::ExitCode;

/// Print version and FIPS 140-3 status to stdout.
pub(crate) fn run() -> ExitCode {
    println!("{}", report());
    ExitCode::Success
}

/// The version report, in the shape `openssl list -providers` uses. A FIPS
/// build names the module as AWS-LC's `openssl version -fips` does, since
/// the module number is what a NIST certificate names, its certification
/// status, and the runtime TLS state.
fn report() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let release = ocync_distribution::awslc_release();
    match CRYPTO_BUILD {
        CryptoBuild::NonFips => {
            format!("ocync {version}\nFIPS 140-3: disabled\n  release: AWS-LC {release}")
        }
        CryptoBuild::Fips => {
            let module = ocync_distribution::fips_module()
                .map_or_else(|| "unknown".to_string(), |m| m.to_string());
            let certificate = match ocync_distribution::certification() {
                Some(Certification::InProcess) => {
                    format!("none yet, module in process ({MODULES_IN_PROCESS_URL})")
                }
                None => "no record for this module".to_string(),
            };
            let tls = match ocync_distribution::tls_fips() {
                TlsFips::Approved => "approved",
                TlsFips::NotApproved => "not-approved",
            };
            format!(
                "ocync {version}\nFIPS 140-3: enabled\n  module: AWS-LC FIPS module {module}\n  \
                 release: AWS-LC {release}\n  certificate: {certificate}\n  tls: {tls}"
            )
        }
    }
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
        assert!(
            lines[2].starts_with("  module: AWS-LC FIPS module "),
            "{report}"
        );
        assert!(!lines[2].ends_with("unknown"), "{report}");
        assert!(lines[3].starts_with("  release: AWS-LC "), "{report}");
        assert_eq!(
            lines[4],
            format!("  certificate: none yet, module in process ({MODULES_IN_PROCESS_URL})")
        );
        assert_eq!(lines[5], "  tls: approved");
    }

    #[cfg(not(feature = "fips"))]
    #[test]
    fn non_fips_report_says_disabled() {
        let report = report();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[1], "FIPS 140-3: disabled");
        assert!(lines[2].starts_with("  release: AWS-LC "), "{report}");
        assert_eq!(lines.len(), 3, "{report}");
    }
}
