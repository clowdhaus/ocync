//! An external account's AWS subject token: a signed STS
//! `GetCallerIdentity` request, written as JSON and URL-encoded (AIP-4117).
//!
//! The request is signed with ocync's AWS credential chain, so IRSA, EKS Pod
//! Identity and every other AWS source work. Upstream reads only the
//! `AWS_*` key variables and the instance metadata role the file names.

use std::time::SystemTime;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{Call, Error, Secret, Setup, parse_url, send, send_text};
use crate::aws::{CallerIdentityConfig, CallerIdentitySigner, Region, imds};

/// The field naming the URL the availability zone is asked of.
const REGION_URL_FIELD: &str = "region_url";

/// The field naming the URL an `IMDSv2` session token is asked of.
const IMDSV2_TOKEN_URL_FIELD: &str = "imdsv2_session_token_url";

/// The field naming the STS URL the subject token is signed for.
const VERIFICATION_URL_FIELD: &str = "regional_cred_verification_url";

/// The STS URL when the file names none. `{region}` is replaced.
const DEFAULT_VERIFICATION_URL: &str =
    "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";

/// The header naming the federation audience, signed with the request.
const TARGET_RESOURCE: &str = "x-goog-cloud-target-resource";

/// The key Google reads the signature under in the subject token.
const AUTHORIZATION_KEY: &str = "Authorization";

/// An AWS subject token source as the credentials file writes it.
/// `environment_id` selects it and is not read further, as upstream; the
/// role URL `url` is not read, as the AWS chain supplies the credentials.
#[derive(Debug, Deserialize)]
pub(super) struct SourceFile {
    #[serde(rename = "environment_id")]
    _environment_id: IgnoredAny,
    region_url: Option<String>,
    regional_cred_verification_url: Option<String>,
    imdsv2_session_token_url: Option<String>,
}

/// The AWS subject token source.
#[derive(Debug)]
pub(super) struct Source {
    signer: CallerIdentitySigner,
    url: Url,
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

impl Source {
    /// The source `file` describes, for `audience`: the region from the
    /// environment or `region_url`, the STS URL from
    /// `regional_cred_verification_url` or the default, and that region's
    /// credentials from the run's AWS chain.
    pub(super) async fn new(
        setup: &Setup<'_>,
        file: &SourceFile,
        audience: &str,
    ) -> Result<Self, Error> {
        let sts_url = StsUrl::parse(file.regional_cred_verification_url.as_deref())?;
        let config = CallerIdentityConfig::load(setup.host);
        let region = match config.region().map_err(Error::AwsRegion)? {
            Some(region) => region,
            None => region_from_metadata(setup, file).await?,
        };
        let signer = config
            .signer(region, setup.aws_credentials)
            .map_err(Error::AwsConfig)?;
        Self::with_signer(signer, &sts_url, audience)
    }

    fn with_signer(
        signer: CallerIdentitySigner,
        sts_url: &StsUrl,
        audience: &str,
    ) -> Result<Self, Error> {
        let url = sts_url.resolve(signer.region())?;
        Ok(Self {
            url,
            signer,
            audience: audience.to_owned(),
        })
    }

