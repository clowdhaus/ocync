//! OCI distribution client library - types, authentication, and registry operations.
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests read JSON they wrote themselves"
    )
)]

/// AIMD concurrency controller - adaptive rate limiting via 429 feedback.
pub mod aimd;
/// Authentication providers and token management.
pub mod auth;
/// Signed calls to the AWS APIs (ECR, ECR Public).
mod aws;
/// Blob operations (upload, download, existence checks, mounting).
pub mod blob;
/// OCI registry HTTP client.
pub mod client;
/// A credential cached until it nears expiry.
mod credential_cache;
/// OCI content-addressable digest type.
pub mod digest;
/// AWS ECR batch operations - bulk blob existence checks.
pub mod ecr;
/// Error types for OCI distribution operations.
pub mod error;
/// One fetch in flight per key, shared with the callers that wait on it.
mod flight;
/// Google Cloud Application Default Credentials.
mod google;
/// JSON decoding shared by the registry and AWS clients.
mod json;
/// Manifest operations (pull, push, head, referrers).
pub mod manifest;
/// Address rules for requests that carry a secret.
mod net;
/// OCI image reference parser.
pub mod reference;
/// Bounded retry for transient failures in the prepare phase.
pub mod retry;
/// The URL schemes requests are sent over.
mod scheme;
/// SHA-256 wrapper backed by aws-lc-rs.
pub mod sha256;
/// OCI image spec types - manifests, descriptors, and platforms.
pub mod spec;
/// Tag listing with pagination.
pub mod tags;
#[cfg(test)]
mod test_logs;

/// What a secret prints as in a `Debug` rendering.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// Install `aws-lc-rs` as the process-wide rustls crypto provider.
///
/// Must be called before any TLS connection is established. Later calls
/// are no-ops.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Which AWS-LC module this build links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoBuild {
    /// The AWS-LC FIPS module.
    Fips,
    /// AWS-LC without FIPS validation.
    NonFips,
}

/// The module this build links. The one source of truth for "is this a FIPS
/// build", so callers in other crates cannot disagree with the linked module.
pub const CRYPTO_BUILD: CryptoBuild = if cfg!(feature = "fips") {
    CryptoBuild::Fips
} else {
    CryptoBuild::NonFips
};

/// The AWS-LC-FIPS module version this build corresponds to, such as `4`.
///
/// This is the module number a NIST certificate names.
pub fn fips_module() -> Option<u32> {
    aws_lc_rs::fips_version()
}

/// The linked AWS-LC release, such as `4.2.0`.
///
/// Distinguishes builds of one FIPS module. It does not identify a
/// certificate on its own.
pub fn awslc_release() -> &'static str {
    aws_lc_rs::awslc_version()
}

/// Whether TLS is restricted to algorithms the FIPS module approves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsFips {
    /// Every cipher suite, key exchange group and signature algorithm is
    /// FIPS-approved, and the module runs in approved mode.
    Approved,
    /// TLS can negotiate an algorithm outside the FIPS-approved set.
    NotApproved,
}

/// Report whether TLS through the installed crypto provider is FIPS-approved.
///
/// `rustls` derives the rest of a client configuration's FIPS state from the
/// provider, including TLS 1.2 extended master secret enforcement, so the
/// provider is the whole answer.
pub fn tls_fips() -> TlsFips {
    match rustls::crypto::CryptoProvider::get_default() {
        Some(provider) if provider.fips() => TlsFips::Approved,
        _ => TlsFips::NotApproved,
    }
}

/// A [`reqwest::Client`] with the crypto provider installed, for tests only.
#[doc(hidden)]
pub fn test_http_client() -> reqwest::Client {
    install_crypto_provider();
    reqwest::Client::new()
}

/// A loopback address nothing listens on.
#[cfg(test)]
const REFUSING: &str = "127.0.0.1:1";

/// What a connection to [`REFUSING`] reports, for tests of a refused
/// connection.
#[cfg(test)]
fn refusal() -> String {
    std::net::TcpStream::connect(REFUSING)
        .unwrap_err()
        .to_string()
}

pub use aws::AwsCredentialProviders;
pub use blob::MountResult;
pub use client::{RegistryClient, RegistryClientBuilder, UploadSlot};
pub use digest::Digest;
pub use ecr::{BatchBlobChecker, BlobExistence};
pub use error::Error;
pub use manifest::ManifestPull;
pub use reference::Reference;
pub use spec::{
    Descriptor, ImageIndex, ImageManifest, ManifestKind, MediaType, Platform, PlatformFilter,
    RegistryAuthority, RepositoryName,
};

#[cfg(all(test, not(feature = "fips")))]
mod tests {
    use super::*;

    #[test]
    fn non_fips_build_reports_tls_not_approved() {
        install_crypto_provider();
        assert_eq!(tls_fips(), TlsFips::NotApproved);
    }
}
