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
/// Bounded retry for transient failures in the prepare phase.
pub mod retry;
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
/// Checks a client configuration of the shape `reqwest` builds, so TLS 1.2
/// extended master secret enforcement counts alongside the provider's
/// algorithm lists.
pub fn tls_fips() -> TlsFips {
    let Some(provider) = rustls::crypto::CryptoProvider::get_default() else {
        return TlsFips::NotApproved;
    };
    let approved = rustls::ClientConfig::builder_with_provider(std::sync::Arc::clone(provider))
        .with_safe_default_protocol_versions()
        .is_ok_and(|builder| {
            builder
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth()
                .fips()
        });
    if approved {
        TlsFips::Approved
    } else {
        TlsFips::NotApproved
    }
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

#[cfg(all(test, feature = "fips"))]
mod tests {
    use super::*;

    #[test]
    fn fips_build_reports_tls_approved() {
        install_crypto_provider();
        assert_eq!(tls_fips(), TlsFips::Approved);
    }

    /// The AWS SDK and `google-cloud-auth` build their own TLS clients from
    /// `aws_lc_rs::default_provider()`, so that function must drop the
    /// algorithms the FIPS module does not approve.
    #[test]
    fn fips_build_drops_chacha20_from_the_provider_other_crates_use() {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        assert!(provider.fips());
        assert!(
            !provider.cipher_suites.iter().any(|suite| matches!(
                suite.suite(),
                rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256
                    | rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
                    | rustls::CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
            )),
            "ChaCha20-Poly1305 is not FIPS-approved"
        );
    }
}
