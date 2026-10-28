//! The IAM Identity Center login token the AWS CLI caches in
//! `~/.aws/sso/cache`, read and refreshed as `aws-config` 1.13.0 does.
//!
//! A session's token is checked for expiry and, within five minutes of it,
//! refreshed through SSO OIDC and written back where the AWS CLI reads it.
//! A process refreshes a token once: loads of one cache file take turns and
//! each reads the file again, so a load that waited sees the token the one
//! before it wrote. A failed refresh keeps the cached token.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use aws_lc_rs::digest;
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::super::config::Config;
use super::super::endpoint::{CredentialApi, Region};
use super::super::host::Host;
use super::super::profile::expand_home;
use super::super::{ApiRequest, ConfigError, Failure, calendar, retry};
use super::{credential_api, json};
use crate::error::error_chain;

/// How long before its expiry a token is refreshed.
const REFRESH_BUFFER: Duration = Duration::from_secs(5 * 60);

/// The least time between two refreshes of one token.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a refresh may take, well under the chain's credential
/// timeout, so a refresh that hangs still leaves time to use the cached
/// token.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const _: () =
    assert!(REFRESH_TIMEOUT.as_millis() < super::super::CREDENTIALS_LOAD_TIMEOUT.as_millis() / 2);

/// The path of SSO OIDC's `CreateToken`.
const CREATE_TOKEN_PATH: &str = "/token";

/// The content type of a `CreateToken` request.
const JSON: &str = "application/json";

/// The grant a refresh asks for.
const REFRESH_GRANT: &str = "refresh_token";

/// The cache file's fields, as the AWS CLI names them.
const ACCESS_TOKEN: &str = "accessToken";
const EXPIRES_AT: &str = "expiresAt";
const REFRESH_TOKEN: &str = "refreshToken";
const CLIENT_ID: &str = "clientId";
const CLIENT_SECRET: &str = "clientSecret";
const REGISTRATION_EXPIRES_AT: &str = "registrationExpiresAt";
const REGION: &str = "region";
const START_URL: &str = "startUrl";

/// Where a profile's login token comes from.
#[derive(Debug, Clone, Copy)]
pub(super) enum Login<'a> {
    /// An `[sso-session]`: its token is cached under the session's name,
    /// checked for expiry and refreshed.
    Session { name: &'a str, start_url: &'a str },
    /// A profile with its own `sso_start_url`: its token is cached under
    /// that URL and neither checked for expiry nor refreshed.
    Legacy { start_url: &'a str },
}

/// A login token, the bearer of a role credentials call.
pub(super) struct AccessToken(String);

