//! Recorded HTTP exchanges answered in order, read from the
//! `http-traffic.json` files of the upstream credential cases, and checked
//! as upstream's replaying client checks them: the method, the URL with its
//! query read as a set and a trailing slash ignored, and the `content-type`
//! and `x-aws-ec2-metadata-token` headers. Bodies are not compared.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;

use super::credentials::TOKEN_HEADER;

/// The request headers compared, as upstream compares them.
const COMPARED_HEADERS: &[&str] = &["content-type", TOKEN_HEADER];

/// One recorded request and its answer.
#[derive(Debug)]
struct Exchange {
    method: String,
    uri: String,
    headers: BTreeMap<String, Vec<String>>,
    status: u16,
    response_headers: BTreeMap<String, Vec<String>>,
    body: Vec<u8>,
    /// How long the answer takes, on tokio's clock.
    delay: Duration,
}

/// The exchanges not answered yet.
#[derive(Debug)]
pub(super) struct Replay {
    pending: Mutex<VecDeque<Exchange>>,
}

/// One event of a traffic file.
#[derive(Deserialize)]
struct Event {
    connection_id: u64,
    action: Action,
}

#[derive(Deserialize)]
enum Action {
    Request { request: Request },
    Response { response: ResponseResult },
    Data { data: Body, direction: Direction },
    Eof {},
}

#[derive(Deserialize)]
struct Request {
    uri: String,
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
enum ResponseResult {
    Ok(Response),
}

#[derive(Deserialize)]
struct Response {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
enum Body {
    Utf8(String),
    Base64(String),
}

#[derive(Deserialize)]
enum Direction {
    Request,
    Response,
}

/// A partly read exchange, by connection.
#[derive(Default)]
struct Draft {
    request: Option<Request>,
    response: Option<Response>,
    body: Vec<u8>,
}

impl Replay {
    /// The exchanges of a traffic file, in the order their connections
    /// opened.
    pub(super) fn from_json(text: &str) -> Self {
        #[derive(Deserialize)]
        struct Traffic {
            events: Vec<Event>,
        }
        let traffic: Traffic = serde_json::from_str(text).expect("a traffic file");
        let mut order = Vec::new();
        let mut drafts: BTreeMap<u64, Draft> = BTreeMap::new();
        for event in traffic.events {
            let draft = drafts.entry(event.connection_id).or_insert_with(|| {
                order.push(event.connection_id);
                Draft::default()
            });
            match event.action {
                Action::Request { request } => draft.request = Some(request),
                Action::Response {
                    response: ResponseResult::Ok(response),
                } => draft.response = Some(response),
                Action::Data {
                    data,
                    direction: Direction::Response,
                } => {
                    draft.body.extend(match data {
                        Body::Utf8(text) => text.into_bytes(),
                        Body::Base64(text) => base64::engine::general_purpose::STANDARD
                            .decode(text)
                            .expect("base64 body"),
                    });
                }
                Action::Data { .. } | Action::Eof {} => {}
            }
        }
        let pending = order
            .into_iter()
            .map(|id| {
                let draft = drafts.remove(&id).expect("each connection is drafted");
                let request = draft.request.expect("each connection has a request");
                let response = draft.response.expect("each connection has a response");
                Exchange {
                    method: request.method,
                    uri: request.uri,
                    headers: lowercase(request.headers),
                    status: response.status,
                    response_headers: response.headers,
                    body: draft.body,
                    delay: Duration::ZERO,
                }
            })
            .collect();
        Self {
            pending: Mutex::new(pending),
        }
    }

    /// The same exchanges, with the one at `index` answered after `delay`.
    pub(super) fn delayed(self, index: usize, delay: Duration) -> Self {
        self.pending.lock().unwrap()[index].delay = delay;
        self
    }

