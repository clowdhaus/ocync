//! The JSON credentials document the instance metadata service and the
//! container endpoint answer with, read as the SDK reads it.

use super::super::calendar;
use super::Credentials;

/// The `Code` of a document that carries credentials.
const SUCCESS: &str = "Success";

/// What a credentials document says.
#[derive(Debug)]
pub(super) enum Document {
    /// A temporary key.
    Credentials(Credentials),
    /// An error, by its `Code` and `Message`.
    Error {
        code: Code,
        /// The `Message` field, or `no message`.
        message: String,
    },
}

/// The `Code` of an error document.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::aws) enum Code {
    /// The role does not trust the service that asked for it.
    AssumeRoleUnauthorizedAccess,
    /// Any other code, as sent.
    Other(String),
}

impl Code {
    const ASSUME_ROLE_UNAUTHORIZED_ACCESS: &str = "AssumeRoleUnauthorizedAccess";
}

impl From<&str> for Code {
    fn from(code: &str) -> Self {
        match code {
            Self::ASSUME_ROLE_UNAUTHORIZED_ACCESS => Self::AssumeRoleUnauthorizedAccess,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AssumeRoleUnauthorizedAccess => Self::ASSUME_ROLE_UNAUTHORIZED_ACCESS,
            Self::Other(code) => code,
        })
    }
}

/// Why a credentials document could not be read.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The text is not a JSON object.
    #[error("invalid JSON in response: {0}")]
    Json(#[source] crate::json::Unreadable),
    /// A field a success needs is missing.
    #[error("Expected field `{0}` in response but it was missing")]
    Missing(Required),
    /// `Expiration` is not an RFC 3339 date-time in UTC.
    #[error("Invalid field in response: `Expiration`. not an RFC-3339 date-time in UTC: {0}")]
    Expiration(String),
}

/// The keys of the `Code` field.
const CODE: &[&str] = &["Code"];

/// The keys of the `Message` field.
const MESSAGE: &[&str] = &["Message"];

/// A field a document with credentials must carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::aws) enum Required {
    /// `AccessKeyId`.
    AccessKeyId,
    /// `SecretAccessKey`.
    SecretAccessKey,
    /// `Token`, or `SessionToken`.
    Token,
    /// `Expiration`, or `ExpiresAt`.
    Expiration,
}

impl Required {
    /// The keys that name this field, matched in any case.
    fn names(self) -> &'static [&'static str] {
        match self {
            Self::AccessKeyId => &["AccessKeyId"],
            Self::SecretAccessKey => &["SecretAccessKey"],
            Self::Token => &["Token", "SessionToken"],
            Self::Expiration => &["Expiration", "ExpiresAt"],
        }
    }
}

/// The field's first key, as upstream names it in errors.
impl std::fmt::Display for Required {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.names()[0])
    }
}

/// The value of the field `names` spell in `document`: keys match in any
/// case and only values `read` takes count, as upstream's JSON readers
/// find one. Of several matches the key last in byte order wins, as the
/// map is key-sorted, where upstream takes the last one written.
pub(super) fn field<'a, T>(
    document: &'a serde_json::Map<String, serde_json::Value>,
    names: &[&str],
    read: impl Fn(&'a serde_json::Value) -> Option<T>,
) -> Option<T> {
    document
        .iter()
        .filter(|(key, _)| names.iter().any(|name| key.eq_ignore_ascii_case(name)))
        .filter_map(|(_, value)| read(value))
        .next_back()
}

