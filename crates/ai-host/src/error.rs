//! Errors returned by the AI plugin host.

/// Result alias for plugin host operations.
pub type Result<T> = std::result::Result<T, AiHostError>;

/// A typed failure while verifying or installing an AI plugin package.
#[derive(Debug, thiserror::Error)]
pub enum AiHostError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid manifest JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest signature verification failed")]
    Signature,
    #[error("invalid manifest: {0}")]
    ManifestInvalid(String),
    #[error("unsafe path in plugin package: {0}")]
    UnsafePath(String),
    #[error("duplicate package entry: {0}")]
    DuplicateEntry(String),
    #[error("package entry is missing: {0}")]
    MissingEntry(String),
    #[error("package entry is not listed in the manifest: {0}")]
    UnlistedEntry(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("install limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("model files split into parts are not supported")]
    UnsupportedParts,
    #[error("unsupported manifest format version: {0}")]
    UnsupportedFormat(u32),
    #[error("unsupported sidecar protocol version: {0}")]
    UnsupportedProtocol(u32),
    #[error("sidecar fatal error: {0}")]
    SidecarFatal(String),
    #[error("sidecar exited unexpectedly")]
    SidecarExited,
    #[error("sidecar request timed out")]
    SidecarTimeout,
    #[error("sidecar protocol violation: {0}")]
    SidecarProtocol(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("model license consent is required")]
    ConsentRequired,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_have_human_readable_messages() {
        assert_eq!(
            AiHostError::Signature.to_string(),
            "manifest signature verification failed"
        );
        assert_eq!(
            AiHostError::UnsafePath("../escape".to_string()).to_string(),
            "unsafe path in plugin package: ../escape"
        );
        assert_eq!(
            AiHostError::UnsupportedProtocol(2).to_string(),
            "unsupported sidecar protocol version: 2"
        );
        assert_eq!(
            AiHostError::SidecarFatal("boom".to_string()).to_string(),
            "sidecar fatal error: boom"
        );
        assert_eq!(
            AiHostError::ConsentRequired.to_string(),
            "model license consent is required"
        );
    }
}
