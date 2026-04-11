//! Hostname-based registry provider detection.

use std::sync::LazyLock;

use regex_lite::Regex;

/// The kind of registry provider detected from a hostname.
///
/// Not all variants have a corresponding [`AuthProvider`](super::AuthProvider)
/// implementation. Currently only [`Ecr`](Self::Ecr) has a full auth provider.
/// Hostnames that don't match any known provider return `None` from
/// [`detect_provider_kind`] — these registries (e.g. Quay, Harbor, private
/// registries) use the standard OCI token-exchange flow via
/// [`AnonymousAuth`](super::anonymous::AnonymousAuth) or Docker config
/// credentials.
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

/// Detect the registry provider kind from a hostname.
///
/// Normalizes the hostname to lowercase, strips any port number and trailing
/// dot before matching. Returns `None` for unrecognized hostnames (e.g. Quay,
/// Harbor, private registries) — these should fall through to the standard OCI
/// token-exchange flow or Docker config credential resolution.
pub fn detect_provider_kind(hostname: &str) -> Option<ProviderKind> {
    let hostname = hostname.to_ascii_lowercase();
    let hostname = hostname.split(':').next().unwrap();
    let hostname = hostname.trim_end_matches('.');

    match hostname {
        "public.ecr.aws" => Some(ProviderKind::EcrPublic),
        "ghcr.io" => Some(ProviderKind::Ghcr),
        "cgr.dev" => Some(ProviderKind::Chainguard),
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => Some(ProviderKind::DockerHub),
        _ if hostname == "gcr.io" || hostname.ends_with(".gcr.io") => Some(ProviderKind::Gcr),
        _ if ECR_RE.is_match(hostname) => Some(ProviderKind::Ecr),
        _ if GAR_RE.is_match(hostname) => Some(ProviderKind::Gcr),
        _ if ACR_RE.is_match(hostname) => Some(ProviderKind::Acr),
        _ => None,
    }
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
    fn detect_gcr_mirror() {
        assert_eq!(
            detect_provider_kind("mirror.gcr.io"),
            Some(ProviderKind::Gcr)
        );
    }

    #[test]
    fn detect_gcr_k8s() {
        assert_eq!(detect_provider_kind("k8s.gcr.io"), Some(ProviderKind::Gcr));
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

    // --- Unknown / generic registries ---

    #[test]
    fn detect_unknown_returns_none() {
        assert_eq!(detect_provider_kind("quay.io"), None);
        assert_eq!(detect_provider_kind("harbor.example.com"), None);
        assert_eq!(
            detect_provider_kind("my-private-registry.example.com"),
            None
        );
    }

    // --- Port and trailing dot stripping ---

    #[test]
    fn detect_strips_port() {
        assert_eq!(
            detect_provider_kind("ghcr.io:443"),
            Some(ProviderKind::Ghcr)
        );
        assert_eq!(
            detect_provider_kind("myregistry.azurecr.io:5000"),
            Some(ProviderKind::Acr)
        );
    }

    #[test]
    fn detect_strips_trailing_dot() {
        assert_eq!(detect_provider_kind("ghcr.io."), Some(ProviderKind::Ghcr));
        assert_eq!(detect_provider_kind("gcr.io."), Some(ProviderKind::Gcr));
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
        assert_eq!(detect_provider_kind("my-registry.azurecr.io"), None);
    }

    #[test]
    fn detect_acr_rejects_empty_name() {
        assert_eq!(detect_provider_kind(".azurecr.io"), None);
    }

    #[test]
    fn detect_empty_and_degenerate_inputs() {
        assert_eq!(detect_provider_kind(""), None);
        assert_eq!(detect_provider_kind("."), None);
        assert_eq!(detect_provider_kind("..."), None);
        assert_eq!(detect_provider_kind(" "), None);
    }
}
