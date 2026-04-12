//! Output formatting and credential redaction utilities.

/// Strip userinfo (username:password) from a URL for safe logging.
pub(crate) fn redact_url(url: &str) -> String {
    // Match pattern: scheme://user:pass@host...
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.find('@') {
            // Check there's no slash before the @, meaning it's actually userinfo
            if !after_scheme[..at_pos].contains('/') {
                return format!(
                    "{}://***@{}",
                    &url[..scheme_end],
                    &after_scheme[at_pos + 1..]
                );
            }
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_with_userinfo() {
        assert_eq!(
            redact_url("https://user:pass@registry.example.com/v2"),
            "https://***@registry.example.com/v2"
        );
    }

    #[test]
    fn redact_url_username_only() {
        assert_eq!(
            redact_url("https://user@registry.example.com/v2"),
            "https://***@registry.example.com/v2"
        );
    }

    #[test]
    fn redact_url_without_userinfo() {
        assert_eq!(
            redact_url("https://registry.example.com/v2"),
            "https://registry.example.com/v2"
        );
    }

    #[test]
    fn redact_url_no_scheme() {
        assert_eq!(redact_url("registry.example.com"), "registry.example.com");
    }

    #[test]
    fn redact_url_at_in_path() {
        // @ after a slash is path content, not userinfo
        assert_eq!(
            redact_url("https://registry.example.com/repo@sha256:abc"),
            "https://registry.example.com/repo@sha256:abc"
        );
    }

    #[test]
    fn redact_url_empty() {
        assert_eq!(redact_url(""), "");
    }
}
