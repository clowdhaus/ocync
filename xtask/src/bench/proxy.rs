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
    /// CDN cache status from `X-Cache` header (e.g. `"Hit from cloudfront"`).
    #[serde(default)]
    pub(crate) x_cache: Option<String>,
    /// Seconds since CDN cached the response, from `Age` header.
    #[serde(default)]
    pub(crate) age: Option<u64>,
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
    /// Cross-repo mount POST requests (URLs containing `mount=` AND `from=`).
    /// These are genuine attempts to mount a blob from another repository.
    pub(crate) mount_attempts: u64,
    /// Cross-repo mount attempts fulfilled by the registry (201 Created).
    pub(crate) mount_successes: u64,
    /// Anonymous mount POSTs (URLs containing `mount=` but NO `from=`).
    /// Used by some tools (e.g. regclient/regsync) as a blob existence check
    /// that also initializes an upload session. NOT cross-repo mounts.
    pub(crate) existence_check_posts: u64,
    /// Blob GET requests to source (non-target) registries. Counts logical
    /// blob fetches (registry URLs containing `/blobs/sha256:`), not CDN
    /// follow-up requests from redirect responses.
    pub(crate) source_blob_gets: u64,
    /// Total response bytes from ALL GET requests to non-target hosts.
    /// Includes both registry redirect responses and CDN follow-up responses
    /// where the actual blob data flows (`CloudFront`, S3, R2, etc.).
    pub(crate) source_blob_bytes: u64,
    /// Source requests where `X-Cache` contained `"Hit"` (CDN cache hit).
    #[serde(default)]
    pub(crate) cdn_hits: u64,
    /// Source requests where `X-Cache` contained `"Miss"` (CDN cache miss).
    #[serde(default)]
    pub(crate) cdn_misses: u64,
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
    // Kill any stale bench-proxy from a previous crashed run.
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", "bench-proxy serve"])
        .status();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let log_path = log_path.to_path_buf();

    let mut child = tokio::process::Command::new(binary)
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

    // Verify the proxy is still running (catches immediate crashes from
    // bad args, missing CA files, port conflicts, etc.).
    if let Some(status) = child.try_wait()? {
        return Err(format!("bench-proxy exited immediately with status: {status}").into());
    }

    Ok(ProxyHandle {
        child,
        log_path,
        port,
    })
}

