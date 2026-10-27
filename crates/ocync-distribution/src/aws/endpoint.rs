//! Endpoint resolution for the AWS APIs ocync calls, matching the endpoint
//! rules the AWS SDK generates for ECR, ECR Public, STS, SSO and SSO OIDC.

use std::sync::LazyLock;

use regex_lite::Regex;
use url::Url;

use super::config::ServiceId;
use super::{ApiId, ConfigError, ErrorFormat, Service};
use crate::scheme::Scheme;

/// The longest label a host name may hold.
const MAX_LABEL_LEN: usize = 63;

/// An AWS region, such as `us-east-1`.
///
/// It is spliced into endpoint host names and signatures, so it must be a
/// valid host label, dots allowed, as the SDK's endpoint rules require.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Region(String);

/// A region that is not a valid host label.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid AWS region '{0}': must be a valid host label")]
pub(crate) struct InvalidRegion(String);

impl Region {
    /// Read `text` as a region.
    pub(super) fn parse(text: &str) -> Result<Self, InvalidRegion> {
        let valid_label = |label: &str| {
            (1..=MAX_LABEL_LEN).contains(&label.len())
                && !label.starts_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        if text.split('.').all(valid_label) {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidRegion(text.to_owned()))
        }
    }

    /// The region as text.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The AWS partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartitionId {
    Aws,
    AwsCn,
    AwsEusc,
    AwsIso,
    AwsIsoB,
    AwsIsoE,
    AwsIsoF,
    AwsUsGov,
}

/// An AWS partition's DNS names, from the SDK's partition table.
struct Partition {
    id: PartitionId,
    region_regex: &'static str,
    dns_suffix: &'static str,
    dual_stack_dns_suffix: &'static str,
}

/// Every partition supports FIPS and dual-stack endpoints, so neither is
/// recorded. A region no pattern matches falls back to `aws`, the first.
const PARTITIONS: &[Partition] = &[
    Partition {
        id: PartitionId::Aws,
        region_regex: r"^(us|eu|ap|sa|ca|me|af|il|mx)\-\w+\-\d+$",
        dns_suffix: "amazonaws.com",
        dual_stack_dns_suffix: "api.aws",
    },
    Partition {
        id: PartitionId::AwsCn,
        region_regex: r"^cn\-\w+\-\d+$",
        dns_suffix: "amazonaws.com.cn",
        dual_stack_dns_suffix: "api.amazonwebservices.com.cn",
    },
    Partition {
        id: PartitionId::AwsEusc,
        region_regex: r"^eusc\-(de)\-\w+\-\d+$",
        dns_suffix: "amazonaws.eu",
        dual_stack_dns_suffix: "api.amazonwebservices.eu",
    },
    Partition {
        id: PartitionId::AwsIso,
        region_regex: r"^us\-iso\-\w+\-\d+$",
        dns_suffix: "c2s.ic.gov",
        dual_stack_dns_suffix: "api.aws.ic.gov",
    },
    Partition {
        id: PartitionId::AwsIsoB,
        region_regex: r"^us\-isob\-\w+\-\d+$",
        dns_suffix: "sc2s.sgov.gov",
        dual_stack_dns_suffix: "api.aws.scloud",
    },
    Partition {
        id: PartitionId::AwsIsoE,
        region_regex: r"^eu\-isoe\-\w+\-\d+$",
        dns_suffix: "cloud.adc-e.uk",
        dual_stack_dns_suffix: "api.cloud-aws.adc-e.uk",
    },
    Partition {
        id: PartitionId::AwsIsoF,
        region_regex: r"^us\-isof\-\w+\-\d+$",
        dns_suffix: "csp.hci.ic.gov",
        dual_stack_dns_suffix: "api.aws.hci.ic.gov",
    },
    Partition {
        id: PartitionId::AwsUsGov,
        region_regex: r"^us\-gov\-\w+\-\d+$",
        dns_suffix: "amazonaws.com",
        dual_stack_dns_suffix: "api.aws",
    },
];

/// [`PARTITIONS`]' region patterns, compiled once, in the same order.
static REGION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    PARTITIONS
        .iter()
        .map(|p| Regex::new(p.region_regex).expect("partition patterns are valid"))
        .collect()
});

fn partition(region: &Region) -> &'static Partition {
    REGION_PATTERNS
        .iter()
        .position(|pattern| pattern.is_match(region.as_str()))
        .map_or(&PARTITIONS[0], |i| &PARTITIONS[i])
}

/// An HTTP or HTTPS endpoint, with the `host` header a signature names.
#[derive(Clone)]
pub(super) struct Endpoint {
    url: Url,
    /// The host, and the port when it is not the scheme's default.
    host: String,
}

/// Only the host: a configured endpoint's path may carry a key.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Endpoint").field(&self.host).finish()
    }
}

