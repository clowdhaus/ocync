//! Health and readiness endpoints for watch mode.
//!
//! Exposes `/healthz` (liveness — always 200) and `/readyz` (readiness —
//! 200 if last sync within `2 * interval`, 503 otherwise) for Kubernetes
//! probes.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Shared health state updated by the watch loop, read by the health server.
pub(crate) struct HealthState {
    last_success: Option<Instant>,
    interval: Duration,
}

impl HealthState {
    /// Create a new health state with no successful sync yet.
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            last_success: None,
            interval,
        }
    }

    /// Record a successful sync completion.
    pub(crate) fn record_success(&mut self) {
        self.last_success = Some(Instant::now());
    }

    /// Returns `true` if the sync loop is healthy: at least one success
    /// within `2 * interval`.
    fn is_healthy(&self) -> bool {
        match self.last_success {
            Some(last) => last.elapsed() < self.interval * 2,
            None => false,
        }
    }
}

/// Format a complete HTTP/1.1 response string with a text/plain body.
fn format_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )
}

/// Route a request path to a formatted HTTP response string.
///
/// - `/healthz` — liveness: always 200 (process is running).
/// - `/readyz`  — readiness: 200 if last sync within `2 * interval`, 503 otherwise.
fn handle_request(path: &str, state: &HealthState) -> String {
    match path {
        "/healthz" => format_response("200 OK", "ok\n"),
        "/readyz" if state.is_healthy() => format_response("200 OK", "ok\n"),
        "/readyz" => format_response("503 Service Unavailable", "sync stale\n"),
        _ => format_response("404 Not Found", "not found\n"),
    }
}

