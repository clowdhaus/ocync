//! `bench-proxy` lifecycle management and log parsing for benchmark
//! HTTP traffic capture.
//!
//! The benchmark used to spawn `mitmdump` (Python, single-threaded TLS)
//! here. That capped throughput at ~250 Mbps regardless of instance
//! size, which made multi-GB corpora painful to iterate on. We now
//! spawn our own `bench-proxy` binary (pure-Rust, tokio + rustls) that
//! scales with CPU cores and writes the same JSONL schema.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Child;

/// Handle to a running `bench-proxy` process.
pub(crate) struct ProxyHandle {
    child: Child,
    log_path: PathBuf,
    port: u16,
}

impl ProxyHandle {
    /// Returns the HTTP proxy URL for this proxy instance.
    pub(crate) fn proxy_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// A single request/response entry from the proxy log.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProxyEntry {
    /// HTTP method (GET, HEAD, PUT, etc.).
    pub(crate) method: String,
    /// Target hostname (preserved from log for per-registry analysis).
    pub(crate) host: String,
    /// Request path (e.g. `/v2/bench/nginx/manifests/latest`).
    pub(crate) url: String,
    /// Request body size in bytes.
    pub(crate) request_bytes: u64,
    /// Response body size in bytes.
    pub(crate) response_bytes: u64,
    /// HTTP response status code.
    pub(crate) status: u16,
    /// Request duration in milliseconds (preserved from log for latency analysis).
    pub(crate) duration_ms: u64,
}

/// Aggregated metrics computed from a set of proxy log entries.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProxyMetrics {
    /// Total number of requests captured.
    pub(crate) total_requests: u64,
    /// Request counts keyed by HTTP method (sorted for deterministic output).
    pub(crate) requests_by_method: BTreeMap<String, u64>,
    /// Number of responses with HTTP 429 status.
    pub(crate) status_429_count: u64,
    /// Total bytes across all request bodies.
    pub(crate) total_request_bytes: u64,
    /// Total bytes across all response bodies.
    pub(crate) total_response_bytes: u64,
    /// Number of blob GET requests made to a URL that was already fetched.
    pub(crate) duplicate_blob_gets: u64,
}

/// Spawns a `bench-proxy` process and waits for it to bind its port.
///
/// Returns a `ProxyHandle` on success. Waits up to 2 seconds for the
/// port to become ready before returning. The proxy's CA certificate
/// must already be installed in the system trust store so client tools
/// (ocync, skopeo, regsync) accept the dynamically-issued leaf certs.
///
/// `ca_cert` and `ca_key` point at PEM files created previously via
/// `bench-proxy ca-init`. Typically these are installed system-wide
/// during instance bootstrap; the CA cert also needs to be trusted by
/// the clients under test.
pub(crate) async fn start(
    binary: &Path,
    ca_cert: &Path,
    ca_key: &Path,
    log_path: &Path,
    port: u16,
) -> Result<ProxyHandle, Box<dyn std::error::Error>> {
    let log_path = log_path.to_path_buf();

    let child = tokio::process::Command::new(binary)
        .args([
            "serve",
            "--ca",
            &ca_cert.to_string_lossy(),
            "--ca-key",
            &ca_key.to_string_lossy(),
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--log",
            &log_path.to_string_lossy(),
        ])
        .kill_on_drop(true)
        .spawn()?;

    // Give the proxy time to bind the port before callers configure the proxy.
    tokio::time::sleep(Duration::from_secs(2)).await;

    Ok(ProxyHandle {
        child,
        log_path,
        port,
    })
}

/// Kills the mitmdump process, waits for it to exit, then parses and returns
/// all log entries written during its lifetime.
pub(crate) async fn stop(
    mut handle: ProxyHandle,
) -> Result<Vec<ProxyEntry>, Box<dyn std::error::Error>> {
    handle.child.kill().await?;
    handle.child.wait().await?;
    parse_log(&handle.log_path)
}

