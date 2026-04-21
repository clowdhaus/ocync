//! MITM proxy core - TCP accept, HTTP/1.1 CONNECT handling, TLS
//! termination, upstream forwarding, and per-request JSONL logging.
//!
//! Each accepted TCP connection is driven by a single tokio task. After
//! the CONNECT handshake we hand the decrypted TLS stream to hyper's
//! HTTP/1.1 server, which calls [`handle_request`] once per request on
//! that connection. Keep-alive and pipelined requests work out of the
//! box because hyper owns the framing.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{
    CONNECTION, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING,
    UPGRADE,
};
use http::uri::Authority;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::Serialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, warn};

use crate::ca::CaSigner;

/// Body type we build for proxied responses. Wraps a byte stream coming
/// from reqwest and reports per-chunk sizes upstream via the counter.
type ProxyBody = http_body_util::combinators::UnsyncBoxBody<Bytes, BodyError>;

/// Opaque error type for streamed proxy bodies.
#[derive(Debug, thiserror::Error)]
#[error("proxy body: {0}")]
pub(crate) struct BodyError(String);

/// Single proxied request/response, emitted as one JSONL entry.
///
/// Field names match `xtask::bench::proxy::ProxyEntry` so downstream
/// aggregation in the benchmark harness is unchanged.
#[derive(Debug, Serialize)]
struct ProxyEntry<'a> {
    timestamp: String,
    method: &'a str,
    host: &'a str,
    url: &'a str,
    request_bytes: u64,
    response_bytes: u64,
    status: u16,
    duration_ms: u64,
}

/// Append-only JSONL log writer, shared across all proxy tasks.
///
/// Serializes all writes through a `Mutex` so concurrent requests don't
/// interleave partial JSON objects on disk. The mutex is held only for
/// the final serialize+write+newline - entries are built without the
/// lock held.
#[derive(Debug)]
pub(crate) struct LogWriter {
    file: Mutex<File>,
    path: PathBuf,
}

impl LogWriter {
    /// Open `path` for appending, truncating anything already there so a
    /// fresh benchmark run starts with a clean log.
    pub(crate) async fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await?;
        Ok(Self {
            file: Mutex::new(file),
            path: path.to_path_buf(),
        })
    }

    async fn write_entry(&self, entry: &ProxyEntry<'_>) {
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to serialize proxy entry");
                return;
            }
        };
        let mut guard = self.file.lock().await;
        if let Err(e) = guard.write_all(line.as_bytes()).await {
            error!(error = %e, path = %self.path.display(), "failed to write proxy log");
            return;
        }
        if let Err(e) = guard.write_all(b"\n").await {
            error!(error = %e, path = %self.path.display(), "failed to write proxy log newline");
        }
    }
}

/// Main accept loop. Spawns one task per inbound connection.
pub(crate) async fn serve(
    listen: SocketAddr,
    ca: Arc<CaSigner>,
    log: Arc<LogWriter>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build a reqwest client once and share it. Native roots (via the
    // `rustls-tls-native-roots` feature in Cargo.toml) so origin certs
    // validate against the same trust store as the tools under test --
    // the proxy's own CA cert goes *in* the client's trust store, but
    // the proxy itself validates origin certs normally.
    //
    // `Policy::none()` is critical: Docker Hub responds to blob GETs
    // with 302 redirects to pre-signed S3 URLs that are tied to the
    // origin host. Following redirects inside the proxy would rewrite
    // the Host header and break the S3 signature. The client under
    // test (ocync) will follow the 302 itself and open a new CONNECT
    // for the S3 host, which is what lets the signature verify.
    let upstream = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(32)
        .build()
        .map_err(|e| format!("reqwest client: {e}"))?;

    let listener = TcpListener::bind(listen).await?;
    tracing::info!(%listen, "bench-proxy listening");

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, peer) = match accept {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!(error = %e, "accept failed, skipping connection");
                        continue;
                    }
                };
                let ca = ca.clone();
                let log = log.clone();
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, peer, ca, log, upstream).await {
                        debug!(%peer, error = %e, "connection closed with error");
                    }
                });
            }
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received, exiting accept loop");
                return Ok(());
            }
        }
    }
}

