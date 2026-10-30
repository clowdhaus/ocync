//! Blob operations - existence checks, pull, push, mount, and upload management.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, LOCATION};
use tracing::{debug, warn};

use crate::aimd::RegistryAction;
use crate::auth::Scope;
use crate::auth::detect::{ProviderKind, detect_provider_kind};
use crate::client::{RegistryClient, auth_headers, build_url, report_permit};
use crate::digest::Digest;
use crate::error::Error;
use crate::sha256::Sha256;
use crate::spec::RepositoryName;

/// Content type for raw blob data in OCI upload requests.
const OCTET_STREAM: &str = "application/octet-stream";

/// Per-PATCH chunk size for the ACR upload fallback, under ACR's ~20 MB
/// limit on a streaming PUT body.
const ACR_PATCH_CHUNK_SIZE: usize = 16 * 1024 * 1024;

/// Upper bound on the `Vec::with_capacity` hint used by [`buffer_stream`].
///
/// The hint is a manifest's declared blob size, which an attacker
/// controls: uncapped, one malicious manifest could allocate gigabytes per
/// concurrent fallback upload. Larger streams grow the `Vec` as they arrive.
const MAX_BUFFER_PREALLOC: usize = 16 * 1024 * 1024;

/// Result of a cross-repository blob mount attempt.
#[derive(Debug)]
pub enum MountResult {
    /// Registry fulfilled the mount (201 Created) without a data transfer.
    Mounted,
    /// The registry answered 202 without mounting. Callers fall through to
    /// push.
    NotMounted,
}

/// Build the path segment for a blob: `/blobs/{digest}`.
fn blob_path(digest: &Digest) -> String {
    format!("blobs/{digest}")
}

/// Check an HTTP response status, returning an error naming
/// `registry/repository` with the response body if it is not `expected`.
async fn expect_status(
    resp: reqwest::Response,
    expected: StatusCode,
    base_url: &url::Url,
    repository: &str,
) -> Result<reqwest::Response, Error> {
    let status = resp.status();
    if status == expected {
        Ok(resp)
    } else {
        let registry = base_url.host_str().unwrap_or("unknown");
        let body = crate::json::error_body(resp).await;
        let message = if body.is_empty() {
            format!("{registry}/{repository}")
        } else {
            format!("{registry}/{repository}: {body}")
        };
        Err(Error::RegistryError { status, message })
    }
}

/// Buffer an entire byte stream, pre-allocating `capacity_hint` up to
/// [`MAX_BUFFER_PREALLOC`].
async fn buffer_stream(
    stream: impl Stream<Item = Result<Bytes, Error>>,
    capacity_hint: Option<u64>,
) -> Result<Vec<u8>, Error> {
    let cap = capacity_hint
        .map(|s| (s as usize).min(MAX_BUFFER_PREALLOC))
        .unwrap_or(0);
    let mut body = Vec::with_capacity(cap);
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok(body)
}

impl RegistryClient {
    /// Check whether a blob exists: `Some(size)` if it does, `None` if not
    /// found.
    ///
    /// `size` comes from `Content-Length` and is `0`, meaning unknown, when
    /// the header is missing or unparseable.
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

