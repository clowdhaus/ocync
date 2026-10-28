//! An external account's AWS subject token: a signed STS
//! `GetCallerIdentity` request, written as JSON and URL-encoded (AIP-4117).
//!
//! The request is signed with ocync's AWS credential chain, so IRSA, EKS Pod
//! Identity and every other AWS source work. Upstream reads only the
//! `AWS_*` key variables and the instance metadata role the file names.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{Call, Error, Secret, Setup, send_text};
use crate::aws::{
    CallerIdentitySigner, Env, InvalidRegion, Region, TOKEN_HEADER, TOKEN_TTL, TOKEN_TTL_HEADER,
};
use crate::retry::{Bounded, retrying};

/// The variables naming the region, in upstream's order.
const REGION_VARIABLES: [&str; 2] = ["AWS_REGION", "AWS_DEFAULT_REGION"];

/// The STS URL when the file names none. `{region}` is replaced.
const DEFAULT_VERIFICATION_URL: &str =
    "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";

/// The header naming the federation audience, signed with the request.
const TARGET_RESOURCE: &str = "x-goog-cloud-target-resource";

/// The request signed.
const METHOD: http::Method = http::Method::POST;

/// An AWS subject token source as the credentials file writes it.
/// `environment_id` selects it and is not read further, as upstream; the
/// role URL `url` is not read, as the AWS chain supplies the credentials.
#[derive(Debug, Deserialize)]
pub(super) struct AwsSourceFile {
    #[serde(rename = "environment_id")]
    _environment_id: IgnoredAny,
    region_url: Option<String>,
    regional_cred_verification_url: Option<String>,
    imdsv2_session_token_url: Option<String>,
}

/// The AWS subject token source.
#[derive(Debug)]
pub(super) struct AwsSource {
    signer: CallerIdentitySigner,
    url: Url,
    /// The host of `url`, signed as `host`.
    host: String,
    audience: String,
}

/// The subject token, before it is URL-encoded.
#[derive(Serialize)]
struct SignedRequest<'a> {
    url: &'a str,
    method: &'a str,
    headers: Vec<Header<'a>>,
    body: &'a str,
}

#[derive(Serialize)]
struct Header<'a> {
    key: &'a str,
    value: &'a str,
}

impl AwsSource {
    /// The source `file` describes, for `audience`: the region from the
    /// environment or `region_url`, the STS URL from
    /// `regional_cred_verification_url` or the default, and that region's
    /// credentials from the run's AWS chain.
    pub(super) async fn new(
        setup: &Setup<'_>,
        file: &AwsSourceFile,
        audience: &str,
    ) -> Result<Self, Error> {
        let region = match region_from_env(&setup.host.env) {
            Some(region) => region,
            None => region_from_metadata(setup, file).await?,
        }
        .map_err(|e| Error::AwsConfig(e.into()))?;
        let signer = CallerIdentitySigner::new(setup.host, region, setup.aws_credentials)
            .map_err(Error::AwsConfig)?;
        Self::with_signer(
            signer,
            file.regional_cred_verification_url.as_deref(),
            audience,
        )
    }

    fn with_signer(
        signer: CallerIdentitySigner,
        verification_url: Option<&str>,
        audience: &str,
    ) -> Result<Self, Error> {
        let url = sts_url(verification_url, signer.region())?;
        Ok(Self {
            host: url.host_str().ok_or(Error::StsUrlHost)?.to_owned(),
            url,
            signer,
            audience: audience.to_owned(),
        })
    }

    /// The subject token signed at `now`.
    pub(super) async fn subject_token(&self, now: SystemTime) -> Result<Secret, Error> {
        let unsigned = [
            ("host", self.host.as_str()),
            (TARGET_RESOURCE, self.audience.as_str()),
        ];
        let signed = self
            .signer
            .sign(&METHOD, &self.url, &unsigned, now)
            .await
            .map_err(Error::AwsSigning)?;
        let headers: BTreeMap<&str, &str> = unsigned
            .into_iter()
            .chain(signed.date_and_token())
            .collect();
        let headers = headers
            .into_iter()
            .chain([("Authorization", signed.authorization.as_str())])
            .map(|(key, value)| Header { key, value })
            .collect();
        let json = serde_json::to_string(&SignedRequest {
            url: self.url.as_str(),
            method: METHOD.as_str(),
            headers,
            body: "",
        })
        .expect("the request serializes");
        Ok(Secret(
            url::form_urlencoded::byte_serialize(json.as_bytes()).collect(),
        ))
    }
}

