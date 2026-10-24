//! Endpoint resolution for the AWS APIs ocync calls, matching the endpoint
//! rules the AWS SDK generates for ECR and ECR Public.

use std::sync::LazyLock;

use regex_lite::Regex;
use url::Url;

use super::{ConfigError, Service};

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

fn partition(region: &str) -> &'static Partition {
    REGION_PATTERNS
        .iter()
        .position(|pattern| pattern.is_match(region))
        .map_or(&PARTITIONS[0], |i| &PARTITIONS[i])
}

/// An HTTP or HTTPS endpoint, with the `host` header a signature names.
#[derive(Debug, Clone)]
pub(super) struct Endpoint {
    url: Url,
    /// The host, and the port when it is not the scheme's default.
    host: String,
}

impl Endpoint {
    /// Parse an endpoint URL from configuration or from the rules below.
    ///
    /// Requests go to the endpoint's path joined with the operation path
    /// `/`, so `https://gw.example/ecr` is called at `/ecr/`. A query is
    /// dropped with a warning. Both are what the SDK does.
    pub(super) fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut url = Url::parse(text).map_err(|source| ConfigError::InvalidEndpoint {
            url: text.to_owned(),
            source,
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::EndpointNotHttp {
                url: text.to_owned(),
            });
        }
        if let Some(query) = url.query() {
            tracing::warn!(query, "a query in an AWS endpoint URL is ignored");
        }
        url.set_query(None);
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
    region: &str,
    fips: Fips,
    dual_stack: DualStack,
    endpoint_url: Option<Endpoint>,
) -> Result<Endpoint, ConfigError> {
    if let Some(url) = endpoint_url {
        return match (fips, dual_stack) {
            (Fips::On, _) => Err(ConfigError::FipsWithEndpointUrl),
            (_, DualStack::On) => Err(ConfigError::DualStackWithEndpointUrl),
            (Fips::Off, DualStack::Off) => Ok(url),
        };
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

#[cfg(test)]
mod tests {
    use super::*;

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
        resolve(service, region, fips, dual_stack, endpoint_url)
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
    /// is built, naming the text. A `mailto:` or `unix:` URL parses but has
    /// no host to sign.
    #[test]
    fn rejects_an_endpoint_that_is_not_an_http_url() {
        for (text, error) in [
            (
                "not a url",
                "invalid AWS endpoint 'not a url': relative URL without a base",
            ),
            (
                "unix:/run/ecr.sock",
                "AWS endpoint 'unix:/run/ecr.sock' is not an http or https URL",
            ),
        ] {
            assert_eq!(Endpoint::parse(text).unwrap_err().to_string(), error);
        }
        let endpoint = Endpoint::parse("http://127.0.0.1:8080/base?x=1").unwrap();
        assert_eq!(endpoint.host(), "127.0.0.1:8080");
        assert_eq!(endpoint.url().as_str(), "http://127.0.0.1:8080/base/");
        assert_eq!(
            Endpoint::parse("https://example.com:443").unwrap().host(),
            "example.com"
        );
    }
}
