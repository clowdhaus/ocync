//! Output formatting and credential redaction utilities.

use std::time::Duration;

/// Format a byte count into a human-readable string (e.g., "1.5 MB").
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format a duration into a short human-readable string (e.g., "5s", "1m 5s", "1h 1m 5s").
pub(crate) fn format_duration_short(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs == 0 {
        let millis = d.as_millis();
        return format!("{millis}ms");
    }

    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

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

/// Redact the value portion of an Authorization header for safe logging.
///
/// Preserves the auth scheme (Bearer/Basic) but replaces the credential
/// value with `[REDACTED]`. Does not expose token length to prevent
/// fingerprinting of the auth mechanism.
pub(crate) fn redact_auth_header(header: &str) -> String {
    if header.strip_prefix("Bearer ").is_some() {
        "Bearer [REDACTED]".to_string()
    } else if header.strip_prefix("Basic ").is_some() {
        "Basic [REDACTED]".to_string()
    } else {
        "[REDACTED]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // format_bytes tests

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_one() {
        assert_eq!(format_bytes(1), "1 B");
    }

    #[test]
    fn format_bytes_under_kb() {
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn format_bytes_one_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }

    #[test]
    fn format_bytes_one_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn format_bytes_one_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn format_bytes_one_tb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 1024), "1.0 TB");
    }

    #[test]
    fn format_bytes_multi_tb() {
        assert_eq!(format_bytes(10 * 1024 * 1024 * 1024 * 1024), "10.0 TB");
    }

    // format_duration_short tests

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration_short(Duration::ZERO), "0ms");
    }

    #[test]
    fn format_duration_millis() {
        assert_eq!(format_duration_short(Duration::from_millis(500)), "500ms");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration_short(Duration::from_secs(5)), "5s");
    }

    #[test]
    fn format_duration_minutes_seconds() {
        assert_eq!(format_duration_short(Duration::from_secs(65)), "1m 5s");
    }

    #[test]
    fn format_duration_exact_minute() {
        assert_eq!(format_duration_short(Duration::from_secs(60)), "1m 0s");
    }

    #[test]
    fn format_duration_hours_minutes_seconds() {
        assert_eq!(format_duration_short(Duration::from_secs(3661)), "1h 1m 1s");
    }

    #[test]
    fn format_duration_exact_hour() {
        assert_eq!(format_duration_short(Duration::from_secs(3600)), "1h 0m 0s");
    }

    // redact_url tests

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

    // redact_auth_header tests

    #[test]
    fn redact_bearer() {
        assert_eq!(
            redact_auth_header("Bearer abc123def456"),
            "Bearer [REDACTED]"
        );
    }

    #[test]
    fn redact_basic() {
        assert_eq!(redact_auth_header("Basic dXNlcjpwYXNz"), "Basic [REDACTED]");
    }

    #[test]
    fn redact_unknown_scheme() {
        assert_eq!(redact_auth_header("CustomScheme token"), "[REDACTED]");
    }

    #[test]
    fn redact_empty_header() {
        assert_eq!(redact_auth_header(""), "[REDACTED]");
    }
}