impl Endpoint {
    /// Parse an endpoint URL from configuration or from the rules below.
    ///
    /// Requests go to the endpoint's path joined with the operation path
    /// `/`, so `https://gw.example/ecr` is called at `/ecr/`. A query and a
    /// user and password are dropped with a warning, as the SDK sends
    /// neither. The URL may carry a key, so it never appears in a log or
    /// error.
    pub(super) fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut url = Url::parse(text).map_err(ConfigError::InvalidEndpoint)?;
        Scheme::try_from(&url).map_err(ConfigError::EndpointNotHttp)?;
        if url.query().is_some() || !url.username().is_empty() || url.password().is_some() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    "the query and user info in the configured AWS endpoint URL are ignored"
                );
            });
        }
        url.set_query(None);
        // Neither fails on an http or https URL, which has a host.
        let _ = url.set_username("");
        let _ = url.set_password(None);
        let path = format!("{}/", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let host = url
            .host_str()
            .expect("an http or https URL has a host")
            .to_owned();
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        Ok(Self { url, host })
    }

    /// The URL requests are sent to.
    pub(super) fn url(&self) -> &Url {
        &self.url
    }

    /// The value of the signed `host` header.
    pub(super) fn host(&self) -> &str {
        &self.host
    }
}

/// Whether to use a FIPS 140 endpoint (`AWS_USE_FIPS_ENDPOINT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fips {
    /// Use the FIPS endpoint.
    On,
    /// Use the standard endpoint.
    Off,
}

/// Whether to use an IPv4 and IPv6 endpoint (`AWS_USE_DUALSTACK_ENDPOINT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DualStack {
    /// Use the dual-stack endpoint.
    On,
    /// Use the IPv4 endpoint.
    Off,
}

/// Resolve the endpoint URL for `service` in `region`. A configured
/// `endpoint_url` wins, and cannot be combined with FIPS or dual-stack.
pub(super) fn resolve(
    service: Service,
    region: &Region,
    fips: Fips,
    dual_stack: DualStack,
    endpoint_url: Option<Endpoint>,
) -> Result<Endpoint, ConfigError> {
    if let Some(url) = endpoint_url {
        return configured(url, fips, dual_stack);
    }
    let p = partition(region);
    let (dns, dual) = (p.dns_suffix, p.dual_stack_dns_suffix);
    Endpoint::parse(&match (service, fips, dual_stack) {
        (Service::Ecr, Fips::On, DualStack::On) => {
            format!("https://ecr-fips.{region}.{dual}")
        }
        (Service::Ecr, Fips::On, DualStack::Off) => {
            format!("https://api.ecr-fips.{region}.{dns}")
        }
        (Service::Ecr, Fips::Off, DualStack::On) => {
            format!("https://ecr.{region}.{dual}")
        }
        (Service::Ecr, Fips::Off, DualStack::Off) => {
            format!("https://api.ecr.{region}.{dns}")
        }
        (Service::EcrPublic, Fips::On, DualStack::On) => {
            format!("https://api.ecr-public-fips.{region}.{dual}")
        }
        (Service::EcrPublic, Fips::On, DualStack::Off) => {
            format!("https://api.ecr-public-fips.{region}.{dns}")
        }
        (Service::EcrPublic, Fips::Off, DualStack::On) if p.id == PartitionId::Aws => {
            format!("https://ecr-public.{region}.api.aws")
        }
        (Service::EcrPublic, Fips::Off, DualStack::On) => {
            format!("https://api.ecr-public.{region}.{dual}")
        }
        (Service::EcrPublic, Fips::Off, DualStack::Off) => {
            format!("https://api.ecr-public.{region}.{dns}")
        }
    })
}

/// A configured endpoint URL, which cannot be combined with FIPS or
/// dual-stack.
fn configured(url: Endpoint, fips: Fips, dual_stack: DualStack) -> Result<Endpoint, ConfigError> {
    match (fips, dual_stack) {
        (Fips::On, _) => Err(ConfigError::FipsWithEndpointUrl),
        (_, DualStack::On) => Err(ConfigError::DualStackWithEndpointUrl),
        (Fips::Off, DualStack::Off) => Ok(url),
    }
}

/// The AWS APIs the credential chain calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialApi {
    /// AWS STS, to assume a role.
    Sts,
    /// The IAM Identity Center portal, for a role's credentials.
    Sso,
    /// IAM Identity Center OIDC, to refresh a login token.
    SsoOidc,
}

impl CredentialApi {
    /// How calls to the API are signed, partitioned and read.
    pub(super) fn api_id(self) -> ApiId {
        let (service_id, signing_name, error_format) = match self {
            Self::Sts => (ServiceId::STS, "sts", ErrorFormat::Xml),
            Self::Sso => (ServiceId::SSO, "awsssoportal", ErrorFormat::Json),
            Self::SsoOidc => (ServiceId::SSO_OIDC, "sso-oauth", ErrorFormat::Json),
        };
        ApiId {
            service_id,
            signing_name,
            error_format,
        }
    }

    /// The host name before the region.
    fn host_label(self) -> &'static str {
        match self {
            Self::Sts => "sts",
            Self::Sso => "portal.sso",
            Self::SsoOidc => "oidc",
        }
    }
}

