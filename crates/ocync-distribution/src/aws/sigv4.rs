//! AWS Signature Version 4 request signing, hashed and keyed with `aws-lc-rs`.
//!
//! Signs in the `Authorization` header, with the session token signed as
//! `x-amz-security-token`, URI path normalization on, and the path encoded
//! twice. That is the form every service ocync calls expects; S3 alone
//! differs, and ocync does not call it.

use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::{digest, hmac};

/// The signing algorithm named in the string to sign and the header.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The header carrying the request time, in [`format_date_time`] form.
const X_AMZ_DATE: &str = "x-amz-date";

/// The header carrying a temporary credential's session token.
const X_AMZ_SECURITY_TOKEN: &str = "x-amz-security-token";

/// AWS credentials to sign with.
#[derive(Clone)]
pub(crate) struct Credentials {
    /// The access key ID.
    pub(crate) access_key_id: String,
    /// The secret access key.
    pub(crate) secret_access_key: String,
    /// The session token of a temporary credential.
    pub(crate) session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// The parts of an HTTP request that are signed.
pub(crate) struct Request<'a> {
    /// The HTTP method.
    pub(crate) method: &'a http::Method,
    /// The path as sent, already percent-encoded once.
    pub(crate) path: &'a str,
    /// The query string as sent, without the `?`.
    pub(crate) query: Option<&'a str>,
    /// The headers to sign, including `host`. The signer adds
    /// [`X_AMZ_DATE`], [`X_AMZ_SECURITY_TOKEN`] and `authorization` itself.
    pub(crate) headers: &'a [(&'a str, &'a str)],
    /// The request body.
    pub(crate) body: &'a [u8],
}

/// Where a signature is valid: one region of one service.
pub(crate) struct Scope<'a> {
    /// The region, such as `us-east-1`.
    pub(crate) region: &'a str,
    /// The signing name, such as `ecr`.
    pub(crate) service: &'a str,
}

/// Sign `request` and return the headers to add to it, in order.
pub(crate) fn sign(
    request: &Request<'_>,
    credentials: &Credentials,
    scope: &Scope<'_>,
    time: SystemTime,
) -> Vec<(&'static str, String)> {
    let date_time = format_date_time(time);
    let (canonical, signed_headers) =
        canonical_request(request, &date_time, credentials.session_token.as_deref());
    let string_to_sign = string_to_sign(&canonical, &date_time, scope);
    let signature = signature(
        &string_to_sign,
        &credentials.secret_access_key,
        &date_time,
        scope,
    );
    let authorization = format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id,
        credential_scope(&date_time, scope),
    );
    let mut headers = vec![(X_AMZ_DATE, date_time)];
    if let Some(token) = &credentials.session_token {
        headers.push((X_AMZ_SECURITY_TOKEN, token.clone()));
    }
    headers.push(("authorization", authorization));
    headers
}

/// The canonical request and its signed header list.
fn canonical_request(
    request: &Request<'_>,
    date_time: &str,
    session_token: Option<&str>,
) -> (String, String) {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), trim_all(value)))
        .chain([(X_AMZ_DATE.to_string(), date_time.to_string())])
        .chain(session_token.map(|t| (X_AMZ_SECURITY_TOKEN.to_string(), t.to_string())))
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

    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method,
        encode(&normalize_path(request.path), Encode::Path),
        canonical_query(request.query.unwrap_or_default()),
        hex::encode(digest::digest(&digest::SHA256, request.body)),
    );
    (canonical, signed_headers)
}

fn string_to_sign(canonical: &str, date_time: &str, scope: &Scope<'_>) -> String {
    format!(
        "{ALGORITHM}\n{date_time}\n{}\n{}",
        credential_scope(date_time, scope),
        hex::encode(digest::digest(&digest::SHA256, canonical.as_bytes())),
    )
}

fn credential_scope(date_time: &str, scope: &Scope<'_>) -> String {
    format!(
        "{}/{}/{}/aws4_request",
        &date_time[..8],
        scope.region,
        scope.service
    )
}

