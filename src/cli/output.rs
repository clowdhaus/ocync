//! Output formatting and credential redaction utilities.

use std::time::Duration;

/// Format a byte count as a human-readable string using SI decimal prefixes.
///
/// Matches the same SI convention as size parsing (1 KB = 1,000 bytes) so
/// that parsed and displayed values round-trip consistently.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format a duration as a human-readable string.
///
/// - Sub-second: `"0.3s"`
/// - Seconds: `"47s"`
/// - Minutes+: `"2m 13s"`
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_bytes() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_kb() {
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(format_bytes(1_500), "1.5 KB");
    }

    #[test]
    fn format_bytes_mb() {
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
        assert_eq!(format_bytes(5_500_000), "5.5 MB");
    }

    #[test]
    fn format_bytes_gb() {
        assert_eq!(format_bytes(1_000_000_000), "1.0 GB");
    }

    #[test]
    fn format_duration_sub_second() {
        assert_eq!(format_duration(Duration::from_millis(300)), "0.3s");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(47)), "47s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(133)), "2m 13s");
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::ZERO), "0.0s");
    }

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
