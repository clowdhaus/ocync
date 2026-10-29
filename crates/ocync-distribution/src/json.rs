//! Answer reading and JSON decoding shared by the registry, AWS and Google
//! clients.

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

/// Why JSON that holds or may hold a credential, such as an endpoint's
/// answer, a helper's output or a credentials file, could not be read.
/// It keeps where and why the JSON failed to parse but no text taken from
/// the input, such as a refused value or an unknown key, which can be a
/// secret. What the type expected is kept only from a message in exactly
/// the form serde writes, and any other part of a message is dropped. A
/// deserializer that writes input into what it says it expected, or into a
/// message of serde's form, defeats this.
#[derive(Debug, Clone)]
pub struct Unreadable(String);

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

/// The longest credential answer or error body read: well past any token,
/// JWT, credentials document, zone or registry error a cloud or registry
/// returns, and short of a body that would exhaust memory.
pub(crate) const MAX_SMALL_ANSWER: usize = 1024 * 1024;

/// The limit on a tag list page or referrers index: none, as a registry may
/// return a whole repository's in one page.
pub(crate) const NO_LIMIT: usize = usize::MAX;

/// `response`'s body, or `None` once it is known to pass `max` bytes: from
/// its declared length before any is read, else as it arrives.
pub(crate) async fn read_capped(
    response: &mut reqwest::Response,
    max: usize,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    // The declared length only refuses: a buffer sized from it would let a
    // server pick how much memory is taken before any byte arrives.
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > max))
    {
        return Ok(None);
    }
    // Joined once at the end, so a long body is not copied as it grows.
    let mut chunks = Vec::new();
    let mut length = 0;
    while let Some(chunk) = response.chunk().await? {
        length += chunk.len();
        if length > max {
            return Ok(None);
        }
        chunks.push(chunk);
    }
    Ok(Some(chunks.concat()))
}

/// Read the start of `response`'s body into `body`, up to
/// [`MAX_SMALL_ANSWER`] bytes. On a failed read, `body` keeps what arrived.
pub(crate) async fn read_start(
    response: &mut reqwest::Response,
    body: &mut Vec<u8>,
) -> Result<(), reqwest::Error> {
    while body.len() < MAX_SMALL_ANSWER {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        body.extend_from_slice(&chunk);
    }
    body.truncate(MAX_SMALL_ANSWER);
    Ok(())
}

/// The text of an error answer's body, read up to [`MAX_SMALL_ANSWER`]
/// bytes and as far as it arrives, so a cut-off answer still names its
/// error code.
pub(crate) async fn error_body(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    // What arrived is the answer's text, whether or not the rest followed.
    let _ = read_start(&mut response, &mut body).await;
    String::from_utf8_lossy(&body).into_owned()
}

/// Read `response`'s body, which holds or may hold a credential, as JSON of
/// type `T`. A body that does not arrive is [`Error::Http`], one over `max`
/// bytes is [`Error::AnswerTooLarge`], and one that is not the JSON expected
/// is [`Error::UnreadableAnswer`], with no value from it. The errors name
/// the address without its query, which can carry a key.
pub(crate) async fn answer<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    max: usize,
) -> Result<T, Error> {
    let Some(body) = read_capped(&mut response, max).await? else {
        return Err(Error::AnswerTooLarge {
            url: shown_url(&response),
            max,
        });
    };
    parse(&body).map_err(|source| Error::UnreadableAnswer {
        url: shown_url(&response),
        source,
    })
}

/// The address that sent `response`, without its query or fragment. The
/// client has already moved any user and password into a header.
fn shown_url(response: &reqwest::Response) -> Box<reqwest::Url> {
    let mut url = response.url().clone();
    url.set_query(None);
    url.set_fragment(None);
    Box::new(url)
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

    /// An answer that is not the JSON expected names who sent it and where
    /// and why it failed, with no value from the body in its text or debug
    /// form, and is not re-sent. Breaks if the body reaches either, the
    /// address is dropped, or the answer is retried.
    #[tokio::test]
    async fn an_unreadable_answer_leaves_the_body_out() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("\"s3cret\""))
            .mount(&server)
            .await;
        let response = crate::test_http_client()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let err = answer::<u64>(response, MAX_SMALL_ANSWER).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "unreadable answer from {}/: invalid type: string, expected u64 at line 1 column 8",
                server.uri()
            )
        );
        assert!(!format!("{err:?}").contains("s3cret"), "{err:?}");
        assert!(!crate::retry::is_transient(&err));
        assert_eq!(
            crate::error::CredentialFailure::of_unreadable(&err),
            crate::error::CredentialFailure::Failed
        );
    }

    /// A body past the limit is refused from its declared length before any
    /// is read, or as it streams when none is declared, and one at the limit
    /// is read. Breaks if either check is dropped or is off by one.
    #[tokio::test]
    async fn read_capped_stops_at_the_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// The body a server answering `head` then `body` gives under a
        /// limit of 4 bytes.
        async fn read(head: &'static str, body: &'static str) -> Option<Vec<u8>> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = stream.read(&mut [0; 1024]).await.unwrap();
                stream
                    .write_all(format!("HTTP/1.1 200 OK\r\n{head}\r\n{body}").as_bytes())
                    .await
                    .unwrap();
            });
            let mut response = crate::test_http_client()
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            read_capped(&mut response, 4).await.unwrap()
        }
        // Declared too long and never sent in full: refused before reading.
        assert_eq!(read("content-length: 5\r\n", "1234").await, None);
        assert_eq!(read("connection: close\r\n", "12345").await, None);
        assert_eq!(
            read("connection: close\r\n", "1234").await,
            Some(b"1234".to_vec())
        );
    }

    /// An error answer cut off partway keeps what arrived, so its error code
    /// is still read. Breaks if a failed read drops the bytes received.
    #[tokio::test]
    async fn a_cut_off_error_body_keeps_what_arrived() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.read(&mut [0; 1024]).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 100\r\n\r\n{\"error\":\"invalid_grant\"")
                .await
                .unwrap();
        });
        let response = crate::test_http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(error_body(response).await, "{\"error\":\"invalid_grant\"");
    }

    /// An answer longer than its limit is refused, naming the address
    /// without its user, password or query. Breaks if the whole body is read
    /// whatever its length, or any of those reaches the error.
    #[tokio::test]
    async fn an_answer_over_the_limit_is_refused() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(vec![
                b' ';
                MAX_SMALL_ANSWER
                    + 1
            ]))
            .mount(&server)
            .await;
        let response = crate::test_http_client()
            .get(server.uri().replace("://", "://user:s3cret@") + "/token?key=s3cret")
            .send()
            .await
            .unwrap();
        let err = answer::<u64>(response, MAX_SMALL_ANSWER).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("answer from {}/token over 1048576 bytes", server.uri())
        );
    }

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