/// Start the health server on a pre-bound listener.
///
/// Serves `/healthz` (liveness) and `/readyz` (readiness) until the task
/// is cancelled. Must be called from within a `spawn_local` on the
/// `current_thread` runtime.
///
/// Accepts a `TcpListener` rather than a port number so the caller
/// controls binding (avoids TOCTOU races in tests and lets production
/// code report bind errors before spawning).
pub(crate) async fn serve(listener: TcpListener, state: Rc<RefCell<HealthState>>) {
    loop {
        let (mut stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "health server accept failed, continuing");
                continue;
            }
        };

        // Read enough to extract the request line (path is always in the
        // first ~50 bytes). Headers beyond this are irrelevant for routing.
        let mut buf = [0u8; 256];
        let n = match stream.read(&mut buf).await {
            Ok(0) => continue, // client closed immediately (e.g. TCP probe)
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "health server read failed, continuing");
                continue;
            }
        };
        let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let path = request.split_whitespace().nth(1).unwrap_or("");

        let response = handle_request(path, &state.borrow());
        if let Err(err) = stream.write_all(response.as_bytes()).await {
            tracing::warn!(error = %err, "failed to write health response");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- handler unit tests (no TCP, no port) --

    #[test]
    fn healthz_returns_200_before_first_sync() {
        let state = HealthState::new(Duration::from_secs(60));
        let resp = handle_request("/healthz", &state);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.ends_with("ok\n"));
    }

    #[test]
    fn healthz_returns_200_after_successful_sync() {
        let mut state = HealthState::new(Duration::from_secs(60));
        state.record_success();
        let resp = handle_request("/healthz", &state);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.ends_with("ok\n"));
    }

    #[test]
    fn healthz_returns_200_when_stale() {
        let interval = Duration::from_millis(10);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - interval * 3);
        let resp = handle_request("/healthz", &state);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.ends_with("ok\n"));
    }

    #[test]
    fn readyz_returns_503_before_first_sync() {
        let state = HealthState::new(Duration::from_secs(60));
        let resp = handle_request("/readyz", &state);
        assert!(resp.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(resp.ends_with("sync stale\n"));
    }

    #[test]
    fn readyz_returns_200_after_successful_sync() {
        let mut state = HealthState::new(Duration::from_secs(60));
        state.record_success();
        let resp = handle_request("/readyz", &state);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.ends_with("ok\n"));
    }

    #[test]
    fn readyz_returns_503_when_stale() {
        let interval = Duration::from_millis(10);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - interval * 3);
        let resp = handle_request("/readyz", &state);
        assert!(resp.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(resp.ends_with("sync stale\n"));
    }

    #[test]
    fn unknown_path_returns_404() {
        let state = HealthState::new(Duration::from_secs(60));
        let resp = handle_request("/foo", &state);
        assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(resp.ends_with("not found\n"));
    }

    #[test]
    fn healthz_200_while_readyz_503() {
        // Core invariant: liveness is always 200 even when readiness is 503.
        let state = HealthState::new(Duration::from_secs(60));
        let liveness = handle_request("/healthz", &state);
        let readiness = handle_request("/readyz", &state);
        assert!(liveness.contains("200 OK"), "liveness: {liveness}");
        assert!(
            readiness.contains("503 Service Unavailable"),
            "readiness: {readiness}"
        );
    }

    // -- state logic unit tests --

    #[test]
    fn health_state_no_success_is_unhealthy() {
        let state = HealthState::new(Duration::from_secs(60));
        assert!(!state.is_healthy());
    }

    #[test]
    fn health_state_recent_success_is_healthy() {
        let mut state = HealthState::new(Duration::from_secs(60));
        state.record_success();
        assert!(state.is_healthy());
    }

    #[test]
    fn health_state_stale_success_is_unhealthy() {
        let interval = Duration::from_millis(10);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - interval * 3);
        assert!(!state.is_healthy());
    }

    #[test]
    fn health_state_boundary_just_inside_threshold() {
        // At exactly 2 * interval - epsilon, should still be healthy.
        let interval = Duration::from_secs(60);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - interval - Duration::from_secs(59));
        assert!(state.is_healthy());
    }

    #[test]
    fn health_state_boundary_just_outside_threshold() {
        // At exactly 2 * interval + epsilon, should be unhealthy.
        let interval = Duration::from_millis(50);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - Duration::from_millis(101));
        assert!(!state.is_healthy());
    }

    // -- response formatting --

    #[test]
    fn format_response_200() {
        assert_eq!(
            format_response("200 OK", "ok\n"),
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n"
        );
    }

    #[test]
    fn format_response_503() {
        assert_eq!(
            format_response("503 Service Unavailable", "sync stale\n"),
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nsync stale\n"
        );
    }

    #[test]
    fn format_response_404() {
        assert_eq!(
            format_response("404 Not Found", "not found\n"),
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nnot found\n"
        );
    }

    // -- TCP integration tests (exercises the real `serve()` code path) --

    /// Send a raw HTTP request to the health server and return the response.
    ///
    /// Passes a pre-bound `TcpListener` to `serve()`, eliminating the
    /// port race that occurs with bind-discover-drop-rebind patterns.
    async fn serve_and_request(state: Rc<RefCell<HealthState>>, request: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::task::spawn_local({
            let state = Rc::clone(&state);
            async move { serve(listener, state).await }
        });

        // Give the server a moment to start accepting.
        tokio::task::yield_now().await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request).await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        server.abort();
        String::from_utf8(response).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_healthz_always_200() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // No record_success — /healthz should still return 200.
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));

                let resp =
                    serve_and_request(state, b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .await;
                assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "got: {resp}");
                assert!(resp.ends_with("ok\n"), "got: {resp}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_readyz_503_before_sync() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));

                let resp =
                    serve_and_request(state, b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .await;
                assert!(
                    resp.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
                    "got: {resp}"
                );
                assert!(resp.ends_with("sync stale\n"), "got: {resp}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_readyz_200_after_sync() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));
                state.borrow_mut().record_success();

                let resp =
                    serve_and_request(state, b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .await;
                assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "got: {resp}");
                assert!(resp.ends_with("ok\n"), "got: {resp}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_unknown_path_404() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));

                let resp =
                    serve_and_request(state, b"GET /unknown HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .await;
                assert!(
                    resp.starts_with("HTTP/1.1 404 Not Found\r\n"),
                    "got: {resp}"
                );
                assert!(resp.ends_with("not found\n"), "got: {resp}");
            })
            .await;
    }
}
