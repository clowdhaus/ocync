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

/// HTTP response components from the health handler.
struct HealthResponse {
    status: &'static str,
    body: &'static str,
}

impl HealthResponse {
    /// Format as an HTTP/1.1 response string.
    fn to_http(&self) -> String {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.status,
            self.body.len(),
            self.body,
        )
    }
}

/// Route a request path to an HTTP response based on health state.
fn handle_request(path: &str, state: &HealthState) -> HealthResponse {
    match path {
        "/healthz" => {
            if state.is_healthy() {
                HealthResponse {
                    status: "200 OK",
                    body: "ok\n",
                }
            } else {
                HealthResponse {
                    status: "503 Service Unavailable",
                    body: "sync stale\n",
                }
            }
        }
        _ => HealthResponse {
            status: "404 Not Found",
            body: "not found\n",
        },
    }
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
        let (mut stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "health server accept failed, continuing");
                continue;
            }
        };

        // Read the request line to extract the path. We only need the
        // first line — ignore headers and body.
        let mut buf = [0u8; 256];
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "health server read failed, continuing");
                continue;
            }
        };
        let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let path = request.split_whitespace().nth(1).unwrap_or("");

        let resp = handle_request(path, &state.borrow());
        let response = resp.to_http();
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
        let resp = handle_request("/healthz", &state);
        assert_eq!(resp.status, "503 Service Unavailable");
        assert_eq!(resp.body, "sync stale\n");
    }

    #[test]
    fn healthz_returns_200_after_successful_sync() {
        let mut state = HealthState::new(Duration::from_secs(60));
        state.record_success();
        let resp = handle_request("/healthz", &state);
        assert_eq!(resp.status, "200 OK");
        assert_eq!(resp.body, "ok\n");
    }

    #[test]
    fn healthz_returns_503_when_stale() {
        let interval = Duration::from_millis(10);
        let mut state = HealthState::new(interval);
        state.last_success = Some(Instant::now() - interval * 3);
        let resp = handle_request("/healthz", &state);
        assert_eq!(resp.status, "503 Service Unavailable");
        assert_eq!(resp.body, "sync stale\n");
    }

    #[test]
    fn unknown_path_returns_404() {
        let state = HealthState::new(Duration::from_secs(60));
        let resp = handle_request("/foo", &state);
        assert_eq!(resp.status, "404 Not Found");
        assert_eq!(resp.body, "not found\n");
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
    fn to_http_200() {
        let resp = HealthResponse {
            status: "200 OK",
            body: "ok\n",
        };
        assert_eq!(
            resp.to_http(),
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n"
        );
    }

    #[test]
    fn to_http_503() {
        let resp = HealthResponse {
            status: "503 Service Unavailable",
            body: "sync stale\n",
        };
        assert_eq!(
            resp.to_http(),
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nsync stale\n"
        );
    }

    #[test]
    fn to_http_404() {
        let resp = HealthResponse {
            status: "404 Not Found",
            body: "not found\n",
        };
        assert_eq!(
            resp.to_http(),
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nnot found\n"
        );
    }
}
