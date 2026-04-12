//! The `auth check` subcommand — credential validation.

use std::path::PathBuf;

use ocync_distribution::RegistryClient;
use ocync_distribution::auth::anonymous::AnonymousAuth;
use ocync_distribution::auth::detect::{ProviderKind, detect_provider_kind};
use ocync_distribution::auth::ecr::EcrAuth;
use url::Url;

use crate::cli::config::{AuthType, RegistryConfig, load_config};

/// Run credential checks against all registries in config files.
pub(crate) async fn run_check(configs: &[PathBuf]) -> i32 {
    if configs.is_empty() {
        eprintln!("error: at least one --config file is required for auth check");
        return 2;
    }

    let mut all_ok = true;

    for path in configs {
        let config = match load_config(path) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("error: {}: {err}", path.display());
                all_ok = false;
                continue;
            }
        };

        for (name, reg) in &config.registries {
            let ok = check_registry(name, reg).await;
            if !ok {
                all_ok = false;
            }
        }
    }

    if all_ok { 0 } else { 1 }
}

async fn check_registry(name: &str, reg: &RegistryConfig) -> bool {
    let base_url = if reg.url.starts_with("http://") || reg.url.starts_with("https://") {
        reg.url.clone()
    } else {
        format!("https://{}", reg.url)
    };

    let url = match Url::parse(&base_url) {
        Ok(u) => u,
        Err(err) => {
            eprintln!("  FAIL  {name} — invalid URL: {err}");
            return false;
        }
    };

    let hostname = reg
        .url
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    // Determine auth provider from config or hostname detection.
    let auth_kind = reg
        .auth_type
        .as_ref()
        .map(|a| match a {
            AuthType::Ecr => Some(ProviderKind::Ecr),
            AuthType::Anonymous => None,
            AuthType::DockerConfig => None,
            // For types without full providers yet, fall back to anonymous.
            _ => None,
        })
        .unwrap_or_else(|| detect_provider_kind(hostname));

    let client = match auth_kind {
        Some(ProviderKind::Ecr) | Some(ProviderKind::EcrPublic) => {
            match EcrAuth::new(hostname).await {
                Ok(auth) => RegistryClient::builder(url).auth(auth).build(),
                Err(err) => {
                    eprintln!("  FAIL  {name} — ECR auth setup: {err}");
                    return false;
                }
            }
        }
        _ => {
            let http = ocync_distribution::reqwest::Client::new();
            let auth = AnonymousAuth::new(hostname, http);
            RegistryClient::builder(url).auth(auth).build()
        }
    };

    let client = match client {
        Ok(c) => c,
        Err(err) => {
            eprintln!("  FAIL  {name} — client setup: {err}");
            return false;
        }
    };

    match client.ping().await {
        Ok(()) => {
            eprintln!("  OK    {name} ({hostname})");
            true
        }
        Err(err) => {
            eprintln!("  FAIL  {name} ({hostname}) — {err}");
            false
        }
    }
}
