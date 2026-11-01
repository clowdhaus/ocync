//! xtask -- workspace automation commands.
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests read JSON they wrote themselves"
    )
)]

mod bench;

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
    /// Resolve the bench corpus to manifest digests and write `bench/corpus.lock.json`.
    BenchLock(bench::lock::LockArgs),
    /// Run skopeo with `--preserve-digests` on `copy`; dregsy's skopeo in the benchmark.
    #[command(hide = true)]
    BenchSkopeo {
        /// Arguments for skopeo.
        #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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
        Cli::BenchLock(args) => rt.block_on(bench::lock::run(args)),
        Cli::BenchSkopeo { args } => bench::skopeo_shim::run(args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
