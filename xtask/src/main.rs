//! xtask -- workspace automation commands.

mod bench;

use std::process::ExitCode;

use clap::Parser;

/// Workspace automation commands for ocync.
#[derive(Parser)]
#[command(name = "xtask")]
enum Cli {
    /// Run benchmark suite comparing ocync against dregsy and regsync.
    Bench(bench::BenchArgs),
    /// Run benchmarks on the remote EC2 bench instance via SSM.
    BenchRemote(bench::remote::BenchRemoteArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result = match cli {
        Cli::Bench(args) => rt.block_on(bench::run(args)),
        Cli::BenchRemote(args) => rt.block_on(bench::remote::run(args)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
