//! Registry access shared by the CDN pre-warm and the corpus lock: hostname
//! normalization, the manifest `Accept` list and the OCI bearer-token flow.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Duration;

use crate::bench::corpus::DockerHubAuth;

/// The manifest media types a source image can be stored as: OCI image,
/// Docker v2 image, OCI index and Docker manifest list.
pub(crate) const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json,\
     application/vnd.docker.distribution.manifest.v2+json,\
     application/vnd.oci.image.index.v1+json,\
     application/vnd.docker.distribution.manifest.list.v2+json";

/// API host Docker Hub serves its registry from.
const DOCKER_HUB_API_HOST: &str = "registry-1.docker.io";

/// Every name Docker Hub's registry goes by in an image reference.
const DOCKER_HUB_NAMES: [&str; 3] = ["docker.io", DOCKER_HUB_API_HOST, "index.docker.io"];

/// The registry a repository lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Host<'a> {
    /// Docker Hub, named `docker.io`, `index.docker.io` or its API host in a
    /// corpus source, and served from its API host.
    DockerHub,
    /// Any other registry, by hostname.
    Other(&'a str),
}

impl Host<'_> {
    /// The hostname requests go to.
    pub(crate) fn api_host(&self) -> &str {
        match self {
            Self::DockerHub => DOCKER_HUB_API_HOST,
            Self::Other(host) => host,
        }
    }
}

/// A repository on a registry, as a corpus source names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Repository<'a> {
    /// The registry holding the repository.
    pub(crate) host: Host<'a>,
    /// Path within the registry, e.g. `library/alpine`.
    pub(crate) path: &'a str,
}

impl std::fmt::Display for Repository<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.host.api_host(), self.path)
    }
}

/// Split a corpus source into its registry and repository path.
///
/// `docker.io/library/alpine` becomes Docker Hub and `library/alpine`.
/// Returns `None` for a source with no `/`.
pub(crate) fn split_source(source: &str) -> Option<Repository<'_>> {
    let (host, path) = source.split_once('/')?;
    Some(Repository {
        host: host_of(host),
        path,
    })
}

/// The registry a hostname names.
fn host_of(name: &str) -> Host<'_> {
    if DOCKER_HUB_NAMES.contains(&name) {
        Host::DockerHub
    } else {
        Host::Other(name)
    }
}

/// Attempts at one request before a transient failure is reported.
const MAX_ATTEMPTS: u32 = 5;

/// Longest a single wait before a retry may last.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long to wait before retry number `attempt` (from 0): the registry's
/// `Retry-After` seconds when it sent them, else doubling from 2 s, never more
/// than [`MAX_BACKOFF`].
fn backoff(attempt: u32, retry_after: Option<&str>) -> Duration {
    let secs = retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| 2u64.saturating_pow(attempt.saturating_add(1)));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Whether a response status is worth another attempt: the registry is