    /// The answer to `request`, which must be the next one recorded, after
    /// its delay.
    pub(super) async fn answer(&self, request: &reqwest::Request) -> reqwest::Response {
        let exchange = self.pending.lock().unwrap().pop_front().unwrap_or_else(|| {
            panic!("unrecorded request: {} {}", request.method(), request.url())
        });
        tokio::time::sleep(exchange.delay).await;
        assert_eq!(
            request.method().as_str(),
            exchange.method,
            "method of {}",
            exchange.uri
        );
        assert_eq!(
            comparable(request.url().as_str()),
            comparable(&exchange.uri),
            "URL of a {} request",
            exchange.method
        );
        for name in COMPARED_HEADERS {
            let sent: Vec<_> = request
                .headers()
                .get_all(*name)
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect();
            let recorded = exchange.headers.get(*name).cloned().unwrap_or_default();
            assert_eq!(sent, recorded, "{name} of {}", exchange.uri);
        }
        let mut response = http::Response::builder().status(exchange.status);
        for (name, values) in &exchange.response_headers {
            for value in values {
                response = response.header(name, value);
            }
        }
        reqwest::Response::from(response.body(exchange.body).unwrap())
    }

    /// Every recorded exchange was answered.
    pub(super) fn assert_done(&self) {
        let pending = self.pending.lock().unwrap();
        assert!(
            pending.is_empty(),
            "requests recorded but not sent: {:?}",
            pending.iter().map(|e| &e.uri).collect::<Vec<_>>()
        );
    }
}

/// `headers` with lowercased names.
fn lowercase(headers: BTreeMap<String, Vec<String>>) -> BTreeMap<String, Vec<String>> {
    headers
        .into_iter()
        .map(|(name, values)| (name.to_ascii_lowercase(), values))
        .collect()
}

/// A URL as compared: scheme, host and port, the path without a trailing
/// slash, and the query as a set of pairs.
fn comparable(text: &str) -> (String, String, BTreeSet<(String, String)>) {
    let url = url::Url::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/').to_owned();
    let query = url.query_pairs().into_owned().collect();
    (origin, path, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRAFFIC: &str = r#"{"events": [
        {"connection_id": 0, "action": {"Request": {"request": {
            "uri": "http://169.254.169.254/latest/api/token", "method": "PUT",
            "headers": {"X-aws-ec2-metadata-token": ["t"]}}}}},
        {"connection_id": 0, "action": {"Response": {"response": {"Ok": {
            "status": 200, "version": "HTTP/1.1", "headers": {}}}}}},
        {"connection_id": 0, "action": {"Data": {"data": {"Utf8": "body"},
            "direction": "Response"}}},
        {"connection_id": 0, "action": {"Eof": {"ok": true, "direction": "Response"}}}
    ]}"#;

    fn request(method: reqwest::Method, url: &str, token: Option<&str>) -> reqwest::Request {
        let mut request = reqwest::Request::new(method, url.parse().unwrap());
        if let Some(token) = token {
            request
                .headers_mut()
                .insert(TOKEN_HEADER, token.parse().unwrap());
        }
        request
    }

    /// A matching request gets the recorded answer, with a trailing slash
    /// ignored. Breaks if the body or status is lost.
    #[tokio::test]
    async fn answers_a_matching_request() {
        let replay = Replay::from_json(TRAFFIC);
        let response = replay
            .answer(&request(
                reqwest::Method::PUT,
                "http://169.254.169.254/latest/api/token/",
                Some("t"),
            ))
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "body");
        replay.assert_done();
    }

    /// A request without the recorded token header fails the test. Breaks
    /// if the compared headers are not compared.
    #[tokio::test]
    #[should_panic(expected = "x-aws-ec2-metadata-token")]
    async fn refuses_a_request_with_other_headers() {
        Replay::from_json(TRAFFIC)
            .answer(&request(
                reqwest::Method::PUT,
                "http://169.254.169.254/latest/api/token",
                None,
            ))
            .await;
    }

    /// A recorded exchange never sent fails the test. Breaks if a source
    /// may skip a request upstream sends.
    #[test]
    #[should_panic(expected = "recorded but not sent")]
    fn refuses_an_unsent_exchange() {
        Replay::from_json(TRAFFIC).assert_done();
    }
}