    /// The subject token signed at `now`.
    pub(super) async fn subject_token(&self, now: SystemTime) -> Result<Secret, Error> {
        let unsigned = [(TARGET_RESOURCE, self.audience.as_str())];
        let signed = self
            .signer
            .sign(&self.url, &unsigned, now)
            .await
            .map_err(Error::AwsSigning)?;
        let headers = signed
            .headers(AUTHORIZATION_KEY)
            .map(|(key, value)| Header { key, value })
            .collect();
        let json = serde_json::to_string(&SignedRequest {
            url: self.url.as_str(),
            method: CallerIdentitySigner::METHOD.as_str(),
            headers,
            body: "",
        })
        .expect("the request serializes");
        Ok(Secret(
            url::form_urlencoded::byte_serialize(json.as_bytes()).collect(),
        ))
    }
}

/// The region of the availability zone `file`'s `region_url` answers, with
/// an `IMDSv2` token from its `imdsv2_session_token_url` when it names one.
/// With `AWS_EC2_METADATA_DISABLED` set it is not asked, as the AWS chain
/// asks nothing of the instance metadata service then either.
async fn region_from_metadata(setup: &Setup<'_>, file: &SourceFile) -> Result<Region, Error> {
    let Some(region_url) = &file.region_url else {
        return Err(Error::NoAwsRegion);
    };
    if imds::disabled(&setup.host.env) {
        return Err(Error::AwsMetadataDisabled);
    }
    let region_url = parse_url(REGION_URL_FIELD, region_url)?;
    let token_url = file
        .imdsv2_session_token_url
        .as_deref()
        .map(|url| parse_url(IMDSV2_TOKEN_URL_FIELD, url))
        .transpose()?;
    let clients = setup.clients;
    let token = match token_url {
        Some(url) => {
            let token = send(clients, Call::AwsRegion, |client| {
                client
                    .put(url)
                    .timeout(imds::REQUEST_TIMEOUT)
                    .header(imds::TOKEN_TTL_HEADER, imds::TOKEN_TTL.as_secs())
            })
            .await?;
            Some(imds::session_token(&token).map_err(Error::AwsMetadataToken)?)
        }
        None => None,
    };
    let zone = send_text(clients, Call::AwsRegion, |client| {
        let request = client.get(region_url).timeout(imds::REQUEST_TIMEOUT);
        match token {
            Some(token) => request.header(imds::TOKEN_HEADER, token),
            None => request,
        }
    })
    .await?;
    let region = region_from_zone(&zone).ok_or(Error::NoAwsRegion)?;
    Region::parse(region).map_err(Error::AwsRegion)
}

/// The region of zone `zone`: the name up to its first part that is a
/// number, without the zone's letter. `us-east-1d` is in `us-east-1`, as
/// are the Local Zone `us-east-1-bos-1a` and the Wavelength Zone
/// `us-east-1-wl1-bos-wlz-1`, which upstream reads as regions that do not
/// exist. A name with no such part is taken as the region itself.
fn region_from_zone(zone: &str) -> Option<&str> {
    let zone = zone.trim();
    if zone.is_empty() {
        return None;
    }
    let mut start = 0;
    for part in zone.split('-') {
        if part.starts_with(|c: char| c.is_ascii_digit()) {
            let number = part.trim_end_matches(|c: char| c.is_ascii_alphabetic());
            if number.bytes().all(|b| b.is_ascii_digit()) {
                return Some(&zone[..start + number.len()]);
            }
            break;
        }
        start += part.len() + 1;
    }
    Some(zone)
}

/// The STS URL template, `{region}` replaced once the region is known.
#[derive(Debug)]
struct StsUrl(String);

impl StsUrl {
    /// `template`, or the default, with `https://` added to a bare host.
    /// Checked with a sample region, so a template bad for every region
    /// fails before the region lookup sends a request. One region can still
    /// fail to resolve where another does.
    fn parse(template: Option<&str>) -> Result<Self, Error> {
        let template = template.unwrap_or(DEFAULT_VERIFICATION_URL);
        let template = if template.starts_with("http") {
            template.to_owned()
        } else {
            format!("https://{template}")
        };
        let parsed = Self(template);
        parsed.resolve(&Region::parse("us-east-1").expect("a valid region"))?;
        Ok(parsed)
    }

    /// The URL for `region`.
    fn resolve(&self, region: &Region) -> Result<Url, Error> {
        parse_url(
            VERIFICATION_URL_FIELD,
            &self.0.replace("{region}", region.as_str()),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::super::tests::{Fixture, host};
    use super::*;
    use crate::aws::Platform;

    fn region(text: &str) -> Region {
        Region::parse(text).unwrap()
    }

    /// Upstream `test_parse_region_from_zone`, and Local and Wavelength
    /// Zones. Breaks if a zone keeps its letter, a region loses its digit,
    /// or a Local Zone's own name is kept.
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
            ("us-west-2-lax-1a", Some("us-west-2")),
            ("us-east-1-wl1-bos-wlz-1", Some("us-east-1")),
            ("ap-northeast-1-tpe-1a", Some("ap-northeast-1")),
        ] {
            assert_eq!(region_from_zone(zone), region, "{zone:?}");
        }
    }

    /// Upstream `test_resolve_sts_url` and `_invalid`. Breaks if the region
    /// is not substituted, a bare host gets no scheme, or a URL that is not
    /// http or https is accepted.
    #[test]
    fn resolves_the_sts_url() {
        let resolve = |template, at| {
            StsUrl::parse(template)
                .unwrap()
                .resolve(&region(at))
                .unwrap()
        };
        assert_eq!(
            resolve(None, "us-west-2").as_str(),
            "https://sts.us-west-2.amazonaws.com/?Action=GetCallerIdentity&Version=2011-06-15"
        );
        assert_eq!(
            resolve(Some("sts.{region}.amazonaws.com"), "eu-west-1").as_str(),
            "https://sts.eu-west-1.amazonaws.com/"
        );
        assert!(StsUrl::parse(Some("http://")).is_err());
        assert!(matches!(
            StsUrl::parse(Some("httpx:/sts")),
            Err(Error::UrlScheme {
                field: VERIFICATION_URL_FIELD,
                ..
            })
        ));
    }

    /// The zone request runs once, at setup, so a 503 is sent again.
    /// Breaks if a throttled lookup fails the registry.
    #[tokio::test]
    async fn retries_a_transient_region_lookup() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .respond_with(ResponseTemplate::new(200).set_body_string("us-east-1a"))
            .expect(1)
            .mount(&server)
            .await;
        let fixture = Fixture::new(host(Platform::Unix, &[], &[]));

