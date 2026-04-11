//! Hostname-based registry provider detection.

use std::sync::LazyLock;

use regex::Regex;

/// The kind of registry provider detected from a hostname.
///
/// Not all variants have a corresponding [`AuthProvider`](super::AuthProvider)
/// implementation. Currently only [`Ecr`](Self::Ecr) has a full auth provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// AWS Elastic Container Registry (private).
    Ecr,
    /// AWS ECR Public Gallery.
    EcrPublic,
    /// Google Container Registry (gcr.io and regional mirrors) or Artifact Registry.
    Gcr,
    /// Azure Container Registry.
    Acr,
    /// GitHub Container Registry.
    Ghcr,
    /// Docker Hub.
    DockerHub,
    /// Chainguard Registry.
    Chainguard,
}

/// Known GCR hostnames (exact matches).
const GCR_HOSTS: &[&str] = &["gcr.io", "us.gcr.io", "eu.gcr.io", "asia.gcr.io"];

/// Known Docker Hub hostnames (exact matches).
const DOCKER_HUB_HOSTS: &[&str] = &["docker.io", "index.docker.io", "registry-1.docker.io"];

/// Compiled regex for ECR private registry hostnames.
///
/// Pattern: `<12-digit-account>.dkr[.-]ecr[-fips].<region>.<partition-domain>`
///
/// Supports all AWS partitions: standard, China (`.com.cn`), EU Sovereign
/// (`.eu`), ISO (`.c2s.ic.gov`), and ISOB (`.sc2s.sgov.gov`). The `[-.]`
/// separator between `dkr` and `ecr` handles both standard and dual-stack
/// endpoint formats.
static ECR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[0-9]{12}\.dkr[-.]ecr(-fips)?\.[-a-z0-9]+\.(?:amazonaws\.com(?:\.cn)?|amazonaws\.eu|c2s\.ic\.gov|sc2s\.sgov\.gov)$",
    )
    .unwrap()
});

/// Compiled regex for Google Artifact Registry hostnames.
///
/// Pattern: `<region>-docker.pkg.dev`
static GAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-a-z0-9]+-docker\.pkg\.dev$").unwrap());

/// Compiled regex for Azure Container Registry hostnames.
///
/// Pattern: `<name>.azurecr.<tld>`
///
/// Supports standard (`.io`), China (`.cn`), and US Government (`.us`) suffixes.
/// ACR registry names are alphanumeric only (no hyphens per Azure naming rules).
static ACR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+\.azurecr\.(?:io|cn|us)$").unwrap());

