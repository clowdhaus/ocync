//! SHA-256 wrapper backed by aws-lc-rs, the AWS-LC FIPS module in a FIPS build.

/// SHA-256.
#[derive(Debug)]
pub struct Sha256;

impl Sha256 {
    /// Hash `data` in one shot.
    pub fn digest(data: &[u8]) -> [u8; 32] {
        let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
        let mut out = [0u8; 32];
        out.copy_from_slice(hash.as_ref());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let hash = Sha256::digest(b"");
        assert_eq!(
            hex::encode(hash),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hello_world() {
        let hash = Sha256::digest(b"hello world");
        assert_eq!(
            hex::encode(hash),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
