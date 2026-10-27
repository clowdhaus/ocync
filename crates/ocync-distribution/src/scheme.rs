//! The URL schemes requests are sent over.

use url::Url;

/// A scheme HTTP requests can be sent over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// `http`.
    Http,
    /// `https`.
    Https,
}

/// A URL scheme that is neither `http` nor `https`.
#[derive(Debug, thiserror::Error)]
#[error("unsupported scheme '{0}', not http or https")]
pub(crate) struct UnsupportedScheme(String);

/// The scheme of a URL.
impl TryFrom<&Url> for Scheme {
    type Error = UnsupportedScheme;

    fn try_from(url: &Url) -> Result<Self, UnsupportedScheme> {
        match url.scheme() {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            other => Err(UnsupportedScheme(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `http` and `https` read, in any case the URL parser accepts,
    /// and a refusal names the scheme. Breaks if another scheme is taken as
    /// HTTP.
    #[test]
    fn reads_only_http_and_https() {
        let of = |text: &str| Scheme::try_from(&Url::parse(text).unwrap());
        assert_eq!(of("http://host/").unwrap(), Scheme::Http);
        assert_eq!(of("HTTPS://host/").unwrap(), Scheme::Https);
        assert_eq!(
            of("ftp://host/").unwrap_err().to_string(),
            "unsupported scheme 'ftp', not http or https"
        );
        assert_eq!(
            of("unix:/run/socket").unwrap_err().to_string(),
            "unsupported scheme 'unix', not http or https"
        );
    }
}