/// Resolve the endpoint URL for `api` in `region`, as the SDK's endpoint
/// rules for STS, SSO and SSO OIDC do. STS's global endpoint is never
/// chosen: the region is always a registry's own.
pub(super) fn resolve_credential_api(
    api: CredentialApi,
    region: &Region,
    fips: Fips,
    dual_stack: DualStack,
    endpoint_url: Option<Endpoint>,
) -> Result<Endpoint, ConfigError> {
    if let Some(url) = endpoint_url {
        return configured(url, fips, dual_stack);
    }
    let p = partition(region);
    let (label, dns, dual) = (api.host_label(), p.dns_suffix, p.dual_stack_dns_suffix);
    Endpoint::parse(&match (fips, dual_stack) {
        (Fips::On, DualStack::On) => format!("https://{label}-fips.{region}.{dual}"),
        // GovCloud's standard endpoints are already FIPS endpoints.
        (Fips::On, DualStack::Off) if p.id == PartitionId::AwsUsGov => {
            format!("https://{label}.{region}.{dns}")
        }
        (Fips::On, DualStack::Off) => format!("https://{label}-fips.{region}.{dns}"),
        (Fips::Off, DualStack::On) => format!("https://{label}.{region}.{dual}"),
        (Fips::Off, DualStack::Off) => format!("https://{label}.{region}.{dns}"),
    })
}