/// Wait for SIGTERM or SIGINT, whichever comes first.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Handle a single client connection. Expects an HTTP/1.1 CONNECT; all
/// other methods are rejected with 405.
async fn handle_connection(
    stream: TcpStream,
    _peer: SocketAddr,
    ca: Arc<CaSigner>,
    log: Arc<LogWriter>,
    upstream: reqwest::Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read the HTTP/1.1 request line + headers ourselves. hyper's server
    // doesn't expose CONNECT handling on the inbound connection so we
    // parse just enough to extract the CONNECT authority, reply 200,
    // then hand the raw socket to tokio-rustls.
    let mut stream = BufReader::new(stream);
    let mut request_line = String::new();
    stream.read_line(&mut request_line).await?;
    let request_line = request_line.trim_end_matches(&['\r', '\n'][..]).to_owned();

    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let _version = parts.next().unwrap_or("");

    // Drain the rest of the request headers (we don't need them).
    loop {
        let mut header = String::new();
        let n = stream.read_line(&mut header).await?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }

    if method != "CONNECT" {
        let resp =
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.get_mut().write_all(resp).await;
        return Ok(());
    }

    // Parse target as `host:port`. The CONNECT authority is what the
    // client will send in SNI on the upcoming TLS handshake.
    let authority: Authority = target
        .parse()
        .map_err(|e| format!("invalid CONNECT target {target:?}: {e}"))?;

    // Ack the CONNECT. Everything after this is TLS.
    stream
        .get_mut()
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    // Reclaim the inner TcpStream; there may be leftover bytes in the
    // BufReader from the client already sending ClientHello - they must
    // be preserved across the BufReader→TlsAcceptor boundary. We always
    // wrap in `PrefixedStream` (even when empty) so the outer type is
    // uniform regardless of whether we prefetched bytes.
    let (tcp, buffered) = into_parts(stream);
    let tls_acceptor = TlsAcceptor::from(Arc::new(build_server_config(ca.clone())));
    let prefixed = PrefixedStream::new(buffered, tcp);
    let tls_stream = tls_acceptor.accept(prefixed).await?;

    // Serve HTTP/1.1 over the decrypted stream. hyper owns framing, so
    // multiple requests (keep-alive / pipelined) work automatically.
    let target_authority = Arc::new(authority);
    hyper::server::conn::http1::Builder::new()
        .keep_alive(true)
        .serve_connection(
            TokioIo::new(tls_stream),
            service_fn(move |req| {
                let upstream = upstream.clone();
                let log = log.clone();
                let target = target_authority.clone();
                async move { handle_request(req, target, upstream, log).await }
            }),
        )
        .await?;

    Ok(())
}

/// Convert the buffered reader back into a `TcpStream` plus whatever
/// bytes had been consumed into the internal buffer but not yet returned
/// through `AsyncRead`.
fn into_parts(reader: BufReader<TcpStream>) -> (TcpStream, Vec<u8>) {
    // tokio::io::BufReader exposes `buffer()` for the already-buffered
    // (unread) portion. We capture those bytes before dropping the
    // wrapper so TLS can see the client's ClientHello.
    let buffered = reader.buffer().to_vec();
    let inner = reader.into_inner();
    (inner, buffered)
}

/// Build the TLS `ServerConfig` using the shared CA to resolve leaf
/// certs on demand (one per SNI host).
fn build_server_config(ca: Arc<CaSigner>) -> ServerConfig {
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(DynamicResolver { ca }));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg
}

/// `ResolvesServerCert` impl that issues (or fetches from cache) a leaf
/// cert matching the `ClientHello`'s SNI.
#[derive(Debug)]
struct DynamicResolver {
    ca: Arc<CaSigner>,
}

impl ResolvesServerCert for DynamicResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = client_hello.server_name()?;
        match self.ca.leaf_for(sni) {
            Ok(ck) => Some(ck),
            Err(e) => {
                warn!(sni = %sni, error = %e, "leaf cert generation failed");
                None
            }
        }
    }
}