/// The region the environment names, if it names one.
fn region_from_env(env: &Env) -> Option<Result<Region, InvalidRegion>> {
    REGION_VARIABLES
        .iter()
        .find_map(|name| env.get(name))
        .map(|region| Region::parse(&region))
}

/// The region of the availability zone `file`'s `region_url` answers, with
/// an `IMDSv2` token from its `imdsv2_session_token_url` when it names one.
/// A transient failure is sent again, as this runs once, when the registry
/// is set up.
async fn region_from_metadata(
    setup: &Setup<'_>,
    file: &AwsSourceFile,
) -> Result<Result<Region, InvalidRegion>, Error> {
    let Some(region_url) = &file.region_url else {
        return Err(Error::NoAwsRegion);
    };
    retrying(
        "AWS region lookup",
        &mut Bounded(Error::is_transient),
        |_| zone_region(setup, file, region_url),
    )
    .await
}

/// One try of [`region_from_metadata`].
async fn zone_region(
    setup: &Setup<'_>,
    file: &AwsSourceFile,
    region_url: &str,
) -> Result<Result<Region, InvalidRegion>, Error> {
    let client = &setup.clients.metadata;
    let token = match &file.imdsv2_session_token_url {
        Some(url) => {
            let request = client
                .put(url)
                .header(TOKEN_TTL_HEADER, TOKEN_TTL.as_secs());
            Some(send_text(Call::AwsRegion, request).await?)
        }
        None => None,
    };
    let mut request = client.get(region_url);
    if let Some(token) = &token {
        request = request.header(TOKEN_HEADER, token);
    }
    region_from_zone(&send_text(Call::AwsRegion, request).await?)
        .map(Region::parse)
        .ok_or(Error::NoAwsRegion)
}

/// The region of zone `zone`: `us-east-1d` is in `us-east-1`. A name that
/// does not end in a letter after a digit is taken as the region itself.
fn region_from_zone(zone: &str) -> Option<&str> {
    let zone = zone.trim();
    if !zone.chars().last()?.is_ascii_alphabetic() {
        return Some(zone);
    }
    // The last character is ASCII, one byte, so this cuts on a boundary.
    let region = &zone[..zone.len() - 1];
    if region.ends_with(|c: char| c.is_ascii_digit()) {
        return Some(region);
    }
    Some(zone)
}

