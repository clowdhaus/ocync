//! xtask — workspace automation commands.

mod bench;

use std::process::ExitCode;

use clap::Parser;

/// Workspace automation commands for ocync.
#[derive(Parser)]
#[command(name = "xtask")]
enum Cli {
    /// Run benchmark suite comparing ocync against dregsy and regsync.
    Bench(bench::BenchArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli {
        Cli::Bench(args) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");

            match rt.block_on(bench::run(args)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
