//! Health and readiness endpoints for watch mode.
//!
//! `/healthz` (liveness) is always 200; `/readyz` (readiness) is 200 if the
//! last sync was within `2 * interval`, 503 otherwise. For Kubernetes probes.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use http::StatusCode;
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

    /// At least one success within `2 * interval`.
    fn is_ready(&self) -> bool {
        match self.last_success {
            Some(last) => last.elapsed() < self.interval * 2,
            None => false,
        }
    }
}

/// Format a complete HTTP/1.1 response with a text/plain body.
fn format_response(status: StatusCode, body: &str) -> String {
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Unknown"),
        body.len(),
    )
}

fn handle_request(path: &str, state: &HealthState) -> String {
    match path {
        "/healthz" => format_response(StatusCode::OK, "ok\n"),
        "/readyz" if state.is_ready() => format_response(StatusCode::OK, "ok\n"),
        "/readyz" => format_response(StatusCode::SERVICE_UNAVAILABLE, "sync stale\n"),
        _ => format_response(StatusCode::NOT_FOUND, "not found\n"),
    }
}

/// Serve `/healthz` and `/readyz` on a pre-bound listener until the task is
/// cancelled. Must run inside a `spawn_local` on the `current_thread` runtime.
///
/// Takes a listener, not a port, so the caller reports bind errors before
/// spawning and tests avoid bind-discover-rebind races.
pub(crate) async fn serve(listener: TcpListener, state: Rc<RefCell<HealthState>>) {
    loop {
        let (mut stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "health server accept failed, continuing");
                continue;
            }
        };

        // Only the request line is needed for routing.
        let mut buf = [0u8; 256];
        let n = match stream.read(&mut buf).await {
            Ok(0) => continue, // closed immediately (TCP probe)
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

    const OK: &str = "HTTP/1.1 200 OK\r\n";
    const UNAVAILABLE: &str = "HTTP/1.1 503 Service Unavailable\r\n";
    const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\n";

    /// A state whose last success was `age` ago, or never.
    fn state(interval: Duration, age: Option<Duration>) -> HealthState {
        let mut state = HealthState::new(interval);
        state.last_success = age.map(|age| Instant::now() - age);
        state
    }

    /// A state that has just recorded a success through the production path.
    fn succeeded(interval: Duration) -> HealthState {
        let mut state = HealthState::new(interval);
        state.record_success();
        state
    }

    const MINUTE: Duration = Duration::from_secs(60);

    /// Cases shared by the handler and TCP tests:
    /// (name, state, path, status line, body suffix). Liveness stays 200 while
    /// readiness is 503, whatever the state.
    fn route_cases() -> Vec<(
        &'static str,
        HealthState,
        &'static str,
        &'static str,
        &'static str,
    )> {
        let short = Duration::from_millis(10);
        vec![
            (
                "healthz before first sync",
                state(MINUTE, None),
                "/healthz",
                OK,
                "ok\n",
            ),
            (
                "healthz after success",
                succeeded(MINUTE),
                "/healthz",
                OK,
                "ok\n",
            ),
            (
                "healthz when stale",
                state(short, Some(short * 3)),
                "/healthz",
                OK,
                "ok\n",
            ),
            (
                "readyz before first sync",
                state(MINUTE, None),
                "/readyz",
                UNAVAILABLE,
                "sync stale\n",
            ),
            (
                "readyz after success",
                succeeded(MINUTE),
                "/readyz",
                OK,
                "ok\n",
            ),
            (
                "readyz when stale",
                state(short, Some(short * 3)),
                "/readyz",
                UNAVAILABLE,
                "sync stale\n",
            ),
            (
                "unknown path",
                state(MINUTE, None),
                "/foo",
                NOT_FOUND,
                "not found\n",
            ),
            (
                "empty path",
                state(MINUTE, None),
                "",
                NOT_FOUND,
                "not found\n",
            ),
        ]
    }

    #[test]
    fn handle_request_routes_by_path_and_state() {
        for (name, state, path, status, body) in route_cases() {
            let resp = handle_request(path, &state);
            assert!(resp.starts_with(status), "{name}: {resp}");
            assert!(resp.ends_with(body), "{name}: {resp}");
        }
    }

    #[test]
    fn is_ready_requires_a_success_within_twice_the_interval() {
        let cases = [
            ("no success", MINUTE, None, false),
            (
                "stale success",
                Duration::from_millis(10),
                Some(Duration::from_millis(30)),
                false,
            ),
            // Just inside 2 * interval.
            (
                "just inside",
                MINUTE,
                Some(MINUTE + Duration::from_secs(59)),
                true,
            ),
            // Just outside 2 * interval.
            (
                "just outside",
                Duration::from_millis(50),
                Some(Duration::from_millis(101)),
                false,
            ),
        ];
        for (name, interval, age, want) in cases {
            assert_eq!(state(interval, age).is_ready(), want, "{name}");
        }
        assert!(succeeded(MINUTE).is_ready(), "recent success");
    }

    #[test]
    fn format_response_writes_status_headers_and_length() {
        let cases = [
            (
                StatusCode::OK,
                "ok\n",
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "sync stale\n",
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nsync stale\n",
            ),
            (
                StatusCode::NOT_FOUND,
                "not found\n",
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nnot found\n",
            ),
        ];
        for (status, body, want) in cases {
            assert_eq!(format_response(status, body), want, "{status}");
        }
    }

    /// Send a raw HTTP request to a real `serve()` on an ephemeral port and
    /// return the response.
    async fn serve_and_request(state: HealthState, request: &[u8]) -> String {
        use tokio::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let state = Rc::new(RefCell::new(state));
        let server = tokio::task::spawn_local(async move { serve(listener, state).await });

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
    async fn serve_answers_over_tcp() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                for (name, state, path, status, body) in route_cases() {
                    if path.is_empty() {
                        continue;
                    }
                    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
                    let resp = serve_and_request(state, request.as_bytes()).await;
                    assert!(resp.starts_with(status), "{name}: got: {resp}");
                    assert!(resp.ends_with(body), "{name}: got: {resp}");
                }
            })
            .await;
    }
}
