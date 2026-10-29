//! JSON decoding shared by the registry, AWS and Google clients.

use serde::Deserialize;

use crate::Error;

/// Read a JSON `null` as the type's default, as the SDK reads a field that
/// is null or absent, and as a registry that sends `null` for an empty tag
/// list means it. Use with
/// `#[serde(default)]` so an absent field reads the same.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(<Option<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// JSON that holds or may hold a credential, such as an endpoint's answer,
/// a helper's output or a credentials file, and is not the JSON expected.
/// It keeps where and why the JSON failed to parse but no text taken from
/// the input, such as a refused value or an unknown key, which can be a
/// secret. What the type expected is kept only from a message in exactly
/// the form serde writes, and any other part of a message is dropped. A
/// deserializer that writes input into what it says it expected, or into a
/// message of serde's form, defeats this.
#[derive(Debug)]
pub(crate) struct Unreadable(String);

/// Starts of the parser's messages that quote no input.
const WHOLE: [&str; 2] = ["missing field `", "duplicate field `"];

/// Starts of the parser's messages for a refused value: its kind, the value
/// quoted for some kinds, then [`EXPECTED`] and what the type expected.
const REFUSED: [&str; 2] = ["invalid type: ", "invalid value: "];

/// The kinds of value serde and `serde_json` name, none a prefix of another.
const KINDS: [&str; 17] = [
    "boolean",
    "integer",
    "floating point",
    "character",
    "string",
    "byte array",
    "unit value",
    "Option value",
    "newtype struct",
    "sequence",
    "map",
    "enum",
    "unit variant",
    "newtype variant",
    "tuple variant",
    "struct variant",
    "null",
];

/// The start of the parser's message for a sequence of the wrong length:
/// the length, then [`EXPECTED`].
const LENGTH: &str = "invalid length ";

/// Starts of the parser's messages that quote a key or tag from the input.
/// Nothing after one is kept: the key can hold text that reads as serde's.
const KEY: [&str; 2] = ["unknown field", "unknown variant"];

/// What follows a refused value in the parser's message.
const EXPECTED: &str = ", expected ";

/// The reason for any other message, which may quote input anywhere.
const UNRECOGNIZED: &str = "a JSON value that does not fit";

/// What follows the value serde quotes after `kind` in `rest`: a string in
/// escaped double quotes, a number, boolean or character in backticks, and
/// nothing for other kinds. `None` when `rest` is not in that form.
fn after_value<'a>(kind: &str, rest: &'a str) -> Option<&'a str> {
    match kind {
        "string" => {
            let mut chars = rest.strip_prefix(" \"")?.char_indices();
            while let Some((at, c)) = chars.next() {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' => return Some(&rest[at + 3..]),
                    _ => {}
                }
            }
            None
        }
        "integer" | "floating point" | "boolean" => {
            let value = rest.strip_prefix(" `")?;
            Some(&value[value.find('`')? + 1..])
        }
        "character" => {
            let value = rest.strip_prefix(" `")?;
            let after = value.chars().next()?.len_utf8();
            value[after..].strip_prefix('`')
        }
        _ => Some(rest),
    }
}

impl From<serde_json::Error> for Unreadable {
    fn from(error: serde_json::Error) -> Self {
        let position = if error.line() > 0 {
            format!(" at line {} column {}", error.line(), error.column())
        } else {
            String::new()
        };
        let text = error.to_string();
        let message = text.strip_suffix(&position).unwrap_or(&text);
        let reason = if !error.is_data() || WHOLE.iter().any(|start| message.starts_with(start)) {
            // Syntax and end-of-input messages name the problem, not the text.
            message.to_owned()
        } else if let Some(start) = REFUSED.iter().find(|start| message.starts_with(*start)) {
            let rest = &message[start.len()..];
            match KINDS.iter().find(|kind| rest.starts_with(**kind)) {
                Some(kind) => {
                    let expected = after_value(kind, &rest[kind.len()..])
                        .filter(|after| after.starts_with(EXPECTED))
                        .unwrap_or_default();
                    format!("{start}{kind}{expected}")
                }
                None => start.trim_end_matches([':', ' ']).to_owned(),
            }
        } else if let Some(rest) = message.strip_prefix(LENGTH) {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let expected = Some(&rest[digits..])
                .filter(|after| after.starts_with(EXPECTED))
                .unwrap_or_default();
            format!("{LENGTH}{}{expected}", &rest[..digits])
        } else if let Some(start) = KEY.iter().find(|start| message.starts_with(*start)) {
            (*start).to_owned()
        } else {
            UNRECOGNIZED.to_owned()
        };
        Self(reason + &position)
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

/// Parse `text`, which holds or may hold a credential, as JSON of type `T`.
#[expect(
    clippy::disallowed_methods,
    reason = "the reader whose error is cleaned"
)]
pub(crate) fn parse<T: serde::de::DeserializeOwned>(
    text: impl AsRef<[u8]>,
) -> Result<T, Unreadable> {
    Ok(serde_json::from_slice(text.as_ref())?)
}

