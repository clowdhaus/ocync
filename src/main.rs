//! OCI container image sync tool.

mod cli;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use ocync_distribution::Reference;

/// OCI registry sync tool.
#[derive(Debug, Parser)]
#[command(name = "ocync", version, about = "OCI registry sync tool")]
pub(crate) struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: Commands,

    /// Increase verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true, conflicts_with = "quiet")]
    pub(crate) verbose: u8,

    /// Quiet mode (errors only).
    #[arg(short, long, global = true)]
    pub(crate) quiet: bool,

    /// Override log format.
    #[arg(long, global = true, value_enum)]
    pub(crate) log_format: Option<LogFormat>,
}

/// Log output format.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum LogFormat {
    /// Human-readable text output.
    Text,
    /// Structured JSON output.
    Json,
}

/// Available CLI subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Run all mappings from config.
    Sync(SyncArgs),
    /// Copy a single image between registries.
    Copy(CopyArgs),
    /// List and filter tags.
    Tags(TagsArgs),
    /// Validate credentials.
    Auth {
        /// Auth subcommand.
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Offline config validation.
    Validate(ValidateArgs),
    /// Show resolved config.
    Expand(ExpandArgs),
    /// Daemon mode.
    Watch(WatchArgs),
    /// Show version information.
    Version,
}

/// Arguments for the `sync` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct SyncArgs {
    /// Config file path.
    #[arg(short, long)]
    pub(crate) config: PathBuf,
    /// Perform a dry run without making changes.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Write output to a file instead of stdout.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
    /// Output format for sync results.
    #[arg(long, value_enum)]
    pub(crate) output_format: Option<OutputFormat>,
}

/// Output format for sync results.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable text output.
    Text,
    /// Structured JSON output.
    Json,
}

/// Arguments for the `copy` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct CopyArgs {
    /// Source image reference (e.g. `docker.io/library/nginx:latest`).
    pub(crate) source: Reference,
    /// Destination image reference (e.g. `ghcr.io/myorg/nginx:latest`).
    pub(crate) destination: Reference,
}

/// Arguments for the `tags` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct TagsArgs {
    /// Repository to list tags from (e.g. `docker.io/library/nginx`).
    pub(crate) repository: Reference,
    /// Config file path for registry auth (optional).
    #[arg(short, long)]
    pub(crate) config: Option<PathBuf>,
    /// Filter tags by glob pattern (repeatable).
    #[arg(long)]
    pub(crate) glob: Vec<String>,
    /// Filter tags by semver range.
    #[arg(long)]
    pub(crate) semver: Option<String>,
    /// Exclude tags matching pattern (repeatable).
    #[arg(long)]
    pub(crate) exclude: Vec<String>,
    /// Sort order for results.
    #[arg(long, value_enum)]
    pub(crate) sort: Option<TagSortOrder>,
    /// Return only the N most recent tags.
    #[arg(long)]
    pub(crate) latest: Option<usize>,
}

/// Sort order for tag listing.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum TagSortOrder {
    /// Sort by semantic version.
    Semver,
    /// Sort alphabetically.
    Alpha,
}

impl From<TagSortOrder> for ocync_sync::filter::SortOrder {
    fn from(s: TagSortOrder) -> Self {
        match s {
            TagSortOrder::Semver => Self::Semver,
            TagSortOrder::Alpha => Self::Alpha,
        }
    }
}

/// Auth subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthAction {
    /// Check credentials for all registries in config.
    Check {
        /// Config file path(s) containing registry definitions.
        #[arg(short, long, required = true)]
        config: Vec<PathBuf>,
    },
}

/// Arguments for the `validate` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct ValidateArgs {
    /// Config file path to validate.
    pub(crate) config: PathBuf,
}

/// Arguments for the `expand` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct ExpandArgs {
    /// Config file path to expand.
    pub(crate) config: PathBuf,
    /// Show secret values instead of redacting them.
    ///
    /// WARNING: This will print credentials to stdout. Do not use
    /// when stdout is piped to a file or logging system.
    #[arg(long)]
    pub(crate) show_secrets: bool,
}

