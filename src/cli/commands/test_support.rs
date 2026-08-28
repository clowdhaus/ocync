//! Shared scaffolding for the prepare-phase concurrency tests.
//!
//! `sync` and `analyze` resolve mappings through the same code and have to
//! prove the same property: that independent network work overlaps. Both need
//! a registry that answers slowly and records when each request arrived, so it
//! lives here rather than being written twice.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ocync_distribution::{RegistryClient, RegistryClientBuilder};
use wiremock::{MockServer, Request, Respond, ResponseTemplate};

/// Instants at which requests reached a stub, shared with the test.
pub(crate) type Arrivals = Arc<Mutex<Vec<Instant>>>;

/// A fresh arrival recorder.
pub(crate) fn arrivals() -> Arrivals {
    Arc::new(Mutex::new(Vec::new()))
}

/// The largest number of requests that were in flight at once.
///
/// A request is in flight from its arrival until `delay` later, so the answer
/// is the most arrivals falling inside any window of that length. Under
/// sequential work consecutive arrivals are at least `delay` apart, so every
/// window holds exactly one and this returns 1.
pub(crate) fn max_in_flight(recorded: &Arrivals, delay: Duration) -> usize {
    let arrivals = recorded.lock().expect("no panic in a stub");
    arrivals
        .iter()
        .map(|start| {
            arrivals
                .iter()
                .filter(|other| **other >= *start && other.duration_since(*start) < delay)
                .count()
        })
        .max()
        .unwrap_or(0)
}

/// Responds with a fixed body after `delay`, recording when each request
/// arrived.
pub(crate) struct SlowRecorder {
    arrivals: Arrivals,
    delay: Duration,
    body: serde_json::Value,
    content_type: Option<&'static str>,
}

impl SlowRecorder {
    /// A `tags/list` response carrying `tags`.
    pub(crate) fn tag_list(arrivals: &Arrivals, delay: Duration, tags: &[&str]) -> Self {
        Self {
            arrivals: Arc::clone(arrivals),
            delay,
            body: serde_json::json!({"name": "repo", "tags": tags}),
            content_type: None,
        }
    }

    /// A single-layer OCI image manifest.
    ///
    /// The client computes the digest from the bytes it receives, so nothing
    /// here has to agree with a header.
    pub(crate) fn image_manifest(arrivals: &Arrivals, delay: Duration) -> Self {
        Self {
            arrivals: Arc::clone(arrivals),
            delay,
            body: serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": format!("sha256:{}", "a".repeat(64)),
                    "size": 100,
                },
                "layers": [{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": format!("sha256:{}", "b".repeat(64)),
                    "size": 200,
                }],
            }),
            content_type: Some("application/vnd.oci.image.manifest.v1+json"),
        }
    }
}

impl Respond for SlowRecorder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.arrivals
            .lock()
            .expect("no panic in a stub")
            .push(Instant::now());
        // `set_body_raw` rather than `set_body_json`: the latter forces
        // `application/json`, and the client picks its manifest parser from
        // the content type, so a manifest served that way is rejected as an
        // unsupported media type.
        let body = serde_json::to_vec(&self.body).expect("stub body serializes");
        ResponseTemplate::new(200)
            .set_body_raw(body, self.content_type.unwrap_or("application/json"))
            .set_delay(self.delay)
    }
}

/// A config whose `mappings` entries all point at `server`.
///
/// `tags` is the filter each mapping carries, written as it would appear under
/// a mapping's `tags:` block.
pub(crate) fn config_yaml(server: &MockServer, repos: &[String], tags: &str) -> String {
    let mut yaml = format!(
        "registries:\n  src:\n    url: {uri}\n  dst:\n    url: {uri}\n\
         defaults:\n  source: src\n  targets: [dst]\nmappings:\n",
        uri = server.uri()
    );
    for repo in repos {
        yaml.push_str(&format!("  - from: {repo}\n    tags:\n{tags}"));
    }
    yaml
}

/// `count` repository names of the shape the stubs match.
pub(crate) fn repo_names(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("repo/img{i}")).collect()
}

/// An anonymous client pointed at a mock server.
pub(crate) fn test_client(url: &str) -> Arc<RegistryClient> {
    Arc::new(
        RegistryClientBuilder::new(url::Url::parse(url).expect("mock server url parses"))
            .build()
            .expect("client builds"),
    )
}
