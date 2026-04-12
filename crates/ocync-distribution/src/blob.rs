//! Blob operations — existence checks, pull, push, mount, and upload management.

use bytes::Bytes;
use futures_util::Stream;
use http::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LOCATION};

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
            Err(Error::NotFound(_)) => Ok(None),
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

        // Extract the upload URL from the Location header.
        let upload_url = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Other("missing Location header in upload response".into()))?
            .to_owned();

        // Resolve relative Location URLs against the base URL.
        let put_url = if upload_url.starts_with("http://") || upload_url.starts_with("https://") {
            upload_url
        } else {
            self.base_url
                .join(&upload_url)
                .map_err(|e| Error::Other(format!("failed to resolve upload URL: {e}")))?
                .to_string()
        };

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
}
