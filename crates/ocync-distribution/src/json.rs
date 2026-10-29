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
/// It keeps where and why the JSON failed to parse but no text taken from
/// the input, such as a refused value or an unknown key, which can be a
/// secret.
#[derive(Debug)]
pub(crate) struct Unreadable(String);

/// Starts of the parser's messages that quote no input.
const WHOLE: [&str; 2] = ["missing field `", "duplicate field `"];

/// Starts of the parser's messages that quote a value and then, after the
/// last [`EXPECTED`], name only what the type expected.
const VALUE_THEN_EXPECTED: [&str; 3] = ["invalid type: ", "invalid value: ", "invalid length "];

/// Starts of the parser's messages that quote a key or tag from the input,
/// kept with nothing after them: what follows can repeat the input.
const KEY: [&str; 2] = ["unknown field", "unknown variant"];

/// What separates the quoted value, first, from what was expected, last.
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
        let reason = if !error.is_data() || WHOLE.iter().any(|start| message.starts_with(start)) {
            // Syntax and end-of-input messages name the problem, not the text.
            message.to_owned()
        } else if let Some(start) = VALUE_THEN_EXPECTED
            .iter()
            .find(|start| message.starts_with(*start))
        {
            let rest = &message[start.len()..];
            let kind = rest[..rest.find(['`', '"', ',']).unwrap_or(rest.len())].trim_end();
            match message.rfind(EXPECTED) {
                Some(at) => format!("{start}{kind}, {}", &message[at + 2..]),
                None => format!("{start}{kind}"),
            }
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
        let custom = serde_json::from_str::<Answer>("{}")
            .map_err(|_| <serde_json::Error as serde::de::Error>::custom("s3cret"))
            .unwrap_err();
        assert_eq!(Unreadable::from(custom).to_string(), UNRECOGNIZED);
    }
}