/// Read `response`'s body, which holds or may hold a credential, as JSON of
/// type `T`. A body that does not arrive is [`Error::Http`]; one that is not
/// the JSON expected is [`Error::UnreadableAnswer`], with no value from it.
pub(crate) async fn answer<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, Error> {
    let body = response.bytes().await?;
    parse(&body).map_err(|e| Error::UnreadableAnswer {
        reason: e.to_string(),
    })
}

/// Read `value`, which holds or may hold a credential, as type `T`.
#[expect(
    clippy::disallowed_methods,
    reason = "the reader whose error is cleaned"
)]
pub(crate) fn value_as<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, Unreadable> {
    Ok(serde_json::from_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON that fails to parse names where and why, but no text from the
    /// input, in its text or its `Debug`. Breaks if a refused value, an
    /// unknown key or a custom message reaches either.
    #[test]
    fn unreadable_leaves_the_input_out() {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Answer {
            #[expect(dead_code)]
            expires_in: u64,
        }
        for (text, expected) in [
            (
                r#"{"expires_in": "s3\\"cret, expected x"}"#,
                "invalid type: string, expected u64 at line 1 column 21",
            ),
            (
                r#"{"expires_in": -7}"#,
                "invalid value: integer, expected u64 at line 1 column 17",
            ),
            (
                r#"{"expires_in": s3cret}"#,
                "expected value at line 1 column 16",
            ),
            (
                r#"{"expires_in": "#,
                "EOF while parsing a value at line 1 column 15",
            ),
            (r#"{}"#, "missing field `expires_in` at line 1 column 2"),
            (
                r#"{"k, expected s3cret": 1}"#,
                "unknown field at line 1 column 21",
            ),
        ] {
            let error = parse::<Answer>(text).unwrap_err();
            assert_eq!(error.to_string(), expected, "{text}");
            assert!(!format!("{error:?}").contains("s3cret"), "{error:?}");
        }
        let value = serde_json::json!({"expires_in": "s3cret"});
        let error = value_as::<Answer>(value).unwrap_err();
        assert_eq!(error.to_string(), "invalid type: string, expected u64");
        // Messages a deserializer writes, with input where serde puts a kind
        // or a key.
        use serde::de::{Error as _, Unexpected};
        for (error, expected) in [
            (serde_json::Error::custom("s3cret"), UNRECOGNIZED),
            (
                serde_json::Error::invalid_value(Unexpected::Other("token s3cret"), &"x"),
                "invalid value",
            ),
            (
                serde_json::Error::invalid_type(Unexpected::Other("string s3cret"), &"x"),
                "invalid type: string",
            ),
            (
                serde_json::Error::custom("unknown field `k, expected s3cret`"),
                "unknown field",
            ),
            (
                serde_json::Error::custom("invalid type: map s3cret, expected s3cret"),
                "invalid type: map",
            ),
            (
                serde_json::Error::invalid_type(Unexpected::Char('`'), &"x"),
                "invalid type: character, expected x",
            ),
            (
                serde_json::Error::invalid_type(Unexpected::Char('€'), &"x"),
                "invalid type: character, expected x",
            ),
        ] {
            assert_eq!(Unreadable::from(error).to_string(), expected);
        }
    }
}
