//! MITM HTTPS forward proxy for benchmark HTTP traffic capture.
//!
//! Replaces `mitmproxy`/`mitmdump` in the ocync benchmark suite with a
//! pure-Rust, multi-threaded equivalent that scales with available cores
//! (mitmdump's single-threaded Python TLS caps at ~250 Mbps, which made
//! multi-GB benchmark corpora impractical).
//!
//! The proxy:
//! - Listens on 127.0.0.1:<port> for HTTP/1.1 CONNECT requests
//! - Terminates client TLS with a dynamically-issued leaf cert signed by
//!   a single long-lived CA (CA cert must already be trusted by the
//!   client, e.g. installed into the system trust store)
//! - Opens a new TLS connection to the origin via `reqwest` and streams
//!   request/response bodies bidirectionally while counting bytes
//! - Emits one JSON Lines entry per completed request to a log file
//!   matching the schema consumed by `xtask::bench::proxy::ProxyEntry`
//!
//! # Subcommands
//!
//! - `ca-init --out <cert.pem> --key <key.pem>`: generate a fresh CA
//!   keypair and write PEM files, then exit. Run once during instance
//!   bootstrap; install the cert into the system trust store.
//! - `serve --ca <cert.pem> --ca-key <key.pem> --listen <addr> --log <file>`:
//!   run the proxy until SIGTERM/SIGINT.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

mod ca;
mod proxy;

#[derive(Parser)]
#[command(
    name = "bench-proxy",
    about = "MITM HTTPS proxy for benchmark traffic capture"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a self-signed CA keypair and exit.
    CaInit {
        /// Path to write the CA certificate PEM.
        #[arg(long)]
        out: PathBuf,
        /// Path to write the CA private key PEM (keep secret).
        #[arg(long)]
        key: PathBuf,
        /// Common name for the CA certificate.
        #[arg(long, default_value = "ocync bench proxy CA")]
        common_name: String,
    },
    /// Run the proxy.
    Serve {
        /// Path to the CA certificate PEM (must be trusted by clients).
        #[arg(long)]
        ca: PathBuf,
        /// Path to the CA private key PEM.
        #[arg(long = "ca-key")]
        ca_key: PathBuf,
        /// Listen address (host:port).
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
        /// Path to append JSONL request log entries to.
        #[arg(long)]
        log: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Install the rustls default crypto provider (aws-lc-rs) once. Required
    // before any `rustls::ServerConfig`/`ClientConfig` is built.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| "failed to install aws-lc-rs crypto provider")?;

    let cli = Cli::parse();
    match cli.command {
        Command::CaInit {
            out,
            key,
            common_name,
        } => {
            let (cert_pem, key_pem) = ca::generate_ca(&common_name)?;
            tokio::fs::write(&out, cert_pem.as_bytes()).await?;
            tokio::fs::write(&key, key_pem.as_bytes()).await?;
            // Key file must not be world-readable.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).await?;
            }
            tracing::info!(cert = %out.display(), "CA certificate written");
            tracing::info!(key = %key.display(), "CA private key written");
            Ok(())
        }
        Command::Serve {
            ca,
            ca_key,
            listen,
            log,
        } => {
            let ca_signer = Arc::new(ca::load_ca(&ca, &ca_key).await?);
            let log_writer = Arc::new(proxy::LogWriter::create(&log).await?);
            proxy::serve(listen, ca_signer, log_writer).await
        }
    }
}