/// Detect the registry provider kind from a hostname.
///
/// Normalizes the hostname to lowercase before matching (DNS is case-insensitive).
/// Checks exact matches first (fast path), then falls through to regex
/// patterns for cloud registries that use dynamic hostnames.
///
/// The hostname must not include a port number or trailing dot. Callers
/// should strip these before calling (e.g. `ghcr.io:443` → `ghcr.io`).
///
/// Returns `None` if the hostname doesn't match any known provider.
pub fn detect_provider_kind(hostname: &str) -> Option<ProviderKind> {
    let hostname = hostname.to_ascii_lowercase();
    let hostname = hostname.as_str();

    // Exact matches (fast path).
    if hostname == "public.ecr.aws" {
        return Some(ProviderKind::EcrPublic);
    }
    if hostname == "ghcr.io" {
        return Some(ProviderKind::Ghcr);
    }
    if hostname == "cgr.dev" {
        return Some(ProviderKind::Chainguard);
    }
    if GCR_HOSTS.contains(&hostname) {
        return Some(ProviderKind::Gcr);
    }
    if DOCKER_HUB_HOSTS.contains(&hostname) {
        return Some(ProviderKind::DockerHub);
    }

    // Regex matches (dynamic hostnames).
    if ECR_RE.is_match(hostname) {
        return Some(ProviderKind::Ecr);
    }
    if GAR_RE.is_match(hostname) {
        return Some(ProviderKind::Gcr);
    }
    if ACR_RE.is_match(hostname) {
        return Some(ProviderKind::Acr);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ECR ---

    #[test]
    fn detect_ecr_standard() {
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_fips() {
        let host = "123456789012.dkr.ecr-fips.us-east-1.amazonaws.com";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_dual_stack() {
        let host = "123456789012.dkr-ecr.us-east-1.amazonaws.com";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_dual_stack_fips() {
        let host = "123456789012.dkr-ecr-fips.us-east-1.amazonaws.com";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_china() {
        let host = "123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_eu_sovereign() {
        let host = "123456789012.dkr.ecr.eusc-de-east-1.amazonaws.eu";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_iso() {
        let host = "123456789012.dkr.ecr.us-iso-east-1.c2s.ic.gov";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_isob() {
        let host = "123456789012.dkr.ecr.us-isob-east-1.sc2s.sgov.gov";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_govcloud() {
        let host = "123456789012.dkr.ecr.us-gov-west-1.amazonaws.com";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_ecr_public() {
        assert_eq!(
            detect_provider_kind("public.ecr.aws"),
            Some(ProviderKind::EcrPublic)
        );
    }

    #[test]
    fn detect_ecr_wrong_format() {
        // Too few digits in account ID.
        assert_eq!(
            detect_provider_kind("12345.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
        // Missing .dkr.ecr segment.
        assert_eq!(
            detect_provider_kind("123456789012.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    #[test]
    fn detect_ecr_rejects_spoofed_suffix() {
        assert_eq!(
            detect_provider_kind("123456789012.dkr.ecr.us-east-1.amazonaws.com.evil.com"),
            None
        );
    }

    // --- GCR / Artifact Registry ---

    #[test]
    fn detect_gcr_io() {
        assert_eq!(detect_provider_kind("gcr.io"), Some(ProviderKind::Gcr));
    }

    #[test]
    fn detect_gcr_us() {
        assert_eq!(detect_provider_kind("us.gcr.io"), Some(ProviderKind::Gcr));
    }

    #[test]
    fn detect_gcr_eu() {
        assert_eq!(detect_provider_kind("eu.gcr.io"), Some(ProviderKind::Gcr));
    }

    #[test]
    fn detect_gcr_asia() {
        assert_eq!(detect_provider_kind("asia.gcr.io"), Some(ProviderKind::Gcr));
    }

    #[test]
    fn detect_gar_us_central() {
        assert_eq!(
            detect_provider_kind("us-central1-docker.pkg.dev"),
            Some(ProviderKind::Gcr)
        );
    }

    #[test]
    fn detect_gar_europe() {
        assert_eq!(
            detect_provider_kind("europe-docker.pkg.dev"),
            Some(ProviderKind::Gcr)
        );
    }

    #[test]
    fn detect_gar_asia() {
        assert_eq!(
            detect_provider_kind("asia-docker.pkg.dev"),
            Some(ProviderKind::Gcr)
        );
    }

    // --- ACR ---

    #[test]
    fn detect_acr_standard() {
        assert_eq!(
            detect_provider_kind("myregistry.azurecr.io"),
            Some(ProviderKind::Acr)
        );
    }

    #[test]
    fn detect_acr_china() {
        assert_eq!(
            detect_provider_kind("myregistry.azurecr.cn"),
            Some(ProviderKind::Acr)
        );
    }

    #[test]
    fn detect_acr_gov() {
        assert_eq!(
            detect_provider_kind("myregistry.azurecr.us"),
            Some(ProviderKind::Acr)
        );
    }

    // --- GHCR ---

    #[test]
    fn detect_ghcr() {
        assert_eq!(detect_provider_kind("ghcr.io"), Some(ProviderKind::Ghcr));
    }

    // --- Docker Hub ---

    #[test]
    fn detect_docker_hub() {
        assert_eq!(
            detect_provider_kind("docker.io"),
            Some(ProviderKind::DockerHub)
        );
        assert_eq!(
            detect_provider_kind("index.docker.io"),
            Some(ProviderKind::DockerHub)
        );
        assert_eq!(
            detect_provider_kind("registry-1.docker.io"),
            Some(ProviderKind::DockerHub)
        );
    }

    // --- Chainguard ---

    #[test]
    fn detect_chainguard() {
        assert_eq!(
            detect_provider_kind("cgr.dev"),
            Some(ProviderKind::Chainguard)
        );
    }

    // --- Unknown ---

    #[test]
    fn detect_unknown() {
        assert_eq!(detect_provider_kind("quay.io"), None);
        assert_eq!(
            detect_provider_kind("my-private-registry.example.com"),
            None
        );
    }

    // --- Case insensitivity ---

    #[test]
    fn detect_case_insensitive_ghcr() {
        assert_eq!(detect_provider_kind("GHCR.IO"), Some(ProviderKind::Ghcr));
        assert_eq!(detect_provider_kind("Ghcr.Io"), Some(ProviderKind::Ghcr));
    }

    #[test]
    fn detect_case_insensitive_gcr() {
        assert_eq!(detect_provider_kind("GCR.IO"), Some(ProviderKind::Gcr));
    }

    #[test]
    fn detect_case_insensitive_docker_hub() {
        assert_eq!(
            detect_provider_kind("Docker.IO"),
            Some(ProviderKind::DockerHub)
        );
    }

    #[test]
    fn detect_case_insensitive_ecr() {
        let host = "123456789012.DKR.ECR.US-EAST-1.AMAZONAWS.COM";
        assert_eq!(detect_provider_kind(host), Some(ProviderKind::Ecr));
    }

    #[test]
    fn detect_case_insensitive_acr() {
        assert_eq!(
            detect_provider_kind("MyRegistry.AzureCR.IO"),
            Some(ProviderKind::Acr)
        );
    }

    #[test]
    fn detect_case_insensitive_gar() {
        assert_eq!(
            detect_provider_kind("US-Central1-Docker.PKG.DEV"),
            Some(ProviderKind::Gcr)
        );
    }

    // --- Negative edge cases ---

    #[test]
    fn detect_ecr_rejects_thirteen_digit_account() {
        assert_eq!(
            detect_provider_kind("1234567890123.dkr.ecr.us-east-1.amazonaws.com"),
            None
        );
    }

    #[test]
    fn detect_acr_rejects_hyphens_in_name() {
        // Azure requires alphanumeric-only registry names.
        assert_eq!(detect_provider_kind("my-registry.azurecr.io"), None);
    }

    #[test]
    fn detect_empty_and_degenerate_inputs() {
        assert_eq!(detect_provider_kind(""), None);
        assert_eq!(detect_provider_kind("."), None);
        assert_eq!(detect_provider_kind("..."), None);
        assert_eq!(detect_provider_kind(" "), None);
    }
}
