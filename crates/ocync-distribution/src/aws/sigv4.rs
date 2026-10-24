//! AWS Signature Version 4 request signing, hashed and keyed with `aws-lc-rs`.
//!
//! Signs in the `Authorization` header, with the session token signed as
//! `x-amz-security-token`, URI path normalization on, the path encoded
//! twice, and no query: the endpoint never carries one. That is the form
//! every service ocync calls expects; S3 alone differs, and ocync does not
//! call it.

use std::fmt::{self, Write as _};
use std::time::SystemTime;

use aws_lc_rs::{digest, hmac};

use super::calendar::amz_date;

/// The signing algorithm named in the string to sign and the header.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The header carrying the request time, in [`AmzDate`] form.
const X_AMZ_DATE: &str = "x-amz-date";

/// The header carrying a temporary credential's session token.
const X_AMZ_SECURITY_TOKEN: &str = "x-amz-security-token";

/// The AWS key to sign with, borrowed from the credential that holds it.
#[derive(Clone, Copy)]
pub(super) struct AccessKey<'a> {
    /// The access key ID.
    pub(super) id: &'a str,
    /// The secret access key.
    pub(super) secret: &'a str,
    /// The session token of a temporary credential.
    pub(super) session_token: Option<&'a str>,
}

impl fmt::Debug for AccessKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The parts of an HTTP request that are signed.
pub(super) struct Request<'a> {
    /// The HTTP method.
    pub(super) method: &'a http::Method,
    /// The path as sent, already percent-encoded once.
    pub(super) path: &'a str,
    /// The headers to sign, including `host`. The signer adds
    /// [`X_AMZ_DATE`], [`X_AMZ_SECURITY_TOKEN`] and `authorization` itself.
    pub(super) headers: &'a [(&'a str, &'a str)],
    /// The request body.
    pub(super) body: &'a [u8],
}

/// Where a signature is valid: one region of one service.
pub(super) struct CredentialScope<'a> {
    /// The region, such as `us-east-1`.
    pub(super) region: &'a str,
    /// The signing name, such as `ecr`.
    pub(super) service: &'a str,
}

/// A request time in the `YYYYMMDDTHHMMSSZ` form AWS signs, in UTC.
struct AmzDate(String);

impl AmzDate {
    fn new(time: SystemTime) -> Self {
        Self(amz_date(time))
    }

    /// The `YYYYMMDD` date the credential scope names.
    fn date(&self) -> &str {
        &self.0[..8]
    }
}

impl fmt::Display for AmzDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Sign `request` and return the headers to add to it, in order.
pub(super) fn sign(
    request: &Request<'_>,
    key: AccessKey<'_>,
    scope: &CredentialScope<'_>,
    time: SystemTime,
) -> Vec<(&'static str, String)> {
    let date = AmzDate::new(time);
    let (canonical, signed_headers) = canonical_request(request, &date, key.session_token);
    let string_to_sign = string_to_sign(&canonical, &date, scope);
    let signature = signature(&signing_key(key.secret, &date, scope), &string_to_sign);
    let authorization = format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={signed_headers}, Signature={signature}",
        key.id,
        credential_scope(&date, scope),
    );
    let mut headers = vec![(X_AMZ_DATE, date.0)];
    if let Some(token) = key.session_token {
        headers.push((X_AMZ_SECURITY_TOKEN, token.to_owned()));
    }
    headers.push(("authorization", authorization));
    headers
}

/// The canonical request and its signed header list.
fn canonical_request(
    request: &Request<'_>,
    date: &AmzDate,
    session_token: Option<&str>,
) -> (String, String) {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), trim_all(value)))
        .chain([(X_AMZ_DATE.to_owned(), date.0.clone())])
        .chain(session_token.map(|t| (X_AMZ_SECURITY_TOKEN.to_owned(), t.to_owned())))
        .collect();
    // Stable, so repeated headers keep their order when joined below.
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let mut canonical_headers = String::new();
    let mut names: Vec<&str> = Vec::new();
    for (name, value) in &headers {
        if names.last() == Some(&name.as_str()) {
            canonical_headers.pop();
            canonical_headers.push(',');
        } else {
            names.push(name);
            canonical_headers.push_str(name);
            canonical_headers.push(':');
        }
        canonical_headers.push_str(value);
        canonical_headers.push('\n');
    }
    let signed_headers = names.join(";");

    // The empty line is the canonical query.
    let canonical = format!(
        "{}\n{}\n\n{canonical_headers}\n{signed_headers}\n{}",
        request.method,
        encode_path(&normalize_path(request.path)),
        hex::encode(digest::digest(&digest::SHA256, request.body)),
    );
    (canonical, signed_headers)
}

