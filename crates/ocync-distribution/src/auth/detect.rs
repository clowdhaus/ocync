//! Hostname-based registry provider detection.

use std::sync::LazyLock;

use regex_lite::Regex;

/// The kind of registry provider detected from a hostname.
///
/// A host matching none (Quay, Harbor, private registries) uses the standard
/// OCI token exchange via [`configured::Auth`](super::configured::Auth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// AWS Elastic Container Registry (private).
    Ecr,
    /// AWS ECR Public Gallery.
    EcrPublic,
    /// Google Container Registry (gcr.io and regional mirrors).
    Gcr,
    /// Google Artifact Registry (*-docker.pkg.dev).
    Gar,
    /// Azure Container Registry.
    Acr,
    /// GitHub Container Registry.
    Ghcr,
    /// Docker Hub.
    DockerHub,
    /// Chainguard Registry.
    Chainguard,
}

/// Compiled regex for ECR private registry hostnames.
///
/// Pattern: `<12-digit-account>.dkr[.-]ecr[-fips].<region>.<partition-domain>`
///
/// Supports all AWS partitions: standard, China (`.com.cn`), EU Sovereign
/// (`.eu`), ISO (`.c2s.ic.gov`), and ISOB (`.sc2s.sgov.gov`). The `[-.]`
/// separator between `dkr` and `ecr` handles both standard and dual-stack
/// endpoint formats.
///
/// Source: <https://docs.aws.amazon.com/AmazonECR/latest/userguide/Registries.html>
static ECR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[0-9]{12}\.dkr[-.]ecr(-fips)?\.[-a-z0-9]+\.(?:amazonaws\.com(?:\.cn)?|amazonaws\.eu|c2s\.ic\.gov|sc2s\.sgov\.gov)$",
    )
    .unwrap()
});

/// Compiled regex for Google Artifact Registry hostnames.
///
/// Pattern: `<region>-docker.pkg.dev`
///
/// Source: <https://cloud.google.com/artifact-registry/docs/repositories/repo-locations>
static GAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-a-z0-9]+-docker\.pkg\.dev$").unwrap());

/// Compiled regex for Azure Container Registry hostnames.
///
/// Pattern: `<name>.azurecr.<tld>`
///
/// Supports standard (`.io`), China (`.cn`), and US Government (`.us`) suffixes.
/// ACR registry names are alphanumeric only (no hyphens per Azure naming rules).
///
/// Source: <https://learn.microsoft.com/en-us/azure/container-registry/container-registry-faq>
static ACR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+\.azurecr\.(?:io|cn|us)$").unwrap());

/// The ECR Public registry hostname.
pub(crate) const ECR_PUBLIC_HOST: &str = "public.ecr.aws";

/// A hostname with its case, port and trailing dot removed, the form every
/// host comparison is made against.
pub(crate) fn normalize_host(hostname: &str) -> String {
    let hostname = hostname.to_ascii_lowercase();
    let hostname = hostname.split(':').next().unwrap();
    hostname.trim_end_matches('.').to_owned()
}

