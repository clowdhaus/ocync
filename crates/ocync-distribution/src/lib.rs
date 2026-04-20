//! OCI distribution client library - types, authentication, and registry operations.

/// AIMD concurrency controller - adaptive rate limiting via 429 feedback.
pub mod aimd;
/// Authentication providers and token management.
pub mod auth;
/// Blob operations (upload, download, existence checks, mounting).
pub mod blob;
/// OCI registry HTTP client.
pub mod client;
/// OCI content-addressable digest type.
pub mod digest;
/// AWS ECR batch operations - bulk blob existence checks.
pub mod ecr;
/// Error types for OCI distribution operations.
pub mod error;
/// Manifest operations (pull, push, head, referrers).
pub mod manifest;
/// OCI image reference parser.
pub mod reference;
/// SHA-256 wrapper backed by aws-lc-rs.
pub mod sha256;
/// OCI image spec types - manifests, descriptors, and platforms.
pub mod spec;
/// Tag listing with pagination.
pub mod tags;

/// Install `aws-lc-rs` as the process-wide rustls crypto provider.
///
/// Must be called before any TLS connection is established. Safe to call
/// multiple times - subsequent calls are no-ops (returns `Err` which is
/// ignored).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Create a [`reqwest::Client`] with the crypto provider installed.
///
/// Convenience for tests that need an HTTP client without going through
/// [`RegistryClientBuilder`]. Production code should use
/// [`install_crypto_provider`] at startup and construct clients directly.
///
/// This is a test utility; do not use in production code.
#[doc(hidden)]
pub fn test_http_client() -> reqwest::Client {
    install_crypto_provider();
    reqwest::Client::new()
}

pub use blob::MountResult;
pub use client::{RegistryClient, RegistryClientBuilder};
pub use digest::Digest;
pub use ecr::BatchBlobChecker;
pub use error::Error;
pub use manifest::{ManifestHead, ManifestPull};
pub use reference::Reference;
pub use spec::{
    Descriptor, ImageIndex, ImageManifest, ManifestKind, MediaType, Platform, PlatformFilter,
    RegistryAuthority, RepositoryName,
};