/// throttling (429) or failing (5xx), not refusing the request.
fn transient(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Send the request `build` makes, waiting out what is transient: a failed
/// connection (always retried), a 429 and a 5xx, up to [`MAX_ATTEMPTS`] times. Any other
/// response, whatever its status, is returned for the caller to judge. `what`
/// names the request in the retry notice.
pub(crate) async fn send_retrying(
    what: &str,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    let mut attempt = 0;
    loop {
        let (failure, retry_after) = match build().send().await {
            Ok(response) if !transient(response.status()) => return Ok(response),
            Ok(response) => {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                (format!("returned {}", response.status()), retry_after)
            }
            Err(e) => (format!("failed: {e}"), None),
        };
        attempt += 1;
        if attempt >= MAX_ATTEMPTS {
            return Err(format!("{failure} ({attempt} attempts)"));
        }
        let wait = backoff(attempt - 1, retry_after.as_deref());
        eprintln!("  {what}: {failure}, retrying in {}s", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

/// A repository as a map key: the API host and the path, owned.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RepoKey {
    /// Hostname requests go to.
    pub(crate) host: String,
    /// Path within the registry.
    pub(crate) path: String,
}

impl From<&Repository<'_>> for RepoKey {
    fn from(repo: &Repository<'_>) -> Self {
        Self {
            host: repo.host.api_host().to_owned(),
            path: repo.path.to_owned(),
        }
    }
}

impl RepoKey {
    /// The repository this key names.
    pub(crate) fn repository(&self) -> Repository<'_> {
        Repository {
            host: host_of(&self.host),
            path: &self.path,
        }
    }
}

/// Bearer tokens by repository, so a repository with several tags is
/// authorized once. A token a registry has since expired is replaced with
/// [`Tokens::renew`] after a 401.
#[derive(Default)]
pub(crate) struct Tokens {
    /// Latest exchange for each repository; a failure is kept so it is
    /// reported for every tag and not retried at once.
    by_repository: HashMap<RepoKey, Result<String, String>>,
}

impl Tokens {
    /// The token for `repo`, exchanging for one the first time it is asked.
    pub(crate) async fn get(
        &mut self,
        client: &reqwest::Client,
        repo: &Repository<'_>,
        dockerhub_auth: Option<&DockerHubAuth>,
    ) -> &Result<String, String> {
        match self.by_repository.entry(RepoKey::from(repo)) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(oci_token(client, repo, dockerhub_auth).await),
        }
    }

    /// Exchange for a new token for `repo`, replacing the one held.
    pub(crate) async fn renew(
        &mut self,
        client: &reqwest::Client,
        repo: &Repository<'_>,
        dockerhub_auth: Option<&DockerHubAuth>,
    ) -> &Result<String, String> {
        let renewed = oci_token(client, repo, dockerhub_auth).await;
        self.by_repository.insert(RepoKey::from(repo), renewed);
        self.get(client, repo, dockerhub_auth).await
    }

    /// Send the request `build` makes with the token for `repo`. A 401 means
    /// the token expired, so a new one is exchanged and the request sent once
    /// more.
    pub(crate) async fn send(
        &mut self,
        client: &reqwest::Client,
        repo: &Repository<'_>,
        dockerhub_auth: Option<&DockerHubAuth>,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, String> {
        let token = self.get(client, repo, dockerhub_auth).await.clone()?;
        let response = build(&token).send().await.map_err(|e| e.to_string())?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let token = self.renew(client, repo, dockerhub_auth).await.clone()?;
        build(&token).send().await.map_err(|e| e.to_string())
    }

    /// The token last exchanged for `repo`, if that succeeded.
    pub(crate) fn cached(&self, repo: &RepoKey) -> Option<&str> {
        self.by_repository.get(repo)?.as_deref().ok()
    }
}

/// OCI token exchange for a single repository scope.
///
/// Follows the standard flow: challenge /v2/, parse `WWW-Authenticate`,
/// request a bearer token from the auth realm. For Docker Hub, includes
/// basic auth credentials if given. The error says which step failed.
pub(crate) async fn oci_token(
    client: &reqwest::Client,
    repo: &Repository<'_>,
    dockerhub_auth: Option<&DockerHubAuth>,
) -> Result<String, String> {
    let registry = repo.host.api_host();

    // Step 1: challenge the registry to get the auth parameters.
    let challenge_url = format!("https://{registry}/v2/");
    let challenge = send_retrying(&format!("{registry} challenge"), || {
        client.get(&challenge_url)
    })
    .await
    .map_err(|e| format!("challenge to {registry} {e}"))?;

    let www_auth = challenge
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            format!(
                "{registry} answered /v2/ with {} and no WWW-Authenticate challenge",
                challenge.status()
            )
        })?;

    // Parse: Bearer realm="https://...",service="...",scope="..."
    let realm = parse_auth_param(www_auth, "realm")
        .ok_or_else(|| format!("{registry} challenge names no realm"))?;
    let service = parse_auth_param(www_auth, "service").unwrap_or_default();

    // Step 2: exchange for a bearer token.
    let scope = format!("repository:{}:pull", repo.path);
    let token_resp = send_retrying(&format!("{repo} token"), || {
        let request = client
            .get(&realm)
            .query(&[("service", &service), ("scope", &scope)]);
        // Docker Hub: include basic auth for authenticated pull rates.
        match (repo.host, dockerhub_auth) {
            (Host::DockerHub, Some(auth)) => request.basic_auth(&auth.username, Some(&auth.token)),
            _ => request,
        }
    })
    .await
    .map_err(|e| format!("token request for {repo} {e}"))?;
    if !token_resp.status().is_success() {
        return Err(format!(
            "token request for {repo} returned {}",
            token_resp.status()
        ));
    }

    let text = token_resp
        .text()
        .await
        .map_err(|e| format!("token response for {repo} unreadable: {e}"))?;
    #[expect(
        clippy::disallowed_methods,
        reason = "the error names the registry and repository, never the response body"
    )]
    let body: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| format!("token response for {repo} is not JSON"))?;
    body.get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("token response for {repo} holds no token"))
}

