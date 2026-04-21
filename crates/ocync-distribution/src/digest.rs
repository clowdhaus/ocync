//! OCI content-addressable digest in `algorithm:hex` format.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// SHA-256 algorithm name as used in OCI digest strings.
const SHA256_ALGO: &str = "sha256";

/// OCI content-addressable digest in `algorithm:hex` format (e.g. `sha256:abcd...`).
#[derive(Debug, Clone, Eq)]
pub struct Digest {
    /// The full `algorithm:hex` string.
    raw: String,
    /// Length of the algorithm portion (byte offset of the `:` separator).
    algo_len: usize,
}

impl Digest {
    /// Build a digest from a 32-byte SHA-256 hash.
    pub fn from_sha256(bytes: [u8; 32]) -> Self {
        let hex = hex::encode(bytes);
        let raw = format!("{SHA256_ALGO}:{hex}");
        Self {
            raw,
            algo_len: SHA256_ALGO.len(),
        }
    }

    /// The algorithm portion (e.g. `sha256`).
    pub fn algorithm(&self) -> &str {
        &self.raw[..self.algo_len]
    }

    /// The hex-encoded hash portion.
    pub fn hex(&self) -> &str {
        &self.raw[self.algo_len + 1..]
    }

    /// Build the OCI tag fallback name for referrers discovery.
    ///
    /// Older registries that do not support the referrers API store artifact
    /// references as tags named `{algorithm}-{hex}` (e.g. `sha256-abcd...`).
    pub fn tag_fallback(&self) -> String {
        format!("{}-{}", self.algorithm(), self.hex())
    }
}

impl FromStr for Digest {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let algo_len = s.find(':').ok_or_else(|| Error::InvalidDigest {
            digest: s.into(),
            reason: "missing ':' separator".into(),
        })?;

        let algorithm = &s[..algo_len];
        let hex_part = &s[algo_len + 1..];

        if algorithm.is_empty() {
            return Err(Error::InvalidDigest {
                digest: s.into(),
                reason: "empty algorithm".into(),
            });
        }

        if hex_part.is_empty() {
            return Err(Error::InvalidDigest {
                digest: s.into(),
                reason: "empty hex portion".into(),
            });
        }

