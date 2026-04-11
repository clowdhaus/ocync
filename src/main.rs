//! OCI container image sync tool.

mod cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ocync", version, about = "OCI registry sync tool")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Quiet mode (errors only)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Override log format (json or text)
    #[arg(long, global = true)]
    pub log_format: Option<String>,

    /// Write detailed logs to file
    #[arg(long, global = true)]
    pub log_file: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run all mappings from config
    Sync {
        #[arg(short, long, required = true)]
        config: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        output: Option<String>,
        #[arg(long)]
        output_format: Option<String>,
    },
    /// Copy a single image between registries
    Copy { source: String, destination: String },
    /// List and filter tags
    Tags {
        repository: String,
        #[arg(long)]
        glob: Option<String>,
        #[arg(long)]
        semver: Option<String>,
        #[arg(long)]
        exclude: Option<String>,
        #[arg(long)]
        sort: Option<String>,
        #[arg(long)]
        latest: Option<usize>,
    },
    /// Validate credentials
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Offline config validation
    Validate { config: String },
    /// Show resolved config
    Expand {
        config: String,
        #[arg(long)]
        show_secrets: bool,
    },
    /// Daemon mode
    Watch {
        #[arg(short, long, required = true)]
        config: Vec<String>,
    },
    /// Show version and FIPS status
    Version,
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Check credentials for all registries
    Check,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    cli::setup_logging(&cli);

    let exit_code = match cli.command {
        Commands::Sync {
            config,
            dry_run,
            output,
            output_format,
        } => cli::commands::sync_cmd::run(config, dry_run, output, output_format).await,
        Commands::Copy {
            source,
            destination,
        } => cli::commands::copy::run(&source, &destination).await,
        Commands::Tags {
            repository,
            glob,
            semver,
            exclude,
            sort,
            latest,
        } => cli::commands::tags::run(&repository, glob, semver, exclude, sort, latest).await,
        Commands::Auth { action } => match action {
            AuthAction::Check => cli::commands::auth::run_check().await,
        },
        Commands::Validate { config } => cli::commands::validate::run(&config),
        Commands::Expand {
            config,
            show_secrets,
        } => cli::commands::expand::run(&config, show_secrets),
        Commands::Watch { config } => cli::commands::watch::run(config).await,
        Commands::Version => cli::commands::version::run(),
    };

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_sync() {
        let cli = Cli::parse_from(["ocync", "sync", "--config", "c.yaml"]);
        assert!(matches!(cli.command, Commands::Sync { .. }));
    }

    #[test]
    fn parse_sync_dry_run() {
        let cli = Cli::parse_from(["ocync", "sync", "--config", "c.yaml", "--dry-run"]);
        if let Commands::Sync { dry_run, .. } = cli.command {
            assert!(dry_run);
        }
    }

    #[test]
    fn parse_copy() {
        let cli = Cli::parse_from(["ocync", "copy", "src", "dst"]);
        assert!(matches!(cli.command, Commands::Copy { .. }));
    }

    #[test]
    fn parse_tags() {
        let cli = Cli::parse_from(["ocync", "tags", "docker.io/nginx"]);
        assert!(matches!(cli.command, Commands::Tags { .. }));
    }

    #[test]
    fn parse_verbose() {
        let cli = Cli::parse_from(["ocync", "-vvv", "sync", "--config", "c.yaml"]);
        assert_eq!(cli.verbose, 3);
    }

    #[test]
    fn parse_quiet() {
        let cli = Cli::parse_from(["ocync", "-q", "sync", "--config", "c.yaml"]);
        assert!(cli.quiet);
    }

    #[test]
    fn parse_log_format() {
        let cli = Cli::parse_from([
            "ocync",
            "--log-format",
            "json",
            "sync",
            "--config",
            "c.yaml",
        ]);
        assert_eq!(cli.log_format.as_deref(), Some("json"));
    }
}