        let found = region_from_metadata(
            &fixture.setup(),
            &file(Some(format!("{}/zone", server.uri())), None),
        )
        .await;

        assert_eq!(found.unwrap().as_str(), "us-east-1");
    }

    /// An instance metadata service that does not answer within its bound
    /// is asked once, as one behind a container hop limit never answers.
    /// Breaks if setup stalls retrying it.
    #[tokio::test]
    async fn gives_up_on_a_silent_region_lookup() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("us-east-1a")
                    .set_delay(imds::REQUEST_TIMEOUT + Duration::from_secs(1)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let fixture = Fixture::new(host(Platform::Unix, &[], &[]));

        let error = region_from_metadata(
            &fixture.setup(),
            &file(Some(format!("{}/zone", server.uri())), None),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, Error::Transport { .. }), "{error}");
        assert!(!error.is_transient());
    }

    fn file(region_url: Option<String>, imdsv2_url: Option<String>) -> SourceFile {
        SourceFile {
            _environment_id: IgnoredAny,
            region_url,
            regional_cred_verification_url: None,
            imdsv2_session_token_url: imdsv2_url,
        }
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
            .and(header(imds::TOKEN_TTL_HEADER, "21600"))
            .respond_with(ResponseTemplate::new(200).set_body_string("imds-token"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .and(header(imds::TOKEN_HEADER, "imds-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("us-gov-west-1a"))
            .expect(1)
            .mount(&server)
            .await;
        let fixture = Fixture::new(host(Platform::Unix, &[], &[]));

        let found = region_from_metadata(
            &fixture.setup(),
            &file(
                Some(format!("{}/zone", server.uri())),
                Some(format!("{}/latest/api/token", server.uri())),
            ),
        )
        .await
        .unwrap();

        assert_eq!(found, region("us-gov-west-1"));
    }

    /// A region lookup that cannot succeed fails at once: with no
    /// `region_url`, with one that is not a URL, with
    /// `AWS_EC2_METADATA_DISABLED` set, with no metadata service to connect
    /// to, with a session token no header can carry, or with a zone that
    /// names no region. Breaks if a bad value panics or is sent on, the
    /// variable is ignored, or a refused connection is sent again, which
    /// stalls a workstation with AWS profile credentials for seconds per
    /// registry.
    #[tokio::test]
    async fn fails_a_region_lookup_at_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("bad\ntoken"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not a zone"))
            .mount(&server)
            .await;
        let no_url: fn(&Error) -> bool = |e| matches!(e, Error::NoAwsRegion);
        let bad_url: fn(&Error) -> bool = |e| {
            matches!(
                e,
                Error::Url {
                    field: REGION_URL_FIELD,
                    ..
                }
            )
        };
        let disabled: fn(&Error) -> bool = |e| matches!(e, Error::AwsMetadataDisabled);
        let refused: fn(&Error) -> bool =
            |e| matches!(e, Error::Transport { .. }) && !e.is_transient();
        let bad_token: fn(&Error) -> bool = |e| matches!(e, Error::AwsMetadataToken(_));
        let bad_zone: fn(&Error) -> bool = |e| matches!(e, Error::AwsRegion(_));
        let unreachable = Some("http://127.0.0.1:1/zone".to_owned());
        let zone = Some(format!("{}/zone", server.uri()));
        let token = Some(format!("{}/token", server.uri()));
        for (vars, region_url, token_url, expected) in [
            (&[][..], None, None, no_url),
            (
                &[][..],
                Some("169.254.169.254/zone".to_owned()),
                None,
                bad_url,
            ),
            (
                &[("AWS_EC2_METADATA_DISABLED", "true")][..],
                unreachable.clone(),
                None,
                disabled,
            ),
            (&[][..], unreachable, None, refused),
            (&[][..], zone.clone(), token, bad_token),
            (&[][..], zone, None, bad_zone),
        ] {
            let fixture = Fixture::new(host(Platform::Unix, vars, &[]));
            let file = file(region_url, token_url);

            let error = region_from_metadata(&fixture.setup(), &file)
                .await
                .unwrap_err();

            assert!(expected(&error), "{file:?}: {error}");
        }
    }

    /// The region, profile and keys all come from the injected host. Breaks
    /// if the signer reads the process environment or the real AWS files.
    #[tokio::test]
    async fn signs_with_the_host_it_is_given() {
        let fixture = Fixture::new(host(
            Platform::Unix,
            &[("HOME", "/home/u"), ("AWS_REGION", "eu-west-3")],
            &[(
                "/home/u/.aws/credentials",
                b"[default]\naws_access_key_id = AKIDINJECTED\naws_secret_access_key = secret\n"
                    .to_vec(),
            )],
        ));

        let source = Source::new(&fixture.setup(), &file(None, None), "audience")
            .await
            .unwrap();
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
        let source = Source::with_signer(
            CallerIdentitySigner::example("us-east-1", Some("session-token")),
            &StsUrl::parse(None).unwrap(),
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