/// Arguments for the `watch` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct WatchArgs {
    /// Config file path.
    #[arg(short, long)]
    pub(crate) config: PathBuf,
    /// Sync interval in seconds (minimum 1).
    #[arg(long, default_value = "300", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) interval: u64,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    cli::setup_logging(&cli);

    // Install signal handlers for graceful shutdown.
    let shutdown = cli::shutdown::ShutdownSignal::new();
    cli::shutdown::install_signal_handlers(shutdown.clone());

    let result = match cli.command {
        Commands::Sync(args) => cli::commands::synchronize::run(&args).await,
        Commands::Copy(args) => cli::commands::copy::run(&args).await,
        Commands::Tags(args) => cli::commands::tags::run(&args).await,
        Commands::Auth { action } => match action {
            AuthAction::Check { config } => cli::commands::auth::run_check(&config).await,
        },
        Commands::Validate(args) => cli::commands::validate::run(&args),
        Commands::Expand(args) => cli::commands::expand::run(&args),
        Commands::Watch(args) => cli::commands::watch::run(&args, shutdown.clone()).await,
        Commands::Version => Ok(cli::commands::version::run()),
    };

    match result {
        Ok(code) => code.into(),
        Err(err) => {
            eprintln!("error: {err}");
            cli::ExitCode::Error.into()
        }
    }
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
        assert!(matches!(cli.command, Commands::Sync(_)));
    }

    #[test]
    fn parse_sync_dry_run() {
        let cli = Cli::parse_from(["ocync", "sync", "--config", "c.yaml", "--dry-run"]);
        if let Commands::Sync(args) = cli.command {
            assert!(args.dry_run);
        }
    }

    #[test]
    fn parse_copy() {
        let cli = Cli::parse_from([
            "ocync",
            "copy",
            "docker.io/library/nginx:latest",
            "ghcr.io/myorg/nginx:latest",
        ]);
        assert!(matches!(cli.command, Commands::Copy(_)));
    }

    #[test]
    fn parse_copy_rejects_invalid_reference() {
        let result = Cli::try_parse_from(["ocync", "copy", "", "dst"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_tags() {
        let cli = Cli::parse_from(["ocync", "tags", "docker.io/nginx"]);
        assert!(matches!(cli.command, Commands::Tags(_)));
    }

    #[test]
    fn parse_tags_multiple_globs() {
        let cli = Cli::parse_from([
            "ocync",
            "tags",
            "docker.io/nginx",
            "--glob",
            "v1.*",
            "--glob",
            "v2.*",
        ]);
        if let Commands::Tags(args) = cli.command {
            assert_eq!(args.glob.len(), 2);
        }
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
        assert!(matches!(cli.log_format, Some(LogFormat::Json)));
    }

    #[test]
    fn verbose_and_quiet_conflict() {
        let result = Cli::try_parse_from(["ocync", "-v", "-q", "sync", "--config", "c.yaml"]);
        assert!(result.is_err());
    }

    #[test]
    fn sync_requires_config() {
        let result = Cli::try_parse_from(["ocync", "sync"]);
        assert!(result.is_err());
    }

    #[test]
    fn log_format_rejects_invalid() {
        let result = Cli::try_parse_from([
            "ocync",
            "--log-format",
            "garbage",
            "sync",
            "--config",
            "c.yaml",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_watch() {
        let cli = Cli::parse_from(["ocync", "watch", "--config", "c.yaml"]);
        if let Commands::Watch(args) = cli.command {
            assert_eq!(args.interval, 300);
        } else {
            panic!("expected Watch command");
        }
    }

    #[test]
    fn parse_watch_custom_interval() {
        let cli = Cli::parse_from(["ocync", "watch", "--config", "c.yaml", "--interval", "60"]);
        if let Commands::Watch(args) = cli.command {
            assert_eq!(args.interval, 60);
        } else {
            panic!("expected Watch command");
        }
    }

    #[test]
    fn watch_interval_zero_rejected() {
        let result =
            Cli::try_parse_from(["ocync", "watch", "--config", "c.yaml", "--interval", "0"]);
        assert!(result.is_err());
    }
}
