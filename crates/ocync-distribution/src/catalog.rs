//! Catalog listing with pagination for OCI registries.

use reqwest::header::HeaderValue;
use serde::Deserialize;

use crate::client::RegistryClient;
use crate::error::Error;
use crate::tags::parse_next_link;

/// Response body from the catalog API.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CatalogResponse {
    /// The list of repository names, or empty if none exist.
    #[serde(default, deserialize_with = "deserialize_null_as_empty")]
    pub(crate) repositories: Vec<String>,
}

/// Deserialize a `Vec<String>` that may be `null` or missing as an empty vec.
fn deserialize_null_as_empty<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<Vec<String>> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

impl RegistryClient {
    /// List all repositories in the registry, automatically following pagination.
    ///
    /// Issues GET requests to `/v2/_catalog` and follows `Link: rel="next"`
    /// headers until all repositories are collected.
    pub async fn catalog(&self) -> Result<Vec<String>, Error> {
        let mut all_repos = Vec::new();

        let base_url = self
            .base_url
            .join("/v2/_catalog")
            .map_err(|e| Error::Other(format!("failed to build catalog URL: {e}")))?;

        let mut url = base_url.clone();

        loop {
            let _permit = self.semaphore.acquire().await.expect("semaphore closed");

            let resp = self
                .send_with_retry(&[], "catalog", |headers| {
                    self.http.get(url.clone()).headers(headers)
                })
                .await?;

            let status = resp.status();
            if !status.is_success() {
                let message = resp.text().await.unwrap_or_default();
                return Err(Error::RegistryError { status, message });
            }

            // Check for Link header before consuming the body.
            let next_url = resp
                .headers()
                .get("link")
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .and_then(parse_next_link);

            let catalog: CatalogResponse = resp.json().await?;
            all_repos.extend(catalog.repositories);

            match next_url {
                Some(next) => {
                    if next.starts_with("http://") || next.starts_with("https://") {
                        url = next.parse().map_err(|e| {
                            Error::Other(format!("invalid catalog pagination URL: {e}"))
                        })?;
                    } else {
                        url = self.base_url.join(&next).map_err(|e| {
                            Error::Other(format!("failed to resolve catalog pagination URL: {e}"))
                        })?;
                    }
                }
                None => break,
            }
        }

        Ok(all_repos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_catalog_response() {
        let json = r#"{"repositories": ["library/nginx", "library/alpine", "myuser/myapp"]}"#;
        let resp: CatalogResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.repositories.len(), 3);
        assert_eq!(resp.repositories[0], "library/nginx");
        assert_eq!(resp.repositories[1], "library/alpine");
        assert_eq!(resp.repositories[2], "myuser/myapp");
    }

    #[test]
    fn parse_catalog_response_empty() {
        let json = r#"{"repositories": []}"#;
        let resp: CatalogResponse = serde_json::from_str(json).unwrap();
        assert!(resp.repositories.is_empty());
    }

    #[test]
    fn parse_catalog_response_null_repositories() {
        let json = r#"{"repositories": null}"#;
        let resp: CatalogResponse = serde_json::from_str(json).unwrap();
        assert!(resp.repositories.is_empty());
    }

    #[test]
    fn parse_catalog_response_missing_repositories() {
        let json = r#"{}"#;
        let resp: CatalogResponse = serde_json::from_str(json).unwrap();
        assert!(resp.repositories.is_empty());
    }
}
