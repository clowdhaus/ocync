use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};

use crate::auth::Scope;
use crate::client::{RegistryClient, build_url};
use crate::digest::Digest;
use crate::error::Error;
use crate::sha256::Sha256;
use crate::spec::{ImageIndex, ManifestKind, media_types};

/// Result of a manifest HEAD request.
#[derive(Debug, Clone)]
pub struct ManifestHead {
    /// The digest of the manifest.
    pub digest: Digest,
    /// The media type reported by the registry.
    pub media_type: String,
    /// The size in bytes.
    pub size: u64,
}

/// Result of a manifest pull (GET) request.
#[derive(Debug, Clone)]
pub struct ManifestPull {
    /// The parsed manifest.
    pub manifest: ManifestKind,
    /// The raw bytes as received from the registry (preserved verbatim for push).
    pub raw_bytes: Vec<u8>,
    /// The media type reported by the registry.
    pub media_type: String,
    /// The digest computed from the raw bytes.
    pub digest: Digest,
}

/// Build the Accept header value for manifest requests.
///
/// Includes all four supported manifest media types.
pub fn manifest_accept_header() -> String {
    [
        media_types::OCI_IMAGE_MANIFEST,
        media_types::OCI_IMAGE_INDEX,
        media_types::DOCKER_MANIFEST_V2,
        media_types::DOCKER_MANIFEST_LIST,
    ]
    .join(", ")
}

/// Build the path segment for a manifest: `/manifests/{reference}`.
pub fn manifest_path(reference: &str) -> String {
    format!("manifests/{reference}")
}

/// Build the path segment for the referrers API: `/referrers/{digest}`.
pub fn referrers_path(digest: &Digest) -> String {
    format!("referrers/{digest}")
}