impl AccessToken {
    /// The token's text.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A cached login token.
struct Cached {
    access_token: AccessToken,
    expires_at: SystemTime,
    /// What a refresh needs, when the file holds all of it.
    registration: Option<Registration>,
    /// The whole file, so a write-back keeps the fields it does not set.
    document: Map<String, Value>,
}

/// The OIDC client registration and refresh token a refresh is made with.
struct Registration {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    expires_at: SystemTime,
}

/// Why the cache file gave no token. The texts are upstream's.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum CacheError {
    /// No home directory to find the cache under.
    #[error("couldn't resolve a home directory")]
    NoHome,
    /// The file could not be read.
    #[error("failed to read `{}`", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The file is not a JSON object.
    #[error("invalid JSON in cached SSO token file")]
    Json(#[source] serde_json::Error),
    /// A field a token needs is missing, or not a string.
    #[error("missing field `{0}` in cached SSO token file")]
    Missing(&'static str),
    /// A time is not an RFC 3339 time in UTC.
    #[error("invalid value for the `{0}` field in the cached SSO token file")]
    Invalid(&'static str),
}

/// Why no login token was loaded. The texts are upstream's.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// A legacy profile's token did not load.
    #[error(transparent)]
    Legacy(CacheError),
    /// A session's token did not load.
    #[error("failed to load the cached SSO token")]
    Session(#[source] CacheError),
    /// A session's token has expired and cannot be refreshed.
    #[error("the SSO token has expired and cannot be refreshed")]
    Expired,
}

/// Why a refresh gave no token. Logged: the cached token is kept.
#[derive(Debug, thiserror::Error)]
enum RefreshError {
    /// The SSO OIDC client could not be set up.
    #[error("the SSO OIDC client could not be set up")]
    Setup(#[source] ConfigError),
    /// The call failed, after its retries.
    #[error("SSO OIDC CreateToken failed")]
    Call(#[source] Failure),
    /// The answer holds no token, or an expiry that is not a count of
    /// seconds.
    #[error("SSO OIDC CreateToken answer could not be read")]
    Answer(#[source] serde_json::Error),
    /// The refresh did not finish within [`REFRESH_TIMEOUT`].
    #[error("SSO OIDC CreateToken timed out after {} seconds", REFRESH_TIMEOUT.as_secs_f64())]
    TimedOut,
}

/// When this process last tried to refresh a token, if it has.
#[derive(Debug, Default)]
struct LastRefresh(Option<SystemTime>);

/// The refresh state of each cached session token, by cache file. Shared by
/// every chain in a process, so one token is refreshed once. Synchronized,
/// not `Rc<RefCell<_>>`, because the AWS client is a `Send + Sync` leaf. The
/// outer lock is never held across an `.await`; a token's own lock is held
/// across its refresh, so loads of it take turns.
#[derive(Debug, Clone, Default)]
pub(in crate::aws) struct Sessions(
    Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<LastRefresh>>>>>,
);

impl Sessions {
    /// The state this process shares.
    pub(in crate::aws) fn process() -> Self {
        static PROCESS: OnceLock<Sessions> = OnceLock::new();
        PROCESS.get_or_init(Self::default).clone()
    }

    /// The state of the token cached at `path`.
    fn of(&self, path: &Path) -> Arc<tokio::sync::Mutex<LastRefresh>> {
        let mut sessions = self.0.lock().expect("no panic while held");
        Arc::clone(sessions.entry(path.to_owned()).or_default())
    }
}

/// What to do with a session's cached token.
enum Decision<'a> {
    /// Use it as it is.
    Use,
    /// Refresh it with this registration.
    Refresh(&'a Registration),
    /// Fail: it has expired and cannot be refreshed.
    Expired,
}

impl<'a> Decision<'a> {
    /// What upstream does with `cached` at `now`, given the `last` refresh
    /// attempt: refresh it within [`REFRESH_BUFFER`] of its expiry when its
    /// registration holds and [`MIN_REFRESH_INTERVAL`] has passed, and fail
    /// only when it has expired and cannot be refreshed.
    fn of(cached: &'a Cached, last: &LastRefresh, now: SystemTime) -> Self {
        let interval_passed = last.0.is_none_or(|at| {
            now.duration_since(at)
                .is_ok_and(|since| since >= MIN_REFRESH_INTERVAL)
        });
        let refreshable = cached
            .registration
            .as_ref()
            .filter(|registration| registration.expires_at > now && interval_passed);
        let expires_soon = cached
            .expires_at
            .checked_sub(REFRESH_BUFFER)
            .is_none_or(|at| at <= now);
        match refreshable {
            None if cached.expires_at <= now => Self::Expired,
            Some(registration) if expires_soon => Self::Refresh(registration),
            _ => Self::Use,
        }
    }
}

/// The login token `login` names, refreshed through SSO OIDC in `region`
/// when it is about to expire, with the settings `config` holds and the
/// `retry` settings of the calls the chain makes.
pub(super) async fn load(
    host: &Host,
    config: &Config,
    retry: &retry::Settings,
    login: Login<'_>,
    region: &Region,
) -> Result<AccessToken, Error> {
    let (name, start_url) = match login {
        Login::Legacy { start_url } => {
            let path = cache_path(host, start_url).map_err(Error::Legacy)?;
            return Ok(read(host, &path).map_err(Error::Legacy)?.access_token);
        }
        Login::Session { name, start_url } => (name, start_url),
    };
    let path = cache_path(host, name).map_err(Error::Session)?;
    let state = host.sso_sessions.of(&path);
    let mut last = state.lock().await;
    let cached = read(host, &path).map_err(Error::Session)?;
    let now = host.clock.now();
    let registration = match Decision::of(&cached, &last, now) {
        Decision::Use => return Ok(cached.access_token),
        Decision::Expired => return Err(Error::Expired),
        Decision::Refresh(registration) => registration,
    };
    tracing::debug!("refreshing the SSO token");
    // Recorded first, so a load cancelled mid-refresh still spaces the next.
    *last = LastRefresh(Some(now));
    let refreshed = tokio::time::timeout(
        REFRESH_TIMEOUT,
        refresh(host, config, retry, region, registration),
    )
    .await
    .unwrap_or(Err(RefreshError::TimedOut));
    match refreshed {
        Ok(refreshed) => {
            let token = AccessToken(refreshed.access_token.clone());
            write_back(host, &path, cached, refreshed, now, region, start_url);
            Ok(token)
        }
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "SSO token refresh failed, using the cached token");
            Ok(cached.access_token)
        }
    }
}

/// The cache file for `key`: `~/.aws/sso/cache/`, then the lowercase hex
/// SHA-1 of `key`, then `.json`.
fn cache_path(host: &Host, key: &str) -> Result<PathBuf, CacheError> {
    let home = host
        .platform
        .home_dir(&host.env)
        .ok_or(CacheError::NoHome)?;
    let digest = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, key.as_bytes());
    let path = format!("~/.aws/sso/cache/{}.json", hex::encode(digest));
    Ok(PathBuf::from(expand_home(
        &path,
        Some(&home),
        host.platform,
    )))
}

/// The token cached at `path`.
fn read(host: &Host, path: &Path) -> Result<Cached, CacheError> {
    let bytes = host.fs.read(path).map_err(|source| CacheError::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&bytes)
}

/// The token in the cache file `bytes`, its fields found as
/// [`json::field`] finds them: a field of another JSON type counts as
/// missing. Times are RFC 3339 in UTC.
fn parse(bytes: &[u8]) -> Result<Cached, CacheError> {
    let document: Map<String, Value> = serde_json::from_slice(bytes).map_err(CacheError::Json)?;
    let text = |name: &'static str| json::field(&document, &[name], Value::as_str);
    let time = |name: &'static str| {
        text(name)
            .map(|time| calendar::parse_rfc3339(time).ok_or(CacheError::Invalid(name)))
            .transpose()
    };
    let access_token = text(ACCESS_TOKEN).ok_or(CacheError::Missing(ACCESS_TOKEN))?;
    let expires_at = time(EXPIRES_AT)?.ok_or(CacheError::Missing(EXPIRES_AT))?;
    let registration_expires_at = time(REGISTRATION_EXPIRES_AT)?;
    let registration = (|| {
        Some(Registration {
            client_id: text(CLIENT_ID)?.to_owned(),
            client_secret: text(CLIENT_SECRET)?.to_owned(),
            refresh_token: text(REFRESH_TOKEN)?.to_owned(),
            expires_at: registration_expires_at?,
        })
    })();
    Ok(Cached {
        access_token: AccessToken(access_token.to_owned()),
        expires_at,
        registration,
        document,
    })
}

/// A `CreateToken` request body, fields in the SDK's order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateToken<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

/// A `CreateToken` answer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Refreshed {
    access_token: String,
    /// Seconds from now. The SDK reads a missing one as 0.
    #[serde(default)]
    expires_in: u32,
    /// A new refresh token, when the service rotates it.
    refresh_token: Option<String>,
}

/// A new token for `registration`, from SSO OIDC in `region`.
async fn refresh(
    host: &Host,
    config: &Config,
    retry: &retry::Settings,
    region: &Region,
    registration: &Registration,
) -> Result<Refreshed, RefreshError> {
    let api = credential_api(host, config, retry, CredentialApi::SsoOidc, region)
        .map_err(RefreshError::Setup)?;
    let body = serde_json::to_vec(&CreateToken {
        client_id: &registration.client_id,
        client_secret: &registration.client_secret,
        grant_type: REFRESH_GRANT,
        refresh_token: &registration.refresh_token,
    })
    .expect("a CreateToken request serializes");
    let request = ApiRequest {
        operation: "CreateToken",
        method: http::Method::POST,
        path: CREATE_TOKEN_PATH,
        headers: &[(CONTENT_TYPE, JSON)],
        body: Bytes::from(body),
        modeled_errors: &[],
    };
    let answer = api
        .call_unsigned(&request, &[])
        .await
        .map_err(RefreshError::Call)?;
    serde_json::from_slice(&answer).map_err(RefreshError::Answer)
}

/// Write `refreshed` over the token cached at `path`, as upstream writes
/// it: its token and expiry from `now`, the refresh token when a new one
/// came, and the session's `region` and `start_url`. Every other field of
/// the file is kept. A file that cannot be written leaves the AWS CLI the
/// old token, with a warning.
fn write_back(
    host: &Host,
    path: &Path,
    cached: Cached,
    refreshed: Refreshed,
    now: SystemTime,
    region: &Region,
    start_url: &str,
) {
    let mut document = cached.document;
    let expires_at = now + Duration::from_secs(refreshed.expires_in.into());
    set(&mut document, ACCESS_TOKEN, refreshed.access_token);
    set(&mut document, EXPIRES_AT, calendar::rfc3339(expires_at));
    if let Some(refresh_token) = refreshed.refresh_token {
        set(&mut document, REFRESH_TOKEN, refresh_token);
    }
    set(&mut document, REGION, region.as_str());
    set(&mut document, START_URL, start_url);
    let bytes = serde_json::to_vec(&document).expect("a JSON object serializes");
    if let Err(e) = host.fs.replace_private(path, &bytes) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "refreshed SSO token not written back, the AWS CLI keeps the old one"
        );
    }
}

