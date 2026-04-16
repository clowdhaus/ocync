//! xtask — workspace automation commands.

mod bench;
mod probe;

use std::process::ExitCode;

use clap::Parser;

/// Workspace automation commands for ocync.
#[derive(Parser)]
#[command(name = "xtask")]
enum Cli {
    /// Run benchmark suite comparing ocync against dregsy and regsync.
    Bench(bench::BenchArgs),
    /// Probe a registry's cross-repo mount behavior (currently ECR only).
    Probe(probe::ProbeArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result: Result<(), Box<dyn std::error::Error>> = match cli {
        Cli::Bench(args) => rt.block_on(bench::run(args)),
        Cli::Probe(args) => rt.block_on(probe::run(args)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