/// Detect the registry provider kind from a hostname, ignoring case, port
/// and a trailing dot. `None` for an unrecognized host.
pub fn detect_provider_kind(hostname: &str) -> Option<ProviderKind> {
    let hostname = normalize_host(hostname);
    let hostname = hostname.as_str();

    match hostname {
        ECR_PUBLIC_HOST => Some(ProviderKind::EcrPublic),
        "ghcr.io" => Some(ProviderKind::Ghcr),
        "cgr.dev" => Some(ProviderKind::Chainguard),
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => Some(ProviderKind::DockerHub),
        _ if hostname == "gcr.io" || hostname.ends_with(".gcr.io") => Some(ProviderKind::Gcr),
        _ if ECR_RE.is_match(hostname) => Some(ProviderKind::Ecr),
        _ if GAR_RE.is_match(hostname) => Some(ProviderKind::Gar),
        _ if ACR_RE.is_match(hostname) => Some(ProviderKind::Acr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each hostname maps to its provider, or to none. Breaks if a pattern
    /// stops matching its hosts, matches a spoofed or malformed one, or
    /// port, trailing-dot or case handling changes.
    #[test]
    fn detects_the_provider_by_hostname() {
        let cases = [
            // ECR
            (
                "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr.ecr-fips.us-east-1.amazonaws.com",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr-ecr.us-east-1.amazonaws.com",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr-ecr-fips.us-east-1.amazonaws.com",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr.ecr.eusc-de-east-1.amazonaws.eu",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr.ecr.us-iso-east-1.c2s.ic.gov",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr.ecr.us-isob-east-1.sc2s.sgov.gov",
                Some(ProviderKind::Ecr),
            ),
            (
                "123456789012.dkr.ecr.us-gov-west-1.amazonaws.com",
                Some(ProviderKind::Ecr),
            ),
            ("public.ecr.aws", Some(ProviderKind::EcrPublic)),
            ("12345.dkr.ecr.us-east-1.amazonaws.com", None),
            ("123456789012.ecr.us-east-1.amazonaws.com", None),
            (
                "123456789012.dkr.ecr.us-east-1.amazonaws.com.evil.com",
                None,
            ),
            // GCR / Artifact Registry
            ("gcr.io", Some(ProviderKind::Gcr)),
            ("us.gcr.io", Some(ProviderKind::Gcr)),
            ("eu.gcr.io", Some(ProviderKind::Gcr)),
            ("asia.gcr.io", Some(ProviderKind::Gcr)),
            ("mirror.gcr.io", Some(ProviderKind::Gcr)),
            ("k8s.gcr.io", Some(ProviderKind::Gcr)),
            ("us-central1-docker.pkg.dev", Some(ProviderKind::Gar)),
            ("europe-docker.pkg.dev", Some(ProviderKind::Gar)),
            ("asia-docker.pkg.dev", Some(ProviderKind::Gar)),
            ("us-docker.pkg.dev", Some(ProviderKind::Gar)),
            ("docker.pkg.dev", None),
            ("pkg.dev", None),
            // ACR
            ("myregistry.azurecr.io", Some(ProviderKind::Acr)),
            ("myregistry.azurecr.cn", Some(ProviderKind::Acr)),
            ("myregistry.azurecr.us", Some(ProviderKind::Acr)),
            // GHCR
            ("ghcr.io", Some(ProviderKind::Ghcr)),
            // Docker Hub
            ("docker.io", Some(ProviderKind::DockerHub)),
            ("index.docker.io", Some(ProviderKind::DockerHub)),
            ("registry-1.docker.io", Some(ProviderKind::DockerHub)),
            // Chainguard
            ("cgr.dev", Some(ProviderKind::Chainguard)),
            // Unknown / generic registries
            ("quay.io", None),
            ("harbor.example.com", None),
            ("my-private-registry.example.com", None),
            ("127.0.0.1", None),
            // Port and trailing dot stripping
            ("ghcr.io:443", Some(ProviderKind::Ghcr)),
            ("myregistry.azurecr.io:5000", Some(ProviderKind::Acr)),
            ("ghcr.io.", Some(ProviderKind::Ghcr)),
            ("gcr.io.", Some(ProviderKind::Gcr)),
            // Case insensitivity
            ("GHCR.IO", Some(ProviderKind::Ghcr)),
            ("Ghcr.Io", Some(ProviderKind::Ghcr)),
            ("GCR.IO", Some(ProviderKind::Gcr)),
            ("Docker.IO", Some(ProviderKind::DockerHub)),
            (
                "123456789012.DKR.ECR.US-EAST-1.AMAZONAWS.COM",
                Some(ProviderKind::Ecr),
            ),
            ("MyRegistry.AzureCR.IO", Some(ProviderKind::Acr)),
            ("US-Central1-Docker.PKG.DEV", Some(ProviderKind::Gar)),
            // Negative edge cases
            ("1234567890123.dkr.ecr.us-east-1.amazonaws.com", None),
            ("my-registry.azurecr.io", None),
            (".azurecr.io", None),
            ("myregistry.azurecr.io.evil.com", None),
            ("", None),
            (".", None),
            ("...", None),
            (" ", None),
        ];
        for (host, expected) in cases {
            assert_eq!(detect_provider_kind(host), expected, "{host}");
        }
    }
}
