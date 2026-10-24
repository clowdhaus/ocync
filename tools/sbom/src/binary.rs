//! Identify the AWS-LC library linked into a binary from the release string
//! AWS-LC compiles in, such as `AWS-LC FIPS 4.2.0` or `AWS-LC 5.11.0`.

use crate::Crypto;

/// The release string prefix every AWS-LC build carries.
const MARKER: &[u8] = b"AWS-LC ";

/// What follows [`MARKER`] in a FIPS module build.
const FIPS: &[u8] = b"FIPS ";

/// The AWS-LC library a binary links.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AwsLc {
    /// The build the release string belongs to.
    pub(crate) crypto: Crypto,
    /// The AWS-LC release, such as `4.2.0`.
    pub(crate) release: String,
}

/// Find the one AWS-LC release linked into `bytes` and confirm it is the
/// `expected` build. A FIPS binary carrying a non-FIPS release string links
/// both libraries, which is a failure.
pub(crate) fn aws_lc(bytes: &[u8], expected: Crypto) -> Result<AwsLc, String> {
    let mut found: Vec<AwsLc> = Vec::new();
    let mut rest = bytes;
    while let Some(at) = find(rest, MARKER) {
        rest = &rest[at + MARKER.len()..];
        let (crypto, tail) = match rest.strip_prefix(FIPS) {
            Some(tail) => (Crypto::Fips, tail),
            None => (Crypto::NonFips, rest),
        };
        if let Some(release) = release(tail) {
            let entry = AwsLc { crypto, release };
            if !found.contains(&entry) {
                found.push(entry);
            }
        }
    }
    match found.as_slice() {
        [one] if one.crypto == expected => Ok(found.remove(0)),
        [] => Err("no AWS-LC release string in the binary".to_string()),
        _ => Err(format!(
            "expected one {expected:?} AWS-LC release, found {}",
            found
                .iter()
                .map(|f| format!("{:?} {}", f.crypto, f.release))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A dotted release such as `4.2.0` at the start of `bytes`, ended by a
/// byte that cannot continue it. Rejects text such as `AWS-LC KEM method`.
fn release(bytes: &[u8]) -> Option<String> {
    let len = bytes
        .iter()
        .take_while(|b| b.is_ascii_digit() || **b == b'.')
        .count();
    let candidate = std::str::from_utf8(&bytes[..len]).ok()?;
    let parts: Vec<&str> = candidate.split('.').collect();
    (parts.len() == 3 && parts.iter().all(|p| !p.is_empty())).then(|| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_fips_release() {
        let bytes = b"\0AWS-LC KEM method\0AWS-LC FIPS 4.2.0\0AWS-LC FIPS failure caused by:\0";
        assert_eq!(
            aws_lc(bytes, Crypto::Fips),
            Ok(AwsLc {
                crypto: Crypto::Fips,
                release: "4.2.0".to_string()
            })
        );
    }

    #[test]
    fn reads_a_non_fips_release() {
        let bytes = b"\0AWS-LC PQ DSA method\0AWS-LC 5.11.0\0";
        assert_eq!(
            aws_lc(bytes, Crypto::NonFips),
            Ok(AwsLc {
                crypto: Crypto::NonFips,
                release: "5.11.0".to_string()
            })
        );
    }

    #[test]
    fn rejects_a_fips_binary_that_also_links_non_fips_aws_lc() {
        let bytes = b"\0AWS-LC FIPS 4.2.0\0AWS-LC 5.11.0\0";
        assert_eq!(
            aws_lc(bytes, Crypto::Fips),
            Err("expected one Fips AWS-LC release, found Fips 4.2.0, NonFips 5.11.0".to_string())
        );
    }

    #[test]
    fn rejects_a_non_fips_binary_where_fips_was_expected() {
        assert_eq!(
            aws_lc(b"\0AWS-LC 5.11.0\0", Crypto::Fips),
            Err("expected one Fips AWS-LC release, found NonFips 5.11.0".to_string())
        );
    }

    #[test]
    fn rejects_a_binary_without_aws_lc() {
        assert_eq!(
            aws_lc(b"\0AWS-LC KEM method\0", Crypto::NonFips),
            Err("no AWS-LC release string in the binary".to_string())
        );
    }
}
