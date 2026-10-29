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
/// It keeps how and where the JSON failed to parse but not the parser's
/// message, which quotes the value it refused, and that value can be a
/// secret.
#[derive(Debug)]
pub(crate) struct Unreadable {
    failure: Failure,
    /// Line and column, absent for a value already parsed.
    position: Option<(usize, usize)>,
}

/// How JSON failed to parse.
#[derive(Debug)]
enum Failure {
    /// Not JSON.
    Syntax,
    /// Ends before the JSON does.
    CutOff,
    /// A value that does not fit the type, the parser's quote of it dropped.
    Value,
    /// A field missing, unknown or repeated, as the parser names it.
    Field(String),
    /// The text could not be read.
    Io,
}

/// Starts of the parser's messages that name a field and quote no value.
const FIELD_MESSAGES: [&str; 3] = ["missing field `", "unknown field `", "duplicate field `"];

impl From<serde_json::Error> for Unreadable {
    fn from(error: serde_json::Error) -> Self {
        let failure = match error.classify() {
            serde_json::error::Category::Syntax => Failure::Syntax,
            serde_json::error::Category::Eof => Failure::CutOff,
            serde_json::error::Category::Io => Failure::Io,
            serde_json::error::Category::Data => {
                let message = error.to_string();
                if FIELD_MESSAGES
                    .iter()
                    .any(|start| message.starts_with(start))
                {
                    Failure::Field(message)
                } else {
                    Failure::Value
                }
            }
        };
        let position = (error.line() > 0).then(|| (error.line(), error.column()));
        Self { failure, position }
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failure {
            // The parser's message already gives the position.
            Failure::Field(message) => return f.write_str(message),
            Failure::Syntax => f.write_str("not JSON")?,
            Failure::CutOff => f.write_str("JSON cut off")?,
            Failure::Value => f.write_str("a JSON value that does not fit")?,
            Failure::Io => f.write_str("JSON that could not be read")?,
        }
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

    /// JSON that fails to parse names how and where, and a missing or
    /// unknown field by name, but never the value refused, in its text or
    /// its `Debug`. Breaks if the parser's quote of a value reaches either.
    #[test]
    fn unreadable_leaves_the_value_out() {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Answer {
            #[expect(dead_code)]
            expires_in: u64,
        }
        for (text, expected) in [
            (
                r#"{"expires_in": "s3cret"}"#,
                "a JSON value that does not fit at line 1 column 23",
            ),
            (r#"{"expires_in": s3cret}"#, "not JSON at line 1 column 16"),
            (r#"{"expires_in": "#, "JSON cut off at line 1 column 15"),
            (r#"{}"#, "missing field `expires_in` at line 1 column 2"),
            (
                r#"{"token": "s3cret"}"#,
                "unknown field `token`, expected `expires_in` at line 1 column 8",
            ),
        ] {
            let error = parse::<Answer>(text).unwrap_err();
            assert_eq!(error.to_string(), expected, "{text}");
            assert!(!format!("{error:?}").contains("s3cret"), "{error:?}");
        }
        let value = serde_json::json!({"expires_in": "s3cret"});
        let error = value_as::<Answer>(value).unwrap_err();
        assert_eq!(error.to_string(), "a JSON value that does not fit");
    }
}
