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

        let (status, body) = match path {
            "/healthz" => {
                if state.borrow().is_healthy() {
                    ("200 OK", "ok\n")
                } else {
                    ("503 Service Unavailable", "sync stale\n")
                }
            }
            _ => ("404 Not Found", "not found\n"),
        };

        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        if let Err(err) = stream.write_all(response.as_bytes()).await {
            tracing::warn!(error = %err, "failed to write health response");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

    /// Helper: start the health server on an OS-assigned port and return the port.
    async fn start_server(state: Rc<RefCell<HealthState>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Re-implement serve inline so we can use the pre-bound listener
        // instead of racing on a port.
        tokio::task::spawn_local(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 256];
                let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                    .await
                    .unwrap_or(0);
                let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
                let path = request.split_whitespace().nth(1).unwrap_or("");

                let (status, body) = match path {
                    "/healthz" => {
                        if state.borrow().is_healthy() {
                            ("200 OK", "ok\n")
                        } else {
                            ("503 Service Unavailable", "sync stale\n")
                        }
                    }
                    _ => ("404 Not Found", "not found\n"),
                };

                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        port
    }

    /// Send a raw HTTP GET and return the full response as a string.
    async fn http_get(port: u16, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[tokio::test]
    async fn healthz_returns_503_before_first_sync() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));
                let port = start_server(state).await;
                let resp = http_get(port, "/healthz").await;
                assert!(
                    resp.starts_with("HTTP/1.1 503"),
                    "expected 503, got: {resp}"
                );
                assert!(resp.ends_with("sync stale\n"));
            })
            .await;
    }

    #[tokio::test]
    async fn healthz_returns_200_after_successful_sync() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));
                state.borrow_mut().record_success();
                let port = start_server(state).await;
                let resp = http_get(port, "/healthz").await;
                assert!(
                    resp.starts_with("HTTP/1.1 200"),
                    "expected 200, got: {resp}"
                );
                assert!(resp.ends_with("ok\n"));
            })
            .await;
    }

    #[tokio::test]
    async fn healthz_returns_503_when_stale() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let interval = Duration::from_millis(50);
                let state = Rc::new(RefCell::new(HealthState::new(interval)));
                // Simulate a success that happened long ago.
                state.borrow_mut().last_success = Some(Instant::now() - interval * 3);
                let port = start_server(state).await;
                let resp = http_get(port, "/healthz").await;
                assert!(
                    resp.starts_with("HTTP/1.1 503"),
                    "expected 503, got: {resp}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let state = Rc::new(RefCell::new(HealthState::new(Duration::from_secs(60))));
                let port = start_server(state).await;
                let resp = http_get(port, "/foo").await;
                assert!(
                    resp.starts_with("HTTP/1.1 404"),
                    "expected 404, got: {resp}"
                );
                assert!(resp.ends_with("not found\n"));
            })
            .await;
    }

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
}
