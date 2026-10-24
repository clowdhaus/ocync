//! xtask -- workspace automation commands.

mod bench;
mod sbom;

use std::process::ExitCode;

use clap::Parser;

/// Workspace automation commands for ocync.
#[derive(Parser)]
#[command(name = "xtask")]
enum Cli {
    /// Run benchmark suite comparing ocync against dregsy and regsync.
    Bench(bench::BenchArgs),
    /// Run benchmarks on a remote bench instance via SSH.
    BenchRemote(bench::remote::BenchRemoteArgs),
    /// Check a release bill of materials against the crates the binary must
    /// and must not contain.
    SbomCheck(sbom::SbomCheckArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result = match cli {
        Cli::Bench(args) => rt.block_on(bench::run(args)),
        Cli::BenchRemote(args) => bench::remote::run(args),
        Cli::SbomCheck(args) => sbom::run(&args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