/// Extract a parameter value from a `WWW-Authenticate: Bearer ...` header.
fn parse_auth_param(header: &str, key: &str) -> Option<String> {
    let pattern = format!("{key}=\"");
    let start = header.find(&pattern)? + pattern.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_source_maps_docker_hub_to_its_api_host() {
        let cases = [
            (
                "docker.io/library/alpine",
                Some((Host::DockerHub, "library/alpine")),
            ),
            (
                "quay.io/jupyter/base-notebook",
                Some((Host::Other("quay.io"), "jupyter/base-notebook")),
            ),
            // Every spelling of Docker Hub is one host; a lookalike is not.
            (
                "registry-1.docker.io/library/alpine",
                Some((Host::DockerHub, "library/alpine")),
            ),
            (
                "index.docker.io/library/alpine",
                Some((Host::DockerHub, "library/alpine")),
            ),
            (
                "notdocker.io/library/alpine",
                Some((Host::Other("notdocker.io"), "library/alpine")),
            ),
            ("alpine", None),
        ];
        for (source, want) in cases {
            let got = split_source(source).map(|r| (r.host, r.path));
            assert_eq!(got, want, "{source}");
        }
        assert_eq!(Host::DockerHub.api_host(), "registry-1.docker.io");
        assert_eq!(Host::Other("quay.io").api_host(), "quay.io");
    }

    #[test]
    fn backoff_follows_retry_after_doubles_without_it_and_is_capped() {
        let cases: [(u32, Option<&str>, u64); 5] = [
            (0, Some("7"), 7),
            (0, None, 2),
            (2, None, 8),
            (0, Some("3600"), 60),
            (1, Some("soon"), 4),
        ];
        for (attempt, header, secs) in cases {
            assert_eq!(
                backoff(attempt, header),
                Duration::from_secs(secs),
                "attempt {attempt}, Retry-After {header:?}"
            );
        }
    }

    #[test]
    fn only_throttling_and_server_failures_are_retried() {
        use reqwest::StatusCode;
        // A refusal retried would hammer a registry that said no; a throttle
        // or outage not retried would fail a run that a short wait saves.
        let cases = [
            (429, true),
            (500, true),
            (503, true),
            (200, false),
            (401, false),
            (404, false),
        ];
        for (code, retried) in cases {
            let status = StatusCode::from_u16(code).unwrap();
            assert_eq!(transient(status), retried, "{code}");
        }
    }

    #[test]
    fn parse_auth_param_reads_quoted_values_and_refuses_absent_keys() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
        assert_eq!(
            parse_auth_param(header, "realm").as_deref(),
            Some("https://auth.docker.io/token")
        );
        assert_eq!(
            parse_auth_param(header, "service").as_deref(),
            Some("registry.docker.io")
        );
        assert_eq!(parse_auth_param(header, "scope"), None);
    }
}
