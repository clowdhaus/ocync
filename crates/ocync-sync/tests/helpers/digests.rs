//! Digest and repository fixtures with no registry mock behind them, so
//! filesystem-only test binaries can include this file alone.

#![allow(dead_code, unused_imports, unreachable_pub)]

use ocync_distribution::Digest;
use ocync_distribution::spec::RepositoryName;

/// A sha256 digest from a short suffix, zero-padded to 64 hex chars.
pub fn make_digest(suffix: &str) -> Digest {
    format!("sha256:{suffix:0>64}").parse().unwrap()
}

const DIGEST_A: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const DIGEST_B: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The digest of the empty blob.
pub fn digest_a() -> Digest {
    DIGEST_A.parse().unwrap()
}

/// A digest unequal to [`digest_a`].
pub fn digest_b() -> Digest {
    DIGEST_B.parse().unwrap()
}

/// A repository name that must be valid.
pub fn repo(name: &str) -> RepositoryName {
    RepositoryName::new(name).unwrap()
}