/// Read `body`. Fields are found as [`field`] finds them, string values
/// only. A document with no `Code`, or `Code` of `Success`, must carry a
/// key, token and expiry.
pub(super) fn read_document(body: &[u8]) -> Result<Document, Error> {
    let fields: serde_json::Map<String, serde_json::Value> =
        crate::json::parse(body).map_err(Error::Json)?;
    let field = |names: &[&str]| field(&fields, names, serde_json::Value::as_str);
    let required = |name: Required| field(name.names()).ok_or(Error::Missing(name));
    match field(CODE) {
        Some(code) if code != SUCCESS => Ok(Document::Error {
            code: Code::from(code),
            message: field(MESSAGE).unwrap_or("no message").to_owned(),
        }),
        None | Some(_) => {
            let access_key_id = required(Required::AccessKeyId)?.to_owned();
            let secret_access_key = required(Required::SecretAccessKey)?.to_owned();
            let session_token = required(Required::Token)?.to_owned();
            let expiration = required(Required::Expiration)?;
            let expiry = calendar::parse_rfc3339(expiration)
                .ok_or_else(|| Error::Expiration(expiration.to_owned()))?;
            Ok(Document::Credentials(Credentials {
                access_key_id,
                secret_access_key,
                session_token: Some(session_token),
                expiry: Some(expiry),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    /// Of two string values under keys that differ only in case, the key
    /// last in byte order wins, whatever order the document writes them
    /// in. Breaks if the first match wins or the order starts to depend on
    /// the document.
    #[test]
    fn takes_the_key_last_in_byte_order() {
        for text in [
            r#"{"AccessKeyId": "upper", "accesskeyid": "lower"}"#,
            r#"{"accesskeyid": "lower", "AccessKeyId": "upper"}"#,
        ] {
            let document: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(text).unwrap();
            let found = field(&document, &["AccessKeyId"], serde_json::Value::as_str);
            assert_eq!(found, Some("lower"), "{text}");
        }
    }

    /// The upstream parser cases: keys in any case, either spelling of the
    /// token and expiry, an error document, and each missing field named.
    /// Breaks if a key is matched case-sensitively or an error document is
    /// read as credentials.
    #[test]
    fn reads_documents_as_the_sdk_does() {
        let body = br#"{"code": "Success", "type": "AWS-HMAC", "accesskeyid": "ASIA",
            "SECRETACCESSKEY": "secret", "SessionToken": "token",
            "ExpiresAt": "2021-09-21T04:16:53Z", "AccountId": "123", "Extra": 7}"#;
        let Document::Credentials(credentials) = read_document(body).unwrap() else {
            panic!("not credentials");
        };
        assert_eq!(credentials.access_key_id(), "ASIA");
        assert_eq!(credentials.secret_access_key(), "secret");
        assert_eq!(credentials.session_token(), Some("token"));
        assert_eq!(
            credentials.expiry(),
            Some(UNIX_EPOCH + Duration::from_secs(1_632_197_813))
        );

        let error =
            read_document(br#"{"Code": "AssumeRoleUnauthorizedAccess", "Message": "denied"}"#);
        assert!(
            matches!(error, Ok(Document::Error { code: Code::AssumeRoleUnauthorizedAccess, ref message })
                if message == "denied"),
            "{error:?}"
        );
        let other = read_document(br#"{"Code": "Throttled"}"#);
        assert!(
            matches!(other, Ok(Document::Error { code: Code::Other(ref code), ref message })
                if code == "Throttled" && message == "no message"),
            "{other:?}"
        );

        for (body, text) in [
            (&br#"{"Code": "Success"}"#[..], "`AccessKeyId`"),
            (br#"{"AccessKeyId": "a"}"#, "`SecretAccessKey`"),
            (
                br#"{"AccessKeyId": "a", "SecretAccessKey": "s"}"#,
                "`Token`",
            ),
            (
                br#"{"AccessKeyId": "a", "SecretAccessKey": "s", "Token": "t"}"#,
                "`Expiration`",
            ),
            (
                br#"{"AccessKeyId": "a", "SecretAccessKey": "s", "Token": "t",
                    "Expiration": "2021-09-21T06:16:53+02:00"}"#,
                "Invalid field in response: `Expiration`",
            ),
            (b"[]", "invalid JSON"),
        ] {
            let err = read_document(body).unwrap_err().to_string();
            assert!(err.contains(text), "{err}");
        }
    }
}
