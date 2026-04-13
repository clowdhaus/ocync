//! OCI container image sync tool.

mod cli;

use std::path::PathBuf;

use anstyle::{Ansi256Color, Color, Style};
use clap::builder::Styles;
use clap::{Parser, Subcommand, ValueEnum};
use ocync_distribution::Reference;

const PINK: Option<Color> = Some(Color::Ansi256(Ansi256Color(213)));
const PURPLE: Option<Color> = Some(Color::Ansi256(Ansi256Color(141)));
const CYAN: Option<Color> = Some(Color::Ansi256(Ansi256Color(75)));

/// CLI help color scheme: purple headers, cyan literals, pink placeholders.
const STYLES: Styles = Styles::styled()
    .header(Style::new().fg_color(PURPLE).bold())
    .usage(Style::new().fg_color(PURPLE).bold())
    .literal(Style::new().fg_color(CYAN).bold())
    .placeholder(Style::new().fg_color(PINK))
    .valid(Style::new().fg_color(CYAN))
    .invalid(Style::new().fg_color(PINK).bold())
    .error(Style::new().fg_color(PINK).bold());

const MAIN_LONG_ABOUT: &str = "\
Sync OCI container images across registries

ocync copies container images between OCI-compliant registries with blob
deduplication, cross-repo mounts, and streaming transfers — no local disk
required.

Examples:
  ocync sync -c config.yaml
  ocync sync -c config.yaml --dry-run
  ocync copy docker.io/library/nginx:1.27 ghcr.io/myorg/nginx:1.27
  ocync tags docker.io/library/nginx --semver '>=1.0' --latest 10";

const SYNC_LONG_ABOUT: &str = "\
Sync images across registries using a config file

Reads a YAML config that defines source and target registry mappings, then syncs
all matching images. Supports glob and semver tag filtering, dry-run previews,
and structured output.

Examples:
  ocync sync -c config.yaml
  ocync sync -c config.yaml --dry-run
  ocync sync -c config.yaml --json > results.json";

const COPY_LONG_ABOUT: &str = "\
Copy a single image from one registry to another

Copies a single tagged image between registries. Useful for one-off transfers
without a config file.

Examples:
  ocync copy docker.io/library/nginx:1.27 ghcr.io/myorg/nginx:1.27
  ocync copy 123456789012.dkr.ecr.us-east-1.amazonaws.com/app:v2 ghcr.io/myorg/app:v2";

const TAGS_LONG_ABOUT: &str = "\
List, filter, and sort repository tags

Query a registry for available tags and apply filters. Useful for discovering
which tags exist before configuring a sync mapping.

Examples:
  ocync tags docker.io/library/nginx
  ocync tags docker.io/library/nginx --semver '>=1.0, <2.0' --latest 5
  ocync tags docker.io/library/nginx --glob 'alpine*' --exclude '*beta*' --sort semver";

const WATCH_LONG_ABOUT: &str = "\
Run sync continuously on a recurring schedule

Runs the sync operation in a loop at the configured interval. Handles graceful
shutdown on SIGINT/SIGTERM.

Examples:
  ocync watch -c config.yaml
  ocync watch -c config.yaml --interval 60";

/// Sync OCI container images across registries.
#[derive(Debug, Parser)]
#[command(
    name = "ocync",
    version,
    about = "Sync OCI container images across registries",
    long_about = MAIN_LONG_ABOUT,
    styles = STYLES,
)]
pub(crate) struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: Commands,

    /// Increase log verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true, conflicts_with = "quiet", help_heading = "Global options")]
    pub(crate) verbose: u8,

    /// Suppress all output except errors.
    #[arg(short, long, global = true, help_heading = "Global options")]
    pub(crate) quiet: bool,

    /// Set the log output format (auto-detected in Kubernetes).
    #[arg(long, global = true, value_enum, help_heading = "Global options")]
    pub(crate) log_format: Option<LogFormat>,
}

/// Log output format.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum LogFormat {
    /// Human-readable log lines.
    Text,
    /// Structured JSON log entries.
    Json,
}