/// The signing key is derived from the secret, the date, the region and the
/// service, so it cannot sign for any other scope.
fn signature(string_to_sign: &str, secret: &str, date_time: &str, scope: &Scope<'_>) -> String {
    let mut key = format!("AWS4{secret}").into_bytes();
    for part in [
        &date_time[..8],
        scope.region,
        scope.service,
        "aws4_request",
        string_to_sign,
    ] {
        key = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), part.as_bytes())
            .as_ref()
            .to_vec();
    }
    hex::encode(key)
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

/// Query parameters decoded, encoded again, and sorted by name then value.
fn canonical_query(query: &str) -> String {
    let mut params: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (encode(&k, Encode::Component), encode(&v, Encode::Component)))
        .collect();
    params.sort();
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Which bytes [`encode`] leaves as they are besides the unreserved set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encode {
    /// A path keeps its `/` separators.
    Path,
    /// A query name or value keeps nothing else.
    Component,
}

/// Percent-encode every byte outside the RFC 3986 unreserved set.
fn encode(value: &str, mode: Encode) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (mode == Encode::Path && byte == b'/')
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
fn format_date_time(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, from Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The time every vector in the suite signs at, 2015-08-30T12:36:00Z.
    const SUITE_TIME: u64 = 1_440_938_160;

    /// The vectors from the AWS signing test suite shipped in `aws-sigv4`
    /// 1.6.3, in `sigv4-test-suite/`, that apply to header signing with
    /// normalized, double-encoded paths. Left out: the unnormalized cases
    /// (S3 only), `sign_body` and `omit_session_token` cases (options ocync
    /// never sets), and `get-space-normalized` and `get-utf8`, which expect
    /// a single-encoded path and which `aws-sigv4` also skips.
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
        "get-vanilla-empty-query-key",
        "get-vanilla-query",
        "get-vanilla-query-order-encoded",
        "get-vanilla-query-order-key-case",
        "get-vanilla-query-unreserved",
        "get-vanilla-utf8-query",
        "get-vanilla-with-session-token",
        "post-header-key-case",
        "post-header-key-sort",
        "post-header-value-case",
        "post-sts-header-before",
        "post-vanilla",
        "post-vanilla-empty-query-value",
        "post-vanilla-query",
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
        query: Option<String>,
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
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (target, None),
        };
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
            path,
            query,
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
            query: raw.query.as_deref(),
            headers: &headers,
            body: raw.body.as_bytes(),
        };
        canonical_request(&request, &format_date_time(time), token).0
    }

    #[test]
    fn signs_the_aws_test_suite() {
        for test in VECTORS {
            let context: Context = serde_json::from_str(&read(test, "context.json")).unwrap();
            assert_eq!(context.timestamp, "2015-08-30T12:36:00Z", "{test}");
            let time = UNIX_EPOCH + Duration::from_secs(SUITE_TIME);
            let raw = parse(&read(test, "request.txt"));
            let token = context.credentials.token.as_deref();
            let scope = Scope {
                region: &context.region,
                service: &context.service,
            };

            let canonical = canonical(&raw, time, token);
            assert_eq!(
                canonical,
                read(test, "header-canonical-request.txt"),
                "{test}: canonical request"
            );
            let date_time = format_date_time(time);
            let to_sign = string_to_sign(&canonical, &date_time, &scope);
            assert_eq!(
                to_sign,
                read(test, "header-string-to-sign.txt"),
                "{test}: string to sign"
            );
            assert_eq!(
                signature(
                    &to_sign,
                    &context.credentials.secret_access_key,
                    &date_time,
                    &scope
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
        let credentials = Credentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: Some(
                "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267".to_string(),
            ),
        };
        let request = Request {
            method: &http::Method::GET,
            path: "/",
            query: None,
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
        };
        let scope = Scope {
            region: "us-east-1",
            service: "service",
        };
        let headers = sign(
            &request,
            &credentials,
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
                format_date_time(UNIX_EPOCH + Duration::from_secs(secs)),
                expected,
                "{secs}"
            );
        }
    }
}
