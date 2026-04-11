pub(crate) mod commands;
pub(crate) mod config;
#[allow(dead_code)]
pub(crate) mod output;
#[allow(dead_code)]
pub(crate) mod shutdown;

use crate::Cli;
use tracing_subscriber::{EnvFilter, fmt};

pub(crate) fn setup_logging(cli: &Cli) {
    let detected = detect_log_format();
    let format = cli
        .log_format
        .as_deref()
        .or(detected.as_deref())
        .unwrap_or("text");

    let filter = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    match format {
        "json" => {
            fmt()
                .json()
                .with_env_filter(env_filter)
                .with_target(false)
                .init();
        }
        _ => {
            fmt().with_env_filter(env_filter).with_target(false).init();
        }
    }
}

fn detect_log_format() -> Option<String> {
    if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
        Some("json".to_string())
    } else {
        None
    }
}
