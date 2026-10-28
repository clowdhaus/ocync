//! An authorized user, as `gcloud auth application-default login` writes
//! one: a refresh token exchanged at the `OAuth2` token endpoint.

use http::HeaderValue;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{AccessToken, Call, Clients, Error, Scopes, Secret, expiring, parse_url, send_json};
use crate::aws::Clock;

/// The refresh grant, RFC 6749 section 6.
const GRANT_TYPE: &str = "refresh_token";

/// The token endpoint when the file names none.
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// The field naming the token endpoint.
const TOKEN_URI_FIELD: &str = "token_uri";

/// The only universe an authorized user may belong to, as upstream.
const DEFAULT_UNIVERSE: &str = "googleapis.com";

/// The header carrying the project billed for a call.
pub(super) const QUOTA_PROJECT_HEADER: &str = "x-goog-user-project";

/// An authorized user file, as upstream reads it.
#[derive(Deserialize)]
struct UserFile {
    client_id: String,
    client_secret: Secret,
    refresh_token: Secret,
    token_uri: Option<String>,
    quota_project_id: Option<String>,
    universe_domain: Option<String>,
}

/// An authorized user's refresh token and where to exchange it.
#[derive(Debug)]
pub(crate) struct AuthorizedUser {
    client_id: String,
    client_secret: Secret,
    refresh_token: Secret,
    token_uri: Url,
    /// The project billed for calls made as this user, as the header
    /// carries it.
    quota_project: Option<HeaderValue>,
    /// The space-joined scopes, or `None` for the token's own.
    scopes: Option<String>,
}

/// The refresh request, in upstream's fields. `scopes` is written as
/// `null` when unset, as upstream writes it.
#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    client_secret: &'a str,
    refresh_token: &'a str,
    scopes: Option<&'a str>,
}

/// The token endpoint's answer.
#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(rename = "token_type")]
    _token_type: IgnoredAny,
    expires_in: Option<u64>,
}

impl AuthorizedUser {
    /// The user `json` describes, asking for `scopes`. A universe other than
    /// `googleapis.com` is refused, as upstream.
    pub(super) fn from_json(json: Value, scopes: Scopes) -> Result<Self, Error> {
        let file: UserFile = serde_json::from_value(json).map_err(Error::Parse)?;
        if file
            .universe_domain
            .as_deref()
            .is_some_and(|universe| universe != DEFAULT_UNIVERSE)
        {
            return Err(Error::UserUniverse);
        }
        Ok(Self {
            client_id: file.client_id,
            client_secret: file.client_secret,
            refresh_token: file.refresh_token,
            token_uri: parse_url(
                TOKEN_URI_FIELD,
                file.token_uri.as_deref().unwrap_or(TOKEN_URI),
            )?,
            quota_project: file
                .quota_project_id
                .map(|project| HeaderValue::from_str(&project).map_err(Error::QuotaProject))
                .transpose()?,
            scopes: scopes
                .registry_or_endpoint_default()
                .map(|scopes| scopes.joined(" ")),
        })
    }

    /// The project billed for calls made as this user.
    pub(super) fn quota_project(&self) -> Option<&HeaderValue> {
        self.quota_project.as_ref()
    }

