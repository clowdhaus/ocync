use thiserror::Error;

#[derive(Debug, Error)]
pub enum DistributionError {
    #[error("invalid image reference '{input}': {reason}")]
    InvalidReference { input: String, reason: String },

    #[error("invalid digest '{digest}': {reason}")]
    InvalidDigest { digest: String, reason: String },

    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },

    #[error("unsupported manifest media type: {media_type}")]
    UnsupportedMediaType { media_type: String },

    #[error("manifest deserialization failed: {0}")]
    ManifestParse(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_reference() {
        let err = DistributionError::InvalidReference {
            input: "!!!".into(),
            reason: "bad chars".into(),
        };
        assert!(err.to_string().contains("!!!"));
        assert!(err.to_string().contains("bad chars"));
    }

    #[test]
    fn display_invalid_digest() {
        let err = DistributionError::InvalidDigest {
            digest: "bad".into(),
            reason: "missing colon".into(),
        };
        assert!(err.to_string().contains("bad"));
        assert!(err.to_string().contains("missing colon"));
    }

    #[test]
    fn display_digest_mismatch() {
        let err = DistributionError::DigestMismatch {
            expected: "sha256:aaa".into(),
            actual: "sha256:bbb".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("sha256:aaa"));
        assert!(msg.contains("sha256:bbb"));
    }

    #[test]
    fn display_unsupported_media_type() {
        let err = DistributionError::UnsupportedMediaType {
            media_type: "text/plain".into(),
        };
        assert!(err.to_string().contains("text/plain"));
    }

    #[test]
    fn display_other() {
        let err = DistributionError::Other("something broke".into());
        assert_eq!(err.to_string(), "something broke");
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DistributionError>();
    }
}
