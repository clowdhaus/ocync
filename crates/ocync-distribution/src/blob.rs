use bytes::Bytes;
use futures_util::Stream;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LOCATION};

use crate::auth::Scope;
use crate::client::{RegistryClient, build_url};
use crate::digest::Digest;
use crate::error::Error;
use crate::sha256::Sha256;

/// Result of a HEAD request to check blob existence.
#[derive(Debug)]
pub enum BlobExistsResult {
    /// The blob exists with the given size in bytes.
    Exists {
        /// Content length reported by the registry.
        size: u64,
    },
    /// The blob was not found.
    NotFound,
}

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
pub fn blob_path(digest: &Digest) -> String {
    format!("blobs/{digest}")
}

/// Build the path segment for blob uploads: `/blobs/uploads/`.
pub fn uploads_path() -> String {
    "blobs/uploads/".to_owned()
}

impl RegistryClient {
    /// Check whether a blob exists in the given repository.
    ///
    /// Issues a HEAD request to `/v2/{repository}/blobs/{digest}`.
    pub async fn blob_exists(
        &self,
        repository: &str,
        digest: &Digest,
    ) -> Result<BlobExistsResult, Error> {
        let path = blob_path(digest);
        match self.head(repository, &path).await {
            Ok(resp) => {
                let size = resp
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(BlobExistsResult::Exists { size })
            }
            Err(Error::NotFound(_)) => Ok(BlobExistsResult::NotFound),
            Err(e) => Err(e),
        }
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
    /// If the registry supports it, returns `Mounted`. Otherwise, falls back to
    /// a regular upload URL.
    pub async fn blob_mount(
        &self,
        repository: &str,
        digest: &Digest,
        from_repo: &str,
    ) -> Result<MountResult, Error> {
        let url = build_url(&self.base_url, repository, &uploads_path())?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let scopes = [Scope::pull_push(repository), Scope::pull(from_repo)];
        let headers = self.auth_headers(&scopes).await?;

        let resp = self
            .http
            .post(url.clone())
            .headers(headers.clone())
            .query(&[
                ("mount", digest.to_string()),
                ("from", from_repo.to_owned()),
            ])
            .send()
            .await?;

        let status = resp.status().as_u16();

        // 401 — refresh token and retry once.
        if status == 401 {
            tracing::debug!(url = %url, "got 401 on blob mount, refreshing auth token");
            let headers = self.auth_headers(&scopes).await?;
            let resp = self
                .http
                .post(url)
                .headers(headers)
                .query(&[
                    ("mount", digest.to_string()),
                    ("from", from_repo.to_owned()),
                ])
                .send()
                .await?;

            return self.classify_mount_response(resp).await;
        }

        self.classify_mount_response(resp).await
    }

    /// Classify a mount response into `Mounted` or `FallbackUpload`.
    async fn classify_mount_response(&self, resp: reqwest::Response) -> Result<MountResult, Error> {
        let status = resp.status().as_u16();

        match status {
            // 201 Created — blob was successfully mounted.
            201 => Ok(MountResult::Mounted),
            // 202 Accepted — mount not supported; registry gave us an upload URL.
            202 => {
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

    /// Push a blob to the given repository using a monolithic upload.
    ///
    /// 1. POST `/v2/{repository}/blobs/uploads/` to initiate the upload.
    /// 2. PUT the entire data with the computed digest.
    ///
    /// Returns the digest of the uploaded blob.
    pub async fn blob_push(&self, repository: &str, data: Vec<u8>) -> Result<Digest, Error> {
        // Compute digest of the data.
        let hash = Sha256::digest(&data);
        let digest = Digest::from_sha256(hash);
        let data_len = data.len();

        let url = build_url(&self.base_url, repository, &uploads_path())?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let scopes = [Scope::pull_push(repository)];
        let headers = self.auth_headers(&scopes).await?;

        // Step 1: Initiate upload with POST.
        let resp = self
            .http
            .post(url.clone())
            .headers(headers.clone())
            .send()
            .await?;

        let status = resp.status().as_u16();

        // Handle 401 retry for initiation.
        let resp = if status == 401 {
            tracing::debug!(url = %url, "got 401 on blob push initiate, refreshing auth token");
            let headers = self.auth_headers(&scopes).await?;
            self.http.post(url).headers(headers).send().await?
        } else {
            resp
        };

        let status = resp.status().as_u16();
        if status != 202 {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        // Extract the upload URL from the Location header.
        let upload_url = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Other("missing Location header in upload response".into()))?
            .to_owned();

        // Resolve relative Location URLs against the base URL.
        let put_url = if upload_url.starts_with("http://") || upload_url.starts_with("https://") {
            upload_url.clone()
        } else {
            self.base_url
                .join(&upload_url)
                .map_err(|e| Error::Other(format!("failed to resolve upload URL: {e}")))?
                .to_string()
        };

        // Step 2: PUT the data with digest query param.
        let headers = self.auth_headers(&scopes).await?;
        let resp = self
            .http
            .put(&put_url)
            .headers(headers)
            .query(&[("digest", digest.to_string())])
            .header(CONTENT_LENGTH, data_len.to_string())
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            )
            .body(data)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status != 201 {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(digest)
    }

    /// Delete an in-progress blob upload.
    ///
    /// Issues a DELETE to the given upload URL.
    pub async fn blob_upload_delete(&self, upload_url: &str) -> Result<(), Error> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        // Upload URLs don't belong to a specific repo scope, but we still need auth.
        // Use an empty scope list — the token from a previous request should still work.
        let headers = self.auth_headers(&[]).await?;

        let resp = self.http.delete(upload_url).headers(headers).send().await?;

        let status = resp.status().as_u16();

        // 401 — retry once.
        if status == 401 {
            tracing::debug!(
                url = upload_url,
                "got 401 on upload delete, refreshing auth token"
            );
            let headers = self.auth_headers(&[]).await?;
            let resp = self.http.delete(upload_url).headers(headers).send().await?;

            let status = resp.status().as_u16();
            if status != 204 && status != 202 {
                let message = resp.text().await.unwrap_or_default();
                return Err(Error::RegistryError { status, message });
            }
            return Ok(());
        }

        if status != 204 && status != 202 {
            let message = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryError { status, message });
        }

        Ok(())
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
    fn uploads_path_format() {
        assert_eq!(uploads_path(), "blobs/uploads/");
    }
}