    /// A token for the refresh token.
    pub(super) async fn access_token(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<AccessToken, Error> {
        let response: RefreshResponse = send_json(clients, Call::UserToken, |client| {
            client.post(self.token_uri.clone()).json(&RefreshRequest {
                grant_type: GRANT_TYPE,
                client_id: &self.client_id,
                client_secret: self.client_secret.expose(),
                refresh_token: self.refresh_token.expose(),
                scopes: self.scopes.as_deref(),
            })
        })
        .await?;
        expiring(response.access_token, clock.now(), response.expires_in)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::google::REGISTRY_SCOPE;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn user_file(token_uri: &str) -> Value {
        serde_json::json!({
            "type": "authorized_user",
            "client_id": "client-id",
            "client_secret": "client-secret",
            "refresh_token": "refresh-token",
            "token_uri": token_uri,
        })
    }

    /// Upstream `oauth2_request_serde`, `token_provider_full` and
    /// `credential_provider_with_scopes`: a JSON body with the refresh
    /// token, the client and the scopes, at the file's `token_uri`, and the
    /// expiry from `expires_in`. Breaks if a field, its name, the content
    /// type or the expiry changes.
    #[tokio::test]
    async fn refreshes_at_the_token_uri() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("content-type", "application/json"))
            .and(body_json(serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": "client-id",
                "client_secret": "client-secret",
                "refresh_token": "refresh-token",
                "scopes": REGISTRY_SCOPE,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.user", "token_type": "Bearer", "expires_in": 3599
            })))
            .expect(1)
            .mount(&server)
            .await;

        let user = AuthorizedUser::from_json(
            user_file(&format!("{}/token", server.uri())),
            Scopes::Registry,
        )
        .unwrap();
        let token = user
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap();
        assert_eq!(token.token.expose(), "ya29.user");
        assert_eq!(
            token.expiry,
            crate::google::Expiry::At(now() + Duration::from_secs(3599))
        );
    }

    /// Upstream `authorized_user_full_from_json_success` and `_partial`: no
    /// `token_uri` means Google's endpoint, and a source user sends
    /// `scopes: null`. Breaks if the default endpoint or the null changes.
    #[test]
    fn defaults_the_endpoint_and_scopes() {
        let mut file = user_file(TOKEN_URI);
        file.as_object_mut().unwrap().remove("token_uri");
        let user = AuthorizedUser::from_json(file, Scopes::Default).unwrap();
        assert_eq!(user.token_uri.as_str(), TOKEN_URI);
        assert_eq!(user.scopes, None);
    }

    /// Upstream `builder_rejects_non_default_universe` and
    /// `authorized_user_parse_fail`: another universe, a file missing the
    /// refresh token, a `token_uri` that is not a URL, and a quota project
    /// no header can carry, are refused when the file loads. Breaks if any
    /// is accepted or left to fail at the first request.
    #[test]
    fn refuses_bad_files() {
        let mut other_universe = user_file(TOKEN_URI);
        other_universe["universe_domain"] = "example.com".into();
        let mut no_token = user_file(TOKEN_URI);
        no_token.as_object_mut().unwrap().remove("refresh_token");
        for file in [other_universe, no_token] {
            assert!(AuthorizedUser::from_json(file, Scopes::Registry).is_err());
        }
        let bad_uri = AuthorizedUser::from_json(user_file("not a url"), Scopes::Registry);
        assert!(
            matches!(
                bad_uri,
                Err(Error::Url {
                    field: TOKEN_URI_FIELD,
                    ..
                })
            ),
            "{bad_uri:?}"
        );
        let mut bad_project = user_file(TOKEN_URI);
        bad_project["quota_project_id"] = "line\nbreak".into();
        let bad_project = AuthorizedUser::from_json(bad_project, Scopes::Registry);
        assert!(
            matches!(bad_project, Err(Error::QuotaProject(_))),
            "{bad_project:?}"
        );
    }

    /// Upstream `credential_provider_retryable_error`,
    /// `token_provider_nonretryable_error` and
    /// `token_provider_malformed_response_is_nonretryable`, with the body cut
    /// to its first 200 bytes. Breaks if the classification changes or a
    /// long body is kept whole.
    #[tokio::test]
    async fn classifies_failures_and_cuts_the_body() {
        let long = "x".repeat(500);
        for (status, body, transient) in [
            (503, long.as_str(), true),
            (401, "denied", false),
            (200, "not json", false),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            let user = AuthorizedUser::from_json(
                user_file(&format!("{}/token", server.uri())),
                Scopes::Registry,
            )
            .unwrap();
            let error = user
                .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
                .await
                .unwrap_err();
            assert_eq!(error.is_transient(), transient, "{status}: {error}");
            if status == 503 {
                assert!(error.to_string().contains(&"x".repeat(200)));
                assert!(!error.to_string().contains(&"x".repeat(201)));
            }
        }
    }
}