/// Set `name` in `document`, replacing every key that differs from it only
/// in case, as the reader takes any of them.
fn set(document: &mut Map<String, Value>, name: &str, value: impl Into<Value>) {
    document.retain(|key, _| !key.eq_ignore_ascii_case(name));
    document.insert(name.to_owned(), value.into());
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::num::NonZeroU32;

    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::aws::host::{Clock, Env, Fs, Network};
    use crate::test_logs::capture_logs;

    /// The session every test loads, as upstream's tests name it.
    const SESSION: Login<'static> = Login::Session {
        name: "test",
        start_url: "https://d-123.awsapps.com/start",
    };

    /// Where [`SESSION`]'s token is cached under the home directory.
    const SESSION_FILE: &str = ".aws/sso/cache/a94a8fe5ccb19ba61c4c0873d391e987982fbbd3.json";

    /// A cached token for [`SESSION`] that can be refreshed, expiring at
    /// `expires_at`.
    fn refreshable(expires_at: &str) -> String {
        serde_json::json!({
            "startUrl": "https://d-123.awsapps.com/start",
            "region": "us-west-2",
            "accessToken": "cachedtoken",
            "expiresAt": expires_at,
            "clientId": "clientid",
            "clientSecret": "YSBzZWNyZXQ=",
            "registrationExpiresAt": "2022-12-25T13:30:00Z",
            "refreshToken": "cachedrefreshtoken",
        })
        .to_string()
    }

    fn at(text: &str) -> SystemTime {
        calendar::parse_rfc3339(text).unwrap()
    }

    /// A Unix host with home `/home/user`, `files` under it, and the clock
    /// at `now`, whose network fails the test on any request.
    fn fixed_host(files: &[(&str, &str)], now: &str) -> Host {
        Host {
            env: Env::Fixed(HashMap::from([(
                "HOME".to_owned(),
                "/home/user".to_owned(),
            )])),
            fs: Fs::Fixed(
                files
                    .iter()
                    .map(|(file, contents)| {
                        (
                            Path::new("/home/user").join(file),
                            contents.as_bytes().to_vec(),
                        )
                    })
                    .collect(),
            ),
            clock: Clock::Fixed(at(now)),
            ..Host::fixed()
        }
    }

    /// A home directory on the real file system holding [`SESSION`]'s
    /// token, removed when dropped.
    struct Home(PathBuf);

    impl Home {
        fn new(test: &str, token: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ocync-sso-{}-{test}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let home = Self(dir);
            let file = home.file();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, token).unwrap();
            home
        }

        fn file(&self) -> PathBuf {
            self.0.join(SESSION_FILE)
        }

        fn document(&self) -> Map<String, Value> {
            serde_json::from_slice(&std::fs::read(self.file()).unwrap()).unwrap()
        }

        /// A host on this home, the real network with SSO OIDC at `oidc`,
        /// and the clock at `now`.
        fn host(&self, oidc: &MockServer, now: &str) -> Host {
            Host {
                env: Env::Fixed(HashMap::from([
                    ("HOME".to_owned(), self.0.display().to_string()),
                    ("AWS_ENDPOINT_URL_SSO_OIDC".to_owned(), oidc.uri()),
                ])),
                fs: Fs::Real,
                clock: Clock::Fixed(at(now)),
                network: Network::Real,
                ..Host::fixed()
            }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Load [`SESSION`]'s token on `host`, one attempt per call.
    async fn load_on(host: &Host, login: Login<'_>) -> Result<AccessToken, Error> {
        let retry = retry::Settings {
            max_attempts: NonZeroU32::MIN,
            ..retry::Settings::default()
        };
        let region = Region::parse("us-west-2").unwrap();
        load(host, &Config::load(host, None), &retry, login, &region).await
    }

    fn created(access_token: &str, refresh_token: Option<&str>) -> ResponseTemplate {
        let mut body = serde_json::json!({
            "tokenType": "Bearer",
            "accessToken": access_token,
            "expiresIn": 28800,
        });
        if let Some(refresh_token) = refresh_token {
            body["refreshToken"] = refresh_token.into();
        }
        ResponseTemplate::new(200).set_body_json(body)
    }

    /// The cache file is named for the session, or a legacy profile's start
    /// URL, by its SHA-1, under the home directory however it ends. Breaks
    /// if the hash, its case or the directory differs from upstream's.
    #[test]
    fn finds_the_cache_file_as_upstream_does() {
        let cases = [
            (
                "admin",
                "/home/someuser",
                "/home/someuser/.aws/sso/cache/d033e22ae348aeb5660fc2140aec35850c4da997.json",
            ),
            (
                "dev-scopes",
                "/home/someuser",
                "/home/someuser/.aws/sso/cache/75e4d41276d8bd17f85986fc6cccef29fd725ce3.json",
            ),
            (
                "https://d-92671207e4.awsapps.com/start",
                "/home/me",
                "/home/me/.aws/sso/cache/13f9d35043871d073ab260e020f0ffde092cb14b.json",
            ),
            (
                "https://d-92671207e4.awsapps.com/start",
                "/home/me/",
                "/home/me/.aws/sso/cache/13f9d35043871d073ab260e020f0ffde092cb14b.json",
            ),
            (
                "dev",
                "/home",
                "/home/.aws/sso/cache/34c6fceca75e456f25e7e99531e2425c6c1de443.json",
            ),
            (
                "https://ssotest.awsapps.com/start",
                "/home",
                "/home/.aws/sso/cache/dace00cba5f8355ec9d274ceb2bcebdfbeed0e12.json",
            ),
        ];
        for (key, home, expected) in cases {
            let host = Host {
                env: Env::Fixed(HashMap::from([("HOME".to_owned(), home.to_owned())])),
                ..Host::fixed()
            };
            assert_eq!(
                cache_path(&host, key).unwrap(),
                Path::new(expected),
                "{key}"
            );
        }
        let err = cache_path(&Host::fixed(), "admin").unwrap_err();
        assert_eq!(err.to_string(), "couldn't resolve a home directory");
    }

    /// The cache file is read as upstream reads it: keys in any case, a
    /// registration only when all four of its fields are there, and each
    /// bad file refused with upstream's reason. Breaks if a partial
    /// registration reads as refreshable or a bad time is skipped.
    #[test]
    fn reads_the_cache_file_as_upstream_does() {
        let full = parse(refreshable("2021-12-25T21:30:00Z").as_bytes()).unwrap();
        assert_eq!(full.access_token.as_str(), "cachedtoken");
        assert_eq!(full.expires_at, at("2021-12-25T21:30:00Z"));
        let registration = full.registration.unwrap();
        assert_eq!(
            (
                registration.client_id.as_str(),
                registration.client_secret.as_str(),
                registration.refresh_token.as_str(),
                registration.expires_at,
            ),
            (
                "clientid",
                "YSBzZWNyZXQ=",
                "cachedrefreshtoken",
                at("2022-12-25T13:30:00Z")
            )
        );
        let minimal =
            parse(br#"{"ACCESSTOKEN": "t", "expiresat": "2021-12-25T21:30:00Z"}"#).unwrap();
        assert_eq!(minimal.access_token.as_str(), "t");
        assert!(minimal.registration.is_none());
        let mut partial: Map<String, Value> =
            serde_json::from_str(&refreshable("2021-12-25T21:30:00Z")).unwrap();
        partial.remove(REFRESH_TOKEN);
        let partial = parse(&serde_json::to_vec(&partial).unwrap()).unwrap();
        assert!(partial.registration.is_none());

        for (file, error) in [
            (
                r#"{"accessToken": "t", "expiresAt": "notatimestamp"}"#,
                "invalid value for the `expiresAt` field in the cached SSO token file",
            ),
            (
                r#"{"expiresAt": "notatimestamp"}"#,
                "missing field `accessToken` in cached SSO token file",
            ),
            (
                r#"{"accessToken": 7, "expiresAt": "2021-12-25T21:30:00Z"}"#,
                "missing field `accessToken` in cached SSO token file",
            ),
            (
                r#"{"accessToken": "t"}"#,
                "missing field `expiresAt` in cached SSO token file",
            ),
            (
                r#"{"accessToken": "t", "expiresAt": "2021-12-25T21:30:00Z",
                    "registrationExpiresAt": "soon"}"#,
                "invalid value for the `registrationExpiresAt` field in the cached SSO token file",
            ),
            ("[]", "invalid JSON in cached SSO token file"),
        ] {
            let message = parse(file.as_bytes()).err().unwrap().to_string();
            assert_eq!(message, error, "{file}");
        }
    }

    /// A token that is not about to expire is used as cached, an expired
    /// one with no live registration is refused, and a legacy profile's is
    /// never checked. Breaks if a token is refreshed early, an expired one
    /// is sent, or a legacy token is refused for its expiry.
    #[tokio::test]
    async fn checks_a_session_token_for_expiry_alone() {
        let token = r#"{"accessToken": "some-token", "expiresAt": "1975-01-01T00:00:00Z"}"#;
        let host = fixed_host(&[(SESSION_FILE, token)], "1974-12-25T00:00:00Z");
        assert_eq!(
            load_on(&host, SESSION).await.unwrap().as_str(),
            "some-token"
        );

        let host = fixed_host(&[(SESSION_FILE, token)], "2023-01-01T00:00:00Z");
        let err = load_on(&host, SESSION).await.err().unwrap();
        assert!(matches!(err, Error::Expired), "{err}");

        let lapsed = refreshable("2021-10-25T13:00:00Z")
            .replace("2022-12-25T13:30:00Z", "2021-11-25T13:30:00Z");
        let host = fixed_host(&[(SESSION_FILE, &lapsed)], "2023-08-11T04:11:17Z");
        let err = load_on(&host, SESSION).await.err().unwrap();
        assert_eq!(
            err.to_string(),
            "the SSO token has expired and cannot be refreshed"
        );

        let legacy = Login::Legacy {
            start_url: "https://ssotest.awsapps.com/start",
        };
        let legacy_file = ".aws/sso/cache/dace00cba5f8355ec9d274ceb2bcebdfbeed0e12.json";
        let host = fixed_host(&[(legacy_file, token)], "2023-01-01T00:00:00Z");
        assert_eq!(load_on(&host, legacy).await.unwrap().as_str(), "some-token");
    }

    /// A missing file is reported with its path, wrapped for a session as
    /// upstream wraps it. Breaks if a missing token reads as no token.
    #[tokio::test]
    async fn names_a_missing_cache_file() {
        let host = fixed_host(&[], "2023-01-01T00:00:00Z");
        let path = format!("/home/user/{SESSION_FILE}");
        let err = load_on(&host, SESSION).await.err().unwrap();
        assert_eq!(
            error_chain(&err),
            format!(
                "failed to load the cached SSO token: failed to read `{path}`: No such file or directory (os error 2)"
            )
        );
        let legacy = Login::Legacy { start_url: "test" };
        let err = load_on(&host, legacy).await.err().unwrap();
        assert_eq!(
            error_chain(&err),
            format!("failed to read `{path}`: No such file or directory (os error 2)")
        );
    }

    /// A token about to expire is refreshed with its registration and
    /// written back owner-only: the new token, its expiry, the new refresh
    /// token, the session's region and start URL, every other field kept,
    /// and a key in another case replaced. Breaks if the request differs
    /// from the SDK's, a field is lost, or a stale key survives.
    #[cfg(unix)]
    #[tokio::test]
    async fn refreshes_a_token_and_writes_it_back() {
        use std::os::unix::fs::PermissionsExt as _;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("content-type", JSON))
            .and(body_json(serde_json::json!({
                "clientId": "clientid",
                "clientSecret": "YSBzZWNyZXQ=",
                "grantType": "refresh_token",
                "refreshToken": "cachedrefreshtoken",
            })))
            .respond_with(created("newtoken", Some("newrefreshtoken")))
            .expect(1)
            .mount(&server)
            .await;
        let mut token: Map<String, Value> =
            serde_json::from_str(&refreshable("2021-12-25T13:00:00Z")).unwrap();
        token.insert("AccessToken".to_owned(), "stale".into());
        token.insert("unknown".to_owned(), 1.into());
        token.insert("region".to_owned(), "us-east-1".into());
        let home = Home::new("refresh", &Value::Object(token).to_string());

        let host = home.host(&server, "2021-12-25T13:30:00Z");
        assert_eq!(load_on(&host, SESSION).await.unwrap().as_str(), "newtoken");

        let expected: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "accessToken": "newtoken",
            "expiresAt": "2021-12-25T21:30:00Z",
            "refreshToken": "newrefreshtoken",
            "clientId": "clientid",
            "clientSecret": "YSBzZWNyZXQ=",
            "registrationExpiresAt": "2022-12-25T13:30:00Z",
            "region": "us-west-2",
            "startUrl": "https://d-123.awsapps.com/start",
            "unknown": 1,
        }))
        .unwrap();
        assert_eq!(home.document(), expected);
        let mode = std::fs::metadata(home.file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// With no new refresh token the cached one is kept, and a failed
    /// refresh, an error or an answer with no token, leaves the file as it
    /// was and the cached token in use.
    /// Breaks if a refresh drops the refresh token, or a failure fails the
    /// load or touches the file.
    #[tokio::test]
    async fn keeps_what_a_refresh_does_not_replace() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(created("newtoken", None))
            .expect(1)
            .mount(&server)
            .await;
        let home = Home::new("keep", &refreshable("2021-12-25T13:00:00Z"));
        let host = home.host(&server, "2021-12-25T13:30:00Z");
        assert_eq!(load_on(&host, SESSION).await.unwrap().as_str(), "newtoken");
        assert_eq!(home.document()[REFRESH_TOKEN], "cachedrefreshtoken");

        let failures = [
            ("error", ResponseTemplate::new(500)),
            (
                "malformed",
                ResponseTemplate::new(200).set_body_string("{}"),
            ),
        ];
        for (name, failure) in failures {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(failure)
                .expect(1)
                .mount(&server)
                .await;
            let cached = refreshable("2021-12-25T13:00:00Z");
            let home = Home::new(name, &cached);
            let host = home.host(&server, "2021-12-25T13:30:00Z");
            let (token, logs) = capture_logs(load_on(&host, SESSION)).await;
            assert_eq!(token.unwrap().as_str(), "cachedtoken", "{name}");
            assert_eq!(std::fs::read_to_string(home.file()).unwrap(), cached);
            assert!(logs.contains("SSO token refresh failed"), "{name}: {logs}");
        }
    }

    /// Upstream's refresh timeline: no refresh outside the five minute
    /// buffer, none within 30 seconds of the last attempt, the cached token
    /// kept through two failures, then the refreshed one. Breaks if either
    /// interval is off or a failed attempt is not counted.
    #[tokio::test]
    async fn spaces_refreshes_as_upstream_does() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(created("second_token", None))
            .expect(1)
            .mount(&server)
            .await;
        let token = refreshable("2023-01-01T00:10:00Z")
            .replace("2022-12-25T13:30:00Z", "2023-01-02T12:00:00Z")
            .replace("cachedtoken", "first_token");
        let home = Home::new("timeline", &token);
        let sessions = Sessions::default();
        for (now, expected) in [
            ("2023-01-01T00:00:00Z", "first_token"),
            ("2023-01-01T00:03:00Z", "first_token"),
            ("2023-01-01T00:05:00Z", "first_token"),
            ("2023-01-01T00:05:15Z", "first_token"),
            ("2023-01-01T00:05:30Z", "first_token"),
            ("2023-01-01T00:06:00Z", "second_token"),
        ] {
            let host = Host {
                sso_sessions: sessions.clone(),
                ..home.host(&server, now)
            };
            assert_eq!(
                load_on(&host, SESSION).await.unwrap().as_str(),
                expected,
                "{now}"
            );
        }
        assert_eq!(home.document()[EXPIRES_AT], "2023-01-01T08:06:00Z");
    }

    /// A refresh that hangs is given up after [`REFRESH_TIMEOUT`] and the
    /// still valid cached token is used. Breaks if the load waits on the
    /// refresh, which the chain's timeout would cut off with no token.
    #[tokio::test(start_paused = true)]
    async fn uses_the_cached_token_when_a_refresh_hangs() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(created("newtoken", None).set_delay(Duration::from_secs(3600)))
            .mount(&server)
            .await;
        let home = Home::new("hangs", &refreshable("2021-12-25T13:33:00Z"));
        let host = home.host(&server, "2021-12-25T13:30:00Z");
        let started = tokio::time::Instant::now();
        assert_eq!(
            load_on(&host, SESSION).await.unwrap().as_str(),
            "cachedtoken"
        );
        assert_eq!(started.elapsed(), REFRESH_TIMEOUT);
    }

    /// A load cut off mid-refresh still counts the attempt, so a load right
    /// after uses the cached token rather than refreshing again. Breaks if
    /// the attempt is counted only once the refresh returns.
    #[tokio::test(start_paused = true)]
    async fn counts_a_refresh_its_load_was_cut_off_in() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(created("newtoken", None).set_delay(Duration::from_secs(3600)))
            .mount(&server)
            .await;
        let home = Home::new("cut", &refreshable("2021-12-25T13:33:00Z"));
        let host = home.host(&server, "2021-12-25T13:30:00Z");
        let cut = tokio::time::timeout(Duration::from_secs(1), load_on(&host, SESSION)).await;
        assert!(cut.is_err(), "the refresh answered");
        let started = tokio::time::Instant::now();
        assert_eq!(
            load_on(&host, SESSION).await.unwrap().as_str(),
            "cachedtoken"
        );
        assert_eq!(started.elapsed(), Duration::ZERO, "refreshed again");
    }

    /// Loads of one token at once refresh it once, and the load that waited
    /// reads the token the first wrote. Breaks if loads refresh in parallel
    /// or a waiting load uses the file as it read it before waiting.
    #[tokio::test(flavor = "current_thread")]
    async fn refreshes_once_for_loads_at_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(created("newtoken", None).set_delay(Duration::from_millis(100)))
            .expect(1)
            .mount(&server)
            .await;
        let home = Home::new("once", &refreshable("2021-12-25T13:00:00Z"));
        let host = home.host(&server, "2021-12-25T13:30:00Z");
        let (first, second) = tokio::join!(load_on(&host, SESSION), load_on(&host, SESSION));
        assert_eq!(first.unwrap().as_str(), "newtoken");
        assert_eq!(second.unwrap().as_str(), "newtoken");
    }

    /// A refreshed token that cannot be written back is still used, with a
    /// warning. Breaks if a failed write fails the load.
    #[tokio::test]
    async fn uses_a_refreshed_token_it_cannot_write() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(created("newtoken", None))
            .mount(&server)
            .await;
        let host = Host {
            env: Env::Fixed(HashMap::from([
                ("HOME".to_owned(), "/home/user".to_owned()),
                ("AWS_ENDPOINT_URL_SSO_OIDC".to_owned(), server.uri()),
            ])),
            network: Network::Real,
            ..fixed_host(
                &[(SESSION_FILE, &refreshable("2021-12-25T13:00:00Z"))],
                "2021-12-25T13:30:00Z",
            )
        };
        let (token, logs) = capture_logs(load_on(&host, SESSION)).await;
        assert_eq!(token.unwrap().as_str(), "newtoken");
        assert!(
            logs.contains("refreshed SSO token not written back"),
            "{logs}"
        );
    }
}