/// The STS URL from `template`, or the default, in `region`, with
/// `https://` before it when it has no scheme. It must name a host.
fn sts_url(template: Option<&str>, region: &Region) -> Result<Url, Error> {
    let url = template
        .unwrap_or(DEFAULT_VERIFICATION_URL)
        .replace("{region}", region.as_str());
    let url = if url.starts_with("http") {
        url
    } else {
        format!("https://{url}")
    };
    Url::parse(&url).map_err(Error::StsUrl)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn region(text: &str) -> Region {
        Region::parse(text).unwrap()
    }

    /// Upstream `test_parse_region_from_zone`. Breaks if a zone keeps its
    /// letter or a region loses its digit.
    #[test]
    fn reads_the_region_of_a_zone() {
        for (zone, region) in [
            ("us-east-1a", Some("us-east-1")),
            ("us-east-1", Some("us-east-1")),
            ("us-gov-west-1a", Some("us-gov-west-1")),
            ("  us-east-1a  ", Some("us-east-1")),
            ("", None),
            ("   ", None),
            ("a", Some("a")),
            ("us-east-1\u{e9}", Some("us-east-1\u{e9}")),
        ] {
            assert_eq!(region_from_zone(zone), region, "{zone:?}");
        }
    }

    /// Upstream `test_resolve_sts_url` and `_invalid`. Breaks if the region
    /// is not substituted, a bare host gets no scheme, or a URL with no host
    /// is accepted.
    #[test]
    fn resolves_the_sts_url() {
        assert_eq!(
            sts_url(None, &region("us-west-2")).unwrap().as_str(),
            "https://sts.us-west-2.amazonaws.com/?Action=GetCallerIdentity&Version=2011-06-15"
        );
        assert_eq!(
            sts_url(Some("sts.{region}.amazonaws.com"), &region("eu-west-1"))
                .unwrap()
                .as_str(),
            "https://sts.eu-west-1.amazonaws.com/"
        );
        assert!(sts_url(Some("http://"), &region("us-east-1")).is_err());
        let no_host = AwsSource::with_signer(
            CallerIdentitySigner::example("us-east-1", None),
            Some("httpx:/sts"),
            "audience",
        );
        assert!(matches!(no_host, Err(Error::StsUrlHost)));
    }

    /// Upstream `test_resolve_region_env`: `AWS_REGION` before
    /// `AWS_DEFAULT_REGION`. Breaks if the order flips.
    #[test]
    fn reads_the_region_from_the_environment() {
        let both = Env::Fixed(HashMap::from([
            ("AWS_REGION".to_owned(), "us-west-2".to_owned()),
            ("AWS_DEFAULT_REGION".to_owned(), "us-east-1".to_owned()),
        ]));
        assert_eq!(
            region_from_env(&both).unwrap().unwrap(),
            region("us-west-2")
        );
        let default = Env::Fixed(HashMap::from([(
            "AWS_DEFAULT_REGION".to_owned(),
            "us-east-1".to_owned(),
        )]));
        assert_eq!(
            region_from_env(&default).unwrap().unwrap(),
            region("us-east-1")
        );
        assert!(region_from_env(&Env::Fixed(HashMap::new())).is_none());
    }

    /// Upstream `test_resolve_region_imds` and `test_resolve_imdsv2_token`:
    /// with no region variable, an `IMDSv2` token is asked for with its TTL
    /// and sent with the zone request, and the zone's region is used.
    /// Breaks if the token is not asked for or not sent, or the zone's
    /// letter is kept.
    #[tokio::test]
    async fn reads_the_region_from_instance_metadata() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/latest/api/token"))
            .and(header(TOKEN_TTL_HEADER, "21600"))
            .respond_with(ResponseTemplate::new(200).set_body_string("imds-token"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .and(header(TOKEN_HEADER, "imds-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("us-gov-west-1a"))
            .expect(1)
            .mount(&server)
            .await;
        let host = super::super::tests::host(crate::aws::Platform::Unix, &[], &[]);
        let clients = super::super::Clients::new().unwrap();
        let aws_credentials = crate::aws::AwsCredentialProviders::default();
        let setup = Setup {
            host: &host,
            clients: &clients,
            aws_credentials: &aws_credentials,
        };

        let file = |region_url: Option<String>, imdsv2_url: Option<String>| AwsSourceFile {
            _environment_id: IgnoredAny,
            region_url,
            regional_cred_verification_url: None,
            imdsv2_session_token_url: imdsv2_url,
        };
        let found = region_from_metadata(
            &setup,
            &file(
                Some(format!("{}/zone", server.uri())),
                Some(format!("{}/latest/api/token", server.uri())),
            ),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(found, region("us-gov-west-1"));
        assert!(matches!(
            region_from_metadata(&setup, &file(None, None)).await,
            Err(Error::NoAwsRegion)
        ));
    }

    /// The zone request runs once, at setup, so a 503 is sent again and a
    /// 404 is not. Breaks if a throttled lookup fails the registry or a
    /// refusal is retried.
    #[tokio::test]
    async fn retries_a_transient_region_lookup() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for (first, calls, succeeds) in [(503, 2, true), (404, 1, false)] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/zone"))
                .respond_with(ResponseTemplate::new(first))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/zone"))
                .respond_with(ResponseTemplate::new(200).set_body_string("us-east-1a"))
                .expect(calls - 1)
                .mount(&server)
                .await;
            let host = super::super::tests::host(crate::aws::Platform::Unix, &[], &[]);
            let clients = super::super::Clients::new().unwrap();
            let aws_credentials = crate::aws::AwsCredentialProviders::default();
            let setup = Setup {
                host: &host,
                clients: &clients,
                aws_credentials: &aws_credentials,
            };
            let file = AwsSourceFile {
                _environment_id: IgnoredAny,
                region_url: Some(format!("{}/zone", server.uri())),
                regional_cred_verification_url: None,
                imdsv2_session_token_url: None,
            };

            let found = region_from_metadata(&setup, &file).await;

            assert_eq!(found.is_ok(), succeeds, "{first}: {found:?}");
        }
    }

    /// The region, profile and keys all come from the injected host. Breaks
    /// if the signer reads the process environment or the real AWS files.
    #[tokio::test]
    async fn signs_with_the_host_it_is_given() {
        let host = super::super::tests::host(
            crate::aws::Platform::Unix,
            &[("HOME", "/home/u"), ("AWS_REGION", "eu-west-3")],
            &[(
                "/home/u/.aws/credentials",
                b"[default]\naws_access_key_id = AKIDINJECTED\naws_secret_access_key = secret\n"
                    .to_vec(),
            )],
        );
        let clients = super::super::Clients::new().unwrap();
        let aws_credentials = crate::aws::AwsCredentialProviders::default();
        let setup = Setup {
            host: &host,
            clients: &clients,
            aws_credentials: &aws_credentials,
        };
        let file = AwsSourceFile {
            _environment_id: IgnoredAny,
            region_url: None,
            regional_cred_verification_url: None,
            imdsv2_session_token_url: None,
        };

        let source = AwsSource::new(&setup, &file, "audience").await.unwrap();
        let token = source.subject_token(UNIX_EPOCH).await.unwrap();

        let json: String = url::form_urlencoded::parse(token.expose().as_bytes())
            .map(|(key, _)| key)
            .collect();
        assert!(
            json.contains("Credential=AKIDINJECTED/19700101/eu-west-3/sts/aws4_request"),
            "{json}"
        );
    }

    /// Upstream `test_subject_token_env_success`, with the whole token
    /// checked: the URL with its query, the method, the headers sorted with
    /// `Authorization` last, the empty body, and a signature that covers the
    /// query and the audience header, computed independently. Breaks if a
    /// header, its order, the query's place in the signature or the
    /// encoding changes.
    #[tokio::test]
    async fn signs_the_caller_identity_request() {
        let source = AwsSource::with_signer(
            CallerIdentitySigner::example("us-east-1", Some("session-token")),
            None,
            "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/aws",
        )
        .unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_440_938_160);

        let token = source.subject_token(now).await.unwrap();

        let json: String = url::form_urlencoded::parse(token.expose().as_bytes())
            .map(|(key, _)| key)
            .collect();
        let expected = serde_json::json!({
            "url": "https://sts.us-east-1.amazonaws.com/?Action=GetCallerIdentity&Version=2011-06-15",
            "method": "POST",
            "headers": [
                {"key": "host", "value": "sts.us-east-1.amazonaws.com"},
                {"key": "x-amz-date", "value": "20150830T123600Z"},
                {"key": "x-amz-security-token", "value": "session-token"},
                {"key": "x-goog-cloud-target-resource", "value": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/aws"},
                {"key": "Authorization", "value": EXPECTED_AUTHORIZATION},
            ],
            "body": "",
        });
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            expected
        );
    }

    /// The `Authorization` header for [`signs_the_caller_identity_request`],
    /// computed with Python's `hmac` and `hashlib` from the canonical
    /// request AWS documents, with the example key.
    const EXPECTED_AUTHORIZATION: &str = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/sts/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token;x-goog-cloud-target-resource, Signature=47e09697078c08e187cc1edad5bb7906e8946c92d645bab69b287d47c005dbdb";
}
