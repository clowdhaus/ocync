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
///
/// Field names match the JSON archive format (`bench/results/*.json`)
/// so this struct can be embedded directly into [`super::report::ToolRecord`]
/// via `#[serde(flatten)]`. Adding a new metric here automatically
/// propagates to the archive -- no manual mapping required.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct ProxyMetrics {
    /// Total number of requests captured.
    pub(crate) requests: u64,
    /// HTTP 429 rate-limit responses.
    pub(crate) rate_limit_429s: u64,
    /// Total HTTP response bytes.
    pub(crate) response_bytes: u64,
    /// Number of blob GET requests made to a URL that was already fetched.
    pub(crate) duplicate_blob_gets: u64,
    /// Cross-repo mount POST requests (URLs containing `mount=` AND `from=`).
    /// These are genuine attempts to mount a blob from another repository.
    pub(crate) mount_attempts: u64,
    /// Cross-repo mount attempts fulfilled by the registry (201 Created).
    pub(crate) mount_successes: u64,
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
    /// Per-action 429 breakdown (e.g. `"BlobUploadInit"` -> 12).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) status_429_by_action: BTreeMap<String, u64>,
    /// Manifest CDN hits (source `/manifests/` requests with X-Cache Hit).
    #[serde(default)]
    pub(crate) manifest_cdn_hits: u64,
    /// Manifest CDN misses (source `/manifests/` requests with X-Cache Miss).
    #[serde(default)]
    pub(crate) manifest_cdn_misses: u64,
    /// Blob CDN hits (non-manifest source requests with X-Cache Hit).
    #[serde(default)]
    pub(crate) blob_cdn_hits: u64,
    /// Blob CDN misses (non-manifest source requests with X-Cache Miss).
    #[serde(default)]
    pub(crate) blob_cdn_misses: u64,
    /// GET requests to `/v2/` (auth challenge pings).
    #[serde(default)]
    pub(crate) auth_v2_pings: u64,
    /// GET requests to token endpoints (`/token` or `/v2/auth`).
    #[serde(default)]
    pub(crate) auth_token_requests: u64,
    /// Calls to the OCI 1.1 referrers API endpoint
    /// (`GET /v2/<repo>/referrers/...`). Marker for artifact-sync workload;
    /// comparable tools (dregsy, regsync) issue zero such calls.
    #[serde(default)]
    pub(crate) referrer_calls: u64,
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

// Action name constants for 429 breakdown. Names correspond to
// `RegistryAction` variants in `ocync-distribution/src/aimd.rs` --
// keep in sync if that enum changes.
const ACTION_MANIFEST_HEAD: &str = "ManifestHead";
const ACTION_MANIFEST_READ: &str = "ManifestRead";
const ACTION_MANIFEST_WRITE: &str = "ManifestWrite";
const ACTION_BLOB_HEAD: &str = "BlobHead";
const ACTION_BLOB_READ: &str = "BlobRead";
const ACTION_BLOB_UPLOAD_INIT: &str = "BlobUploadInit";
const ACTION_BLOB_UPLOAD_CHUNK: &str = "BlobUploadChunk";
const ACTION_BLOB_UPLOAD_COMPLETE: &str = "BlobUploadComplete";
const ACTION_TAG_LIST: &str = "TagList";
const ACTION_OTHER: &str = "Other";

/// Classifies a registry request by ECR action from the HTTP method and URL path.
///
/// Used to break down 429 rate-limit responses by the operation that triggered them.
fn classify_registry_action(method: &str, url: &str) -> &'static str {
    match method {
        "HEAD" if url.contains("/manifests/") => ACTION_MANIFEST_HEAD,
        "GET" if url.contains("/manifests/") => ACTION_MANIFEST_READ,
        "PUT" if url.contains("/manifests/") => ACTION_MANIFEST_WRITE,
        "HEAD" if url.contains("/blobs/sha256:") => ACTION_BLOB_HEAD,
        "GET" if url.contains("/blobs/sha256:") => ACTION_BLOB_READ,
        "POST" if url.contains("/blobs/uploads/") => ACTION_BLOB_UPLOAD_INIT,
        "PATCH" if url.contains("/blobs/uploads/") => ACTION_BLOB_UPLOAD_CHUNK,
        "PUT" if url.contains("/blobs/uploads/") => ACTION_BLOB_UPLOAD_COMPLETE,
        "GET" if url.contains("/tags/list") => ACTION_TAG_LIST,
        _ => ACTION_OTHER,
    }
}

