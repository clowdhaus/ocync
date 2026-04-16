//! Blob operations — existence checks, pull, push, mount, and upload management.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, LOCATION};
use tracing::{debug, warn};

use crate::aimd::RegistryAction;
use crate::auth::Scope;
use crate::auth::detect::{ProviderKind, detect_provider_kind};
use crate::client::{RegistryClient, build_url};
use crate::digest::Digest;
use crate::error::Error;
use crate::sha256::Sha256;
use crate::spec::RepositoryName;

/// Blobs at or below this size are uploaded monolithically (POST + PUT) to
/// save the extra round-trip cost of chunked upload negotiation.
const MONOLITHIC_THRESHOLD: u64 = 1024 * 1024;

/// Content type for raw blob data in OCI upload requests.
const OCTET_STREAM: &str = "application/octet-stream";

/// Result of a cross-repository blob mount attempt.
///
/// `NotFulfilled` and `Skipped` both mean "mount did not happen, fall
/// through to HEAD + push," but they are kept distinct because callers
/// that maintain a mount-source cache must treat them differently:
///
/// - `NotFulfilled` came from a registry response, so the cached hint
///   (which repo the blob lived in) is probably stale and should be
///   invalidated.
/// - `Skipped` happened entirely client-side; the hint was never tested
///   on the wire and is still trustworthy.
#[derive(Debug)]
pub enum MountResult {
    /// Registry fulfilled the mount (201 Created). The blob is now
    /// referenced from the target repo without a data transfer.
    Mounted,
    /// Registry returned 202 without fulfilling the mount. Any cached
    /// mount-source hint is suspect.
    NotFulfilled,
    /// Client short-circuited before issuing the POST because the target
    /// provider is known to never fulfill mount (see
    /// [`ProviderKind::fulfills_cross_repo_mount`]). No network request
    /// was made; any cached mount-source hint is still valid.
    Skipped,
}

/// Build the path segment for a blob: `/blobs/{digest}`.
fn blob_path(digest: &Digest) -> String {
    format!("blobs/{digest}")
}

/// Check an HTTP response status, returning an error with the response body
/// if the status does not match `expected`.
async fn expect_status(
    resp: reqwest::Response,
    expected: StatusCode,
) -> Result<reqwest::Response, Error> {
    let status = resp.status();
    if status == expected {
        Ok(resp)
    } else {
        let message = resp.text().await.unwrap_or_default();
        Err(Error::RegistryError { status, message })
    }
}

/// Buffer an entire byte stream into a `Vec<u8>`.
///
/// When `capacity_hint` is `Some(n)`, the buffer is pre-allocated to `n` bytes
/// to avoid reallocations. Used by fallback upload paths (GAR, GHCR, monolithic
/// threshold) that cannot stream chunks to the registry.
async fn buffer_stream(
    stream: impl Stream<Item = Result<Bytes, Error>>,
    capacity_hint: Option<u64>,
) -> Result<Vec<u8>, Error> {
    let mut body = match capacity_hint {
        Some(s) => Vec::with_capacity(s as usize),
        None => Vec::new(),
    };
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok(body)
}

impl RegistryClient {
    /// Check whether a blob exists in the given repository.
    ///
    /// Issues a HEAD request to `/v2/{repository}/blobs/{digest}`.
    /// Returns `Some(size)` if the blob exists, `None` if not found.
    pub async fn blob_exists(
        &self,
        repository: &RepositoryName,
        digest: &Digest,
    ) -> Result<Option<u64>, Error> {
        let path = blob_path(digest);
        match self
            .head(repository, &path, None, RegistryAction::BlobHead)
            .await
        {
            Ok(resp) => {
                let size = resp
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(Some(size))
            }
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Pull a blob as a streaming response.
    ///
    /// Issues a GET request to `/v2/{repository}/blobs/{digest}` and returns
    /// a byte stream for the response body.
    pub async fn blob_pull(
        &self,
        repository: &RepositoryName,
        digest: &Digest,
    ) -> Result<impl Stream<Item = Result<Bytes, reqwest::Error>> + 'static, Error> {
        let path = blob_path(digest);
        let resp = self
            .get(repository, &path, None, RegistryAction::BlobRead)
            .await?;
        Ok(resp.bytes_stream())
    }

    /// Attempt a cross-repository blob mount.
    ///
    /// Issues a POST to `/v2/{repository}/blobs/uploads/?mount={digest}&from={from_repo}`.
    /// Returns [`MountResult::Mounted`] on 201, [`MountResult::NotFulfilled`] on 202,
    /// or [`MountResult::Skipped`] without a network request when the target provider
    /// is known to never fulfill mount (see [`ProviderKind::fulfills_cross_repo_mount`]).
    pub async fn blob_mount(
        &self,
        repository: &RepositoryName,
        digest: &Digest,
        from_repo: &RepositoryName,
    ) -> Result<MountResult, Error> {
        let provider_kind = self.base_url.host_str().and_then(detect_provider_kind);
        if provider_kind.is_some_and(|k| !k.fulfills_cross_repo_mount()) {
            debug!(
                target = %repository,
                %digest,
                ?provider_kind,
                "skipping cross-repo mount POST: provider does not fulfill mount"
            );
            return Ok(MountResult::Skipped);
        }

        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let scopes = [
            Scope::pull_push(repository.as_str()),
            Scope::pull(from_repo.as_str()),
        ];
        let digest_str = digest.to_string();
        let from = from_repo.to_string();

        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadInit,
                &scopes,
                "blob mount",
                |headers| {
                    self.http
                        .post(url.clone())
                        .headers(headers)
                        .query(&[("mount", &digest_str), ("from", &from)])
                },
            )
            .await?;

        classify_mount_response(resp).await
    }

