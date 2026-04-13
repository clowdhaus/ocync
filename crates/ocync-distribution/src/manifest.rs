//! Manifest operations — pull, push, head checks, and referrers queries.

use http::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};

use crate::aimd::RegistryAction;
use crate::auth::Scope;
use crate::client::{RegistryClient, build_url};
use crate::digest::Digest;
use crate::error::Error;
use crate::sha256::Sha256;
use crate::spec::{ImageIndex, ManifestKind, MediaType};

/// OCI-specific header returned by registries to provide the content-addressable
/// digest of a manifest. Not part of the HTTP standard, so no constant exists in
/// the `http` crate.
const DOCKER_CONTENT_DIGEST: &str = "docker-content-digest";

/// Result of a manifest HEAD request.
#[derive(Debug, Clone)]
pub struct ManifestHead {
    /// The digest of the manifest.
    pub digest: Digest,
    /// The media type reported by the registry.
    pub media_type: MediaType,
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
    pub media_type: MediaType,
    /// The digest computed from the raw bytes.
    pub digest: Digest,
}

/// Build the Accept header value for manifest requests.
fn manifest_accept_header() -> String {
    [
        MediaType::OciManifest.as_str(),
        MediaType::OciIndex.as_str(),
        MediaType::DockerManifestV2.as_str(),
        MediaType::DockerManifestList.as_str(),
    ]
    .join(", ")
}

/// Build the path segment for a manifest: `/manifests/{reference}`.
fn manifest_path(reference: &str) -> String {
    format!("manifests/{reference}")
}

/// Build the path segment for the referrers API: `/referrers/{digest}`.
fn referrers_path(digest: &Digest) -> String {
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
        match self
            .head(repository, &path, RegistryAction::ManifestHead)
            .await
        {
            Ok(resp) => {
                let headers = resp.headers();

                let digest_str = headers
                    .get(DOCKER_CONTENT_DIGEST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                let digest: Digest = digest_str.parse().map_err(|_| {
                    Error::Other(format!(
                        "invalid digest in Docker-Content-Digest header: {digest_str}"
                    ))
                })?;

                let media_type: MediaType = headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .into();

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
            Err(e) if e.is_not_found() => Ok(None),
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
        let resp = self
            .get(
                repository,
                &path,
                Some(&accept),
                RegistryAction::ManifestRead,
            )
            .await?;

        let media_type: MediaType = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(MediaType::OciManifest.as_str())
            .into();

        let raw_bytes = resp.bytes().await?.to_vec();

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
        media_type: &MediaType,
        raw_bytes: &[u8],
    ) -> Result<Digest, Error> {
        let hash = Sha256::digest(raw_bytes);
        let digest = Digest::from_sha256(hash);

        let url = build_url(&self.base_url, repository, &manifest_path(reference))?;
        let scopes = [Scope::pull_push(repository)];

        let content_type = HeaderValue::from_str(media_type.as_str())
            .map_err(|e| Error::Other(format!("invalid media type header value: {e}")))?;

        let resp = self
            .send_with_aimd(
                RegistryAction::ManifestWrite,
                &scopes,
                "manifest push",
                |headers| {
                    self.http
                        .put(url.clone())
                        .headers(headers)
                        .header(CONTENT_TYPE, content_type.clone())
                        .header(CONTENT_LENGTH, raw_bytes.len().to_string())
                        .body(raw_bytes.to_vec())
                },
            )
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(digest)
    }

    /// Query the referrers API for a given digest.
    ///
    /// Returns `None` if the registry does not support the referrers API (404).
    /// Optionally filters by `artifact_type` (an OCI artifact MIME type string,
    /// e.g. `application/vnd.dev.cosign.simplesigning.v1+json`).
    pub async fn referrers(
        &self,
        repository: &str,
        digest: &Digest,
        artifact_type: Option<&str>,
    ) -> Result<Option<ImageIndex>, Error> {
        let path = referrers_path(digest);
        let accept = MediaType::OciIndex.as_str();

        // Build URL with optional artifactType filter.
        let mut url = build_url(&self.base_url, repository, &path)?;
        if let Some(at) = artifact_type {
            url.query_pairs_mut().append_pair("artifactType", at);
        }

        let scopes = [Scope::pull(repository)];

        let resp = self
            .send_with_aimd(
                RegistryAction::ManifestRead,
                &scopes,
                "referrers",
                |mut headers| {
                    if let Ok(val) = HeaderValue::from_str(accept) {
                        headers.insert(reqwest::header::ACCEPT, val);
                    }
                    self.http.get(url.clone()).headers(headers)
                },
            )
            .await?;

        classify_referrers_response(resp).await
    }
}

/// Classify a referrers response.
async fn classify_referrers_response(resp: reqwest::Response) -> Result<Option<ImageIndex>, Error> {
    let status = resp.status();

    match status {
        StatusCode::OK => {
            let index: ImageIndex = resp.json().await?;
            Ok(Some(index))
        }
        StatusCode::NOT_FOUND => Ok(None),
        _ => {
            let message = resp.text().await.unwrap_or_default();
            Err(Error::RegistryError { status, message })
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
    fn accept_header_contains_all_manifest_types() {
        let accept = manifest_accept_header();
        assert!(accept.contains(MediaType::OciManifest.as_str()));
        assert!(accept.contains(MediaType::OciIndex.as_str()));
        assert!(accept.contains(MediaType::DockerManifestV2.as_str()));
        assert!(accept.contains(MediaType::DockerManifestList.as_str()));
    }
}
