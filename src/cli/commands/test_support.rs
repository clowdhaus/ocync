//! Shared scaffolding for the command modules' prepare-phase concurrency tests.
//!
//! `sync` and `analyze` resolve mappings through the same code and prove the
//! same property: independent network work overlaps. Both need a registry that
//! answers slowly and records when each request arrived.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wiremock::{MockServer, Request, Respond, ResponseTemplate};

/// Instants at which requests reached a stub, shared with the test.
pub(crate) type Arrivals = Arc<Mutex<Vec<Instant>>>;

/// A fresh arrival recorder.
pub(crate) fn arrivals() -> Arrivals {
    Arc::new(Mutex::new(Vec::new()))
}

/// The largest number of requests in flight at once: the most arrivals inside
/// any window of `delay`. Sequential work spaces arrivals at least `delay`
/// apart, so this returns 1.
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

/// Responds with a fixed body after `delay`, recording each arrival.
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

    /// A single-layer OCI image manifest. The client digests the bytes it
    /// receives, so no header has to agree.
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
        // `set_body_json` forces `application/json`, and the client picks its
        // manifest parser from the content type, so it would reject the manifest.
        let body = serde_json::to_vec(&self.body).expect("stub body serializes");
        ResponseTemplate::new(200)
            .set_body_raw(body, self.content_type.unwrap_or("application/json"))
            .set_delay(self.delay)
    }
}

/// A config whose `mappings` all point at `server`; `tags_block` is the body of
/// each mapping's `tags:` block.
pub(crate) fn config_yaml(server: &MockServer, repos: &[String], tags_block: &str) -> String {
    let mut yaml = format!(
        "registries:\n  src:\n    url: {uri}\n  dst:\n    url: {uri}\n\
         defaults:\n  source: src\n  targets: [dst]\nmappings:\n",
        uri = server.uri()
    );
    for repo in repos {
        yaml.push_str(&format!("  - from: {repo}\n    tags:\n{tags_block}"));
    }
    yaml
}

/// `count` repository names of the shape the stubs match.
pub(crate) fn repo_names(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("repo/img{i}")).collect()
}

/// A mapping that failed to resolve, classified as `code`.
pub(crate) fn unresolved_mapping(
    from: &str,
    code: crate::cli::ExitCode,
) -> crate::cli::commands::synchronize::UnresolvedMapping {
    crate::cli::commands::synchronize::UnresolvedMapping {
        from: from.into(),
        error: "boom".into(),
        code,
    }
}

/// A registry that answers tag listings and manifest pulls after a delay,
/// with the config and clients pointed at it.
pub(crate) struct SlowRegistry {
    /// The stub server; dropping it returns the pooled server and resets its
    /// mocks, so the struct stays whole for the test's duration.
    #[expect(dead_code, reason = "held only for its drop")]
    pub(crate) server: MockServer,
    /// A config with one mapping per repository, all through the server.
    pub(crate) config: crate::cli::config::Config,
    /// Clients for the config's `src` and `dst` registries.
    pub(crate) clients: crate::cli::commands::synchronize::ClientMap,
    /// When each `tags/list` request arrived.
    pub(crate) tag_arrivals: Arrivals,
    /// When each manifest request arrived.
    pub(crate) manifest_arrivals: Arrivals,
}

/// Start a [`SlowRegistry`] serving `tags` for each of `repos`.
pub(crate) async fn slow_registry(
    repos: &[String],
    tags: &[&str],
    delay: Duration,
) -> SlowRegistry {
    use crate::cli::test_support::test_client;
    use wiremock::matchers::{method, path_regex};

    let server = MockServer::start().await;
    let tag_arrivals = arrivals();
    let manifest_arrivals = arrivals();

    wiremock::Mock::given(method("GET"))
        .and(path_regex(r"^/v2/repo/img\d+/tags/list$"))
        .respond_with(SlowRecorder::tag_list(&tag_arrivals, delay, tags))
        .mount(&server)
        .await;
    wiremock::Mock::given(method("GET"))
        .and(path_regex(r"^/v2/repo/img\d+/manifests/.+$"))
        .respond_with(SlowRecorder::image_manifest(&manifest_arrivals, delay))
        .mount(&server)
        .await;

    let yaml = config_yaml(&server, repos, "      glob: [\"*\"]\n");
    let config = serde_yaml::from_str(&yaml).expect("generated config parses");
    let clients = crate::cli::commands::synchronize::ClientMap::from([
        ("src".to_string(), Ok(test_client(&server.uri()))),
        ("dst".to_string(), Ok(test_client(&server.uri()))),
    ]);
    SlowRegistry {
        server,
        config,
        clients,
        tag_arrivals,
        manifest_arrivals,
    }
}