fn string_to_sign(canonical: &str, date: &AmzDate, scope: &CredentialScope<'_>) -> String {
    format!(
        "{ALGORITHM}\n{date}\n{}\n{}",
        credential_scope(date, scope),
        hex::encode(digest::digest(&digest::SHA256, canonical.as_bytes())),
    )
}

fn credential_scope(date: &AmzDate, scope: &CredentialScope<'_>) -> String {
    format!(
        "{}/{}/{}/aws4_request",
        date.date(),
        scope.region,
        scope.service
    )
}

/// The key is derived from the secret, the date, the region and the
/// service, so it cannot sign for any other scope.
fn signing_key(secret: &str, date: &AmzDate, scope: &CredentialScope<'_>) -> hmac::Key {
    let mut key = hmac::Key::new(hmac::HMAC_SHA256, format!("AWS4{secret}").as_bytes());
    for part in [date.date(), scope.region, scope.service, "aws4_request"] {
        key = hmac::Key::new(
            hmac::HMAC_SHA256,
            hmac::sign(&key, part.as_bytes()).as_ref(),
        );
    }
    key
}

fn signature(key: &hmac::Key, string_to_sign: &str) -> String {
    hex::encode(hmac::sign(key, string_to_sign.as_bytes()))
}

/// Header values are trimmed of spaces and inner runs of spaces become one.
/// Only spaces: a tab is part of the value, as in `aws-sigv4`.
fn trim_all(value: &str) -> String {
    value
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Remove `.` and `..` segments and empty segments (RFC 3986 section 5.2.4),
/// keeping a trailing slash.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut normalized = format!("/{}", segments.join("/"));
    let trailing = ["/", "/.", "/.."].iter().any(|s| path.ends_with(s));
    if trailing && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

/// Percent-encode every byte outside the RFC 3986 unreserved set but the
/// `/` separators.
fn encode_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    /// The time every vector in the suite signs at, 2015-08-30T12:36:00Z.
    const SUITE_TIME: u64 = 1_440_938_160;

    /// The vectors from the AWS signing test suite shipped in `aws-sigv4`
    /// 1.6.3, in `sigv4-test-suite/`, that apply to header signing with
    /// normalized, double-encoded paths. Left out: the unnormalized cases
    /// (S3 only), `sign_body` and `omit_session_token` cases (options ocync
    /// never sets), the query cases (an endpoint never carries one), and
    /// `get-space-normalized` and `get-utf8`, which expect a single-encoded
    /// path and which `aws-sigv4` also skips.
    const VECTORS: &[&str] = &[
        "get-header-key-duplicate",
        "get-header-value-multiline",
        "get-header-value-order",
        "get-header-value-trim",
        "get-relative-normalized",
        "get-relative-relative-normalized",
        "get-slash-dot-slash-normalized",
        "get-slash-normalized",
        "get-slash-pointless-dot-normalized",
        "get-slashes-normalized",
        "get-unreserved",
        "get-vanilla",
        "get-vanilla-with-session-token",
        "post-header-key-case",
        "post-header-key-sort",
        "post-header-value-case",
        "post-sts-header-before",
        "post-vanilla",
    ];

    fn read(test: &str, file: &str) -> String {
        let path = format!(
            "{}/src/aws/sigv4-test-suite/{test}/{file}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// A raw request as the suite writes it: request line, headers with
    /// folded continuation lines, a blank line, then the body. Bytes outside
    /// ASCII in the request line are percent-encoded, as a URL carries them.
    struct Raw {
        method: http::Method,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    fn parse(raw: &str) -> Raw {
        let (head, body) = raw.split_once("\n\n").unwrap_or((raw, ""));
        let mut lines = head.lines();
        let line = lines.next().unwrap();
        let (method, rest) = line.split_once(' ').unwrap();
        let target = rest.rsplit_once(' ').unwrap().0;
        let target: String = target
            .bytes()
            .map(|b| {
                if b.is_ascii() {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        assert!(!target.contains('?'), "{target}: the signer signs no query");
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            if line.starts_with(char::is_whitespace) {
                let last = headers.last_mut().unwrap();
                last.1.push(' ');
                last.1.push_str(line);
            } else {
                let (name, value) = line.split_once(':').unwrap();
                // The signer owns the time header, as it does in `aws-sigv4`.
                if !name.eq_ignore_ascii_case(X_AMZ_DATE) {
                    headers.push((name.to_string(), value.to_string()));
                }
            }
        }
        Raw {
            method: method.parse().unwrap(),
            path: target,
            headers,
            body: body.to_string(),
        }
    }

    #[derive(serde::Deserialize)]
    struct Context {
        credentials: ContextCredentials,
        region: String,
        service: String,
        timestamp: String,
    }

    #[derive(serde::Deserialize)]
    struct ContextCredentials {
        access_key_id: String,
        secret_access_key: String,
        token: Option<String>,
    }

    fn canonical(raw: &Raw, time: SystemTime, token: Option<&str>) -> String {
        let headers: Vec<(&str, &str)> = raw
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let request = Request {
            method: &raw.method,
            path: &raw.path,
            headers: &headers,
            body: raw.body.as_bytes(),
        };
        canonical_request(&request, &AmzDate::new(time), token).0
    }

    #[test]
    fn signs_the_aws_test_suite() {
        for test in VECTORS {
            let context: Context = serde_json::from_str(&read(test, "context.json")).unwrap();
            assert_eq!(context.timestamp, "2015-08-30T12:36:00Z", "{test}");
            let time = UNIX_EPOCH + Duration::from_secs(SUITE_TIME);
            let raw = parse(&read(test, "request.txt"));
            let token = context.credentials.token.as_deref();
            let scope = CredentialScope {
                region: &context.region,
                service: &context.service,
            };

            let canonical = canonical(&raw, time, token);
            assert_eq!(
                canonical,
                read(test, "header-canonical-request.txt"),
                "{test}: canonical request"
            );
            let date = AmzDate::new(time);
            let to_sign = string_to_sign(&canonical, &date, &scope);
            assert_eq!(
                to_sign,
                read(test, "header-string-to-sign.txt"),
                "{test}: string to sign"
            );
            assert_eq!(
                signature(
                    &signing_key(&context.credentials.secret_access_key, &date, &scope),
                    &to_sign
                ),
                read(test, "header-signature.txt"),
                "{test}: signature"
            );
            assert!(!context.credentials.access_key_id.is_empty());
        }
    }

    /// Non-S3 services take the path encoded twice: an `%3D` already in the
    /// path is signed as `%253D`. Signing it once is rejected by the service.
    #[test]
    fn encodes_the_path_twice() {
        // 2021-05-11T15:40:45Z, the time both vectors were signed at.
        let time = UNIX_EPOCH + Duration::from_secs(1_620_747_645);
        for test in ["double-encode-path", "double-url-encode"] {
            let raw = parse(&read(test, "request.txt"));
            assert_eq!(
                canonical(&raw, time, None),
                read(test, "header-canonical-request.txt"),
                "{test}"
            );
        }
    }

    /// The `authorization` header carries the access key, the scope, the
    /// signed header list and the signature, and a session token is sent
    /// as well as signed.
    #[test]
    fn returns_the_headers_to_send() {
        let key = AccessKey {
            id: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token: Some("6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267"),
        };
        let request = Request {
            method: &http::Method::GET,
            path: "/",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
        };
        let scope = CredentialScope {
            region: "us-east-1",
            service: "service",
        };
        let headers = sign(
            &request,
            key,
            &scope,
            UNIX_EPOCH + Duration::from_secs(SUITE_TIME),
        );
        assert_eq!(
            headers,
            [
                (X_AMZ_DATE, "20150830T123600Z".to_string()),
                (
                    X_AMZ_SECURITY_TOKEN,
                    "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267".to_string()
                ),
                (
                    "authorization",
                    "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
                     SignedHeaders=host;x-amz-date;x-amz-security-token, \
                     Signature=07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8"
                        .to_string()
                ),
            ]
        );
    }

    /// AWS folds runs of spaces only. Folding a tab as well, as a generic
    /// whitespace split does, signs a different value than the service.
    #[test]
    fn folds_spaces_but_keeps_tabs() {
        assert_eq!(trim_all("  a   b\tc  "), "a b\tc");
    }

    /// Leap days and year ends, where a hand-written date conversion breaks.
    #[test]
    fn formats_dates_across_calendar_edges() {
        for (secs, expected) in [
            (0, "19700101T000000Z"),
            (951_782_400, "20000229T000000Z"),
            (1_709_251_199, "20240229T235959Z"),
            (1_735_689_599, "20241231T235959Z"),
            (4_107_542_400, "21000301T000000Z"),
        ] {
            assert_eq!(
                AmzDate::new(UNIX_EPOCH + Duration::from_secs(secs)).to_string(),
                expected,
                "{secs}"
            );
        }
    }
}