/// Process one HTTP/1.1 request arriving on a MITM'd connection:
/// forward it to the origin, stream the response back, and log.
async fn handle_request(
    req: Request<Incoming>,
    target: Arc<Authority>,
    upstream: reqwest::Client,
    log: Arc<LogWriter>,
) -> Result<Response<ProxyBody>, Infallible> {
    let start = Instant::now();
    let method = req.method().clone();
    let uri = req.uri().clone();

    // Build the upstream URL. CONNECT pinned the authority; the
    // inbound request's URI is just a path+query (HTTP origin form).
    let host = target.host().to_owned();
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| uri.path().to_owned());
    let url = format!("https://{}{}", target.as_str(), path_and_query);

    // Collect request headers, skipping hop-by-hop entries we must not
    // forward per RFC 7230  section6.1.
    //
    // `append` (not `insert`) preserves multiple values for the same
    // header name. Go's net/http sends each Accept media type as a
    // separate header line; `insert` would keep only the last one,
    // which for regclient/regsync is `v1+prettyjws` -- causing quay.io
    // to return Docker v1 manifests instead of OCI.
    let mut headers = http::HeaderMap::new();
    for (name, value) in req.headers() {
        if is_hop_by_hop(name) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    // Inbound Host is usually the CONNECT authority already, but force
    // it to match the origin to avoid any chance of leaking the
    // proxy's own hostname.
    headers.insert(
        HOST,
        HeaderValue::from_str(&host).unwrap_or_else(|_| HeaderValue::from_static("")),
    );

    // Buffer the request body. PATCH/PUT blob chunks are bounded (ocync
    // emits 5–50 MB chunks) and manifests are KB; buffering keeps the
    // forward path simple and lets us report `request_bytes` exactly
    // once we've read the whole thing.
    let collected = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return Ok(error_response(500, format!("read request body: {e}"))),
    };
    let request_bytes = collected.len() as u64;

    let upstream_req = upstream
        .request(method.clone(), &url)
        .headers(headers)
        .body(collected);

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Log the failed hop too so benchmark analytics see the miss.
            let entry = ProxyEntry {
                timestamp: now_iso8601(),
                method: method.as_str(),
                host: &host,
                url: &path_and_query,
                request_bytes,
                response_bytes: 0,
                status: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
            log.write_entry(&entry).await;
            return Ok(error_response(
                502,
                format!("upstream request to {url}: {e}"),
            ));
        }
    };

    let status = upstream_resp.status();
    // Snapshot headers before consuming the body. Use `append` to
    // preserve multi-value headers (same rationale as request headers).
    let mut resp_headers = http::HeaderMap::new();
    for (name, value) in upstream_resp.headers() {
        if is_hop_by_hop(name) {
            continue;
        }
        resp_headers.append(name.clone(), value.clone());
    }

    // Stream the body back to the client, counting bytes, then log once
    // the stream fully drains.
    let byte_counter = Arc::new(AtomicU64::new(0));
    let counter_for_stream = byte_counter.clone();
    let log_for_stream = log.clone();
    let method_for_log = method.as_str().to_owned();
    let host_for_log = host.clone();
    let url_for_log = path_and_query.clone();

    let stream = upstream_resp.bytes_stream().map(move |item| {
        item.map(|b| {
            counter_for_stream.fetch_add(b.len() as u64, Ordering::Relaxed);
            Frame::data(b)
        })
        .map_err(|e| BodyError(e.to_string()))
    });
    // Attach a final future that fires after the stream drains.
    let logging_stream = LogOnDrop {
        inner: stream,
        logged: false,
        on_drop: Some(Box::new(move || {
            let entry_bytes = byte_counter.load(Ordering::Relaxed);
            let elapsed = start.elapsed();
            let log = log_for_stream.clone();
            let method = method_for_log.clone();
            let host = host_for_log.clone();
            let url = url_for_log.clone();
            let status_code = status.as_u16();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let entry = ProxyEntry {
                        timestamp: now_iso8601(),
                        method: &method,
                        host: &host,
                        url: &url,
                        request_bytes,
                        response_bytes: entry_bytes,
                        status: status_code,
                        duration_ms: elapsed.as_millis() as u64,
                    };
                    log.write_entry(&entry).await;
                });
            }
        })),
    };
    let body = StreamBody::new(logging_stream).boxed_unsync();

    let mut out = Response::builder().status(status);
    if let Some(h) = out.headers_mut() {
        *h = resp_headers;
    }
    Ok(out
        .body(body)
        .unwrap_or_else(|_| error_response(500, "failed to build response".into())))
}

/// Convert a synchronously-built error message into a simple response.
fn error_response(code: u16, msg: String) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(msg))
        .map_err(|e: Infallible| BodyError(e.to_string()))
        .boxed_unsync();
    Response::builder()
        .status(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .body(body)
        .expect("error response builder")
}