    /// Pull a blob as a streaming response. The GET and any redirect it
    /// follows go over HTTP/1.1, so a body left unread holds back only its
    /// own connection. A download permit is held by the returned stream
    /// until it is dropped, bounding open download connections; it is never
    /// an upload permit, which a copy's upload waits for while its download
    /// is open.
    pub async fn blob_pull(
        &self,
        repository: &RepositoryName,
        digest: &Digest,
    ) -> Result<impl Stream<Item = Result<Bytes, reqwest::Error>> + 'static, Error> {
        let permit = self.acquire_blob_download_permit().await;
        let path = blob_path(digest);
        let resp = self
            .get(repository, &path, None, RegistryAction::BlobRead)
            .await?;
        Ok(resp.bytes_stream().map(move |chunk| {
            let _held = &permit;
            chunk
        }))
    }

    /// Attempt a cross-repository blob mount from `from_repo`.
    pub async fn blob_mount(
        &self,
        repository: &RepositoryName,
        digest: &Digest,
        from_repo: &RepositoryName,
    ) -> Result<MountResult, Error> {
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

        classify_mount_response(resp, &self.base_url, repository).await
    }

    /// Push a blob with a monolithic upload (POST, then one PUT), returning
    /// its digest.
    pub async fn blob_push(
        &self,
        repository: &RepositoryName,
        data: &[u8],
    ) -> Result<Digest, Error> {
        let hash = Sha256::digest(data);
        let digest = Digest::from_sha256(hash);

        let scopes = [Scope::pull_push(repository.as_str())];
        let put_url = self
            .initiate_blob_upload(repository, &scopes, "blob push initiate")
            .await?;

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
        expect_status(resp, StatusCode::CREATED, &self.base_url, repository).await?;

        Ok(digest)
    }

    /// Push a blob with a streaming upload: a POST, then a PUT whose body is
    /// the stream, unbuffered, with the digest for the registry to verify.
    /// GHCR and ACR take buffered fallbacks instead.
    pub async fn blob_push_stream<E>(
        &self,
        repository: &RepositoryName,
        expected_digest: &Digest,
        known_size: Option<u64>,
        stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
    ) -> Result<Digest, Error>
    where
        E: Into<Error> + Send,
    {
        let stream = stream.map(|r| r.map_err(Into::into));

        // Exhaustive, so a new `ProviderKind` fails to compile until its
        // upload path is chosen here.
        let provider = self.base_url.host_str().and_then(detect_provider_kind);
        match provider {
            Some(ProviderKind::Ghcr) => {
                return self
                    .blob_push_stream_ghcr(repository, expected_digest, known_size, stream)
                    .await;
            }
            Some(ProviderKind::Acr) => {
                return self
                    .blob_push_stream_acr(repository, expected_digest, known_size, stream)
                    .await;
            }
            // Artifact Registry answers a second PATCH in one session with
            // 405 and a monolithic POST with 502, but takes the streaming
            // PUT.
            Some(
                ProviderKind::Ecr
                | ProviderKind::EcrPublic
                | ProviderKind::Gar
                | ProviderKind::Gcr
                | ProviderKind::DockerHub
                | ProviderKind::Chainguard,
            )
            | None => {}
        }

        debug!(
            repository = repository.as_str(),
            %expected_digest,
            "starting streaming blob upload"
        );

        let scopes = [Scope::pull_push(repository.as_str())];
        let upload_url = self
            .initiate_blob_upload(repository, &scopes, "blob push stream initiate")
            .await?;

        // Held for the long-lived PUT body only; the POST is brief.
        let _upload_permit = self.acquire_blob_upload_permit().await;

        // Not `send_with_aimd`: the stream is consumed once, so the request
        // cannot be rebuilt for a 401 retry. A 401 here drops the token and
        // returns `UploadTokenRefused` for the caller to restart the upload.
        let permit = self.aimd.acquire(RegistryAction::BlobUploadComplete).await;
        let sent = self.auth_token(&scopes).await?;
        let headers = auth_headers(sent.as_ref())?;
        let digest_str = expected_digest.to_string();
        let body = reqwest::Body::wrap_stream(stream);
        let result = self
            .http
            .put(&upload_url)
            .headers(headers)
            .query(&[("digest", &digest_str)])
            .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
            .body(body)
            .send()
            .await
            .map_err(Error::from);
        report_permit(permit, &result);
        let resp = result?;
        match expect_status(resp, StatusCode::CREATED, &self.base_url, repository).await {
            Ok(_) => Ok(expected_digest.clone()),
            Err(Error::RegistryError {
                status: StatusCode::UNAUTHORIZED,
                message,
            }) => {
                self.refuse_auth(sent).await;
                Err(Error::UploadTokenRefused { message })
            }
            Err(error) => Err(error),
        }
    }

    /// POST `/v2/{repository}/blobs/uploads/` to initiate an upload session,
    /// returning the upload URL from its `Location` header.
    async fn initiate_blob_upload(
        &self,
        repository: &RepositoryName,
        scopes: &[Scope],
        log_context: &str,
    ) -> Result<String, Error> {
        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let resp = self
            .send_with_aimd(RegistryAction::BlobUploadInit, scopes, log_context, |h| {
                self.http.post(url.clone()).headers(h)
            })
            .await?;
        let resp = expect_status(resp, StatusCode::ACCEPTED, &self.base_url, repository).await?;
        extract_location(&resp, &self.base_url)
    }

    /// GHCR fallback: single PATCH upload to avoid multi-PATCH corruption.
    ///
    /// GHCR's chunked upload keeps only the last chunk, as each PATCH
    /// overwrites the previous ones. This buffers the stream and sends one
    /// PATCH with no `Content-Range`, then a PUT to finalize.
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

        let _permits = self.acquire_buffered_upload_permits(known_size).await;
        let scopes = [Scope::pull_push(repository.as_str())];
        let upload_url = self
            .initiate_blob_upload(repository, &scopes, "blob push ghcr initiate")
            .await?;

        // Buffer entire stream and verify digest before uploading.
        let raw = buffer_stream(stream, known_size).await?;
        let actual_digest = Digest::from_sha256(Sha256::digest(&raw));
        if &actual_digest != expected_digest {
            return Err(Error::DigestMismatch {
                expected: expected_digest.clone(),
                actual: actual_digest,
            });
        }
        let body = Bytes::from(raw);
        let body_len = body.len();

        // Single PATCH - no Content-Range header.
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
        let resp = expect_status(resp, StatusCode::ACCEPTED, &self.base_url, repository).await?;
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
        expect_status(resp, StatusCode::CREATED, &self.base_url, repository).await?;

        Ok(expected_digest.clone())
    }

    /// ACR fallback: chunked PATCH under Azure Container Registry's
    /// ~20 MB streaming-PUT body limit.
    ///
    /// Buffers the stream so the digest is verified before any PATCH is
    /// sent, then sends [`ACR_PATCH_CHUNK_SIZE`] PATCHes and a finalize PUT,
    /// each to the `Location` the previous response named.
    async fn blob_push_stream_acr(
        &self,
        repository: &RepositoryName,
        expected_digest: &Digest,
        known_size: Option<u64>,
        stream: impl Stream<Item = Result<Bytes, Error>>,
    ) -> Result<Digest, Error> {
        warn!(
            repository = repository.as_str(),
            "ACR rejects streaming PUT above ~20 MB; buffering blob for chunked PATCH upload"
        );

        let _permits = self.acquire_buffered_upload_permits(known_size).await;

        let raw = buffer_stream(stream, known_size).await?;
        let actual_digest = Digest::from_sha256(Sha256::digest(&raw));
        if &actual_digest != expected_digest {
            return Err(Error::DigestMismatch {
                expected: expected_digest.clone(),
                actual: actual_digest,
            });
        }
        let total_len = raw.len() as u64;
        let body = Bytes::from(raw);

        let scopes = [Scope::pull_push(repository.as_str())];
        let mut upload_url = self
            .initiate_blob_upload(repository, &scopes, "blob push acr initiate")
            .await?;
        // An empty blob sends no PATCH: the OCI distribution spec accepts
        // the finalize PUT alone.
        let mut offset: u64 = 0;
        while offset < total_len {
            let chunk_len = (ACR_PATCH_CHUNK_SIZE as u64).min(total_len - offset);
            let chunk = body.slice((offset as usize)..((offset + chunk_len) as usize));
            let range_end = offset + chunk_len - 1;
            // OCI spec Content-Range format: `{start}-{end}` (NOT RFC 7233).
            let range_header = format!("{offset}-{range_end}");
            let chunk_len_str = chunk_len.to_string();

            let resp = self
                .send_with_aimd(
                    RegistryAction::BlobUploadChunk,
                    &scopes,
                    "blob push acr patch",
                    |headers| {
                        self.http
                            .patch(&upload_url)
                            .headers(headers)
                            .header(CONTENT_LENGTH, &chunk_len_str)
                            .header(CONTENT_RANGE, &range_header)
                            .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                            .body(chunk.clone())
                    },
                )
                .await?;
            let resp =
                expect_status(resp, StatusCode::ACCEPTED, &self.base_url, repository).await?;
            upload_url = extract_location(&resp, &self.base_url)?;
            offset += chunk_len;
        }

        let digest_str = expected_digest.to_string();
        let resp = self
            .send_with_aimd(
                RegistryAction::BlobUploadComplete,
                &scopes,
                "blob push acr finalize",
                |headers| {
                    self.http
                        .put(&upload_url)
                        .headers(headers)
                        .query(&[("digest", &digest_str)])
                        .header(CONTENT_LENGTH, "0")
                        .header(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM))
                },
            )
            .await?;
        expect_status(resp, StatusCode::CREATED, &self.base_url, repository).await?;

        Ok(expected_digest.clone())
    }
}