        if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::InvalidDigest {
                digest: s.into(),
                reason: "hex portion contains invalid characters".into(),
            });
        }

        // SHA-256 must be exactly 64 hex chars (case-insensitive algorithm match).
        if algorithm.eq_ignore_ascii_case(SHA256_ALGO) && hex_part.len() != 64 {
            return Err(Error::InvalidDigest {
                digest: s.into(),
                reason: format!("sha256 hex must be 64 characters, got {}", hex_part.len()),
            });
        }

        // Normalize to lowercase so equality and hashing are case-insensitive.
        // Fast path: skip allocation when already lowercase (common case).
        let already_lower = !s.bytes().any(|b| b.is_ascii_uppercase());
        if already_lower {
            Ok(Self {
                raw: s.to_owned(),
                algo_len: algorithm.len(),
            })
        } else {
            let algo_lower = algorithm.to_ascii_lowercase();
            let raw = format!("{}:{}", algo_lower, hex_part.to_ascii_lowercase());
            Ok(Self {
                raw,
                algo_len: algo_lower.len(),
            })
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialEq for Digest {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Hash for Digest {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const TEST_DIGEST: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn parse_valid() {
        let d: Digest = TEST_DIGEST.parse().unwrap();
        assert_eq!(d.algorithm(), "sha256");
        assert_eq!(
            d.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn missing_colon() {
        let r = "sha256abc".parse::<Digest>();
        assert!(r.is_err());
    }

    #[test]
    fn empty_hex() {
        let r = "sha256:".parse::<Digest>();
        assert!(r.is_err());
    }

    #[test]
    fn invalid_hex_chars() {
        let r = "sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
            .parse::<Digest>();
        assert!(r.is_err());
    }

    #[test]
    fn wrong_length() {
        let r = "sha256:abcd".parse::<Digest>();
        assert!(r.is_err());
    }

    #[test]
    fn from_sha256_bytes() {
        let hash = crate::sha256::Sha256::digest(b"");
        let d = Digest::from_sha256(hash);
        assert_eq!(d.to_string(), TEST_DIGEST);
    }

    #[test]
    fn serde_roundtrip() {
        let d: Digest = TEST_DIGEST.parse().unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let d2: Digest = serde_json::from_str(&json).unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn equality_and_hash() {
        let d1: Digest = TEST_DIGEST.parse().unwrap();
        let d2: Digest = TEST_DIGEST.parse().unwrap();
        assert_eq!(d1, d2);

        let mut set = HashSet::new();
        set.insert(d1.clone());
        assert!(set.contains(&d2));
    }

    #[test]
    fn parse_sha512() {
        let hex = "a".repeat(128);
        let input = format!("sha512:{hex}");
        let d: Digest = input.parse().unwrap();
        assert_eq!(d.algorithm(), "sha512");
        assert_eq!(d.hex().len(), 128);
    }

    #[test]
    fn parse_unknown_algorithm() {
        let hex = "a".repeat(96);
        let input = format!("sha384:{hex}");
        let d: Digest = input.parse().unwrap();
        assert_eq!(d.algorithm(), "sha384");
    }

    #[test]
    fn uppercase_hex_accepted() {
        let hex = "A".repeat(64);
        let input = format!("sha256:{hex}");
        let d: Digest = input.parse().unwrap();
        // Parsing normalizes to lowercase.
        assert_eq!(d.hex(), hex.to_ascii_lowercase());
        // Upper and lower parse to the same value.
        let lower_input = format!("sha256:{}", "a".repeat(64));
        let d_lower: Digest = lower_input.parse().unwrap();
        assert_eq!(d, d_lower);
    }

    #[test]
    fn mixed_case_digests_are_equal() {
        let lower: Digest =
            "sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                .parse()
                .unwrap();
        let upper: Digest =
            "sha256:ABCDEF1234567890ABCDEF1234567890ABCDEF1234567890ABCDEF1234567890"
                .parse()
                .unwrap();
        assert_eq!(lower, upper);
        // Also verify the stored representation is lowercase.
        assert!(upper.to_string().chars().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn leading_colon_error() {
        let r = ":abcdef".parse::<Digest>();
        assert!(r.is_err());
    }

    #[test]
    fn display_is_normalized_lowercase() {
        let d: Digest = TEST_DIGEST.parse().unwrap();
        assert_eq!(d.to_string(), TEST_DIGEST);
        // Uppercase input is normalized to lowercase on display.
        let upper = format!("sha256:{}", "A".repeat(64));
        let d2: Digest = upper.parse().unwrap();
        assert_eq!(d2.to_string(), format!("sha256:{}", "a".repeat(64)));
    }

    #[test]
    fn deserialize_non_string_errors() {
        let result: Result<Digest, _> = serde_json::from_str("123");
        assert!(result.is_err());
    }

    #[test]
    fn uppercase_algorithm_normalized() {
        let hex = "a".repeat(64);
        let input = format!("SHA256:{hex}");
        let d: Digest = input.parse().unwrap();
        assert_eq!(d.algorithm(), "sha256");
        assert_eq!(d.to_string(), format!("sha256:{hex}"));

        // Uppercase algorithm + lowercase hex matches lowercase algorithm + lowercase hex.
        let lower_input = format!("sha256:{hex}");
        let d_lower: Digest = lower_input.parse().unwrap();
        assert_eq!(d, d_lower);
    }

    #[test]
    fn mixed_case_algorithm_normalized() {
        let hex = "b".repeat(64);
        let input = format!("Sha256:{hex}");
        let d: Digest = input.parse().unwrap();
        assert_eq!(d.algorithm(), "sha256");
    }

    #[test]
    fn tag_fallback_replaces_colon_with_dash() {
        let d: Digest = TEST_DIGEST.parse().unwrap();
        let tag = d.tag_fallback();
        assert_eq!(
            tag,
            "sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(!tag.contains(':'), "tag fallback must not contain ':'");
    }
}
