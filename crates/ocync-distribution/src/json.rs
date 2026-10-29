//! JSON decoding shared by the registry, AWS and Google clients.

use serde::Deserialize;

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
/// It keeps where and why the JSON failed to parse and what the type
/// expected, but no text taken from the input, such as a refused value or
/// an unknown key, which can be a secret. A part of the parser's message it
/// does not recognize is dropped, not kept.
#[derive(Debug)]
pub(crate) struct Unreadable(String);

/// Starts of the parser's messages that quote no input.
const WHOLE: [&str; 2] = ["missing field `", "duplicate field `"];

/// Starts of the parser's messages that describe a refused value, then
/// name what was expected after the last [`EXPECTED`].
const REFUSED: [&str; 2] = ["invalid type: ", "invalid value: "];

/// The kinds of value the parser names before quoting one. Only these are
/// kept from a refused value's description.
const KINDS: [&str; 18] = [
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
    "number",
];

/// The start of the parser's message for a sequence of the wrong length,
/// followed by the length.
const LENGTH: &str = "invalid length ";

/// Starts of the parser's messages that quote a key or tag from the input.
const KEY: [&str; 2] = ["unknown field", "unknown variant"];

/// Ends of a [`KEY`] message for a type with no fields or variants, which
/// then holds no [`EXPECTED`] of its own.
const NONE_EXPECTED: [&str; 2] = ["there are no fields", "there are no variants"];

/// What separates the quoted input, first, from what was expected, last.
const EXPECTED: &str = ", expected ";

/// The reason for any other message, which may quote input anywhere.
const UNRECOGNIZED: &str = "a JSON value that does not fit";

impl From<serde_json::Error> for Unreadable {
    fn from(error: serde_json::Error) -> Self {
        let position = if error.line() > 0 {
            format!(" at line {} column {}", error.line(), error.column())
        } else {
            String::new()
        };
        let text = error.to_string();
        let message = text.strip_suffix(&position).unwrap_or(&text);
        let expected = message
            .rfind(EXPECTED)
            .map(|at| &message[at..])
            .unwrap_or_default();
        let reason = if !error.is_data() || WHOLE.iter().any(|start| message.starts_with(start)) {
            // Syntax and end-of-input messages name the problem, not the text.
            message.to_owned()
        } else if let Some(start) = REFUSED.iter().find(|start| message.starts_with(*start)) {
            let rest = &message[start.len()..];
            let kind = KINDS.iter().filter(|kind| rest.starts_with(**kind));
            match kind.max_by_key(|kind| kind.len()) {
                Some(kind) => format!("{start}{kind}{expected}"),
                None => format!("{}{expected}", start.trim_end_matches([':', ' '])),
            }
        } else if let Some(rest) = message.strip_prefix(LENGTH) {
            let digits = &rest[..rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len())];
            format!("{LENGTH}{digits}{expected}")
        } else if let Some(start) = KEY.iter().find(|start| message.starts_with(*start)) {
            if NONE_EXPECTED.iter().any(|end| message.ends_with(end)) {
                (*start).to_owned()
            } else {
                format!("{start}{expected}")
            }
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
pub(crate) fn parse<T: serde::de::DeserializeOwned>(
    text: impl AsRef<[u8]>,
) -> Result<T, Unreadable> {
    Ok(serde_json::from_slice(text.as_ref())?)
}

/// Read `value`, which holds or may hold a credential, as type `T`.
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
                r#"{"expires_in": "s3cret, expected x"}"#,
                "invalid type: string, expected u64 at line 1 column 35",
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
                "unknown field, expected `expires_in` at line 1 column 21",
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
                "invalid value, expected x",
            ),
            (
                serde_json::Error::invalid_type(Unexpected::Other("string s3cret"), &"x"),
                "invalid type: string, expected x",
            ),
            (
                serde_json::Error::unknown_field("k, expected s3cret", &[]),
                "unknown field",
            ),
        ] {
            assert_eq!(Unreadable::from(error).to_string(), expected);
        }
    }
}
