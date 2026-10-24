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

/// Which AWS-LC module this build links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoBuild {
    /// The FIPS 140-3 validated AWS-LC-FIPS module.
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

/// NIST FIPS 140-3 certification status of an AWS-LC-FIPS module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certification {
    /// Tested by an accredited lab and submitted to NIST, but not yet
    /// certified. Listed on [`MODULES_IN_PROCESS_URL`].
    InProcess,
}

/// NIST's list of modules submitted for validation and not yet certified.
pub const MODULES_IN_PROCESS_URL: &str = "https://csrc.nist.gov/Projects/Cryptographic-Module-Validation-Program/Modules-In-Process/Modules-In-Process-List";

/// Certification status of the linked AWS-LC-FIPS module, from the
/// validations AWS-LC lists in `crypto/fipsmodule/FIPS.md` for the static
/// library, or `None` for a module this build has no record of.
pub fn certification() -> Option<Certification> {
    match fips_module()? {
        4 => Some(Certification::InProcess),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails when aws-lc-rs moves to a module this crate has no
    /// certification record for, so the change cannot ship unrecorded.
    #[cfg(feature = "fips")]
    #[test]
    fn linked_module_has_a_certification_record() {
        assert!(
            certification().is_some(),
            "no certification record for AWS-LC-FIPS module {:?}",
            fips_module()
        );
    }

    #[cfg(not(feature = "fips"))]
    #[test]
    fn non_fips_build_reports_tls_not_approved() {
        install_crypto_provider();
        assert_eq!(tls_fips(), TlsFips::NotApproved);
    }
}
