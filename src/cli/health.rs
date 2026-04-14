//! Health endpoint for watch mode.
//!
//! Exposes `/healthz` for Kubernetes liveness probes. Returns 200 if
//! the last successful sync completed within `2 * interval`, 503
//! otherwise.

use std::cell::RefCell;
use std::io;
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

/// Route a request path to an HTTP status and body based on health state.
fn handle_request<'a>(path: &str, state: &HealthState) -> (&'a str, &'a str) {
    match path {
        "/healthz" => {
            if state.is_healthy() {
                ("200 OK", "ok\n")
            } else {
                ("503 Service Unavailable", "sync stale\n")
            }
        }
        _ => ("404 Not Found", "not found\n"),
    }
}

/// Format an HTTP/1.1 response with the given status and body.
fn format_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )
}

/// Start the health server on the given port.
///
/// Binds to `0.0.0.0:{port}` and serves `/healthz`. Runs until the
/// listener is dropped. Must be called from within a `spawn_local` on
/// the `current_thread` runtime.
pub(crate) async fn serve(port: u16, state: Rc<RefCell<HealthState>>) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "health server listening");

    loop {
        let (mut stream, _addr) = listener.accept().await?;

        // Read the request line to extract the path. We only need the
        // first line — ignore headers and body.
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).await?;
        let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let path = request.split_whitespace().nth(1).unwrap_or("");

        let (status, body) = handle_request(path, &state.borrow());
        let response = format_response(status, body);
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
    fn healthz_returns_503_before_first_sync() {
        let state = HealthState::new(Duration::from_secs(60));
        let (status, body) = handle_request("/healthz", &state);
        assert_eq!(status, "503 Service Unavailable");
        assert_eq!(body, "sync stale\n");
    }

    #[test]
    fn healthz_returns_200_after_successful_sync() {
        let mut state = HealthState::new(Duration::from_secs(60));
        state.record_success();
        let (status, body) = handle_request("/healthz", &state);
        assert_eq!(status, "200 OK");
        assert_eq!(body, "ok\n");
    }

    #[test]
    fn healthz_returns_503_when_stale() {
        let interval = Duration::from_millis(10);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - interval * 3);
        let (status, body) = handle_request("/healthz", &state);
        assert_eq!(status, "503 Service Unavailable");
        assert_eq!(body, "sync stale\n");
    }

    #[test]
    fn unknown_path_returns_404() {
        let state = HealthState::new(Duration::from_secs(60));
        let (status, body) = handle_request("/foo", &state);
        assert_eq!(status, "404 Not Found");
        assert_eq!(body, "not found\n");
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

    // -- response formatting --

    #[test]
    fn format_response_is_valid_http() {
        let response = format_response("200 OK", "ok\n");
        assert_eq!(
            response,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n"
        );
    }
}