    /// Push a blob to the given repository using a monolithic upload.
    ///
    /// 1. POST `/v2/{repository}/blobs/uploads/` to initiate the upload.
    /// 2. PUT the entire data with the computed digest.
    ///
    /// Returns the digest of the uploaded blob.
    pub async fn blob_push(
        &self,
        repository: &RepositoryName,
        data: &[u8],
    ) -> Result<Digest, Error> {
        let hash = Sha256::digest(data);
        let digest = Digest::from_sha256(hash);

        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let scopes = [Scope::pull_push(repository.as_str())];

        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadInit,
                &scopes,
                "blob push initiate",
                |headers| self.http.post(url.clone()).headers(headers),
            )
            .await?;
        let resp = expect_status(resp, StatusCode::ACCEPTED).await?;
        let put_url = extract_location(&resp, &self.base_url)?;

        let digest_str = digest.to_string();
        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadComplete,
                &scopes,
                "blob push upload",
                |headers| {
                    self.http
                        .put(&put_url)
                        .headers(headers)
                        .query(&[("digest", &digest_str)])
                        .header(CONTENT_LENGTH, data.len().to_string())
                        .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                        .body(data.to_vec())
                },
            )
            .await?;
        expect_status(resp, StatusCode::CREATED).await?;

        Ok(digest)
    }

    /// Push a blob to the given repository using a chunked streaming upload.
    ///
    /// 1. POST `/v2/{repository}/blobs/uploads/` to initiate the upload.
    /// 2. Accumulate stream chunks into `chunk_size` buffers, issuing PATCH
    ///    requests with `Content-Range` headers for each buffer.
    /// 3. PUT to finalize with `?digest=` query param — the registry verifies
    ///    the uploaded content matches the digest.
    ///
    /// When `known_size` is `Some(n)` and `n <= 1 MiB`, the stream is buffered
    /// and sent as a monolithic upload, saving one HTTP round-trip.
    ///
    /// **GHCR fallback**: GitHub Container Registry's multi-PATCH chunked
    /// upload is broken — each PATCH overwrites all previous chunks. Blobs
    /// pushed to `ghcr.io` use a single PATCH with no `Content-Range` header.
    ///
    /// **GAR fallback**: Google Artifact Registry does not support chunked
    /// uploads, so hosts ending in `-docker.pkg.dev` buffer the entire stream
    /// and delegate to [`blob_push`](Self::blob_push).
    pub async fn blob_push_stream<E>(
        &self,
        repository: &RepositoryName,
        expected_digest: &Digest,
        known_size: Option<u64>,
        stream: impl Stream<Item = Result<Bytes, E>>,
    ) -> Result<Digest, Error>
    where
        E: Into<Error>,
    {
        // Map stream errors to our Error type at the boundary so all
        // internal code works uniformly with `Result<Bytes, Error>`.
        let stream = stream.map(|r| r.map_err(Into::into));

        // Monolithic threshold: small blobs skip chunked upload entirely.
        if known_size.is_some_and(|s| s <= MONOLITHIC_THRESHOLD) {
            debug!(
                repository = repository.as_str(),
                %expected_digest,
                size = known_size.unwrap(),
                "blob below monolithic threshold, using POST+PUT upload"
            );
            let body = buffer_stream(stream, known_size).await?;
            let actual_digest = self.blob_push(repository, &body).await?;
            if &actual_digest != expected_digest {
                return Err(Error::Other(format!(
                    "monolithic upload digest mismatch: expected {expected_digest}, got {actual_digest}"
                )));
            }
            return Ok(actual_digest);
        }

        // Registry-specific upload fallbacks, detected via the canonical
        // provider detection (handles case-insensitivity, ports, trailing dots).
        let provider = self.base_url.host_str().and_then(detect_provider_kind);

        // GHCR fallback: single PATCH (no Content-Range) to avoid the
        // multi-PATCH corruption bug.
        if provider == Some(ProviderKind::Ghcr) {
            return self
                .blob_push_stream_ghcr(repository, expected_digest, known_size, stream)
                .await;
        }

        // GAR fallback: buffer entire stream and use monolithic push.
        // GAR (Artifact Registry) does not support chunked uploads; legacy
        // gcr.io hosts support it fine, so only Gar triggers this path.
        if provider == Some(ProviderKind::Gar) {
            return self
                .blob_push_stream_gar_fallback(repository, expected_digest, stream)
                .await;
        }

        debug!(
            repository = repository.as_str(),
            %expected_digest,
            chunk_size = self.chunk_size,
            "starting chunked blob upload"
        );

        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let scopes = [Scope::pull_push(repository.as_str())];

        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadInit,
                &scopes,
                "blob push stream initiate",
                |headers| self.http.post(url.clone()).headers(headers),
            )
            .await?;
        let resp = expect_status(resp, StatusCode::ACCEPTED).await?;
        let upload_url = extract_location(&resp, &self.base_url)?;

        // Stream chunks, issuing PATCH for each chunk_size buffer.
        let mut buffer = Vec::with_capacity(self.chunk_size);
        let mut current_url = upload_url;
        let mut offset: u64 = 0;

        futures_util::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.extend_from_slice(&chunk);

            if buffer.len() >= self.chunk_size {
                let range_start = offset;
                let range_end = offset + buffer.len() as u64 - 1;
                offset += buffer.len() as u64;

                let patch_chunk =
                    std::mem::replace(&mut buffer, Vec::with_capacity(self.chunk_size));
                current_url = self
                    .send_patch_chunk(&scopes, &current_url, range_start, range_end, patch_chunk)
                    .await?;
            }
        }

        // Flush any remaining data in the buffer.
        if !buffer.is_empty() {
            let range_start = offset;
            let range_end = offset + buffer.len() as u64 - 1;
            current_url = self
                .send_patch_chunk(&scopes, &current_url, range_start, range_end, buffer)
                .await?;
        }

        // PUT to finalize — the registry verifies the digest.
        let digest_str = expected_digest.to_string();
        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadComplete,
                &scopes,
                "blob push stream finalize",
                |headers| {
                    self.http
                        .put(&current_url)
                        .headers(headers)
                        .query(&[("digest", &digest_str)])
                        .header(CONTENT_LENGTH, "0")
                        .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                },
            )
            .await?;
        expect_status(resp, StatusCode::CREATED).await?;

        Ok(expected_digest.clone())
    }

    /// Send a PATCH chunk for a chunked upload and return the next upload URL.
    async fn send_patch_chunk(
        &self,
        scopes: &[Scope],
        upload_url: &str,
        range_start: u64,
        range_end: u64,
        chunk: Vec<u8>,
    ) -> Result<String, Error> {
        let content_range = HeaderValue::from_str(&format!("{range_start}-{range_end}"))
            .map_err(|e| Error::Other(format!("failed to build Content-Range header: {e}")))?;
        let chunk_len = chunk.len();
        let url = upload_url.to_owned();
        // Convert to Bytes so .clone() in the retry closure is a cheap
        // reference-count increment instead of a full memcpy per chunk.
        let chunk = Bytes::from(chunk);

        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadChunk,
                scopes,
                "blob push stream patch",
                |headers| {
                    self.http
                        .patch(&url)
                        .headers(headers)
                        .header(CONTENT_RANGE, content_range.clone())
                        .header(CONTENT_LENGTH, chunk_len.to_string())
                        .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                        .body(chunk.clone())
                },
            )
            .await?;
        // The OCI spec requires 202 Accepted for PATCH chunks, but ECR
        // returns 201 Created. Accept both to handle this deviation.
        let status = resp.status();
        if status != StatusCode::ACCEPTED && status != StatusCode::CREATED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }
        extract_location(&resp, &self.base_url)
    }

    /// GAR fallback: buffer the entire stream and delegate to monolithic push.
    ///
    /// Google Artifact Registry does not support chunked uploads, so the
    /// entire stream is buffered in memory and sent as a monolithic upload.
    /// The digest returned by the monolithic push is verified against the
    /// caller's expected digest to catch data corruption.
    async fn blob_push_stream_gar_fallback(
        &self,
        repository: &RepositoryName,
        expected_digest: &Digest,
        stream: impl Stream<Item = Result<Bytes, Error>>,
    ) -> Result<Digest, Error> {
        warn!(
            repository = repository.as_str(),
            host = self.base_url.host_str().unwrap_or("unknown"),
            "GAR does not support chunked uploads; buffering entire blob in memory"
        );
        let body = buffer_stream(stream, None).await?;
        let actual_digest = self.blob_push(repository, &body).await?;

        if &actual_digest != expected_digest {
            return Err(Error::Other(format!(
                "GAR fallback digest mismatch: expected {expected_digest}, got {actual_digest}"
            )));
        }

        Ok(actual_digest)
    }

    /// GHCR fallback: single PATCH upload to avoid multi-PATCH corruption.
    ///
    /// GitHub Container Registry's chunked upload is broken: each PATCH
    /// request overwrites all previous chunks, so only the last chunk is
    /// stored. This fallback buffers the entire stream and sends a single
    /// PATCH with no `Content-Range` header, followed by a PUT to finalize.
    async fn blob_push_stream_ghcr(
        &self,
        repository: &RepositoryName,
        expected_digest: &Digest,
        known_size: Option<u64>,
        stream: impl Stream<Item = Result<Bytes, Error>>,
    ) -> Result<Digest, Error> {
        warn!(
            repository = repository.as_str(),
            "GHCR multi-PATCH chunked upload is broken; buffering blob for single-PATCH upload"
        );

        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let scopes = [Scope::pull_push(repository.as_str())];

        // Initiate upload.
        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadInit,
                &scopes,
                "blob push ghcr initiate",
                |headers| self.http.post(url.clone()).headers(headers),
            )
            .await?;
        let resp = expect_status(resp, StatusCode::ACCEPTED).await?;
        let upload_url = extract_location(&resp, &self.base_url)?;

        // Buffer entire stream.
        let body = Bytes::from(buffer_stream(stream, known_size).await?);
        let body_len = body.len();

        // Single PATCH — no Content-Range header.
        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadChunk,
                &scopes,
                "blob push ghcr patch",
                |headers| {
                    self.http
                        .patch(&upload_url)
                        .headers(headers)
                        .header(CONTENT_LENGTH, body_len.to_string())
                        .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                        .body(body.clone())
                },
            )
            .await?;
        let resp = expect_status(resp, StatusCode::ACCEPTED).await?;
        let finalize_url = extract_location(&resp, &self.base_url)?;

        // PUT to finalize with digest query param.
        let digest_str = expected_digest.to_string();
        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadComplete,
                &scopes,
                "blob push ghcr finalize",
                |headers| {
                    self.http
                        .put(&finalize_url)
                        .headers(headers)
                        .query(&[("digest", &digest_str)])
                        .header(CONTENT_LENGTH, "0")
                        .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                },
            )
            .await?;
        expect_status(resp, StatusCode::CREATED).await?;

        Ok(expected_digest.clone())
    }
}

