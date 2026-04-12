//! Blob operations — existence checks, pull, push, mount, and upload management.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, LOCATION};
use tracing::{debug, warn};

use crate::auth::Scope;
use crate::client::{RegistryClient, build_url};
use crate::digest::Digest;
use crate::error::Error;
use crate::sha256::Sha256;

/// Result of a cross-repository blob mount attempt.
#[derive(Debug)]
pub enum MountResult {
    /// The blob was successfully mounted.
    Mounted,
    /// The registry did not support mounting; use the returned upload URL instead.
    FallbackUpload {
        /// The URL to use for a chunked upload.
        upload_url: String,
    },
}

/// Build the path segment for a blob: `/blobs/{digest}`.
fn blob_path(digest: &Digest) -> String {
    format!("blobs/{digest}")
}

impl RegistryClient {
    /// Check whether a blob exists in the given repository.
    ///
    /// Issues a HEAD request to `/v2/{repository}/blobs/{digest}`.
    /// Returns `Some(size)` if the blob exists, `None` if not found.
    pub async fn blob_exists(
        &self,
        repository: &str,
        digest: &Digest,
    ) -> Result<Option<u64>, Error> {
        let path = blob_path(digest);
        match self.head(repository, &path).await {
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

    /// Pull a blob and return the complete bytes.
    ///
    /// Issues a GET request to `/v2/{repository}/blobs/{digest}` and returns
    /// the full response body as a `Vec<u8>`. For large blobs, prefer
    /// [`blob_pull`](Self::blob_pull) which returns a streaming response.
    pub async fn blob_pull_all(&self, repository: &str, digest: &Digest) -> Result<Vec<u8>, Error> {
        let path = blob_path(digest);
        let resp = self.get(repository, &path, None).await?;
        let bytes = resp.bytes().await?;
        Ok(bytes.into())
    }

    /// Pull a blob as a streaming response.
    ///
    /// Issues a GET request to `/v2/{repository}/blobs/{digest}` and returns
    /// a byte stream for the response body.
    pub async fn blob_pull(
        &self,
        repository: &str,
        digest: &Digest,
    ) -> Result<impl Stream<Item = Result<Bytes, reqwest::Error>>, Error> {
        let path = blob_path(digest);
        let resp = self.get(repository, &path, None).await?;
        Ok(resp.bytes_stream())
    }

    /// Attempt a cross-repository blob mount.
    ///
    /// Issues a POST to `/v2/{repository}/blobs/uploads/?mount={digest}&from={from_repo}`.
    /// If the registry supports it, returns [`MountResult::Mounted`]. Otherwise,
    /// falls back to a regular upload URL.
    pub async fn blob_mount(
        &self,
        repository: &str,
        digest: &Digest,
        from_repo: &str,
    ) -> Result<MountResult, Error> {
        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        let scopes = [Scope::pull_push(repository), Scope::pull(from_repo)];
        let digest_str = digest.to_string();
        let from = from_repo.to_owned();

        let resp = self
            .send_with_retry(&scopes, "blob mount", |headers| {
                self.http
                    .post(url.clone())
                    .headers(headers)
                    .query(&[("mount", &digest_str), ("from", &from)])
            })
            .await?;

        classify_mount_response(resp).await
    }

    /// Push a blob to the given repository using a monolithic upload.
    ///
    /// 1. POST `/v2/{repository}/blobs/uploads/` to initiate the upload.
    /// 2. PUT the entire data with the computed digest.
    ///
    /// Returns the digest of the uploaded blob.
    pub async fn blob_push(&self, repository: &str, data: &[u8]) -> Result<Digest, Error> {
        let hash = Sha256::digest(data);
        let digest = Digest::from_sha256(hash);

        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        let scopes = [Scope::pull_push(repository)];

        // Step 1: Initiate upload with POST.
        let resp = self
            .send_with_retry(&scopes, "blob push initiate", |headers| {
                self.http.post(url.clone()).headers(headers)
            })
            .await?;

        let status = resp.status();
        if status != StatusCode::ACCEPTED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        // Extract and resolve the upload URL from the Location header.
        let put_url = extract_location(&resp, &self.base_url)?;

        // Step 2: PUT the data with digest query param.
        let digest_str = digest.to_string();
        let resp = self
            .send_with_retry(&scopes, "blob push upload", |headers| {
                self.http
                    .put(&put_url)
                    .headers(headers)
                    .query(&[("digest", &digest_str)])
                    .header(CONTENT_LENGTH, data.len().to_string())
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    )
                    .body(data.to_vec())
            })
            .await?;

        let status = resp.status();
        if status != StatusCode::CREATED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(digest)
    }

    /// Push a blob to the given repository using a chunked streaming upload.
    ///
    /// 1. POST `/v2/{repository}/blobs/uploads/` to initiate the upload.
    /// 2. Accumulate stream chunks into `chunk_size` buffers, issuing PATCH
    ///    requests with `Content-Range` headers for each buffer.
    /// 3. PUT to finalize with `?digest=` query param.
    ///
    /// If the computed digest does not match `expected_digest`, the upload is
    /// deleted and [`Error::DigestMismatch`] is returned.
    ///
    /// **GAR fallback**: Google Artifact Registry does not support chunked
    /// uploads, so hosts ending in `-docker.pkg.dev` buffer the entire stream
    /// and delegate to [`blob_push`](Self::blob_push).
    pub async fn blob_push_stream(
        &self,
        repository: &str,
        expected_digest: &Digest,
        content_length: u64,
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    ) -> Result<Digest, Error> {
        // GAR fallback: buffer entire stream and use monolithic push.
        if self
            .base_url
            .host_str()
            .is_some_and(|h| h.ends_with("-docker.pkg.dev"))
        {
            return self
                .blob_push_stream_gar_fallback(repository, expected_digest, stream)
                .await;
        }

        debug!(
            repository,
            %expected_digest,
            content_length,
            chunk_size = self.chunk_size,
            "starting chunked blob upload"
        );

        let url = build_url(&self.base_url, repository, "blobs/uploads/")?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        let scopes = [Scope::pull_push(repository)];

        // Step 1: Initiate upload with POST.
        let resp = self
            .send_with_retry(&scopes, "blob push stream initiate", |headers| {
                self.http.post(url.clone()).headers(headers)
            })
            .await?;

        let status = resp.status();
        if status != StatusCode::ACCEPTED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        let upload_url = extract_location(&resp, &self.base_url)?;

        // Step 2: Stream chunks, issuing PATCH for each chunk_size buffer.
        let mut hasher = Sha256::new();
        let mut buffer = Vec::with_capacity(self.chunk_size);
        let mut current_url = upload_url;
        let mut offset: u64 = 0;

        futures_util::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            buffer.extend_from_slice(&chunk);

            if buffer.len() >= self.chunk_size {
                let range_start = offset;
                let range_end = offset + buffer.len() as u64 - 1;
                offset += buffer.len() as u64;

                let patch_data =
                    std::mem::replace(&mut buffer, Vec::with_capacity(self.chunk_size));
                current_url = self
                    .send_patch_chunk(&scopes, &current_url, range_start, range_end, patch_data)
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

        // Verify digest before finalizing.
        let hash = hasher.finalize();
        let actual_digest = Digest::from_sha256(hash);
        if actual_digest != *expected_digest {
            // DELETE the upload to clean up.
            let _ = self
                .send_with_retry(&scopes, "blob push stream delete", |headers| {
                    self.http.delete(&current_url).headers(headers)
                })
                .await;
            return Err(Error::DigestMismatch {
                expected: expected_digest.to_string(),
                actual: actual_digest.to_string(),
            });
        }

        // Step 3: PUT to finalize.
        let digest_str = actual_digest.to_string();
        let resp = self
            .send_with_retry(&scopes, "blob push stream finalize", |headers| {
                self.http
                    .put(&current_url)
                    .headers(headers)
                    .query(&[("digest", &digest_str)])
                    .header(CONTENT_LENGTH, "0")
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    )
            })
            .await?;

        let status = resp.status();
        if status != StatusCode::CREATED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(actual_digest)
    }

    /// Send a PATCH chunk for a chunked upload and return the next upload URL.
    async fn send_patch_chunk(
        &self,
        scopes: &[Scope],
        upload_url: &str,
        range_start: u64,
        range_end: u64,
        data: Vec<u8>,
    ) -> Result<String, Error> {
        let content_range = HeaderValue::from_str(&format!("{range_start}-{range_end}"))
            .map_err(|e| Error::Other(format!("failed to build Content-Range header: {e}")))?;
        let data_len = data.len();
        let url = upload_url.to_owned();
        // Convert to Bytes so .clone() in the retry closure is a cheap
        // reference-count increment instead of a full memcpy per chunk.
        let data = Bytes::from(data);

        let resp = self
            .send_with_retry(scopes, "blob push stream patch", |headers| {
                self.http
                    .patch(&url)
                    .headers(headers)
                    .header(CONTENT_RANGE, content_range.clone())
                    .header(CONTENT_LENGTH, data_len.to_string())
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    )
                    .body(data.clone())
            })
            .await?;

        let status = resp.status();
        if status != StatusCode::ACCEPTED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        extract_location(&resp, &self.base_url)
    }

