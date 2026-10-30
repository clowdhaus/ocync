//! Output formatting and credential redaction utilities.

use std::time::Duration;

/// Format a byte count with SI decimal prefixes (1 KB = 1,000 bytes), the
/// convention of size parsing, so parsed and displayed values round-trip.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000_000_000_000;

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

/// Format a duration as `"0.3s"`, `"47s"`, `"2m 13s"` or `"1h 30m"`.
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{hours}h {mins}m")
    }
}

/// Strip userinfo (username:password) from a URL for safe logging.
pub(crate) fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.find('@') {
            // An `@` after a slash is path content, not userinfo.
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
    fn format_bytes_uses_si_prefixes() {
        let cases = [
            (0, "0 B"),
            (512, "512 B"),
            (1_000, "1.0 KB"),
            (1_500, "1.5 KB"),
            (1_000_000, "1.0 MB"),
            (5_500_000, "5.5 MB"),
            (1_000_000_000, "1.0 GB"),
            (1_000_000_000_000, "1.0 TB"),
            (5_500_000_000_000, "5.5 TB"),
        ];
        for (bytes, want) in cases {
            assert_eq!(format_bytes(bytes), want, "{bytes} bytes");
        }
    }

    #[test]
    fn format_duration_picks_the_largest_units() {
        let cases = [
            (Duration::ZERO, "0.0s"),
            (Duration::from_millis(300), "0.3s"),
            (Duration::from_secs(5), "5s"),
            (Duration::from_secs(47), "47s"),
            (Duration::from_secs(60), "1m 0s"),
            (Duration::from_secs(133), "2m 13s"),
            (Duration::from_secs(3600), "1h 0m"),
            (Duration::from_secs(3661), "1h 1m"),
            (Duration::from_secs(7200), "2h 0m"),
        ];
        for (d, want) in cases {
            assert_eq!(format_duration(d), want, "{d:?}");
        }
    }

    #[test]
    fn redact_url_hides_only_userinfo() {
        let cases = [
            (
                "user and password",
                "https://user:pass@registry.example.com/v2",
                "https://***@registry.example.com/v2",
            ),
            (
                "username only",
                "https://user@registry.example.com/v2",
                "https://***@registry.example.com/v2",
            ),
            (
                "no userinfo",
                "https://registry.example.com/v2",
                "https://registry.example.com/v2",
            ),
            ("no scheme", "registry.example.com", "registry.example.com"),
            // An `@` after a slash is path content.
            (
                "at sign in path",
                "https://registry.example.com/repo@sha256:abc",
                "https://registry.example.com/repo@sha256:abc",
            ),
            ("empty", "", ""),
        ];
        for (name, url, want) in cases {
            assert_eq!(redact_url(url), want, "{name}");
        }
    }
}
