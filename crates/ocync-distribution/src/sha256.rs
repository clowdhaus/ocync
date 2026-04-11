//! SHA-256 wrapper backed by aws-lc-rs (FIPS 140-3 validated, NIST Certificate #4816).

/// SHA-256 hasher.
pub struct Sha256 {
    inner: aws_lc_rs::digest::Context,
}

impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sha256").finish_non_exhaustive()
    }
}

impl Sha256 {
    /// Create a new SHA-256 hasher.
    pub fn new() -> Self {
        Self {
            inner: aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256),
        }
    }

    /// Feed data into the hasher.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finalize and return the 32-byte SHA-256 hash.
    pub fn finalize(self) -> [u8; 32] {
        let result = self.inner.finish();
        let mut out = [0u8; 32];
        out.copy_from_slice(result.as_ref());
        out
    }

    /// Convenience: hash `data` in one shot and return the 32-byte digest.
    pub fn digest(data: &[u8]) -> [u8; 32] {
        let mut h = Self::new();
        h.update(data);
        h.finalize()
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn incremental_equals_single_shot() {
        let data = b"hello world";
        let single = Sha256::digest(data);

        let mut hasher = Sha256::new();
        hasher.update(b"hello ");
        hasher.update(b"world");
        let incremental = hasher.finalize();

        assert_eq!(single, incremental);
    }
}
