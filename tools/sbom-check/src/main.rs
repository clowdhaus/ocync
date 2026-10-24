//! `sbom-check`: confirm release bills of materials list the crates compiled
//! into the binary, including the AWS-LC module it links.
//!
//! The list comes from cargo's dependency graph, which also names crates a
//! weak feature mentions (`ring` through `rustls-webpki`) and crates resolved
//! but not linked (`aws-lc-sys` in a FIPS build). `cargo deny` is the ban
//! check, so this checks only what must be present.
//!
//! A crate of its own so release jobs build it in seconds, before any
//! registry credentials exist.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use serde::Deserialize;

/// The package URL prefix CycloneDX gives a Rust crate. Image bills of
/// materials also list OS packages, which carry other prefixes.
const CARGO_PURL: &str = "pkg:cargo/";

/// Which AWS-LC build the binary must link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Crypto {
    /// The AWS-LC FIPS module, `aws-lc-fips-sys`.
    Fips,
    /// AWS-LC without the FIPS module, `aws-lc-sys`.
    NonFips,
}

impl Crypto {
    fn sys_crate(self) -> &'static str {
        match self {
            Self::Fips => "aws-lc-fips-sys",
            Self::NonFips => "aws-lc-sys",
        }
    }
}

/// Check that CycloneDX bills of materials list `ocync` and the AWS-LC
/// module the build links.
#[derive(Parser)]
#[command(name = "sbom-check")]
struct Args {
    /// The AWS-LC build every binary must link.
    #[arg(long, value_enum)]
    crypto: Crypto,
    /// CycloneDX JSON documents to check.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Document {
    #[serde(default)]
    components: Vec<Component>,
}

#[derive(Deserialize)]
struct Component {
    name: String,
    purl: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let failures: Vec<String> = args
        .paths
        .iter()
        .filter_map(|path| {
            std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|text| check(&text, args.crypto))
                .err()
                .map(|e| format!("{}: {e}", path.display()))
        })
        .collect();
    for failure in &failures {
        eprintln!("error: {failure}");
    }
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn check(text: &str, crypto: Crypto) -> Result<(), String> {
    let document: Document =
        serde_json::from_str(text).map_err(|e| format!("not a CycloneDX document: {e}"))?;
    let crates: BTreeSet<&str> = document
        .components
        .iter()
        .filter(|c| c.purl.as_deref().is_some_and(|p| p.starts_with(CARGO_PURL)))
        .map(|c| c.name.as_str())
        .collect();

    let missing: Vec<String> = ["ocync", "aws-lc-rs", crypto.sys_crate()]
        .into_iter()
        .filter(|name| !crates.contains(name))
        .map(|name| format!("`{name}`"))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "bill of materials ({} crates) is missing {}",
            crates.len(),
            missing.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(crates: &[&str]) -> String {
        let components: Vec<String> = crates
            .iter()
            .map(|n| format!(r#"{{"name":"{n}","purl":"pkg:cargo/{n}@1.0.0"}}"#))
            .chain([r#"{"name":"ca-certificates-bundle","purl":"pkg:apk/wolfi/ca-certificates-bundle@1"}"#.to_string()])
            .collect();
        format!(
            r#"{{"bomFormat":"CycloneDX","components":[{}]}}"#,
            components.join(",")
        )
    }

    #[test]
    fn accepts_a_fips_binary() {
        let text = document(&["ocync", "aws-lc-rs", "aws-lc-fips-sys", "aws-lc-sys"]);
        assert_eq!(check(&text, Crypto::Fips), Ok(()));
    }

    #[test]
    fn accepts_a_non_fips_binary() {
        let text = document(&["ocync", "aws-lc-rs", "aws-lc-sys"]);
        assert_eq!(check(&text, Crypto::NonFips), Ok(()));
    }

    #[test]
    fn rejects_a_binary_built_without_its_dependency_list() {
        assert_eq!(
            check(&document(&[]), Crypto::Fips),
            Err(
                "bill of materials (0 crates) is missing `ocync`, `aws-lc-rs`, `aws-lc-fips-sys`"
                    .to_string()
            )
        );
    }

    #[test]
    fn rejects_a_fips_binary_without_the_fips_module() {
        let text = document(&["ocync", "aws-lc-rs", "aws-lc-sys"]);
        assert_eq!(
            check(&text, Crypto::Fips),
            Err("bill of materials (3 crates) is missing `aws-lc-fips-sys`".to_string())
        );
    }

    #[test]
    fn ignores_os_packages_with_a_crate_name() {
        let text = r#"{"components":[{"name":"ocync","purl":"pkg:apk/wolfi/ocync@1"}]}"#;
        assert_eq!(
            check(text, Crypto::NonFips),
            Err(
                "bill of materials (0 crates) is missing `ocync`, `aws-lc-rs`, `aws-lc-sys`"
                    .to_string()
            )
        );
    }
}
