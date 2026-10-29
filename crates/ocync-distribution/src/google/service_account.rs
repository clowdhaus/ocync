//! A service account key: a self-signed RS256 JWT, used directly as the
//! access token, as upstream uses it. No `token_uri` exchange is made.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AccessToken, DEFAULT_SCOPE, Error, Expiry, Scopes, Secret};

/// How far `iat` is set before now and `exp` after the lifetime, so a
/// clock a little ahead of Google's is not refused.
const CLOCK_SKEW_FUDGE: Duration = Duration::from_secs(10);

/// How long a token is valid, as upstream's `DEFAULT_TOKEN_TIMEOUT`.
const LIFETIME: Duration = Duration::from_secs(3600);

/// The JWT signing algorithm, RSASSA-PKCS1-v1_5 with SHA-256 (RFC 7518).
const ALGORITHM: &str = "RS256";

/// The JWT `typ` header (RFC 7519).
const TOKEN_TYPE: &str = "JWT";

/// The PEM armor of a PKCS#8 private key.
const PKCS8_ARMOR: (&str, &str) = ("-----BEGIN PRIVATE KEY-----", "-----END PRIVATE KEY-----");

/// A service account key file, as upstream reads it. `project_id` is
/// required but unused, as upstream.
#[derive(Deserialize)]
struct KeyFile {
    client_email: String,
    private_key_id: String,
    private_key: Secret,
    #[serde(rename = "project_id")]
    _project_id: IgnoredAny,
}

/// Why a service account's private key was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum KeyError {
    /// The key is not in PKCS#8 `PRIVATE KEY` PEM armor, as a PKCS#1
    /// `RSA PRIVATE KEY` is not.
    #[error("expected a PKCS#8 `PRIVATE KEY` PEM")]
    NotPkcs8Pem,
    /// The armored body is not base64.
    #[error("the PEM body is not base64")]
    Base64(#[source] base64::DecodeError),
    /// The key is not an RSA key `aws-lc-rs` accepts.
    #[error("the key was rejected")]
    Rejected(#[source] aws_lc_rs::error::KeyRejected),
}

/// A service account and the key it signs with.
pub(super) struct Account {
    client_email: String,
    private_key_id: String,
    key: RsaKeyPair,
    /// The space-joined scopes the token carries.
    scope: String,
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("client_email", &self.client_email)
            .field("private_key_id", &self.private_key_id)
            .finish_non_exhaustive()
    }
}

/// The JWT header, in upstream's field order.
#[derive(Serialize)]
struct Header<'a> {
    alg: &'a str,
    typ: &'a str,
    kid: &'a str,
}

/// The JWT claims, in upstream's field order. `aud` is written as `null`,
/// as upstream writes it.
#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: Option<&'a str>,
    exp: u64,
    iat: u64,
    sub: &'a str,
}

impl Account {
    /// The service account `json` describes, asking for `scopes`. The key
    /// is parsed here, so a bad key fails when the credentials are loaded.
    pub(super) fn from_json(json: Value, scopes: Scopes) -> Result<Self, Error> {
        let file: KeyFile = crate::json::value_as(json).map_err(Error::Parse)?;
        Ok(Self {
            key: parse_pkcs8_pem(file.private_key.expose()).map_err(Error::PrivateKey)?,
            client_email: file.client_email,
            private_key_id: file.private_key_id,
            scope: scopes.resolve(None, DEFAULT_SCOPE).joined(" "),
        })
    }

    /// A token signed at `now`, valid for [`LIFETIME`] past
    /// [`CLOCK_SKEW_FUDGE`].
    pub(super) fn access_token(&self, now: SystemTime) -> Result<AccessToken, Error> {
        let iat = unix_seconds(now)?
            .checked_sub(CLOCK_SKEW_FUDGE.as_secs())
            .ok_or(Error::ClockBeforeEpoch)?;
        let expiry = now + CLOCK_SKEW_FUDGE + LIFETIME;
        let header = Header {
            alg: ALGORITHM,
            typ: TOKEN_TYPE,
            kid: &self.private_key_id,
        };
        let claims = Claims {
            iss: &self.client_email,
            scope: &self.scope,
            aud: None,
            exp: unix_seconds(expiry)?,
            iat,
            sub: &self.client_email,
        };
        let signed = format!("{}.{}", encode_json(&header), encode_json(&claims));
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(Error::Sign)?;
        Ok(AccessToken {
            token: Secret(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))),
            expiry: Expiry::At(expiry),
        })
    }
}

/// `value` as JSON, base64url-encoded without padding.
fn encode_json(value: &impl Serialize) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).expect("JWT parts serialize"))
}

