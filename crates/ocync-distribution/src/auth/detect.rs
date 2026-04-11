use std::sync::LazyLock;

use regex::Regex;

/// The kind of registry provider detected from a hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// AWS Elastic Container Registry (private).
    Ecr,
    /// AWS ECR Public Gallery.
    EcrPublic,
    /// Google Container Registry (gcr.io and regional mirrors).
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
const DOCKER_HUB_HOSTS: &[&str] = &["docker.io", "registry-1.docker.io"];

/// Compiled regex for ECR private registry hostnames.
///
/// Pattern: `<12-digit-account>.dkr.ecr[-fips].<region>.amazonaws.com`
static ECR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]{12}\.dkr\.ecr(-fips)?\.[-a-z0-9]+\.amazonaws\.com$").unwrap()
});

/// Compiled regex for Google Artifact Registry hostnames.
///
/// Pattern: `<region>-docker.pkg.dev`
static GAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-a-z0-9]+-docker\.pkg\.dev$").unwrap());

/// Compiled regex for Azure Container Registry hostnames.
///
/// Pattern: `<name>.azurecr.io`
static ACR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]+\.azurecr\.io$").unwrap());

/// Detect the registry provider kind from a hostname.
///
/// Checks exact matches first (fast path), then falls through to regex
/// patterns for cloud registries that use dynamic hostnames.
///
/// Returns `None` if the hostname doesn't match any known provider.
pub fn detect_provider_kind(hostname: &str) -> Option<ProviderKind> {
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
    fn detect_ecr_public() {
        assert_eq!(
            detect_provider_kind("public.ecr.aws"),
            Some(ProviderKind::EcrPublic)
        );
    }

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
    fn detect_gar() {
        assert_eq!(
            detect_provider_kind("us-central1-docker.pkg.dev"),
            Some(ProviderKind::Gcr)
        );
    }

    #[test]
    fn detect_acr() {
        assert_eq!(
            detect_provider_kind("myregistry.azurecr.io"),
            Some(ProviderKind::Acr)
        );
    }

    #[test]
    fn detect_ghcr() {
        assert_eq!(detect_provider_kind("ghcr.io"), Some(ProviderKind::Ghcr));
    }

    #[test]
    fn detect_docker_hub() {
        assert_eq!(
            detect_provider_kind("docker.io"),
            Some(ProviderKind::DockerHub)
        );
        assert_eq!(
            detect_provider_kind("registry-1.docker.io"),
            Some(ProviderKind::DockerHub)
        );
    }

    #[test]
    fn detect_chainguard() {
        assert_eq!(
            detect_provider_kind("cgr.dev"),
            Some(ProviderKind::Chainguard)
        );
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(detect_provider_kind("quay.io"), None);
        assert_eq!(
            detect_provider_kind("my-private-registry.example.com"),
            None
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
}