/// Extract and resolve the `Location` header from an upload response,
/// enforcing same-origin against `base_url`.
///
/// The upload chain carries the registry's bearer token, so a `Location`
/// on another host would leak it on the next request.
fn extract_location(resp: &reqwest::Response, base_url: &url::Url) -> Result<String, Error> {
    let raw = resp
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::UploadProtocol {
            reason: "missing Location header in upload response".into(),
        })?;

    let resolved = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_owned()
    } else {
        base_url
            .join(raw)
            .map(|u| u.to_string())
            .map_err(|e| Error::UploadProtocol {
                reason: format!("failed to resolve upload URL: {e}"),
            })?
    };

    let resolved_url = url::Url::parse(&resolved).map_err(|e| Error::UploadProtocol {
        reason: format!("upload Location is not a valid URL: {e}"),
    })?;
    if resolved_url.host_str() != base_url.host_str() {
        return Err(Error::UploadProtocol {
            reason: format!(
                "upload Location host {:?} differs from base host {:?}; refusing to forward credentials cross-host",
                resolved_url.host_str(),
                base_url.host_str(),
            ),
        });
    }

    Ok(resolved)
}

/// Classify a mount response into [`MountResult`].
///
/// A 202's upload URL is discarded: the engine falls through to a fresh
/// HEAD and push.
async fn classify_mount_response(
    resp: reqwest::Response,
    base_url: &url::Url,
    repository: &str,
) -> Result<MountResult, Error> {
    let status = resp.status();
    match status {
        StatusCode::CREATED => Ok(MountResult::Mounted),
        StatusCode::ACCEPTED => Ok(MountResult::NotMounted),
        _ => {
            let registry = base_url.host_str().unwrap_or("unknown");
            let body = crate::json::error_body(resp).await;
            let message = if body.is_empty() {
                format!("{registry}/{repository}")
            } else {
                format!("{registry}/{repository}: {body}")
            };
            Err(Error::RegistryError { status, message })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::TryStreamExt;

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

    #[tokio::test]
    async fn expect_status_error_includes_registry_and_repo() {
        let resp = http::Response::builder()
            .status(500)
            .body("internal error")
            .unwrap();
        let reqwest_resp = reqwest::Response::from(resp);
        let base = url::Url::parse("https://ghcr.io").unwrap();
        let err = expect_status(reqwest_resp, StatusCode::CREATED, &base, "myorg/myrepo")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghcr.io"), "missing registry in error: {msg}");
        assert!(msg.contains("myorg/myrepo"), "missing repo in error: {msg}");
    }

    #[tokio::test]
    async fn classify_mount_error_includes_registry_and_repo() {
        let resp = http::Response::builder()
            .status(403)
            .body("forbidden")
            .unwrap();
        let reqwest_resp = reqwest::Response::from(resp);
        let base = url::Url::parse("https://ghcr.io").unwrap();
        let err = classify_mount_response(reqwest_resp, &base, "myorg/myrepo")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghcr.io"), "missing registry in error: {msg}");
        assert!(msg.contains("myorg/myrepo"), "missing repo in error: {msg}");
    }

    /// The upload `Location` resolves against the registry, absolute or
    /// relative, keeping its query; a missing header is refused. Breaks if
    /// the query that carries upload state is dropped or a relative path
    /// is not joined to the registry.
    #[test]
    fn the_upload_location_resolves_against_the_registry() {
        let base = url::Url::parse("https://registry.example.com").unwrap();
        let cases = [
            (
                Some("https://registry.example.com/upload/uuid-1"),
                Some("https://registry.example.com/upload/uuid-1"),
            ),
            (
                Some("/v2/repo/blobs/uploads/uuid-1"),
                Some("https://registry.example.com/v2/repo/blobs/uploads/uuid-1"),
            ),
            (
                Some("/v2/repo/blobs/uploads/uuid-1?_state=token123&foo=bar"),
                Some(
                    "https://registry.example.com/v2/repo/blobs/uploads/uuid-1?_state=token123&foo=bar",
                ),
            ),
            (None, None),
        ];
        for (header, expected) in cases {
            let mut resp = http::Response::builder();
            if let Some(location) = header {
                resp = resp.header("Location", location);
            }
            let resp = reqwest::Response::from(resp.body("").unwrap());
            let result = extract_location(&resp, &base).ok();
            assert_eq!(result.as_deref(), expected, "{header:?}");
        }
    }

    /// Build a `RegistryClient` with the given hostname, routing all traffic
    /// for that hostname to a local wiremock port. Used by the tests below
    /// that need a real-looking hostname (e.g. `ghcr.io`,
    /// `<acct>.dkr.ecr.<region>.amazonaws.com`) to exercise hostname-keyed
    /// behavior in the client.
    fn build_test_client(host: &str, port: u16) -> RegistryClient {
        let base_url = url::Url::parse(&format!("http://{host}:{port}")).unwrap();
        crate::client::RegistryClientBuilder::new(base_url)
            .resolve(host, std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .build()
            .unwrap()
    }

    fn test_digest(data: &[u8]) -> Digest {
        Digest::from_sha256(Sha256::digest(data))
    }

    /// A blob download waits for the download cap while another body is
    /// open, and proceeds once that body is dropped.
    #[tokio::test(flavor = "current_thread")]
    async fn blob_pull_waits_for_the_download_cap_until_a_body_is_dropped() {
        let server = wiremock::MockServer::start().await;
        let data = b"layer".to_vec();
        let digest = test_digest(&data);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/v2/repo/{}",
                blob_path(&digest)
            )))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(data.clone()))
            .expect(2)
            .mount(&server)
            .await;
        let client = crate::client::RegistryClientBuilder::new(server.uri().parse().unwrap())
            .blob_download_concurrency(1)
            .build()
            .unwrap();
        let repo = RepositoryName::new("repo").unwrap();

        let first = client.blob_pull(&repo, &digest).await.unwrap();
        let blocked =
            tokio::time::timeout(Duration::from_millis(200), client.blob_pull(&repo, &digest))
                .await;
        assert!(
            blocked.is_err(),
            "second download must wait while the first body is open"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "second download must not be sent while the first body is open"
        );

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(5), client.blob_pull(&repo, &digest))
            .await
            .expect("second download must start once the first body is dropped")
            .unwrap();
        let body: Vec<Bytes> = second.try_collect().await.unwrap();
        assert_eq!(body.concat(), data);
    }

    fn data_stream(
        data: &[u8],
        chunk_size: usize,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + 'static + use<> {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = data
            .chunks(chunk_size)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        futures_util::stream::iter(chunks)
    }

    /// GHCR: the whole blob, streamed in several chunks, goes in one PATCH
    /// with Content-Length and no Content-Range. Breaks if GHCR is sent
    /// more than one PATCH, which it overwrites, or a ranged one.
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

        let repo = RepositoryName::new("my-org/my-image").unwrap();
        let result = client
            .blob_push_stream(&repo, &digest, None, data_stream(data, 4))
            .await
            .unwrap();

        assert_eq!(result, digest);
        let requests = server.received_requests().await.unwrap();
        let patch = requests
            .iter()
            .find(|request| request.method == wiremock::http::Method::PATCH)
            .unwrap();
        assert_eq!(patch.body, data);
        assert!(
            patch.headers.contains_key("content-length"),
            "{:?}",
            patch.headers
        );
        assert!(
            !patch.headers.contains_key("content-range"),
            "{:?}",
            patch.headers
        );
    }

    /// ACR: small blob (under chunk size) takes the ACR path with one
    /// PATCH carrying an OCI-format `Content-Range` header
    /// (`{start}-{end}`, not RFC 7233).
    #[tokio::test]
    async fn blob_push_stream_acr_small_blob_single_patch_with_content_range() {
        let server = wiremock::MockServer::start().await;
        let data = b"acr blob content fits in one patch";
        let digest = test_digest(data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/myreg/myimg/blobs/uploads/"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/myreg/myimg/blobs/uploads/acr-uuid"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // PATCH must carry a Content-Range header in OCI `{start}-{end}`
        // format. Exact match catches off-by-one (matches the precise
        // `0-{len-1}` value -- a regex like `^0-\d+$` would silently accept
        // an off-by-one in `range_end`).
        let expected_range = format!("0-{}", data.len() - 1);
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path(
                "/v2/myreg/myimg/blobs/uploads/acr-uuid",
            ))
            .and(wiremock::matchers::header(
                "content-range",
                expected_range.as_str(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(202).append_header(
                "Location",
                "/v2/myreg/myimg/blobs/uploads/acr-uuid?after-patch",
            ))
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

        let client = build_test_client("myacr.azurecr.io", port);
        let repo = RepositoryName::new("myreg/myimg").unwrap();
        let result = client
            .blob_push_stream(&repo, &digest, None, data_stream(data, 4))
            .await
            .unwrap();
        assert_eq!(result, digest);
    }

    /// ACR: blob larger than `ACR_PATCH_CHUNK_SIZE` splits into multiple
    /// `PATCHes`, each under the 20 MB ceiling, with increasing
    /// `Content-Range` offsets.
    #[tokio::test]
    async fn blob_push_stream_acr_large_blob_splits_into_chunks() {
        // 17 MB -- just over the 16 MB chunk size, so we get 2 PATCHes
        // (one full 16 MB chunk + one 1 MB tail).
        let data = vec![0xABu8; 17 * 1024 * 1024];
        let total_len = data.len() as u64;
        let server = wiremock::MockServer::start().await;
        let digest = test_digest(&data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/proj/img/blobs/uploads/"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/proj/img/blobs/uploads/acr-1"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // First chunk: 16 MB at offset 0. Content-Range: 0-16777215.
        let first_end = ACR_PATCH_CHUNK_SIZE as u64 - 1;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/v2/proj/img/blobs/uploads/acr-1"))
            .and(wiremock::matchers::header(
                "content-range",
                format!("0-{first_end}").as_str(),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/proj/img/blobs/uploads/acr-2"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Second chunk: 1 MB at offset 16 MB.
        let total_end = total_len - 1;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/v2/proj/img/blobs/uploads/acr-2"))
            .and(wiremock::matchers::header(
                "content-range",
                format!("{}-{total_end}", ACR_PATCH_CHUNK_SIZE).as_str(),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/proj/img/blobs/uploads/acr-3"),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/v2/proj/img/blobs/uploads/acr-3"))
            .and(wiremock::matchers::query_param(
                "digest",
                digest.to_string(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_test_client("myacr.azurecr.io", port);
        let repo = RepositoryName::new("proj/img").unwrap();
        let result = client
            .blob_push_stream(&repo, &digest, Some(total_len), data_stream(&data, 65536))
            .await
            .unwrap();
        assert_eq!(result, digest);
    }

    /// ACR: zero-byte blob (e.g. the OCI empty config used by signature
    /// referrers) skips the PATCH loop and goes straight to finalize PUT.
    #[tokio::test]
    async fn blob_push_stream_acr_zero_byte_blob_skips_patch_loop() {
        let server = wiremock::MockServer::start().await;
        let data: &[u8] = b"";
        let digest = test_digest(data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/repo/blobs/uploads/"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/repo/blobs/uploads/empty-id"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // PATCH must NOT be issued for a zero-byte blob.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        // Finalize PUT carries the empty-blob digest and Content-Length: 0.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/v2/repo/blobs/uploads/empty-id"))
            .and(wiremock::matchers::query_param(
                "digest",
                digest.to_string(),
            ))
            .and(wiremock::matchers::header("content-length", "0"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_test_client("myacr.azurecr.io", port);
        let repo = RepositoryName::new("repo").unwrap();
        let result = client
            .blob_push_stream(&repo, &digest, Some(0), data_stream(data, 4))
            .await
            .unwrap();
        assert_eq!(result, digest);
    }

    /// Cross-host `Location` returned at any step of the upload chain (POST
    /// initiate, PATCH chunk, finalize PUT) must be rejected by
    /// `extract_location`'s same-origin check. The registry bearer would
    /// otherwise be forwarded to an attacker host on the next request.
    #[tokio::test]
    async fn blob_push_stream_acr_cross_host_patch_location_is_rejected() {
        let registry = wiremock::MockServer::start().await;
        let attacker = wiremock::MockServer::start().await;
        let data = b"acr cross-host rejection test bytes";
        let digest = test_digest(data);
        let registry_port = url::Url::parse(&registry.uri()).unwrap().port().unwrap();

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/r/i/blobs/uploads/"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/r/i/blobs/uploads/acr-1"),
            )
            .expect(1)
            .mount(&registry)
            .await;

        // First PATCH returns a Location at the attacker host. The client
        // must reject this before issuing any subsequent request.
        let attacker_url = format!("{}/v2/r/i/blobs/uploads/exfil", attacker.uri());
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/v2/r/i/blobs/uploads/acr-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(202).append_header("Location", attacker_url),
            )
            .expect(1)
            .mount(&registry)
            .await;

        // Attacker host MUST receive no requests.
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&attacker)
            .await;

        // Finalize PUT MUST NOT be issued at the registry either.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&registry)
            .await;

        let client = build_test_client("myacr.azurecr.io", registry_port);
        let repo = RepositoryName::new("r/i").unwrap();
        let err = client
            .blob_push_stream(&repo, &digest, None, data_stream(data, 4))
            .await
            .expect_err("cross-host PATCH Location must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("differs from base host"),
            "expected same-origin rejection error, got: {msg}"
        );
    }

    /// `blob_pull` follows 3xx redirects through reqwest's default policy.
    /// ECR private serves blob bytes via a 307 to a presigned S3 URL; this
    /// test exercises the same redirect-following path locally.
    #[tokio::test]
    async fn blob_pull_follows_307_redirect_to_blob_bytes() {
        let server = wiremock::MockServer::start().await;
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();
        let payload = b"final blob bytes from redirected location";
        let digest = test_digest(payload);
        let blob_path_str = format!("/v2/some/repo/blobs/{digest}");

        // First request: 307 with Location pointing at the data path.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(blob_path_str.clone()))
            .respond_with(
                wiremock::ResponseTemplate::new(307).append_header("Location", "/blob-data"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Redirected request: serves the actual bytes.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/blob-data"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(payload.to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_test_client("localhost", port);
        let repo = RepositoryName::new("some/repo").unwrap();
        let stream = client.blob_pull(&repo, &digest).await.unwrap();
        let body: Vec<u8> = StreamExt::collect::<Vec<_>>(stream)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .flat_map(|b| b.to_vec())
            .collect();
        assert_eq!(body, payload);
    }

    /// `blob_pull` follows cross-host 307 (e.g. ECR -> S3 presigned URL) and
    /// reqwest strips the `Authorization` header on the redirected request.
    /// The S3 URL's signature lives in the query string, so missing Authorization
    /// is the correct outcome -- forwarding registry credentials to a third-party
    /// host would leak them.
    #[tokio::test]
    async fn blob_pull_cross_host_redirect_strips_authorization() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let registry = wiremock::MockServer::start().await;
        let backend = wiremock::MockServer::start().await;
        let registry_port = url::Url::parse(&registry.uri()).unwrap().port().unwrap();

        let payload = b"cross-host bytes";
        let digest = test_digest(payload);
        let blob_path_str = format!("/v2/repo/blobs/{digest}");

        // Registry: 307 redirect to the backend's full URL.
        let backend_url = format!("{}/blob-data", backend.uri());
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(blob_path_str.clone()))
            .respond_with(
                wiremock::ResponseTemplate::new(307).append_header("Location", backend_url),
            )
            .expect(1)
            .mount(&registry)
            .await;

        // Backend: assert NO Authorization header present, serve the bytes.
        let saw_auth = Arc::new(AtomicBool::new(false));
        let saw_auth_clone = Arc::clone(&saw_auth);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/blob-data"))
            .respond_with(move |req: &wiremock::Request| {
                if req.headers.contains_key("authorization") {
                    saw_auth_clone.store(true, Ordering::SeqCst);
                }
                wiremock::ResponseTemplate::new(200).set_body_bytes(payload.to_vec())
            })
            .expect(1)
            .mount(&backend)
            .await;

        // Use a client with a fake bearer to confirm the header would have
        // been on the original request had reqwest forwarded it.
        let base = url::Url::parse(&format!("http://localhost:{registry_port}")).unwrap();
        let client = crate::client::RegistryClientBuilder::new(base)
            .resolve(
                "localhost",
                std::net::SocketAddr::from(([127, 0, 0, 1], registry_port)),
            )
            .auth(crate::auth::static_token::StaticTokenAuth::new(
                "localhost",
                "fake-leak-canary-token",
            ))
            .build()
            .unwrap();

        let repo = RepositoryName::new("repo").unwrap();
        let stream = client.blob_pull(&repo, &digest).await.unwrap();
        let body: Vec<u8> = StreamExt::collect::<Vec<_>>(stream)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .flat_map(|b| b.to_vec())
            .collect();
        assert_eq!(body, payload);
        assert!(
            !saw_auth.load(Ordering::SeqCst),
            "Authorization header must NOT be forwarded across hosts -- would leak registry credentials to the redirect target"
        );
    }

    /// Covers the hostname-keyed mount routing:
    ///
    /// - ECR (private + public): client short-circuits, zero POSTs issued,
    ///   returns `NotMounted`.
    /// - GHCR, 201 response: `Mounted`.
    /// - GHCR, 202 response: `NotMounted` (server fallback).
    /// - Unknown host, 201 response: `Mounted` (optimistic default).
    ///
    /// The short-circuit rows are negative assertions: `.expect(0)` on the
    /// POST catches a regression that would re-introduce the wasted call.
    #[tokio::test]
    async fn blob_mount_routes_per_provider() {
        /// `server` is `None` for rows where the short-circuit should fire
        /// and no POST should reach the wire.
        struct Case {
            host: &'static str,
            server: Option<u16>,
            expect_mounted: bool,
        }

        let cases = [
            Case {
                host: "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                server: Some(201),
                expect_mounted: true,
            },
            Case {
                host: "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                server: Some(202),
                expect_mounted: false,
            },
            Case {
                host: "ghcr.io",
                server: Some(201),
                expect_mounted: true,
            },
            Case {
                host: "ghcr.io",
                server: Some(202),
                expect_mounted: false,
            },
            Case {
                host: "my-private-registry.example.com",
                server: Some(201),
                expect_mounted: true,
            },
        ];

        for case in cases {
            let server = wiremock::MockServer::start().await;
            let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

            let response_status = case.server.unwrap();

            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v2/tgt/repo/blobs/uploads/"))
                .respond_with(wiremock::ResponseTemplate::new(response_status))
                .expect(1)
                .mount(&server)
                .await;

            let client = build_test_client(case.host, port);
            let result = client
                .blob_mount(
                    &RepositoryName::new("tgt/repo").unwrap(),
                    &test_digest(case.host.as_bytes()),
                    &RepositoryName::new("src/repo").unwrap(),
                )
                .await
                .unwrap();

            let ok = matches!(
                (case.expect_mounted, &result),
                (true, MountResult::Mounted) | (false, MountResult::NotMounted)
            );
            assert!(
                ok,
                "{} ({:?}): unexpected result {result:?}",
                case.host, case.server
            );
        }
    }

    /// The default upload path, Artifact Registry's included, sends the
    /// blob as one streaming PUT after the POST: no PATCH, and no
    /// `Content-Length`, as the body is never buffered. Breaks if a host
    /// takes a buffered or chunked path.
    #[tokio::test]
    async fn blob_push_stream_uses_streaming_put() {
        let size = Some(b"streaming upload blob data".len() as u64);
        for host in ["registry.example.com", "us-docker.pkg.dev"] {
            for known_size in [None, size] {
                streams_one_put(host, known_size).await;
            }
        }
    }

    async fn streams_one_put(host: &str, known_size: Option<u64>) {
        let server = wiremock::MockServer::start().await;
        let data = b"streaming upload blob data";
        let digest = test_digest(data);
        let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();

        // POST: initiate upload.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/stream/repo/blobs/uploads/"))
            .respond_with(
                wiremock::ResponseTemplate::new(202)
                    .append_header("Location", "/v2/stream/repo/blobs/uploads/stream-uuid"),
            )
            .expect(1)
            .mount(&server)
            .await;

        // PUT: streaming upload with full body (no Content-Length).
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::query_param(
                "digest",
                digest.to_string(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        // PATCH: must NOT be called for streaming uploads.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = build_test_client(host, port);

        let repo = RepositoryName::new("stream/repo").unwrap();
        let result = client
            .blob_push_stream(&repo, &digest, known_size, data_stream(data, 4))
            .await
            .unwrap();

        assert_eq!(result, digest, "{host}");
        let requests = server.received_requests().await.unwrap();
        let put = requests
            .iter()
            .find(|request| request.method == wiremock::http::Method::PUT)
            .unwrap();
        assert!(
            !put.headers.contains_key("content-length"),
            "{host}: {:?}",
            put.headers
        );
        assert_eq!(put.body, data, "{host}");
    }

    /// Pins the byte-budget queueing contract for the buffered fallback
    /// paths (GHCR / ACR).
    ///
    /// Two concurrent GHCR uploads, each declaring half the budget plus
    /// a few bytes, would together exceed the byte budget if they ran
    /// fully overlapped. The first upload is delayed by the wiremock
    /// mock (200 ms PATCH/PUT response delay) so the second must wait
    /// on the byte-budget permit before its own POST initiates.
    ///
    /// Asserts: the second upload's POST start time is at least
    /// `delay` after the first upload's POST start time. This is the
    /// observable signal that the byte-budget semaphore actually queued
    /// the second upload behind the first.
    #[tokio::test]
    async fn buffered_upload_byte_budget_queues_concurrent_uploads() {
        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;
        use std::time::Instant;
        use tokio::time::Duration;
        use url::Url;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

        let server = MockServer::start().await;
        let port = Url::parse(&server.uri()).unwrap().port().unwrap();
        let post_times: Arc<StdMutex<Vec<Instant>>> = Arc::new(StdMutex::new(Vec::new()));
        let post_times_clone = Arc::clone(&post_times);

        // Two distinct repos so the POST paths don't collide; record the
        // POST timestamps so we can verify the ordering after the fact.
        Mock::given(matchers::method("POST"))
            .and(matchers::path_regex(r"^/v2/repo-[ab]/blobs/uploads/$"))
            .respond_with(move |req: &wiremock::Request| {
                post_times_clone.lock().unwrap().push(Instant::now());
                // Return a Location pointing back at the same upload path so
                // the PATCH+PUT chain can complete.
                let uri_path = req.url.path();
                let upload_path = format!("{uri_path}upload-id");
                ResponseTemplate::new(202).append_header("Location", upload_path)
            })
            .mount(&server)
            .await;

        // PATCH delays 200ms so the first upload holds its byte permit
        // long enough for the second to definitively wait. PUT is fast.
        let patch_delay = Duration::from_millis(200);
        Mock::given(matchers::method("PATCH"))
            .respond_with(
                ResponseTemplate::new(202)
                    .append_header("Location", "/v2/done")
                    .set_delay(patch_delay),
            )
            .mount(&server)
            .await;
        Mock::given(matchers::method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        // 6 MB blobs: large enough that two together (12 MB) exceed an
        // 8 MB budget. Use distinct contents per upload so the digests
        // differ.
        let blob_a = vec![0xAAu8; 6 * 1024 * 1024];
        let blob_b = vec![0xBBu8; 6 * 1024 * 1024];
        let digest_a = test_digest(&blob_a);
        let digest_b = test_digest(&blob_b);

        // Use the GHCR fallback path (single-PATCH then PUT, buffered),
        // with a small 8 MB byte budget. Two 6 MB uploads must serialize.
        let base_url = Url::parse(&format!("http://ghcr.io:{port}")).unwrap();
        let client = Arc::new(
            crate::client::RegistryClientBuilder::new(base_url)
                .resolve(
                    "ghcr.io",
                    std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                )
                .buffered_blob_bytes(8 * 1024 * 1024)
                .build()
                .unwrap(),
        );

        let repo_a = RepositoryName::new("repo-a").unwrap();
        let repo_b = RepositoryName::new("repo-b").unwrap();

        let client_1 = Arc::clone(&client);
        let blob_a_clone = blob_a.clone();
        let digest_a_clone = digest_a.clone();
        let h1 = tokio::spawn(async move {
            client_1
                .blob_push_stream(
                    &repo_a,
                    &digest_a_clone,
                    Some(blob_a_clone.len() as u64),
                    data_stream(&blob_a_clone, 65536),
                )
                .await
        });

        // Deterministic synchronization (no head-start race): wait until
        // task 1's POST has been recorded -- by that point task 1 has
        // already acquired the byte permit, so task 2 is guaranteed to
        // see the budget pressure when it tries. Polls a shared
        // `post_times` Mutex on a short cadence and bails out after a
        // generous deadline if task 1 never makes progress (treating
        // that as a separate test failure).
        let wait_for_first_post = async {
            loop {
                if !post_times.lock().unwrap().is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait_for_first_post)
            .await
            .expect("first upload POST must fire within 5s (task 1 likely stuck)");

        let client_2 = Arc::clone(&client);
        let blob_b_clone = blob_b.clone();
        let digest_b_clone = digest_b.clone();
        let h2 = tokio::spawn(async move {
            client_2
                .blob_push_stream(
                    &repo_b,
                    &digest_b_clone,
                    Some(blob_b_clone.len() as u64),
                    data_stream(&blob_b_clone, 65536),
                )
                .await
        });

        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();
        assert!(r1.is_ok(), "first upload should succeed: {:?}", r1.err());
        assert!(r2.is_ok(), "second upload should succeed: {:?}", r2.err());

        let times = post_times.lock().unwrap();
        assert_eq!(times.len(), 2, "both POSTs must have fired");
        let gap = times[1].duration_since(times[0]);
        // Task 2 cannot POST until task 1 releases the byte permit,
        // which happens when task 1's `blob_push_stream` returns
        // (after PATCH-delay + fast PUT). The lower bound is
        // patch_delay minus a generous CI margin (100 ms) since we
        // already eliminated the head-start race above.
        let lower_bound = patch_delay.saturating_sub(Duration::from_millis(100));
        assert!(
            gap >= lower_bound,
            "second POST started after only {gap:?}; expected the byte budget to queue it for at least ~{lower_bound:?} behind the first (patch_delay = {patch_delay:?})"
        );
    }
}