/// Extract and resolve the Location header from an upload response.
fn extract_location(resp: &reqwest::Response, base_url: &url::Url) -> Result<String, Error> {
    let raw = resp
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::Other("missing Location header in upload response".into()))?;

    if raw.starts_with("http://") || raw.starts_with("https://") {
        Ok(raw.to_owned())
    } else {
        base_url
            .join(raw)
            .map(|u| u.to_string())
            .map_err(|e| Error::Other(format!("failed to resolve upload URL: {e}")))
    }
}

/// Classify a mount response into [`MountResult`].
///
/// The 202 response carries a fresh upload URL that the engine could
/// in principle chain on; in practice it falls through to a fresh
/// HEAD + push, so the URL is discarded and only the binary outcome
/// is surfaced.
async fn classify_mount_response(resp: reqwest::Response) -> Result<MountResult, Error> {
    let status = resp.status();
    match status {
        StatusCode::CREATED => Ok(MountResult::Mounted),
        StatusCode::ACCEPTED => Ok(MountResult::NotFulfilled),
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
    fn blob_path_format() {
        let digest: Digest =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap();
        assert_eq!(
            blob_path(&digest),
            "blobs/sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn extract_location_absolute() {
        let resp = http::Response::builder()
            .header("Location", "https://registry.example.com/upload/uuid-1")
            .body("")
            .unwrap();
        let reqwest_resp = reqwest::Response::from(resp);
        let base = url::Url::parse("https://registry.example.com").unwrap();
        let result = extract_location(&reqwest_resp, &base).unwrap();
        assert_eq!(result, "https://registry.example.com/upload/uuid-1");
    }

    #[test]
    fn extract_location_relative() {
        let resp = http::Response::builder()
            .header("Location", "/v2/repo/blobs/uploads/uuid-1")
            .body("")
            .unwrap();
        let reqwest_resp = reqwest::Response::from(resp);
        let base = url::Url::parse("https://registry.example.com").unwrap();
        let result = extract_location(&reqwest_resp, &base).unwrap();
        assert_eq!(
            result,
            "https://registry.example.com/v2/repo/blobs/uploads/uuid-1"
        );
    }

    #[test]
    fn extract_location_missing() {
        let resp = http::Response::builder().body("").unwrap();
        let reqwest_resp = reqwest::Response::from(resp);
        let base = url::Url::parse("https://registry.example.com").unwrap();
        let result = extract_location(&reqwest_resp, &base);
        assert!(result.is_err());
    }

    #[test]
    fn extract_location_preserves_query_params() {
        let resp = http::Response::builder()
            .header(
                "Location",
                "/v2/repo/blobs/uploads/uuid-1?_state=token123&foo=bar",
            )
            .body("")
            .unwrap();
        let reqwest_resp = reqwest::Response::from(resp);
        let base = url::Url::parse("https://registry.example.com").unwrap();
        let result = extract_location(&reqwest_resp, &base).unwrap();
        assert_eq!(
            result,
            "https://registry.example.com/v2/repo/blobs/uploads/uuid-1?_state=token123&foo=bar"
        );
    }

    /// Build a `RegistryClient` with the given hostname, routing all traffic
    /// for that hostname to a local wiremock port. Used by the tests below
    /// that need a real-looking hostname (e.g. `ghcr.io`,
    /// `<acct>.dkr.ecr.<region>.amazonaws.com`) to exercise hostname-keyed
    /// behavior in the client.
    fn build_test_client(host: &str, port: u16) -> RegistryClient {
        let base_url = url::Url::parse(&format!("http://{host}:{port}")).unwrap();
        RegistryClient {
            chunk_size: 4, // small chunk keeps streaming tests fast
            ..crate::client::RegistryClientBuilder::new(base_url)
                .resolve(host, std::net::SocketAddr::from(([127, 0, 0, 1], port)))
                .build()
                .unwrap()
        }
    }

    fn test_digest(data: &[u8]) -> Digest {
        Digest::from_sha256(Sha256::digest(data))
    }

    fn data_stream(
        data: &[u8],
        chunk_size: usize,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = data
            .chunks(chunk_size)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        futures_util::stream::iter(chunks)
    }

    #[tokio::test]
    async fn blob_push_stream_gar_uses_monolithic_upload() {
        let server = wiremock::MockServer::start().await;
        let data = b"gar blob content";
        let digest = test_digest(data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        // POST: initiate monolithic upload.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v2/my-project/my-repo/blobs/uploads/",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/my-project/my-repo/blobs/uploads/gar-uuid"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // PUT: monolithic upload with full body.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::query_param(
                "digest",
                digest.to_string(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        // PATCH: must NOT be called for GAR.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = build_test_client("us-docker.pkg.dev", port);

        let repo = RepositoryName::new("my-project/my-repo");
        let result = client
            .blob_push_stream(&repo, &digest, None, data_stream(data, 4))
            .await
            .unwrap();

        assert_eq!(result, digest);
    }

    /// GHCR: single PATCH with Content-Length, no Content-Range.
    #[tokio::test]
    async fn blob_push_stream_ghcr_uses_single_patch() {
        let server = wiremock::MockServer::start().await;
        let data = b"ghcr blob content";
        let digest = test_digest(data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        // POST: initiate upload.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v2/my-org/my-image/blobs/uploads/",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/my-org/my-image/blobs/uploads/ghcr-uuid"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // PATCH: single PATCH with Content-Length.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path(
                "/v2/my-org/my-image/blobs/uploads/ghcr-uuid",
            ))
            .respond_with(wiremock::ResponseTemplate::new(202).append_header(
                "Location",
                "/v2/my-org/my-image/blobs/uploads/ghcr-uuid?after-patch",
            ))
            .expect(1)
            .mount(&server)
            .await;

        // PUT: finalize with digest query param.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::query_param(
                "digest",
                digest.to_string(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_test_client("ghcr.io", port);

        // Pass None so the monolithic threshold does not intercept the call;
        // we are verifying the GHCR-specific single-PATCH path directly.
        let repo = RepositoryName::new("my-org/my-image");
        let result = client
            .blob_push_stream(&repo, &digest, None, data_stream(data, 4))
            .await
            .unwrap();

        assert_eq!(result, digest);
    }

    /// GHCR: exactly one PATCH regardless of blob size vs `chunk_size`.
    #[tokio::test]
    async fn blob_push_stream_ghcr_single_patch_large_blob() {
        let server = wiremock::MockServer::start().await;
        // 16 bytes >> chunk_size=4; only one PATCH must be issued.
        let data = b"abcdefghijklmnop";
        let digest = test_digest(data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/repo/blobs/uploads/"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/repo/blobs/uploads/ghcr-id"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // One PATCH with the full 16-byte body.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/repo/blobs/uploads/ghcr-id?done"),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::query_param(
                "digest",
                digest.to_string(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_test_client("ghcr.io", port);

        // No known_size — GHCR path still taken based on hostname alone.
        let repo = RepositoryName::new("repo");
        let result = client
            .blob_push_stream(&repo, &digest, None, data_stream(data, 4))
            .await
            .unwrap();

        assert_eq!(result, digest);
    }

    /// Covers the hostname-keyed mount routing and the three `MountResult`
    /// outcomes:
    ///
    /// - ECR: client short-circuits, zero POSTs issued, `Skipped`.
    /// - GHCR, 201 response: `Mounted`.
    /// - GHCR, 202 response: `NotFulfilled` (server-side fallback).
    /// - Unknown host, 201 response: `Mounted` (optimistic default).
    ///
    /// The ECR row's `.expect(0)` is the negative assertion that pins the
    /// short-circuit — if it breaks, the test fails.
    #[tokio::test]
    async fn blob_mount_routes_per_provider() {
        /// `server` is `None` for rows where the short-circuit should fire
        /// and no POST should reach the wire.
        struct Case {
            host: &'static str,
            server: Option<u16>,
            expected: MountOutcome,
        }

        enum MountOutcome {
            Mounted,
            NotFulfilled,
            Skipped,
        }

        let cases = [
            Case {
                host: "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                server: None,
                expected: MountOutcome::Skipped,
            },
            Case {
                // Inferred from ECR private — same AWS backend, assumed
                // to share mount behavior until proven otherwise.
                host: "public.ecr.aws",
                server: None,
                expected: MountOutcome::Skipped,
            },
            Case {
                host: "ghcr.io",
                server: Some(201),
                expected: MountOutcome::Mounted,
            },
            Case {
                host: "ghcr.io",
                server: Some(202),
                expected: MountOutcome::NotFulfilled,
            },
            Case {
                host: "my-private-registry.example.com",
                server: Some(201),
                expected: MountOutcome::Mounted,
            },
        ];

        for case in cases {
            let server = wiremock::MockServer::start().await;
            let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

            let (expected_post_count, response_status) = match case.server {
                Some(s) => (1, s),
                // Short-circuit expected — any POST that escapes fails the
                // test. The response template is unreachable; 500 flags it
                // loudly if the `.expect(0)` guard ever breaks.
                None => (0, 500),
            };

            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v2/tgt/repo/blobs/uploads/"))
                .respond_with(wiremock::ResponseTemplate::new(response_status))
                .expect(expected_post_count)
                .mount(&server)
                .await;

            let client = build_test_client(case.host, port);
            let result = client
                .blob_mount(
                    &RepositoryName::new("tgt/repo"),
                    &test_digest(case.host.as_bytes()),
                    &RepositoryName::new("src/repo"),
                )
                .await
                .unwrap();

            let ok = match case.expected {
                MountOutcome::Mounted => matches!(result, MountResult::Mounted),
                MountOutcome::NotFulfilled => matches!(result, MountResult::NotFulfilled),
                MountOutcome::Skipped => matches!(result, MountResult::Skipped),
            };
            assert!(
                ok,
                "{} ({:?}): unexpected result {result:?}",
                case.host, case.server
            );
        }
    }
}