/// `path` joined to `endpoint`'s own path with one slash, and the
/// endpoint's query dropped, as the SDK joins an operation's path to every
/// endpoint, the instance metadata one included.
pub(super) fn join_path(endpoint: &Url, path: &str) -> Url {
    let mut url = endpoint.clone();
    url.set_query(None);
    let base = endpoint.path();
    url.set_path(&format!(
        "{}/{}",
        base.strip_suffix('/').unwrap_or(base),
        path.strip_prefix('/').unwrap_or(path)
    ));
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One slash joins the paths whatever each side ends or starts with,
    /// and an endpoint's query is dropped, as in the SDK's `merge_paths`.
    /// Breaks if a slash is lost or doubled, or the query is sent.
    #[test]
    fn joins_paths_as_the_sdk_does() {
        for (endpoint, path, joined) in [
            ("https://h", "/op", "https://h/op"),
            ("https://h/", "op", "https://h/op"),
            ("https://h/base", "/op", "https://h/base/op"),
            ("https://h/base/", "/op", "https://h/base/op"),
            ("https://h/base//", "/op", "https://h/base//op"),
            ("https://h/base?k=v", "/op", "https://h/base/op"),
        ] {
            let url = join_path(&Url::parse(endpoint).unwrap(), path);
            assert_eq!(url.as_str(), joined, "{endpoint} + {path}");
        }
    }

    /// The endpoint rule tests generated into `aws-sdk-ecr` 1.135.0 and
    /// `aws-sdk-ecrpublic` 1.120.0, less the "Missing Region" cases, which
    /// cannot arise: the region is always parsed from the registry hostname
    /// or fixed.
    #[test]
    fn matches_the_sdk_endpoint_rule_tests() {
        use DualStack as D;
        use Fips as F;
        use Service::{Ecr, EcrPublic};
        let cases: &[(Service, &str, Fips, DualStack, &str)] = &[
            (
                Ecr,
                "us-east-1",
                F::On,
                D::On,
                "https://ecr-fips.us-east-1.api.aws",
            ),
            (
                Ecr,
                "us-east-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.us-east-1.amazonaws.com",
            ),
            (
                Ecr,
                "us-east-1",
                F::Off,
                D::On,
                "https://ecr.us-east-1.api.aws",
            ),
            (
                Ecr,
                "us-east-1",
                F::Off,
                D::Off,
                "https://api.ecr.us-east-1.amazonaws.com",
            ),
            (
                Ecr,
                "cn-northwest-1",
                F::On,
                D::On,
                "https://ecr-fips.cn-northwest-1.api.amazonwebservices.com.cn",
            ),
            (
                Ecr,
                "cn-northwest-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.cn-northwest-1.amazonaws.com.cn",
            ),
            (
                Ecr,
                "cn-northwest-1",
                F::Off,
                D::On,
                "https://ecr.cn-northwest-1.api.amazonwebservices.com.cn",
            ),
            (
                Ecr,
                "cn-northwest-1",
                F::Off,
                D::Off,
                "https://api.ecr.cn-northwest-1.amazonaws.com.cn",
            ),
            (
                Ecr,
                "eusc-de-east-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.eusc-de-east-1.amazonaws.eu",
            ),
            (
                Ecr,
                "eusc-de-east-1",
                F::Off,
                D::Off,
                "https://api.ecr.eusc-de-east-1.amazonaws.eu",
            ),
            (
                Ecr,
                "us-iso-east-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.us-iso-east-1.c2s.ic.gov",
            ),
            (
                Ecr,
                "us-iso-east-1",
                F::Off,
                D::Off,
                "https://api.ecr.us-iso-east-1.c2s.ic.gov",
            ),
            (
                Ecr,
                "us-isob-east-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                Ecr,
                "us-isob-east-1",
                F::Off,
                D::Off,
                "https://api.ecr.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                Ecr,
                "eu-isoe-west-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.eu-isoe-west-1.cloud.adc-e.uk",
            ),
            (
                Ecr,
                "eu-isoe-west-1",
                F::Off,
                D::Off,
                "https://api.ecr.eu-isoe-west-1.cloud.adc-e.uk",
            ),
            (
                Ecr,
                "us-isof-south-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.us-isof-south-1.csp.hci.ic.gov",
            ),
            (
                Ecr,
                "us-isof-south-1",
                F::Off,
                D::Off,
                "https://api.ecr.us-isof-south-1.csp.hci.ic.gov",
            ),
            (
                Ecr,
                "us-gov-west-1",
                F::On,
                D::On,
                "https://ecr-fips.us-gov-west-1.api.aws",
            ),
            (
                Ecr,
                "us-gov-west-1",
                F::On,
                D::Off,
                "https://api.ecr-fips.us-gov-west-1.amazonaws.com",
            ),
            (
                Ecr,
                "us-gov-west-1",
                F::Off,
                D::On,
                "https://ecr.us-gov-west-1.api.aws",
            ),
            (
                Ecr,
                "us-gov-west-1",
                F::Off,
                D::Off,
                "https://api.ecr.us-gov-west-1.amazonaws.com",
            ),
            (
                EcrPublic,
                "us-east-1",
                F::Off,
                D::Off,
                "https://api.ecr-public.us-east-1.amazonaws.com",
            ),
            (
                EcrPublic,
                "us-east-1",
                F::Off,
                D::On,
                "https://ecr-public.us-east-1.api.aws",
            ),
            (
                EcrPublic,
                "us-west-2",
                F::Off,
                D::Off,
                "https://api.ecr-public.us-west-2.amazonaws.com",
            ),
            (
                EcrPublic,
                "us-east-1",
                F::On,
                D::On,
                "https://api.ecr-public-fips.us-east-1.api.aws",
            ),
            (
                EcrPublic,
                "us-east-1",
                F::On,
                D::Off,
                "https://api.ecr-public-fips.us-east-1.amazonaws.com",
            ),
            (
                EcrPublic,
                "cn-north-1",
                F::On,
                D::On,
                "https://api.ecr-public-fips.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                EcrPublic,
                "cn-north-1",
                F::On,
                D::Off,
                "https://api.ecr-public-fips.cn-north-1.amazonaws.com.cn",
            ),
            (
                EcrPublic,
                "cn-north-1",
                F::Off,
                D::On,
                "https://api.ecr-public.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                EcrPublic,
                "cn-north-1",
                F::Off,
                D::Off,
                "https://api.ecr-public.cn-north-1.amazonaws.com.cn",
            ),
            (
                EcrPublic,
                "us-gov-east-1",
                F::On,
                D::On,
                "https://api.ecr-public-fips.us-gov-east-1.api.aws",
            ),
            (
                EcrPublic,
                "us-gov-east-1",
                F::On,
                D::Off,
                "https://api.ecr-public-fips.us-gov-east-1.amazonaws.com",
            ),
            (
                EcrPublic,
                "us-gov-east-1",
                F::Off,
                D::On,
                "https://api.ecr-public.us-gov-east-1.api.aws",
            ),
            (
                EcrPublic,
                "us-gov-east-1",
                F::Off,
                D::Off,
                "https://api.ecr-public.us-gov-east-1.amazonaws.com",
            ),
            (
                EcrPublic,
                "us-iso-east-1",
                F::On,
                D::Off,
                "https://api.ecr-public-fips.us-iso-east-1.c2s.ic.gov",
            ),
            (
                EcrPublic,
                "us-iso-east-1",
                F::Off,
                D::Off,
                "https://api.ecr-public.us-iso-east-1.c2s.ic.gov",
            ),
            (
                EcrPublic,
                "us-isob-east-1",
                F::On,
                D::Off,
                "https://api.ecr-public-fips.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                EcrPublic,
                "us-isob-east-1",
                F::Off,
                D::Off,
                "https://api.ecr-public.us-isob-east-1.sc2s.sgov.gov",
            ),
        ];
        for (service, region, fips, dual, expected) in cases {
            assert_eq!(
                resolved(*service, region, *fips, *dual, None).as_deref(),
                Ok(*expected),
                "{service:?} {region} {fips:?} {dual:?}"
            );
        }
    }

    /// The resolved URL as the SDK's tests spell it, without the root path
    /// `Url` adds, or the error message.
    fn resolved(
        service: Service,
        region: &str,
        fips: Fips,
        dual_stack: DualStack,
        endpoint_url: Option<&str>,
    ) -> Result<String, String> {
        let endpoint_url = endpoint_url.map(|u| Endpoint::parse(u).unwrap());
        let region = Region::parse(region).unwrap();
        resolve(service, &region, fips, dual_stack, endpoint_url)
            .map(|endpoint| endpoint.url().as_str().trim_end_matches('/').to_owned())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_configured_endpoint_wins_and_excludes_fips_and_dual_stack() {
        for service in [Service::Ecr, Service::EcrPublic] {
            let url = Some("https://example.com");
            assert_eq!(
                resolved(service, "us-east-1", Fips::Off, DualStack::Off, url).as_deref(),
                Ok("https://example.com")
            );
            assert_eq!(
                resolved(service, "us-east-1", Fips::On, DualStack::Off, url),
                Err("Invalid Configuration: FIPS and custom endpoint are not supported".into())
            );
            assert_eq!(
                resolved(service, "us-east-1", Fips::Off, DualStack::On, url),
                Err(
                    "Invalid Configuration: Dualstack and custom endpoint are not supported".into()
                )
            );
        }
    }

    /// A region is a host label, dots allowed, as the SDK's endpoint rules
    /// require, so nothing else can reach an endpoint host name.
    #[test]
    fn reads_a_region_as_a_host_label() {
        for good in ["us-east-1", "a.b", "x", &"a".repeat(63)] {
            assert_eq!(Region::parse(good).unwrap().as_str(), good);
        }
        for bad in [
            "",
            "-us",
            "us_east_1",
            "us east 1",
            "a.b.",
            ".a",
            "evil.com/x",
            &"a".repeat(64),
        ] {
            assert_eq!(
                Region::parse(bad).unwrap_err().to_string(),
                format!("invalid AWS region '{bad}': must be a valid host label"),
            );
        }
    }

    /// A region outside every pattern resolves in the `aws` partition, as the
    /// SDK's partition resolver does.
    #[test]
    fn an_unknown_region_falls_back_to_the_aws_partition() {
        assert_eq!(
            resolved(Service::Ecr, "xx-mars-1", Fips::Off, DualStack::Off, None).as_deref(),
            Ok("https://api.ecr.xx-mars-1.amazonaws.com")
        );
    }

    /// A configured endpoint that is not an HTTP URL fails when the client
    /// is built, without echoing the text, which may carry a key. A `mailto:`
    /// or `unix:` URL parses but has no host to sign.
    #[test]
    fn rejects_an_endpoint_that_is_not_an_http_url() {
        for (text, error) in [
            (
                "gw/ecr?key=secret",
                "invalid AWS endpoint URL: relative URL without a base",
            ),
            (
                "unix:/run/ecr.sock?key=secret",
                "invalid AWS endpoint URL: unsupported scheme 'unix', not http or https",
            ),
        ] {
            assert_eq!(Endpoint::parse(text).unwrap_err().to_string(), error);
        }
    }

    /// A query and a user and password are dropped, so neither a key nor a
    /// second `Authorization` header (reqwest sends URL user info as Basic
    /// auth) goes out with a signed request.
    #[test]
    fn drops_the_query_and_user_info() {
        let endpoint = Endpoint::parse("http://user:secret@127.0.0.1:8080/base?x=1").unwrap();
        assert_eq!(endpoint.host(), "127.0.0.1:8080");
        assert_eq!(endpoint.url().as_str(), "http://127.0.0.1:8080/base/");
        let keyed = Endpoint::parse("https://gw.example/secret-key/ecr").unwrap();
        assert_eq!(format!("{keyed:?}"), r#"Endpoint("gw.example")"#);
        assert_eq!(
            Endpoint::parse("https://example.com:443").unwrap().host(),
            "example.com"
        );
    }

    /// The endpoint rule tests generated into `aws-sdk-sts` 1.122.0, less
    /// the global endpoint cases (`aws-global` or `UseGlobalEndpoint`), the
    /// configured endpoint cases, which share [`configured`] with ECR, and
    /// "Missing Region". Breaks if a partition, FIPS or dual-stack host
    /// differs from the SDK's, such as the `us-gov` FIPS host, which is the
    /// standard one.
    #[test]
    fn matches_the_sdk_sts_endpoint_rule_tests() {
        use DualStack as D;
        use Fips as F;
        let cases: &[(&str, Fips, DualStack, &str)] = &[
            (
                "af-south-1",
                F::Off,
                D::Off,
                "https://sts.af-south-1.amazonaws.com",
            ),
            (
                "ap-east-1",
                F::Off,
                D::Off,
                "https://sts.ap-east-1.amazonaws.com",
            ),
            (
                "ap-northeast-1",
                F::Off,
                D::Off,
                "https://sts.ap-northeast-1.amazonaws.com",
            ),
            (
                "ap-northeast-2",
                F::Off,
                D::Off,
                "https://sts.ap-northeast-2.amazonaws.com",
            ),
            (
                "ap-northeast-3",
                F::Off,
                D::Off,
                "https://sts.ap-northeast-3.amazonaws.com",
            ),
            (
                "ap-south-1",
                F::Off,
                D::Off,
                "https://sts.ap-south-1.amazonaws.com",
            ),
            (
                "ap-southeast-1",
                F::Off,
                D::Off,
                "https://sts.ap-southeast-1.amazonaws.com",
            ),
            (
                "ap-southeast-2",
                F::Off,
                D::Off,
                "https://sts.ap-southeast-2.amazonaws.com",
            ),
            (
                "ap-southeast-3",
                F::Off,
                D::Off,
                "https://sts.ap-southeast-3.amazonaws.com",
            ),
            (
                "ca-central-1",
                F::Off,
                D::Off,
                "https://sts.ca-central-1.amazonaws.com",
            ),
            (
                "eu-central-1",
                F::Off,
                D::Off,
                "https://sts.eu-central-1.amazonaws.com",
            ),
            (
                "eu-north-1",
                F::Off,
                D::Off,
                "https://sts.eu-north-1.amazonaws.com",
            ),
            (
                "eu-south-1",
                F::Off,
                D::Off,
                "https://sts.eu-south-1.amazonaws.com",
            ),
            (
                "eu-west-1",
                F::Off,
                D::Off,
                "https://sts.eu-west-1.amazonaws.com",
            ),
            (
                "eu-west-2",
                F::Off,
                D::Off,
                "https://sts.eu-west-2.amazonaws.com",
            ),
            (
                "eu-west-3",
                F::Off,
                D::Off,
                "https://sts.eu-west-3.amazonaws.com",
            ),
            (
                "me-south-1",
                F::Off,
                D::Off,
                "https://sts.me-south-1.amazonaws.com",
            ),
            (
                "sa-east-1",
                F::Off,
                D::Off,
                "https://sts.sa-east-1.amazonaws.com",
            ),
            (
                "us-east-1",
                F::Off,
                D::Off,
                "https://sts.us-east-1.amazonaws.com",
            ),
            (
                "us-east-1",
                F::On,
                D::Off,
                "https://sts-fips.us-east-1.amazonaws.com",
            ),
            (
                "us-east-2",
                F::Off,
                D::Off,
                "https://sts.us-east-2.amazonaws.com",
            ),
            (
                "us-east-2",
                F::On,
                D::Off,
                "https://sts-fips.us-east-2.amazonaws.com",
            ),
            (
                "us-west-1",
                F::Off,
                D::Off,
                "https://sts.us-west-1.amazonaws.com",
            ),
            (
                "us-west-1",
                F::On,
                D::Off,
                "https://sts-fips.us-west-1.amazonaws.com",
            ),
            (
                "us-west-2",
                F::Off,
                D::Off,
                "https://sts.us-west-2.amazonaws.com",
            ),
            (
                "us-west-2",
                F::On,
                D::Off,
                "https://sts-fips.us-west-2.amazonaws.com",
            ),
            (
                "us-east-1",
                F::On,
                D::On,
                "https://sts-fips.us-east-1.api.aws",
            ),
            ("us-east-1", F::Off, D::On, "https://sts.us-east-1.api.aws"),
            (
                "cn-north-1",
                F::Off,
                D::Off,
                "https://sts.cn-north-1.amazonaws.com.cn",
            ),
            (
                "cn-northwest-1",
                F::Off,
                D::Off,
                "https://sts.cn-northwest-1.amazonaws.com.cn",
            ),
            (
                "cn-north-1",
                F::On,
                D::On,
                "https://sts-fips.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                "cn-north-1",
                F::On,
                D::Off,
                "https://sts-fips.cn-north-1.amazonaws.com.cn",
            ),
            (
                "cn-north-1",
                F::Off,
                D::On,
                "https://sts.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                "us-gov-east-1",
                F::Off,
                D::Off,
                "https://sts.us-gov-east-1.amazonaws.com",
            ),
            (
                "us-gov-east-1",
                F::On,
                D::Off,
                "https://sts.us-gov-east-1.amazonaws.com",
            ),
            (
                "us-gov-west-1",
                F::Off,
                D::Off,
                "https://sts.us-gov-west-1.amazonaws.com",
            ),
            (
                "us-gov-west-1",
                F::On,
                D::Off,
                "https://sts.us-gov-west-1.amazonaws.com",
            ),
            (
                "us-gov-east-1",
                F::On,
                D::On,
                "https://sts-fips.us-gov-east-1.api.aws",
            ),
            (
                "us-gov-east-1",
                F::Off,
                D::On,
                "https://sts.us-gov-east-1.api.aws",
            ),
            (
                "us-iso-east-1",
                F::Off,
                D::Off,
                "https://sts.us-iso-east-1.c2s.ic.gov",
            ),
            (
                "us-iso-west-1",
                F::Off,
                D::Off,
                "https://sts.us-iso-west-1.c2s.ic.gov",
            ),
            (
                "us-iso-east-1",
                F::On,
                D::Off,
                "https://sts-fips.us-iso-east-1.c2s.ic.gov",
            ),
            (
                "us-isob-east-1",
                F::Off,
                D::Off,
                "https://sts.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                "us-isob-east-1",
                F::On,
                D::Off,
                "https://sts-fips.us-isob-east-1.sc2s.sgov.gov",
            ),
        ];
        assert_eq!(cases.len(), 44);
        for &(region, fips, dual_stack, expected) in cases {
            assert_resolves(CredentialApi::Sts, region, fips, dual_stack, expected);
        }
    }

    /// The endpoint rule tests generated into `aws-sdk-sso` 1.117.0 and
    /// `aws-sdk-ssooidc` 1.119.0, less the configured endpoint cases, which
    /// share [`configured`] with ECR, and "Missing Region". Breaks if a
    /// host label, partition, FIPS or dual-stack host differs from the
    /// SDK's.
    #[test]
    fn matches_the_sdk_sso_endpoint_rule_tests() {
        use CredentialApi::{Sso, SsoOidc};
        use DualStack as D;
        use Fips as F;
        let cases: &[(CredentialApi, &str, Fips, DualStack, &str)] = &[
            (
                Sso,
                "ap-east-1",
                F::Off,
                D::Off,
                "https://portal.sso.ap-east-1.amazonaws.com",
            ),
            (
                Sso,
                "ap-northeast-1",
                F::Off,
                D::Off,
                "https://portal.sso.ap-northeast-1.amazonaws.com",
            ),
            (
                Sso,
                "ap-northeast-2",
                F::Off,
                D::Off,
                "https://portal.sso.ap-northeast-2.amazonaws.com",
            ),
            (
                Sso,
                "ap-northeast-3",
                F::Off,
                D::Off,
                "https://portal.sso.ap-northeast-3.amazonaws.com",
            ),
            (
                Sso,
                "ap-south-1",
                F::Off,
                D::Off,
                "https://portal.sso.ap-south-1.amazonaws.com",
            ),
            (
                Sso,
                "ap-southeast-1",
                F::Off,
                D::Off,
                "https://portal.sso.ap-southeast-1.amazonaws.com",
            ),
            (
                Sso,
                "ap-southeast-2",
                F::Off,
                D::Off,
                "https://portal.sso.ap-southeast-2.amazonaws.com",
            ),
            (
                Sso,
                "ca-central-1",
                F::Off,
                D::Off,
                "https://portal.sso.ca-central-1.amazonaws.com",
            ),
            (
                Sso,
                "eu-central-1",
                F::Off,
                D::Off,
                "https://portal.sso.eu-central-1.amazonaws.com",
            ),
            (
                Sso,
                "eu-north-1",
                F::Off,
                D::Off,
                "https://portal.sso.eu-north-1.amazonaws.com",
            ),
            (
                Sso,
                "eu-south-1",
                F::Off,
                D::Off,
                "https://portal.sso.eu-south-1.amazonaws.com",
            ),
            (
                Sso,
                "eu-west-1",
                F::Off,
                D::Off,
                "https://portal.sso.eu-west-1.amazonaws.com",
            ),
            (
                Sso,
                "eu-west-2",
                F::Off,
                D::Off,
                "https://portal.sso.eu-west-2.amazonaws.com",
            ),
            (
                Sso,
                "eu-west-3",
                F::Off,
                D::Off,
                "https://portal.sso.eu-west-3.amazonaws.com",
            ),
            (
                Sso,
                "me-south-1",
                F::Off,
                D::Off,
                "https://portal.sso.me-south-1.amazonaws.com",
            ),
            (
                Sso,
                "sa-east-1",
                F::Off,
                D::Off,
                "https://portal.sso.sa-east-1.amazonaws.com",
            ),
            (
                Sso,
                "us-east-1",
                F::Off,
                D::Off,
                "https://portal.sso.us-east-1.amazonaws.com",
            ),
            (
                Sso,
                "us-east-2",
                F::Off,
                D::Off,
                "https://portal.sso.us-east-2.amazonaws.com",
            ),
            (
                Sso,
                "us-west-2",
                F::Off,
                D::Off,
                "https://portal.sso.us-west-2.amazonaws.com",
            ),
            (
                Sso,
                "us-east-1",
                F::On,
                D::On,
                "https://portal.sso-fips.us-east-1.api.aws",
            ),
            (
                Sso,
                "us-east-1",
                F::On,
                D::Off,
                "https://portal.sso-fips.us-east-1.amazonaws.com",
            ),
            (
                Sso,
                "us-east-1",
                F::Off,
                D::On,
                "https://portal.sso.us-east-1.api.aws",
            ),
            (
                Sso,
                "cn-north-1",
                F::On,
                D::On,
                "https://portal.sso-fips.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                Sso,
                "cn-north-1",
                F::On,
                D::Off,
                "https://portal.sso-fips.cn-north-1.amazonaws.com.cn",
            ),
            (
                Sso,
                "cn-north-1",
                F::Off,
                D::On,
                "https://portal.sso.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                Sso,
                "cn-north-1",
                F::Off,
                D::Off,
                "https://portal.sso.cn-north-1.amazonaws.com.cn",
            ),
            (
                Sso,
                "us-gov-east-1",
                F::Off,
                D::Off,
                "https://portal.sso.us-gov-east-1.amazonaws.com",
            ),
            (
                Sso,
                "us-gov-west-1",
                F::Off,
                D::Off,
                "https://portal.sso.us-gov-west-1.amazonaws.com",
            ),
            (
                Sso,
                "us-gov-east-1",
                F::On,
                D::On,
                "https://portal.sso-fips.us-gov-east-1.api.aws",
            ),
            (
                Sso,
                "us-gov-east-1",
                F::On,
                D::Off,
                "https://portal.sso.us-gov-east-1.amazonaws.com",
            ),
            (
                Sso,
                "us-gov-east-1",
                F::Off,
                D::On,
                "https://portal.sso.us-gov-east-1.api.aws",
            ),
            (
                Sso,
                "us-iso-east-1",
                F::On,
                D::Off,
                "https://portal.sso-fips.us-iso-east-1.c2s.ic.gov",
            ),
            (
                Sso,
                "us-iso-east-1",
                F::Off,
                D::Off,
                "https://portal.sso.us-iso-east-1.c2s.ic.gov",
            ),
            (
                Sso,
                "us-isob-east-1",
                F::On,
                D::Off,
                "https://portal.sso-fips.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                Sso,
                "us-isob-east-1",
                F::Off,
                D::Off,
                "https://portal.sso.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                SsoOidc,
                "ap-east-1",
                F::Off,
                D::Off,
                "https://oidc.ap-east-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "ap-northeast-1",
                F::Off,
                D::Off,
                "https://oidc.ap-northeast-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "ap-northeast-2",
                F::Off,
                D::Off,
                "https://oidc.ap-northeast-2.amazonaws.com",
            ),
            (
                SsoOidc,
                "ap-northeast-3",
                F::Off,
                D::Off,
                "https://oidc.ap-northeast-3.amazonaws.com",
            ),
            (
                SsoOidc,
                "ap-south-1",
                F::Off,
                D::Off,
                "https://oidc.ap-south-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "ap-southeast-1",
                F::Off,
                D::Off,
                "https://oidc.ap-southeast-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "ap-southeast-2",
                F::Off,
                D::Off,
                "https://oidc.ap-southeast-2.amazonaws.com",
            ),
            (
                SsoOidc,
                "ca-central-1",
                F::Off,
                D::Off,
                "https://oidc.ca-central-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "eu-central-1",
                F::Off,
                D::Off,
                "https://oidc.eu-central-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "eu-north-1",
                F::Off,
                D::Off,
                "https://oidc.eu-north-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "eu-south-1",
                F::Off,
                D::Off,
                "https://oidc.eu-south-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "eu-west-1",
                F::Off,
                D::Off,
                "https://oidc.eu-west-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "eu-west-2",
                F::Off,
                D::Off,
                "https://oidc.eu-west-2.amazonaws.com",
            ),
            (
                SsoOidc,
                "eu-west-3",
                F::Off,
                D::Off,
                "https://oidc.eu-west-3.amazonaws.com",
            ),
            (
                SsoOidc,
                "me-south-1",
                F::Off,
                D::Off,
                "https://oidc.me-south-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "sa-east-1",
                F::Off,
                D::Off,
                "https://oidc.sa-east-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-east-1",
                F::Off,
                D::Off,
                "https://oidc.us-east-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-east-2",
                F::Off,
                D::Off,
                "https://oidc.us-east-2.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-west-2",
                F::Off,
                D::Off,
                "https://oidc.us-west-2.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-east-1",
                F::On,
                D::On,
                "https://oidc-fips.us-east-1.api.aws",
            ),
            (
                SsoOidc,
                "us-east-1",
                F::On,
                D::Off,
                "https://oidc-fips.us-east-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-east-1",
                F::Off,
                D::On,
                "https://oidc.us-east-1.api.aws",
            ),
            (
                SsoOidc,
                "cn-north-1",
                F::On,
                D::On,
                "https://oidc-fips.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                SsoOidc,
                "cn-north-1",
                F::On,
                D::Off,
                "https://oidc-fips.cn-north-1.amazonaws.com.cn",
            ),
            (
                SsoOidc,
                "cn-north-1",
                F::Off,
                D::On,
                "https://oidc.cn-north-1.api.amazonwebservices.com.cn",
            ),
            (
                SsoOidc,
                "cn-north-1",
                F::Off,
                D::Off,
                "https://oidc.cn-north-1.amazonaws.com.cn",
            ),
            (
                SsoOidc,
                "us-gov-east-1",
                F::Off,
                D::Off,
                "https://oidc.us-gov-east-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-gov-west-1",
                F::Off,
                D::Off,
                "https://oidc.us-gov-west-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-gov-east-1",
                F::On,
                D::On,
                "https://oidc-fips.us-gov-east-1.api.aws",
            ),
            (
                SsoOidc,
                "us-gov-east-1",
                F::On,
                D::Off,
                "https://oidc.us-gov-east-1.amazonaws.com",
            ),
            (
                SsoOidc,
                "us-gov-east-1",
                F::Off,
                D::On,
                "https://oidc.us-gov-east-1.api.aws",
            ),
            (
                SsoOidc,
                "us-iso-east-1",
                F::On,
                D::Off,
                "https://oidc-fips.us-iso-east-1.c2s.ic.gov",
            ),
            (
                SsoOidc,
                "us-iso-east-1",
                F::Off,
                D::Off,
                "https://oidc.us-iso-east-1.c2s.ic.gov",
            ),
            (
                SsoOidc,
                "us-isob-east-1",
                F::On,
                D::Off,
                "https://oidc-fips.us-isob-east-1.sc2s.sgov.gov",
            ),
            (
                SsoOidc,
                "us-isob-east-1",
                F::Off,
                D::Off,
                "https://oidc.us-isob-east-1.sc2s.sgov.gov",
            ),
        ];
        assert_eq!(cases.len(), 70);
        for &(api, region, fips, dual_stack, expected) in cases {
            assert_resolves(api, region, fips, dual_stack, expected);
        }
    }

    /// `api` in `region` resolves to `expected`.
    fn assert_resolves(
        api: CredentialApi,
        region: &str,
        fips: Fips,
        dual_stack: DualStack,
        expected: &str,
    ) {
        let region = Region::parse(region).unwrap();
        let endpoint = resolve_credential_api(api, &region, fips, dual_stack, None).unwrap();
        assert_eq!(
            endpoint.url().as_str().trim_end_matches('/'),
            expected,
            "{api:?} {region} {fips:?} {dual_stack:?}"
        );
    }

    /// A configured URL replaces the rule's host, and FIPS with it is an
    /// error, as for ECR. Breaks if a credential API skips the check.
    #[test]
    fn credential_apis_honor_a_configured_url() {
        let region = Region::parse("us-east-1").unwrap();
        let url = || Some(Endpoint::parse("https://example.com").unwrap());
        for api in [
            CredentialApi::Sts,
            CredentialApi::Sso,
            CredentialApi::SsoOidc,
        ] {
            let resolve = |fips| resolve_credential_api(api, &region, fips, DualStack::Off, url());
            assert_eq!(resolve(Fips::Off).unwrap().host(), "example.com");
            let err = resolve(Fips::On).unwrap_err();
            assert!(matches!(err, ConfigError::FipsWithEndpointUrl), "{err}");
        }
    }
}
