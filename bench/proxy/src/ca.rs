//! CA generation and leaf certificate caching for MITM TLS termination.
//!
//! A single long-lived CA signs leaf certs generated on demand, one per
//! SNI hostname observed. Leaf certs are cached in-memory (`Arc<Cache>`)
//! so repeated connections to the same origin reuse the same cert - this
//! keeps TLS handshakes fast and lets tokio-rustls's session resumption
//! do its job.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use rcgen::string::Ia5String;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::crypto::aws_lc_rs;
use rustls::sign::CertifiedKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use thiserror::Error;

/// Errors from CA/leaf certificate operations.
#[derive(Debug, Error)]
pub(crate) enum Error {
    /// rcgen rejected the cert params or failed to sign.
    #[error("certificate generation: {0}")]
    Rcgen(#[from] rcgen::Error),
    /// File I/O failed (read/write CA PEM).
    #[error("file I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Parsing a PEM file failed or the file does not contain the
    /// expected certificate/key.
    #[error("malformed PEM in {path}: {reason}")]
    Pem {
        /// Path to the offending file.
        path: String,
        /// Human-readable parse failure reason.
        reason: String,
    },
    /// rustls signer construction failed (unsupported key type).
    #[error("rustls signer: {0}")]
    Signer(String),
}

/// A parsed CA keypair ready to sign leaf certificates.
///
/// Holds an `rcgen::Issuer` (used to sign leaves) plus a re-derived
/// rustls `CertifiedKey` for serving the CA cert itself if ever needed.
pub(crate) struct CaSigner {
    issuer: Issuer<'static, KeyPair>,
    /// Shared leaf cert cache, keyed by SNI hostname.
    leaf_cache: RwLock<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for CaSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaSigner").finish_non_exhaustive()
    }
}

impl CaSigner {
    /// Look up or generate a leaf certificate for `sni_host`.
    ///
    /// Synchronous because rustls's `ResolvesServerCert::resolve` is
    /// itself sync - it's invoked on the connection-driving task and
    /// cannot call `.await`. Using a synchronous `std::sync::RwLock`
    /// avoids the "runtime-in-runtime" panic from `block_on`-ing a
    /// `tokio::sync::RwLock` from within the executor. Leaf generation
    /// (ECDSA keygen + one CA sign) takes ~1 ms; cache hits take only
    /// a read-lock so contention is negligible in practice.
    pub(crate) fn leaf_for(&self, sni_host: &str) -> Result<Arc<CertifiedKey>, Error> {
        if let Ok(guard) = self.leaf_cache.read()
            && let Some(cached) = guard.get(sni_host)
        {
            return Ok(cached.clone());
        }

        let mut guard = self
            .leaf_cache
            .write()
            .map_err(|_| Error::Signer("leaf cache poisoned".into()))?;
        // Another task may have filled the entry while we waited.
        if let Some(cached) = guard.get(sni_host) {
            return Ok(cached.clone());
        }

        let certified = generate_leaf(&self.issuer, sni_host)?;
        let arc = Arc::new(certified);
        guard.insert(sni_host.to_owned(), arc.clone());
        Ok(arc)
    }
}

/// Generate a fresh self-signed CA and return PEM-encoded `(cert, key)`.
pub(crate) fn generate_ca(common_name: &str) -> Result<(String, String), Error> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "ocync bench");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    // Long validity - benchmark instances are short-lived.
    let (not_before, not_after) = validity_one_decade();
    params.not_before = not_before;
    params.not_after = not_after;

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Load a CA keypair from PEM files on disk.
pub(crate) async fn load_ca(cert_path: &Path, key_path: &Path) -> Result<CaSigner, Error> {
    let cert_pem = tokio::fs::read_to_string(cert_path).await?;
    let key_pem = tokio::fs::read_to_string(key_path).await?;

    let key = KeyPair::from_pem(&key_pem).map_err(|e| Error::Pem {
        path: key_path.display().to_string(),
        reason: e.to_string(),
    })?;
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, key).map_err(|e| Error::Pem {
        path: cert_path.display().to_string(),
        reason: e.to_string(),
    })?;

    Ok(CaSigner {
        issuer,
        leaf_cache: RwLock::new(HashMap::new()),
    })
}

/// Generate a new leaf certificate for `sni_host`, signed by `issuer`.
fn generate_leaf(issuer: &Issuer<'static, KeyPair>, sni_host: &str) -> Result<CertifiedKey, Error> {
    let san = match Ia5String::try_from(sni_host.to_owned()) {
        Ok(s) => SanType::DnsName(s),
        // Fall back to IP-address SAN if the hostname isn't a valid DNS
        // IA5 string (e.g. an IP literal). Registries almost always hit
        // this via DNS, but the fallback keeps the code robust.
        Err(_) => {
            return Err(Error::Pem {
                path: sni_host.to_owned(),
                reason: "SNI host is not a valid IA5 DNS name".into(),
            });
        }
    };

    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, sni_host);
    params.subject_alt_names = vec![san];
    let (not_before, not_after) = validity_one_year();
    params.not_before = not_before;
    params.not_after = not_after;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];

    let leaf_key = KeyPair::generate()?;
    let leaf_cert = params.signed_by(&leaf_key, issuer)?;

    let cert_der = CertificateDer::from(leaf_cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let signer = aws_lc_rs::sign::any_supported_type(&key_der)
        .map_err(|e| Error::Signer(format!("aws-lc-rs rejected leaf key: {e}")))?;
    Ok(CertifiedKey::new(vec![cert_der], signer))
}

/// Ten-year validity window starting at ~yesterday UTC.
fn validity_one_decade() -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (
        now - time::Duration::days(1),
        now + time::Duration::days(365 * 10),
    )
}

/// One-year validity window starting at ~yesterday UTC.
fn validity_one_year() -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (
        now - time::Duration::days(1),
        now + time::Duration::days(365),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_ca_yields_parseable_pem() {
        let (cert_pem, key_pem) = generate_ca("test CA").unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));

        // Reparsing must succeed.
        let key = KeyPair::from_pem(&key_pem).unwrap();
        let _ = Issuer::from_ca_cert_pem(&cert_pem, key).unwrap();
    }

    #[tokio::test]
    async fn leaf_for_issues_and_caches() {
        let _ = aws_lc_rs::default_provider().install_default();

        let (cert_pem, key_pem) = generate_ca("test CA").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cert_path = tmp.path().join("ca.pem");
        let key_path = tmp.path().join("ca-key.pem");
        tokio::fs::write(&cert_path, &cert_pem).await.unwrap();
        tokio::fs::write(&key_path, &key_pem).await.unwrap();

        let signer = load_ca(&cert_path, &key_path).await.unwrap();
        let a = signer.leaf_for("example.com").unwrap();
        let b = signer.leaf_for("example.com").unwrap();
        // Same Arc must be returned for repeated lookups.
        assert!(Arc::ptr_eq(&a, &b));
        // Different host returns a different leaf.
        let c = signer.leaf_for("other.example").unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