/// Seconds since the epoch, as a JWT time.
fn unix_seconds(time: SystemTime) -> Result<u64, Error> {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .map_err(|_| Error::ClockBeforeEpoch)
}

/// The RSA key in a PKCS#8 PEM. Any other key, such as a PKCS#1
/// `RSA PRIVATE KEY`, is refused, as upstream refuses it.
fn parse_pkcs8_pem(pem: &str) -> Result<RsaKeyPair, KeyError> {
    let (begin, end) = PKCS8_ARMOR;
    let body = pem
        .trim()
        .strip_prefix(begin)
        .and_then(|rest| rest.strip_suffix(end))
        .ok_or(KeyError::NotPkcs8Pem)?;
    let base64: String = body.split_ascii_whitespace().collect();
    let der = STANDARD.decode(base64).map_err(KeyError::Base64)?;
    RsaKeyPair::from_pkcs8(&der).map_err(KeyError::Rejected)
}

#[cfg(test)]
pub(super) mod tests {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;

    /// A fresh 2048-bit key in PKCS#8 PEM, made for the test, as upstream's
    /// tests make theirs.
    pub(in crate::google) fn pkcs8_pem() -> String {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der<'_>>::as_der(&key).unwrap();
        let body = STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        format!(
            "{}\n{}\n{}\n",
            PKCS8_ARMOR.0,
            lines.join("\n"),
            PKCS8_ARMOR.1
        )
    }

    /// A service account key file holding `pem`.
    pub(in crate::google) fn key_file(pem: &str) -> Value {
        serde_json::json!({
            "type": "service_account",
            "client_email": "sa@project.iam.gserviceaccount.com",
            "private_key_id": "key-id",
            "private_key": pem,
            "project_id": "project",
        })
    }

    fn decode(part: &str) -> String {
        String::from_utf8(URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    /// Upstream `get_service_account_token_pkcs8_key_success`,
    /// `test_jws_header_encode`, `test_jws_claims_encode_partial`,
    /// `validate_token_issue_time` and `validate_token_expiry_time`: the
    /// header and claims as upstream writes them, byte for byte, times 10
    /// seconds either side of the hour, and a signature the key's public
    /// half verifies. Breaks if a field, its order or a time changes, or the
    /// signature is not RS256 over the first two parts.
    #[test]
    fn signs_the_jwt_upstream_writes() {
        let account = Account::from_json(key_file(&pkcs8_pem()), Scopes::Registry).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let token = account.access_token(now).unwrap();

        assert_eq!(token.expiry, Expiry::At(now + Duration::from_secs(3610)));
        let jwt = token.token.expose();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            decode(parts[0]),
            r#"{"alg":"RS256","typ":"JWT","kid":"key-id"}"#
        );
        assert_eq!(
            decode(parts[1]),
            concat!(
                r#"{"iss":"sa@project.iam.gserviceaccount.com","#,
                r#""scope":"https://www.googleapis.com/auth/devstorage.read_write","#,
                r#""aud":null,"exp":1700003610,"iat":1699999990,"#,
                r#""sub":"sa@project.iam.gserviceaccount.com"}"#
            )
        );
        let public = account.key.public_key().as_ref().to_vec();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            )
            .unwrap();
    }

    /// A source service account asks for upstream's default scope. Breaks if
    /// the registry scope leaks into a source credential.
    #[test]
    fn a_source_asks_for_the_default_scope() {
        let account = Account::from_json(key_file(&pkcs8_pem()), Scopes::Default).unwrap();
        assert_eq!(account.scope, DEFAULT_SCOPE);
    }

    /// Upstream `get_service_account_headers_pkcs1_private_key_failure` and
    /// `_invalid_key_failure`: a PKCS#1 key and a key that is not one are
    /// refused. Breaks if either is accepted.
    #[test]
    fn refuses_keys_that_are_not_pkcs8_rsa() {
        let pkcs1 = "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----\n";
        for pem in [
            pkcs1,
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
        ] {
            let error = Account::from_json(key_file(pem), Scopes::Registry).unwrap_err();
            assert!(matches!(error, Error::PrivateKey(_)), "{error}");
        }
    }

    /// Upstream `get_service_account_invalid_json_failure`: a key file
    /// lacking a required field is refused. Breaks if `project_id` stops
    /// being required.
    #[test]
    fn refuses_a_file_missing_a_field() {
        let mut file = key_file(&pkcs8_pem());
        file.as_object_mut().unwrap().remove("project_id");
        let error = Account::from_json(file, Scopes::Registry).unwrap_err();
        let error = crate::error::error_chain(&error);
        assert!(error.contains("project_id"), "{error}");
    }
}