/// Reads a JSONL proxy log file and returns the parsed entries.
fn parse_log(path: &Path) -> Result<Vec<ProxyEntry>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: ProxyEntry = serde_json::from_str(line)?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Computes aggregated metrics from a slice of proxy log entries.
pub(crate) fn aggregate(entries: &[ProxyEntry]) -> ProxyMetrics {
    let mut requests_by_method: BTreeMap<String, u64> = BTreeMap::new();
    let mut status_429_count = 0u64;
    let mut total_request_bytes = 0u64;
    let mut total_response_bytes = 0u64;

    // Track GET requests to blob URLs for duplicate detection.
    let mut blob_get_counts: HashMap<String, u64> = HashMap::new();

    for entry in entries {
        *requests_by_method.entry(entry.method.clone()).or_insert(0) += 1;

        if entry.status == 429 {
            status_429_count += 1;
        }

        total_request_bytes += entry.request_bytes;
        total_response_bytes += entry.response_bytes;

        if entry.method == "GET" && entry.url.contains("/blobs/sha256:") {
            *blob_get_counts.entry(entry.url.clone()).or_insert(0) += 1;
        }
    }

    // A duplicate is any blob URL fetched more than once; count the extra fetches.
    let duplicate_blob_gets: u64 = blob_get_counts
        .values()
        .filter(|&&count| count > 1)
        .map(|&count| count - 1)
        .sum();

    ProxyMetrics {
        total_requests: entries.len() as u64,
        requests_by_method,
        status_429_count,
        total_request_bytes,
        total_response_bytes,
        duplicate_blob_gets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_entries() -> Vec<ProxyEntry> {
        vec![
            ProxyEntry {
                method: "HEAD".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/manifests/latest".into(),
                request_bytes: 100,
                response_bytes: 0,
                status: 200,
                duration_ms: 30,
            },
            ProxyEntry {
                method: "GET".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/blobs/sha256:abc123".into(),
                request_bytes: 50,
                response_bytes: 5000,
                status: 200,
                duration_ms: 120,
            },
            ProxyEntry {
                method: "GET".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/blobs/sha256:abc123".into(),
                request_bytes: 50,
                response_bytes: 5000,
                status: 200,
                duration_ms: 115,
            },
            ProxyEntry {
                method: "PUT".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/manifests/latest".into(),
                request_bytes: 2000,
                response_bytes: 100,
                status: 429,
                duration_ms: 10,
            },
        ]
    }

    #[test]
    fn aggregate_counts_methods() {
        let metrics = aggregate(&sample_entries());
        assert_eq!(metrics.total_requests, 4);
        assert_eq!(metrics.requests_by_method["HEAD"], 1);
        assert_eq!(metrics.requests_by_method["GET"], 2);
        assert_eq!(metrics.requests_by_method["PUT"], 1);
    }

    #[test]
    fn aggregate_counts_429s() {
        assert_eq!(aggregate(&sample_entries()).status_429_count, 1);
    }

    #[test]
    fn aggregate_detects_duplicate_blob_gets() {
        assert_eq!(aggregate(&sample_entries()).duplicate_blob_gets, 1);
    }

    #[test]
    fn aggregate_sums_bytes() {
        let metrics = aggregate(&sample_entries());
        assert_eq!(metrics.total_request_bytes, 2200);
        assert_eq!(metrics.total_response_bytes, 10100);
    }

    #[test]
    fn aggregate_empty_entries() {
        let metrics = aggregate(&[]);
        assert_eq!(metrics.total_requests, 0);
        assert_eq!(metrics.duplicate_blob_gets, 0);
        assert!(metrics.requests_by_method.is_empty());
    }

    #[test]
    fn parse_log_roundtrip() {
        let entries = sample_entries();
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        for entry in &entries {
            writeln!(tmpfile, "{}", serde_json::to_string(entry).unwrap()).unwrap();
        }
        tmpfile.flush().unwrap();

        let parsed = parse_log(tmpfile.path()).unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].method, "HEAD");
        assert_eq!(parsed[2].status, 200);
    }

    #[test]
    fn parse_log_skips_blank_lines() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        let entry = &sample_entries()[0];
        writeln!(tmpfile).unwrap();
        writeln!(tmpfile, "{}", serde_json::to_string(entry).unwrap()).unwrap();
        writeln!(tmpfile, "  ").unwrap();
        tmpfile.flush().unwrap();

        let parsed = parse_log(tmpfile.path()).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].method, "HEAD");
    }

    #[test]
    fn parse_log_empty_file() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let parsed = parse_log(tmpfile.path()).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_log_malformed_json_returns_error() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmpfile, "not valid json").unwrap();
        tmpfile.flush().unwrap();

        assert!(parse_log(tmpfile.path()).is_err());
    }

    #[test]
    fn aggregate_head_on_blob_url_not_counted_as_duplicate() {
        let entries = vec![
            ProxyEntry {
                method: "GET".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/blobs/sha256:abc123".into(),
                request_bytes: 50,
                response_bytes: 5000,
                status: 200,
                duration_ms: 100,
            },
            ProxyEntry {
                method: "HEAD".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/blobs/sha256:abc123".into(),
                request_bytes: 50,
                response_bytes: 0,
                status: 200,
                duration_ms: 10,
            },
        ];
        let metrics = aggregate(&entries);
        assert_eq!(metrics.duplicate_blob_gets, 0);
    }

    #[test]
    fn aggregate_no_duplicates_with_unique_blobs() {
        let entries = vec![
            ProxyEntry {
                method: "GET".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/blobs/sha256:aaa".into(),
                request_bytes: 50,
                response_bytes: 5000,
                status: 200,
                duration_ms: 100,
            },
            ProxyEntry {
                method: "GET".into(),
                host: "ecr.aws".into(),
                url: "/v2/bench/nginx/blobs/sha256:bbb".into(),
                request_bytes: 50,
                response_bytes: 3000,
                status: 200,
                duration_ms: 80,
            },
        ];
        let metrics = aggregate(&entries);
        assert_eq!(metrics.duplicate_blob_gets, 0);
    }

    #[test]
    fn parse_log_missing_required_field_returns_error() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        // Valid JSON but missing required fields (e.g. no `method`).
        writeln!(tmpfile, r#"{{"host":"ecr.aws","url":"/v2/","request_bytes":0,"response_bytes":0,"status":200,"duration_ms":10}}"#).unwrap();
        tmpfile.flush().unwrap();

        assert!(parse_log(tmpfile.path()).is_err());
    }
}
