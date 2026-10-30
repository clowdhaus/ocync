//! Registry access shared by the CDN pre-warm and the corpus lock: hostname
//! normalization, the manifest `Accept` list and the OCI bearer-token flow.

use crate::bench::corpus::DockerHubAuth;

/// The manifest media types a source image can be stored as: OCI image,
/// Docker v2 image, OCI index and Docker manifest list.
pub(crate) const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json,\
     application/vnd.docker.distribution.manifest.v2+json,\
     application/vnd.oci.image.index.v1+json,\
     application/vnd.docker.distribution.manifest.list.v2+json";

/// Normalize Docker Hub's registry hostname to its API endpoint.
fn normalize_registry(raw: &str) -> &str {
    if raw == "docker.io" {
        "registry-1.docker.io"
    } else {
        raw
    }
}

/// Split a corpus source into the registry's API host and the repository.
///
/// `docker.io/library/alpine` becomes `("registry-1.docker.io", "library/alpine")`.
/// Returns `None` for a source with no `/`.
pub(crate) fn split_source(source: &str) -> Option<(&str, &str)> {
    let (registry, repo) = source.split_once('/')?;
    Some((normalize_registry(registry), repo))
}

/// OCI token exchange for a single repository scope.
///
/// Follows the standard flow: challenge /v2/, parse `WWW-Authenticate`,
/// request a bearer token from the auth realm. For Docker Hub, includes
/// basic auth credentials if given. The error says which step failed.
pub(crate) async fn oci_token(
    client: &reqwest::Client,
    registry: &str,
    repo: &str,
    dockerhub_auth: Option<&DockerHubAuth>,
) -> Result<String, String> {
    // Step 1: challenge the registry to get the auth parameters.
    let challenge = client
        .get(format!("https://{registry}/v2/"))
        .send()
        .await
        .map_err(|e| format!("challenge to {registry} failed: {e}"))?;

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
    let scope = format!("repository:{repo}:pull");
    let mut token_req = client
        .get(&realm)
        .query(&[("service", &service), ("scope", &scope)]);

    // Docker Hub: include basic auth for authenticated pull rates.
    if registry.contains("docker.io") {
        if let Some(auth) = dockerhub_auth {
            token_req = token_req.basic_auth(&auth.username, Some(&auth.token));
        }
    }

    let token_resp = token_req
        .send()
        .await
        .map_err(|e| format!("token request for {registry}/{repo} failed: {e}"))?;
    if !token_resp.status().is_success() {
        return Err(format!(
            "token request for {registry}/{repo} returned {}",
            token_resp.status()
        ));
    }

    let text = token_resp
        .text()
        .await
        .map_err(|e| format!("token response for {registry}/{repo} unreadable: {e}"))?;
    #[expect(
        clippy::disallowed_methods,
        reason = "the error names the registry and repository, never the response body"
    )]
    let body: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| format!("token response for {registry}/{repo} is not JSON"))?;
    body.get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("token response for {registry}/{repo} holds no token"))
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
                Some(("registry-1.docker.io", "library/alpine")),
            ),
            (
                "quay.io/jupyter/base-notebook",
                Some(("quay.io", "jupyter/base-notebook")),
            ),
            ("alpine", None),
        ];
        for (source, want) in cases {
            assert_eq!(split_source(source), want, "{source}");
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