/// Computes aggregated metrics from a slice of proxy log entries.
///
/// `target_registry` is the ECR hostname (e.g. `111...dkr.ecr.us-east-1.amazonaws.com`).
/// Requests to this host are target operations; all others are source pulls.
pub(crate) fn aggregate(entries: &[ProxyEntry], target_registry: &str) -> ProxyMetrics {
    let mut rate_limit_429s = 0u64;
    let mut status_429_by_action: BTreeMap<String, u64> = BTreeMap::new();
    let mut response_bytes = 0u64;
    let mut mount_attempts = 0u64;
    let mut mount_successes = 0u64;
    let mut source_blob_gets = 0u64;
    let mut source_blob_bytes = 0u64;
    let mut cdn_hits = 0u64;
    let mut cdn_misses = 0u64;
    let mut manifest_cdn_hits = 0u64;
    let mut manifest_cdn_misses = 0u64;
    let mut blob_cdn_hits = 0u64;
    let mut blob_cdn_misses = 0u64;
    let mut auth_v2_pings = 0u64;
    let mut auth_token_requests = 0u64;
    let mut referrer_calls = 0u64;

    // Track GET requests to blob URLs for duplicate detection.
    let mut blob_get_counts: HashMap<String, u64> = HashMap::new();

    for entry in entries {
        if entry.status == 429 {
            rate_limit_429s += 1;
            let action = classify_registry_action(&entry.method, &entry.url);
            *status_429_by_action.entry(action.to_owned()).or_insert(0) += 1;
        }

        response_bytes += entry.response_bytes;

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

        // CDN cache status from X-Cache header (source requests only),
        // split by manifest vs blob.
        if entry.host != target_registry {
            if let Some(ref xc) = entry.x_cache {
                let lower = xc.to_ascii_lowercase();
                let is_manifest = entry.url.contains("/manifests/");
                if lower.contains("hit") {
                    cdn_hits += 1;
                    if is_manifest {
                        manifest_cdn_hits += 1;
                    } else {
                        blob_cdn_hits += 1;
                    }
                } else if lower.contains("miss") {
                    cdn_misses += 1;
                    if is_manifest {
                        manifest_cdn_misses += 1;
                    } else {
                        blob_cdn_misses += 1;
                    }
                }
            }
        }

        // Auth request tracking.
        if entry.method == "GET" {
            if entry.url == "/v2/" {
                auth_v2_pings += 1;
            }
            // Token endpoints: auth.docker.io/token, ghcr.io/token, quay.io/v2/auth, etc.
            if entry.url.starts_with("/token") || entry.url.starts_with("/v2/auth") {
                auth_token_requests += 1;
            }
        }

        // OCI 1.1 referrer-artifact discovery -- marks artifact-sync calls.
        // Comparable tools (dregsy, regsync) do not implement this API.
        if entry.method == "GET" && entry.url.contains("/referrers/") {
            referrer_calls += 1;
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
        requests: entries.len() as u64,
        rate_limit_429s,
        status_429_by_action,
        response_bytes,
        duplicate_blob_gets,
        mount_attempts,
        mount_successes,
        source_blob_gets,
        source_blob_bytes,
        cdn_hits,
        cdn_misses,
        manifest_cdn_hits,
        manifest_cdn_misses,
        blob_cdn_hits,
        blob_cdn_misses,
        auth_v2_pings,
        auth_token_requests,
        referrer_calls,
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
    fn aggregate_counts_requests() {
        let metrics = aggregate(&sample_entries(), "ecr.aws");
        assert_eq!(metrics.requests, 4);
    }

    #[test]
    fn aggregate_counts_429s() {
        assert_eq!(aggregate(&sample_entries(), "ecr.aws").rate_limit_429s, 1);
    }

    #[test]
    fn aggregate_detects_duplicate_blob_gets() {
        assert_eq!(
            aggregate(&sample_entries(), "ecr.aws").duplicate_blob_gets,
            1
        );
    }

    #[test]
    fn aggregate_sums_response_bytes() {
        let metrics = aggregate(&sample_entries(), "ecr.aws");
        assert_eq!(metrics.response_bytes, 10100);
    }

    #[test]
    fn aggregate_empty_entries() {
        let metrics = aggregate(&[], "ecr.aws");
        assert_eq!(metrics.requests, 0);
        assert_eq!(metrics.duplicate_blob_gets, 0);
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
    fn classify_registry_action_all_variants() {
        assert_eq!(
            classify_registry_action("HEAD", "/v2/repo/manifests/sha256:abc"),
            "ManifestHead"
        );
        assert_eq!(
            classify_registry_action("GET", "/v2/repo/manifests/latest"),
            "ManifestRead"
        );
        assert_eq!(
            classify_registry_action("PUT", "/v2/repo/manifests/latest"),
            "ManifestWrite"
        );
        assert_eq!(
            classify_registry_action("HEAD", "/v2/repo/blobs/sha256:abc"),
            "BlobHead"
        );
        assert_eq!(
            classify_registry_action("GET", "/v2/repo/blobs/sha256:abc"),
            "BlobRead"
        );
        assert_eq!(
            classify_registry_action("POST", "/v2/repo/blobs/uploads/?mount=sha256:abc"),
            "BlobUploadInit"
        );
        assert_eq!(
            classify_registry_action("PATCH", "/v2/repo/blobs/uploads/uuid-123"),
            "BlobUploadChunk"
        );
        assert_eq!(
            classify_registry_action("PUT", "/v2/repo/blobs/uploads/uuid-123?digest=sha256:abc"),
            "BlobUploadComplete"
        );
        assert_eq!(
            classify_registry_action("GET", "/v2/repo/tags/list"),
            "TagList"
        );
        assert_eq!(classify_registry_action("GET", "/v2/"), "Other");
    }

    #[test]
    fn aggregate_429_by_action() {
        let entries = vec![
            entry(
                "PUT",
                "ecr.aws",
                "/v2/bench/nginx/manifests/latest",
                2000,
                100,
                429,
                10,
            ),
            entry(
                "POST",
                "ecr.aws",
                "/v2/bench/nginx/blobs/uploads/?mount=sha256:abc",
                50,
                0,
                429,
                5,
            ),
            entry(
                "PUT",
                "ecr.aws",
                "/v2/bench/nginx/manifests/v2",
                1000,
                100,
                429,
                10,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.aws");
        assert_eq!(metrics.rate_limit_429s, 3);
        assert_eq!(metrics.status_429_by_action["ManifestWrite"], 2);
        assert_eq!(metrics.status_429_by_action["BlobUploadInit"], 1);
    }

    #[test]
    fn aggregate_cdn_manifest_blob_split() {
        let mut manifest_hit = entry(
            "GET",
            "docker.io",
            "/v2/library/nginx/manifests/latest",
            50,
            1000,
            200,
            50,
        );
        manifest_hit.x_cache = Some("Hit from cloudfront".into());

        let mut blob_miss = entry(
            "GET",
            "cdn.docker.io",
            "/v2/library/nginx/blobs/sha256:abc",
            50,
            9000,
            200,
            100,
        );
        blob_miss.x_cache = Some("Miss from cloudfront".into());

        let mut manifest_miss = entry(
            "HEAD",
            "docker.io",
            "/v2/library/nginx/manifests/v2",
            50,
            0,
            200,
            20,
        );
        manifest_miss.x_cache = Some("Miss from cloudfront".into());

        let entries = vec![manifest_hit, blob_miss, manifest_miss];
        let metrics = aggregate(&entries, "ecr.aws");
        assert_eq!(metrics.cdn_hits, 1);
        assert_eq!(metrics.cdn_misses, 2);
        assert_eq!(metrics.manifest_cdn_hits, 1);
        assert_eq!(metrics.manifest_cdn_misses, 1);
        assert_eq!(metrics.blob_cdn_hits, 0);
        assert_eq!(metrics.blob_cdn_misses, 1);
    }

    #[test]
    fn aggregate_auth_request_tracking() {
        let entries = vec![
            entry("GET", "docker.io", "/v2/", 0, 0, 401, 10),
            entry(
                "GET",
                "auth.docker.io",
                "/token?service=registry.docker.io&scope=repository:nginx:pull",
                0,
                500,
                200,
                50,
            ),
            entry("GET", "docker.io", "/v2/", 0, 0, 401, 10),
            entry(
                "GET",
                "quay.io",
                "/v2/auth?service=quay.io",
                0,
                300,
                200,
                30,
            ),
            // Non-auth GET should not count.
            entry(
                "GET",
                "docker.io",
                "/v2/library/nginx/manifests/latest",
                0,
                1000,
                200,
                40,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.aws");
        assert_eq!(metrics.auth_v2_pings, 2);
        assert_eq!(metrics.auth_token_requests, 2);
    }

    #[test]
    fn parse_log_missing_required_field_returns_error() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        // Valid JSON but missing required fields (e.g. no `method`).
        writeln!(tmpfile, r#"{{"host":"ecr.aws","url":"/v2/","request_bytes":0,"response_bytes":0,"status":200,"duration_ms":10}}"#).unwrap();
        tmpfile.flush().unwrap();

        assert!(parse_log(tmpfile.path()).is_err());
    }

    #[test]
    fn aggregate_counts_referrer_calls() {
        let entries = vec![
            entry(
                "GET",
                "public.ecr.aws",
                "/v2/foo/bar/referrers/sha256:abc",
                0,
                376,
                200,
                12,
            ),
            entry(
                "GET",
                "public.ecr.aws",
                "/v2/foo/bar/referrers/sha256:def",
                0,
                412,
                200,
                10,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.us-east-2.amazonaws.com");
        assert_eq!(metrics.referrer_calls, 2);
    }

    #[test]
    fn aggregate_does_not_count_manifests_or_blobs_as_referrers() {
        // Same hostname and shape -- only the path segment differs. Catches
        // a lazy `contains("ref")` or wrong substring.
        let entries = vec![
            entry(
                "GET",
                "public.ecr.aws",
                "/v2/foo/bar/manifests/sha256:abc",
                0,
                1024,
                200,
                12,
            ),
            entry(
                "GET",
                "public.ecr.aws",
                "/v2/foo/bar/blobs/sha256:def",
                0,
                2048,
                200,
                10,
            ),
        ];
        let metrics = aggregate(&entries, "ecr.us-east-2.amazonaws.com");
        assert_eq!(metrics.referrer_calls, 0);
    }
}