/// Hop-by-hop headers that must not be forwarded (RFC 7230  section6.1).
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name,
        n if n == CONNECTION
            || n == PROXY_AUTHENTICATE
            || n == PROXY_AUTHORIZATION
            || n == TE
            || n == TRAILER
            || n == TRANSFER_ENCODING
            || n == UPGRADE
    ) || name.as_str().eq_ignore_ascii_case("keep-alive")
}

/// Current UTC time formatted as ISO-8601 with second precision,
/// matching the mitmproxy addon's format for backward compatibility.
fn now_iso8601() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_else(|_| String::new())
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A `Stream` wrapper that fires `on_drop` when the inner stream ends
/// (yields `None`) or the value is dropped - whichever comes first.
/// Used to log a proxy entry exactly once after the response body has
/// fully drained.
struct LogOnDrop<S> {
    inner: S,
    logged: bool,
    on_drop: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl<S> Drop for LogOnDrop<S> {
    fn drop(&mut self) {
        if !self.logged {
            if let Some(f) = self.on_drop.take() {
                f();
            }
        }
    }
}

impl<S> Stream for LogOnDrop<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(None) = &item {
            if !self.logged {
                if let Some(f) = self.on_drop.take() {
                    self.logged = true;
                    f();
                }
            }
        }
        item
    }
}