impl RegistryClient {
    /// Check whether a manifest exists and retrieve its metadata.
    ///
    /// Returns `None` if the manifest is not found (404).
    pub async fn manifest_head(
        &self,
        repository: &str,
        reference: &str,
    ) -> Result<Option<ManifestHead>, Error> {
        let path = manifest_path(reference);
        match self.head(repository, &path).await {
            Ok(resp) => {
                let headers = resp.headers();

                let digest_str = headers
                    .get("docker-content-digest")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                let digest: Digest = digest_str.parse().map_err(|_| {
                    Error::Other(format!(
                        "invalid digest in Docker-Content-Digest header: {digest_str}"
                    ))
                })?;

                let media_type = headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();

                let size = headers
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);

                Ok(Some(ManifestHead {
                    digest,
                    media_type,
                    size,
                }))
            }
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Pull a manifest from the registry.
    ///
    /// Issues a GET with all supported media types in the Accept header, parses
    /// the manifest, and computes the digest from the raw bytes.
    pub async fn manifest_pull(
        &self,
        repository: &str,
        reference: &str,
    ) -> Result<ManifestPull, Error> {
        let path = manifest_path(reference);
        let accept = manifest_accept_header();
        let resp = self.get(repository, &path, Some(&accept)).await?;

        let media_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(media_types::OCI_IMAGE_MANIFEST)
            .to_owned();

        let raw_bytes = resp.bytes().await?.to_vec();

        // Compute digest from the raw bytes.
        let hash = Sha256::digest(&raw_bytes);
        let digest = Digest::from_sha256(hash);

        let manifest = ManifestKind::from_json(&media_type, &raw_bytes)?;

        Ok(ManifestPull {
            manifest,
            raw_bytes,
            media_type,
            digest,
        })
    }

    /// Push a manifest to the registry.
    ///
    /// Issues a PUT to `/v2/{repository}/manifests/{reference}` with the raw bytes
    /// exactly as provided. Returns the digest computed from those bytes.
    pub async fn manifest_push(
        &self,
        repository: &str,
        reference: &str,
        media_type: &str,
        raw_bytes: Vec<u8>,
    ) -> Result<Digest, Error> {
        let hash = Sha256::digest(&raw_bytes);
        let digest = Digest::from_sha256(hash);
        let data_len = raw_bytes.len();

        let url = build_url(&self.base_url, repository, &manifest_path(reference))?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let scopes = [Scope::pull_push(repository)];
        let headers = self.auth_headers(&scopes).await?;

        let content_type = HeaderValue::from_str(media_type).map_err(|e| {
            Error::Other(format!("invalid media type header value: {e}"))
        })?;

        let resp = self
            .http
            .put(url.clone())
            .headers(headers)
            .header(CONTENT_TYPE, content_type.clone())
            .header(CONTENT_LENGTH, data_len.to_string())
            .body(raw_bytes.clone())
            .send()
            .await?;

        let status = resp.status().as_u16();

        // 401 — refresh token and retry once.
        if status == 401 {
            tracing::debug!(url = %url, "got 401 on manifest push, refreshing auth token");
            let headers = self.auth_headers(&scopes).await?;
            let resp = self
                .http
                .put(url)
                .headers(headers)
                .header(CONTENT_TYPE, content_type)
                .header(CONTENT_LENGTH, data_len.to_string())
                .body(raw_bytes)
                .send()
                .await?;

            let status = resp.status().as_u16();
            if status != 201 && status != 200 {
                let message = resp.text().await.unwrap_or_default();
                return Err(Error::RegistryError { status, message });
            }
            return Ok(digest);
        }

        if status != 201 && status != 200 {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(digest)
    }

    /// Query the referrers API for a given digest.
    ///
    /// Returns `None` if the registry does not support the referrers API (404).
    /// Optionally filters by `artifact_type`.
    pub async fn referrers(
        &self,
        repository: &str,
        digest: &Digest,
        artifact_type: Option<&str>,
    ) -> Result<Option<ImageIndex>, Error> {
        let path = referrers_path(digest);
        let accept = media_types::OCI_IMAGE_INDEX;

        // Build URL with optional artifactType filter.
        let mut url = build_url(&self.base_url, repository, &path)?;
        if let Some(at) = artifact_type {
            url.query_pairs_mut().append_pair("artifactType", at);
        }

        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        let scopes = [Scope::pull(repository)];
        let headers = self.auth_headers(&scopes).await?;

        let mut req_headers = headers.clone();
        if let Ok(val) = HeaderValue::from_str(accept) {
            req_headers.insert(reqwest::header::ACCEPT, val);
        }

        let resp = self
            .http
            .get(url.clone())
            .headers(req_headers)
            .send()
            .await?;

        let status = resp.status().as_u16();

        // 401 — refresh and retry.
        if status == 401 {
            tracing::debug!(url = %url, "got 401 on referrers, refreshing auth token");
            let headers = self.auth_headers(&scopes).await?;
            let mut req_headers = headers;
            if let Ok(val) = HeaderValue::from_str(accept) {
                req_headers.insert(reqwest::header::ACCEPT, val);
            }

            let resp = self.http.get(url).headers(req_headers).send().await?;

            return self.classify_referrers_response(resp).await;
        }

        self.classify_referrers_response(resp).await
    }

    /// Classify a referrers response.
    async fn classify_referrers_response(
        &self,
        resp: reqwest::Response,
    ) -> Result<Option<ImageIndex>, Error> {
        let status = resp.status().as_u16();

        match status {
            200 => {
                let index: ImageIndex = resp.json().await?;
                Ok(Some(index))
            }
            404 => Ok(None),
            _ => {
                let message = resp.text().await.unwrap_or_default();
                Err(Error::RegistryError { status, message })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_path_format() {
        assert_eq!(manifest_path("latest"), "manifests/latest");
        assert_eq!(manifest_path("v1.0"), "manifests/v1.0");
    }

    #[test]
    fn referrers_path_format() {
        let digest: Digest =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap();
        assert_eq!(
            referrers_path(&digest),
            "referrers/sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn accept_header_contains_all_types() {
        let accept = manifest_accept_header();
        assert!(accept.contains(media_types::OCI_IMAGE_MANIFEST));
        assert!(accept.contains(media_types::OCI_IMAGE_INDEX));
        assert!(accept.contains(media_types::DOCKER_MANIFEST_V2));
        assert!(accept.contains(media_types::DOCKER_MANIFEST_LIST));
    }
}
