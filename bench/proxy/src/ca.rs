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

/// CA validity. Long, because benchmark instances are short-lived.
const CA_VALIDITY_DAYS: i64 = 365 * 10;

/// Leaf certificate validity.
const LEAF_VALIDITY_DAYS: i64 = 365;

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
    /// SNI hostname could not be parsed as a valid DNS name for leaf
    /// certificate generation.
    #[error("invalid SNI hostname {host}: {reason}")]
    InvalidSni {
        /// The SNI value that failed parsing.
        host: String,
        /// Human-readable parse failure reason.
        reason: String,
    },
    /// rustls signer construction failed (unsupported key type).
    #[error("rustls signer: {0}")]
    Signer(String),
}

/// A parsed CA keypair ready to sign leaf certificates.
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
    /// Synchronous because rustls's `ResolvesServerCert::resolve` is sync and
    /// runs on the connection-driving task, so a `tokio::sync::RwLock` would
    /// panic from `block_on` inside the runtime. Leaf generation (ECDSA keygen
    /// plus one CA sign) takes ~1 ms; a cache hit takes only a read lock.
    pub(crate) fn leaf_for(&self, sni_host: &str) -> Result<Arc<CertifiedKey>, Error> {
        if let Ok(guard) = self.leaf_cache.read() {
            if let Some(cached) = guard.get(sni_host) {
                return Ok(cached.clone());
            }
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
    (params.not_before, params.not_after) = validity_window(CA_VALIDITY_DAYS);

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Write the CA private key so no other user can read it at any point: a new
/// file is created owner-only, and an existing one is restricted before the
/// key goes into it.
pub(crate) async fn write_private_key(path: &Path, key_pem: &str) -> Result<(), Error> {
    use tokio::io::AsyncWriteExt;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    file.write_all(key_pem.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

/// Load a CA keypair from PEM files on disk.
pub(crate) async fn load_ca(cert_path: &Path, key_path: &Path) -> Result<CaSigner, Error> {
    let cert_pem = tokio::fs::read_to_string(cert_path).await?;
    let key_pem = tokio::fs::read_to_string(key_path).await?;

    let pem_error = |path: &Path| {
        let path = path.display().to_string();
        move |e: rcgen::Error| Error::Pem {
            path,
            reason: e.to_string(),
        }
    };
    let key = KeyPair::from_pem(&key_pem).map_err(pem_error(key_path))?;
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, key).map_err(pem_error(cert_path))?;

    Ok(CaSigner {
        issuer,
        leaf_cache: RwLock::new(HashMap::new()),
    })
}

/// Generate a new leaf certificate for `sni_host`, signed by `issuer`.
fn generate_leaf(issuer: &Issuer<'static, KeyPair>, sni_host: &str) -> Result<CertifiedKey, Error> {
    let san = match Ia5String::try_from(sni_host.to_owned()) {
        Ok(s) => SanType::DnsName(s),
        // IP addresses are not supported as SANs; return an error.
        // Registries always connect via DNS hostnames in practice.
        Err(_) => {
            return Err(Error::InvalidSni {
                host: sni_host.to_owned(),
                reason: "SNI host is not a valid IA5 DNS name".into(),
            });
        }
    };

    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, sni_host);
    params.subject_alt_names = vec![san];
    (params.not_before, params.not_after) = validity_window(LEAF_VALIDITY_DAYS);
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

/// A validity window of `days`, starting at ~yesterday UTC.
fn validity_window(days: i64) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (
        now - time::Duration::days(1),
        now + time::Duration::days(days),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn private_key_is_owner_only_whether_or_not_the_file_existed() {
        use std::os::unix::fs::PermissionsExt;

        for (name, existing) in [("new file", None), ("world-readable file", Some(0o644))] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ca.key");
            if let Some(mode) = existing {
                std::fs::write(&path, "old key").unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            write_private_key(&path, "new key").await.unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{name}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "new key", "{name}");
        }
    }

    #[tokio::test]
    async fn load_ca_names_the_file_that_is_not_pem() {
        let (cert_pem, key_pem) = generate_ca("test CA").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cert_path = tmp.path().join("ca.pem");
        let key_path = tmp.path().join("ca-key.pem");
        tokio::fs::write(&cert_path, &cert_pem).await.unwrap();
        tokio::fs::write(&key_path, &key_pem).await.unwrap();

        let garbage = tmp.path().join("garbage.pem");
        tokio::fs::write(&garbage, "not pem").await.unwrap();

        for (name, cert, key, offender) in [
            ("bad key", &cert_path, &garbage, &garbage),
            ("bad cert", &garbage, &key_path, &garbage),
        ] {
            match load_ca(cert, key).await {
                Err(Error::Pem { path, .. }) => {
                    assert_eq!(path, offender.display().to_string(), "{name}");
                }
                other => panic!("{name}: expected Error::Pem, got {other:?}"),
            }
        }
    }
}