    /// GAR fallback: buffer the entire stream and delegate to monolithic push.
    ///
    /// Google Artifact Registry does not support chunked uploads, so the
    /// entire stream is buffered in memory and sent as a monolithic upload.
    async fn blob_push_stream_gar_fallback(
        &self,
        repository: &str,
        expected_digest: &Digest,
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    ) -> Result<Digest, Error> {
        warn!(
            repository,
            host = self.base_url.host_str().unwrap_or("unknown"),
            "GAR does not support chunked uploads; buffering entire blob in memory"
        );
        let mut body = Vec::new();
        futures_util::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            body.extend_from_slice(&chunk);
        }

        let digest = self.blob_push(repository, &body).await?;
        if digest != *expected_digest {
            return Err(Error::DigestMismatch {
                expected: expected_digest.to_string(),
                actual: digest.to_string(),
            });
        }

        Ok(digest)
    }

    /// Delete an in-progress blob upload.
    ///
    /// Issues a DELETE to the given upload URL.
    pub async fn blob_upload_delete(&self, upload_url: &str) -> Result<(), Error> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let resp = self
            .send_with_retry(&[], "upload delete", |headers| {
                self.http.delete(upload_url).headers(headers)
            })
            .await?;

        let status = resp.status();
        if status != StatusCode::NO_CONTENT && status != StatusCode::ACCEPTED {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(())
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
async fn classify_mount_response(resp: reqwest::Response) -> Result<MountResult, Error> {
    let status = resp.status();

    match status {
        StatusCode::CREATED => Ok(MountResult::Mounted),
        StatusCode::ACCEPTED => {
            let upload_url = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            Ok(MountResult::FallbackUpload { upload_url })
        }
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

    /// Build a `RegistryClient` with a GAR hostname resolving to a local port.
    fn build_gar_client(port: u16) -> RegistryClient {
        let base_url = url::Url::parse(&format!("http://us-docker.pkg.dev:{port}")).unwrap();
        let http = reqwest::Client::builder()
            .resolve(
                "us-docker.pkg.dev",
                std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            )
            .build()
            .unwrap();

        RegistryClient {
            base_url,
            http,
            auth: None,
            semaphore: tokio::sync::Semaphore::new(8),
            chunk_size: 4,
        }
    }

    fn test_digest(data: &[u8]) -> Digest {
        Digest::from_sha256(Sha256::digest(data))
    }

    fn data_stream(
        data: &[u8],
        chunk_size: usize,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static {
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

        let client = build_gar_client(port);

        let result = client
            .blob_push_stream(
                "my-project/my-repo",
                &digest,
                data.len() as u64,
                data_stream(data, 4),
            )
            .await
            .unwrap();

        assert_eq!(result, digest);
    }
}
