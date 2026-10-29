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
pub(crate) struct Unreadable {
    /// The parser's message with the input it quoted removed.
    reason: String,
    /// Line and column, absent for a value already parsed.
    position: Option<(usize, usize)>,
}

/// Starts of the parser's messages that quote no input.
const WHOLE: [&str; 2] = ["missing field `", "duplicate field `"];

/// Starts of the parser's messages that quote input, kept up to the
/// quote. The rest of the message, after [`EXPECTED`], names only what the
/// type expected.
const UP_TO_THE_QUOTE: [&str; 5] = [
    "invalid type: ",
    "invalid value: ",
    "invalid length ",
    "unknown field",
    "unknown variant",
];

/// What separates the quoted input, first, from what was expected, last.
const EXPECTED: &str = ", expected ";

/// The reason for a custom message, which may quote input anywhere.
const UNRECOGNIZED: &str = "a JSON value that does not fit";

impl From<serde_json::Error> for Unreadable {
    fn from(error: serde_json::Error) -> Self {
        let position = (error.line() > 0).then(|| (error.line(), error.column()));
        let text = error.to_string();
        let message = match position {
            Some((line, column)) => text
                .strip_suffix(&format!(" at line {line} column {column}"))
                .unwrap_or(&text),
            None => &text,
        };
        let reason = if !error.is_data() || WHOLE.iter().any(|start| message.starts_with(start)) {
            // Syntax and end-of-input messages name the problem, not the text.
            message.to_owned()
        } else if let Some(start) = UP_TO_THE_QUOTE
            .iter()
            .find(|start| message.starts_with(*start))
        {
            let rest = &message[start.len()..];
            let kind = rest[..rest.find(['`', '"', ',']).unwrap_or(rest.len())].trim_end();
            let expected = message.rfind(EXPECTED).map(|at| &message[at + 2..]);
            match expected {
                Some(expected) => format!("{start}{kind}, {expected}"),
                None => format!("{start}{kind}"),
            }
        } else {
            UNRECOGNIZED.to_owned()
        };
        Self { reason, position }
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)?;
        if let Some((line, column)) = self.position {
            write!(f, " at line {line} column {column}")?;
        }
        Ok(())
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
                r#"{"s3cret": 1}"#,
                "unknown field, expected `expires_in` at line 1 column 9",
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
