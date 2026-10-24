//! `sbom`: finish the CycloneDX bill of materials for a release binary or
//! image.
//!
//! syft reads the dependency list `cargo auditable` embeds, which comes from
//! `cargo metadata`. That lists an optional dependency whenever a weak
//! feature (`dep?/feature`) names it, even when nothing enables it, so it
//! names crates `ocync` never compiles. `finish` keeps only the crates cargo's resolver
//! compiles for `ocync`, drops `aws-lc-sys` from a FIPS build once the binary
//! shows it carries no non-FIPS AWS-LC, names the linked AWS-LC library, and
//! checks the result describes the binary being released. A crate of its own
//! so release jobs build it in seconds, before any registry credentials
//! exist.

mod binary;
mod document;
mod resolver;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Which AWS-LC build the binary must link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Crypto {
    /// The AWS-LC FIPS module, `aws-lc-fips-sys`.
    Fips,
    /// AWS-LC without the FIPS module, `aws-lc-sys`.
    NonFips,
}

impl Crypto {
    /// The crate that builds the AWS-LC library this build links.
    fn sys_crate(self) -> &'static str {
        match self {
            Self::Fips => "aws-lc-fips-sys",
            Self::NonFips => "aws-lc-sys",
        }
    }

    /// A crate cargo resolves for this build that the binary does not link.
    /// `rustls` enables `aws-lc-rs/aws-lc-sys` even when the FIPS module is
    /// selected.
    fn resolved_not_linked(self) -> Option<&'static str> {
        match self {
            Self::Fips => Some("aws-lc-sys"),
            Self::NonFips => None,
        }
    }

    /// `cargo` arguments selecting this build's features.
    fn cargo_features(self) -> &'static [&'static str] {
        match self {
            Self::Fips => &[],
            Self::NonFips => &["--no-default-features", "--features", "non-fips"],
        }
    }
}

/// Finish the bill of materials for one release binary or image.
#[derive(Parser)]
#[command(name = "sbom")]
struct Args {
    /// The AWS-LC build the binary must link.
    #[arg(long, value_enum)]
    crypto: Crypto,
    /// The `ocync` binary the bill of materials describes. For an image, the
    /// binary copied out of it.
    #[arg(long)]
    binary: PathBuf,
    /// syft's CycloneDX JSON for the binary or image.
    #[arg(long)]
    raw: PathBuf,
    /// Where to write the finished CycloneDX JSON.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let bytes =
        std::fs::read(&args.binary).map_err(|e| format!("{}: {e}", args.binary.display()))?;
    let aws_lc = binary::aws_lc(&bytes, args.crypto)?;
    let raw = read(&args.raw)?;
    let compiled = resolver::compiled(args.crypto)?;
    let finished = document::finish(&raw, &compiled, &aws_lc, args.crypto)?;
    write(&args.out, &finished)
}

fn read(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &std::path::Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}