/// Sends SIGTERM for graceful shutdown, waits briefly, then falls back
/// to SIGKILL. Parses and returns all log entries written during the
/// proxy's lifetime.
pub(crate) async fn stop(
    mut handle: ProxyHandle,
) -> Result<Vec<ProxyEntry>, Box<dyn std::error::Error>> {
    // Send SIGTERM so the proxy can flush its log and exit cleanly.
    if let Some(pid) = handle.child.id() {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        // Give the proxy time to flush logs and exit.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // If still running after SIGTERM, fall back to SIGKILL.
    match handle.child.try_wait()? {
        Some(_) => {} // Exited gracefully.
        None => {
            handle.child.kill().await?;
            handle.child.wait().await?;
        }
    }

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
///
/// `target_registry` is the ECR hostname (e.g. `111...dkr.ecr.us-east-1.amazonaws.com`).
/// Requests to this host are target operations; all others are source pulls.
pub(crate) fn aggregate(entries: &[ProxyEntry], target_registry: &str) -> ProxyMetrics {
    let mut requests_by_method: BTreeMap<String, u64> = BTreeMap::new();
    let mut status_429_count = 0u64;
    let mut total_request_bytes = 0u64;
    let mut total_response_bytes = 0u64;
    let mut mount_attempts = 0u64;
    let mut mount_successes = 0u64;
    let mut existence_check_posts = 0u64;
    let mut source_blob_gets = 0u64;
    let mut source_blob_bytes = 0u64;
    let mut cdn_hits = 0u64;
    let mut cdn_misses = 0u64;

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
            *blob_get_counts
                .entry(format!("{}:{}", entry.host, entry.url))
                .or_insert(0) += 1;
            // Count logical source blob fetches (registry GETs, not CDN).
            if entry.host != target_registry {
                source_blob_gets += 1;
            }
        }

        // Source blob bytes: sum ALL GET response bytes from non-target hosts.
        // Registry blob GETs return 302/307 redirects (0-1KB), and the actual
        // blob data is served from CDN hosts (CloudFront, S3, R2) with different
        // URL patterns that don't match `/blobs/sha256:`. Summing all non-target
        // GET responses captures both the redirect and CDN responses.
        if entry.method == "GET" && entry.host != target_registry {
            source_blob_bytes += entry.response_bytes;
        }

        // CDN cache status from X-Cache header (source requests only).
        if entry.host != target_registry {
            if let Some(ref xc) = entry.x_cache {
                let lower = xc.to_ascii_lowercase();
                if lower.contains("hit") {
                    cdn_hits += 1;
                } else if lower.contains("miss") {
                    cdn_misses += 1;
                }
            }
        }

        // OCI cross-repo blob mount: `POST /v2/.../blobs/uploads/?mount=<digest>&from=<repo>`.
        // A successful mount returns 201 Created and skips the data upload entirely; a
        // 202 means the registry started a new upload session (mount fallback).
        //
        // Anonymous mount (mount= without from=) is a different operation:
        // an existence check that also initializes an upload session. Used by
        // regclient/regsync. We count these separately.
        if entry.method == "POST"
            && entry.url.contains("/blobs/uploads/")
            && entry.url.contains("mount=")
        {
            if entry.url.contains("from=") {
                mount_attempts += 1;
                if entry.status == 201 {
                    mount_successes += 1;
                }
            } else {
                existence_check_posts += 1;
            }
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
        mount_attempts,
        mount_successes,
        existence_check_posts,
        source_blob_gets,
        source_blob_bytes,
        cdn_hits,
        cdn_misses,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entry(
        method: &str,
        host: &str,
        url: &str,
        request_bytes: u64,
        response_bytes: u64,
        status: u16,
        duration_ms: u64,
    ) -> ProxyEntry {
        ProxyEntry {
            method: method.into(),
            host: host.into(),
            url: url.into(),
            request_bytes,
            response_bytes,
            status,
            duration_ms,
            x_cache: None,
            age: None,
        }
    }

    fn sample_entries() -> Vec<ProxyEntry> {
        vec![
            entry(
                "HEAD",
                "ecr.aws",
                "/v2/bench/nginx/manifests/latest",
                100,
                0,
                200,
                30,
            ),
            entry(
                "GET",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:abc123",
                50,
                5000,
                200,
                120,
            ),
            entry(
                "GET",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:abc123",
                50,
                5000,
                200,
                115,
            ),
            entry(
                "PUT",
                "ecr.aws",
                "/v2/bench/nginx/manifests/latest",
                2000,
                100,
                429,
                10,
            ),
        ]
    }

    #[test]
    fn aggregate_counts_methods() {
        let metrics = aggregate(&sample_entries(), "ecr.aws");
        assert_eq!(metrics.total_requests, 4);
        assert_eq!(metrics.requests_by_method["HEAD"], 1);
        assert_eq!(metrics.requests_by_method["GET"], 2);
        assert_eq!(metrics.requests_by_method["PUT"], 1);
    }

    #[test]
    fn aggregate_counts_429s() {
        assert_eq!(aggregate(&sample_entries(), "ecr.aws").status_429_count, 1);
    }

    #[test]
    fn aggregate_detects_duplicate_blob_gets() {
        assert_eq!(
            aggregate(&sample_entries(), "ecr.aws").duplicate_blob_gets,
            1
        );
    }

    #[test]
    fn aggregate_sums_bytes() {
        let metrics = aggregate(&sample_entries(), "ecr.aws");
        assert_eq!(metrics.total_request_bytes, 2200);
        assert_eq!(metrics.total_response_bytes, 10100);
    }

    #[test]
    fn aggregate_empty_entries() {
        let metrics = aggregate(&[], "ecr.aws");
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
            entry(
                "GET",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:abc123",
                50,
                5000,
                200,
                100,
            ),
            entry(
                "HEAD",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:abc123",
                50,
                0,
                200,
                10,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.aws");
        assert_eq!(metrics.duplicate_blob_gets, 0);
    }

    #[test]
    fn aggregate_no_duplicates_with_unique_blobs() {
        let entries = vec![
            entry(
                "GET",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:aaa",
                50,
                5000,
                200,
                100,
            ),
            entry(
                "GET",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:bbb",
                50,
                3000,
                200,
                80,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.aws");
        assert_eq!(metrics.duplicate_blob_gets, 0);
    }

    #[test]
    fn aggregate_source_blob_gets() {
        let entries = vec![
            // Source blob GET (host != target).
            entry(
                "GET",
                "docker.io",
                "/v2/library/nginx/blobs/sha256:abc",
                50,
                9000,
                200,
                100,
            ),
            // Target blob GET (host == target).
            entry(
                "GET",
                "ecr.aws",
                "/v2/bench/nginx/blobs/sha256:abc",
                50,
                9000,
                200,
                80,
            ),
            // Source non-blob GET (manifest, not /blobs/sha256:).
            // Does NOT count as a source_blob_get, but its response bytes
            // ARE included in source_blob_bytes (all non-target GET bytes).
            entry(
                "GET",
                "docker.io",
                "/v2/library/nginx/manifests/latest",
                50,
                1000,
                200,
                50,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.aws");
        assert_eq!(metrics.source_blob_gets, 1);
        // source_blob_bytes includes ALL non-target GET responses:
        // source blob (9000) + source manifest (1000) = 10000.
        assert_eq!(metrics.source_blob_bytes, 10000);
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