/// An `AsyncRead`/`AsyncWrite` wrapper that emits `prefix` bytes first,
/// then defers to the underlying stream. Used to prepend bytes the
/// HTTP/1.1 parser already consumed (but hadn't yet returned) before
/// handing the socket to TLS.
struct PrefixedStream<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let remaining = &self.prefix[self.offset..];
            let n = std::cmp::min(remaining.len(), buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.offset += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures_util::stream;
    use http::HeaderName;
    use tokio::io::AsyncReadExt;

    use super::*;

    // -----------------------------------------------------------------
    // is_hop_by_hop
    // -----------------------------------------------------------------

    #[test]
    fn hop_by_hop_matches_rfc7230_headers() {
        let hop = [
            "connection",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "keep-alive",
        ];
        for name in &hop {
            let hn = HeaderName::from_bytes(name.as_bytes()).unwrap();
            assert!(is_hop_by_hop(&hn), "{name} should be hop-by-hop");
        }
    }

    #[test]
    fn hop_by_hop_rejects_non_hop_headers() {
        let non_hop = [
            "content-type",
            "accept",
            "host",
            "authorization",
            "content-length",
            "cache-control",
        ];
        for name in &non_hop {
            let hn = HeaderName::from_bytes(name.as_bytes()).unwrap();
            assert!(!is_hop_by_hop(&hn), "{name} should NOT be hop-by-hop");
        }
    }

    #[test]
    fn hop_by_hop_keep_alive_case_insensitive() {
        // HeaderName lowercases on construction, but verify the
        // eq_ignore_ascii_case path works for the custom keep-alive check.
        let hn = HeaderName::from_bytes(b"keep-alive").unwrap();
        assert!(is_hop_by_hop(&hn));
    }

    // -----------------------------------------------------------------
    // PrefixedStream
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn prefixed_stream_emits_prefix_then_inner() {
        let prefix = b"hello".to_vec();
        let inner = Cursor::new(b" world");
        let mut stream = PrefixedStream::new(prefix, inner);

        let mut out = vec![0u8; 11];
        let n = stream.read(&mut out).await.unwrap();
        // First read returns the prefix.
        assert_eq!(&out[..n], b"hello");

        let n2 = stream.read(&mut out).await.unwrap();
        // Second read returns the inner stream data.
        assert_eq!(&out[..n2], b" world");
    }

    #[tokio::test]
    async fn prefixed_stream_empty_prefix_reads_inner_directly() {
        let inner = Cursor::new(b"direct");
        let mut stream = PrefixedStream::new(vec![], inner);

        let mut out = vec![0u8; 16];
        let n = stream.read(&mut out).await.unwrap();
        assert_eq!(&out[..n], b"direct");
    }

    #[tokio::test]
    async fn prefixed_stream_partial_prefix_read() {
        let prefix = b"abcdef".to_vec();
        let inner = Cursor::new(b"ghij");
        let mut stream = PrefixedStream::new(prefix, inner);

        // Read with a small buffer that cannot hold the entire prefix.
        let mut small = vec![0u8; 3];
        let n1 = stream.read(&mut small).await.unwrap();
        assert_eq!(&small[..n1], b"abc");

        let n2 = stream.read(&mut small).await.unwrap();
        assert_eq!(&small[..n2], b"def");

        let n3 = stream.read(&mut small).await.unwrap();
        assert_eq!(&small[..n3], b"ghi");

        let n4 = stream.read(&mut small).await.unwrap();
        assert_eq!(&small[..n4], b"j");
    }

    #[tokio::test]
    async fn prefixed_stream_read_to_end() {
        let prefix = b"pre-".to_vec();
        let inner = Cursor::new(b"data");
        let mut stream = PrefixedStream::new(prefix, inner);

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"pre-data");
    }

    // -----------------------------------------------------------------
    // LogOnDrop
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn log_on_drop_fires_callback_when_stream_ends() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let items: Vec<Result<i32, &str>> = vec![Ok(1), Ok(2)];
        let inner = stream::iter(items);

        let mut wrapper = LogOnDrop {
            inner,
            logged: false,
            on_drop: Some(Box::new(move || {
                fired_clone.store(true, Ordering::SeqCst);
            })),
        };

        use futures_util::StreamExt;
        // Drain the stream.
        assert_eq!(wrapper.next().await, Some(Ok(1)));
        assert!(!fired.load(Ordering::SeqCst), "should not fire mid-stream");
        assert_eq!(wrapper.next().await, Some(Ok(2)));
        assert!(!fired.load(Ordering::SeqCst), "should not fire mid-stream");
        // Stream ends.
        assert_eq!(wrapper.next().await, None);
        assert!(fired.load(Ordering::SeqCst), "should fire when stream ends");
    }

    #[tokio::test]
    async fn log_on_drop_fires_callback_on_early_drop() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let items: Vec<Result<i32, &str>> = vec![Ok(1), Ok(2), Ok(3)];
        let inner = stream::iter(items);

        let mut wrapper = LogOnDrop {
            inner,
            logged: false,
            on_drop: Some(Box::new(move || {
                fired_clone.store(true, Ordering::SeqCst);
            })),
        };

        use futures_util::StreamExt;
        // Read one item then drop without draining.
        assert_eq!(wrapper.next().await, Some(Ok(1)));
        assert!(!fired.load(Ordering::SeqCst));
        drop(wrapper);
        assert!(
            fired.load(Ordering::SeqCst),
            "should fire on drop when stream not drained"
        );
    }

    #[tokio::test]
    async fn log_on_drop_does_not_double_fire() {
        let count = Arc::new(AtomicU64::new(0));
        let count_clone = count.clone();
        let items: Vec<Result<i32, &str>> = vec![Ok(1)];
        let inner = stream::iter(items);

        let mut wrapper = LogOnDrop {
            inner,
            logged: false,
            on_drop: Some(Box::new(move || {
                count_clone.fetch_add(1, Ordering::SeqCst);
            })),
        };

        use futures_util::StreamExt;
        // Drain fully (fires on None).
        let _ = wrapper.next().await;
        let _ = wrapper.next().await; // None - fires callback
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // Drop after stream end should not fire again.
        drop(wrapper);
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "callback must fire exactly once"
        );
    }

    // -----------------------------------------------------------------
    // LogWriter
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn log_writer_writes_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let writer = LogWriter::create(&path).await.unwrap();

        let entry = ProxyEntry {
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            method: "GET",
            host: "example.com",
            url: "/v2/repo/manifests/latest",
            request_bytes: 0,
            response_bytes: 1234,
            status: 200,
            duration_ms: 42,
        };
        writer.write_entry(&entry).await;

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "should have exactly one JSONL line");

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["method"], "GET");
        assert_eq!(parsed["host"], "example.com");
        assert_eq!(parsed["status"], 200);
        assert_eq!(parsed["response_bytes"], 1234);
        assert_eq!(parsed["duration_ms"], 42);
    }

    #[tokio::test]
    async fn log_writer_truncates_on_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        tokio::fs::write(&path, b"stale data\n").await.unwrap();

        let writer = LogWriter::create(&path).await.unwrap();
        let entry = ProxyEntry {
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            method: "HEAD",
            host: "registry.example",
            url: "/v2/",
            request_bytes: 0,
            response_bytes: 0,
            status: 200,
            duration_ms: 1,
        };
        writer.write_entry(&entry).await;

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !content.contains("stale data"),
            "create should truncate existing content"
        );
        assert_eq!(content.lines().count(), 1);
    }
}