/// Available commands.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Sync images across registries using a config file.
    #[command(long_about = SYNC_LONG_ABOUT)]
    Sync(SyncArgs),

    /// Copy a single image from one registry to another.
    #[command(long_about = COPY_LONG_ABOUT)]
    Copy(CopyArgs),

    /// List, filter, and sort repository tags.
    #[command(long_about = TAGS_LONG_ABOUT)]
    Tags(TagsArgs),

    /// Manage registry authentication.
    #[command(long_about = "\
Manage registry authentication

Verify that credentials are valid for all registries defined in a config file.")]
    Auth {
        /// Auth subcommand.
        #[command(subcommand)]
        action: AuthAction,
    },

    /// Validate a config file without connecting to registries.
    #[command(long_about = "\
Validate a config file without connecting to registries

Checks config syntax, structure, and references without making any network
requests. Catches errors before attempting a sync.")]
    Validate(ValidateArgs),

    /// Display config with all environment variables resolved.
    #[command(long_about = "\
Display config with all environment variables resolved

Shows the fully expanded config after variable substitution. Secrets are
redacted by default unless --show-secrets is passed.")]
    Expand(ExpandArgs),

    /// Run sync continuously on a recurring schedule.
    #[command(long_about = WATCH_LONG_ABOUT)]
    Watch(WatchArgs),

    /// Print version and build information.
    Version,
}

/// Arguments for the `sync` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct SyncArgs {
    /// Path to the sync config file.
    #[arg(short, long)]
    pub(crate) config: PathBuf,
    /// Preview what would be synced without making changes.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Output the full sync report as JSON instead of a text summary.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments for the `copy` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct CopyArgs {
    /// Source image reference (e.g., `docker.io/library/nginx:latest`).
    pub(crate) source: Reference,
    /// Destination image reference (e.g., `ghcr.io/myorg/nginx:latest`).
    pub(crate) destination: Reference,
}

/// Arguments for the `tags` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct TagsArgs {
    /// Repository to list tags from (e.g., `docker.io/library/nginx`).
    pub(crate) repository: Reference,
    /// Config file for registry credentials.
    #[arg(short, long)]
    pub(crate) config: Option<PathBuf>,
    /// Include tags matching a glob pattern (repeatable).
    #[arg(long)]
    pub(crate) glob: Vec<String>,
    /// Include tags matching a semver range (e.g., `>=1.0, <2.0`).
    #[arg(long)]
    pub(crate) semver: Option<String>,
    /// Exclude tags matching a pattern (repeatable).
    #[arg(long)]
    pub(crate) exclude: Vec<String>,
    /// Sort order for listed tags.
    #[arg(long, value_enum)]
    pub(crate) sort: Option<TagSortOrder>,
    /// Show only the N most recent tags.
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

/// Authentication subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthAction {
    /// Verify credentials for all registries in config.
    Check {
        /// Config file path(s) containing registry definitions.
        #[arg(short, long, required = true)]
        config: Vec<PathBuf>,
    },
}

/// Arguments for the `validate` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct ValidateArgs {
    /// Path to the config file to validate.
    pub(crate) config: PathBuf,
}

/// Arguments for the `expand` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct ExpandArgs {
    /// Path to the config file to expand.
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
    /// Path to the sync config file.
    #[arg(short, long)]
    pub(crate) config: PathBuf,
    /// Seconds between sync runs (minimum: 1).
    #[arg(long, default_value = "300", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) interval: u64,
    /// Output sync reports as JSON instead of text summaries.
    #[arg(long)]
    pub(crate) json: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    cli::setup_logging(&cli);

    // Install signal handlers for graceful shutdown.
    let shutdown = cli::shutdown::ShutdownSignal::new();
    cli::shutdown::install_signal_handlers(shutdown.clone());

    let result = match cli.command {
        Commands::Sync(args) => cli::commands::synchronize::run(&args, Some(&shutdown)).await,
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
            err.exit_code().into()
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
    fn parse_sync_json() {
        let cli = Cli::parse_from(["ocync", "sync", "--config", "c.yaml", "--json"]);
        if let Commands::Sync(args) = cli.command {
            assert!(args.json);
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
