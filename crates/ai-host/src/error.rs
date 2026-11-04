//! Errors returned by the AI plugin host.

use vrc_photon_ai_protocol::SidecarError;

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
    #[error("model download failed: {0}")]
    Network(String),
    #[error("install limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("model files split into parts are not supported")]
    UnsupportedParts,
    #[error("unsupported manifest format version: {0}")]
    UnsupportedFormat(u32),
    #[error("unsupported sidecar protocol version: {0}")]
    UnsupportedProtocol(u32),
    #[error("sidecar identity mismatch: {0}")]
    SidecarIdentity(String),
    #[error("sidecar backend mismatch: requested {expected}, sidecar reported {actual}")]
    SidecarBackendMismatch { expected: String, actual: String },
    #[error("sidecar fatal error: {}: {}", .0.code.as_str(), .0.message)]
    SidecarFatal(SidecarError),
    /// The sidecar answered a request with `ok: false`.
    ///
    /// The structured code and `retryable` flag drive the daemon's retry
    /// policy, so they must survive the host boundary.
    #[error("sidecar rejected the request: {}: {}", .0.code.as_str(), .0.message)]
    SidecarRejected(SidecarError),
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
        use vrc_photon_ai_protocol::SidecarErrorCode;

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
            AiHostError::SidecarIdentity("plugin_id: expected a, got b".to_string()).to_string(),
            "sidecar identity mismatch: plugin_id: expected a, got b"
        );
        assert_eq!(
            AiHostError::SidecarBackendMismatch {
                expected: "vulkan".to_string(),
                actual: "cpu".to_string(),
            }
            .to_string(),
            "sidecar backend mismatch: requested vulkan, sidecar reported cpu"
        );
        assert_eq!(
            AiHostError::SidecarFatal(SidecarError {
                code: SidecarErrorCode::ModelLoadFailed,
                message: "boom".to_string(),
                retryable: false,
            })
            .to_string(),
            "sidecar fatal error: model_load_failed: boom"
        );
        assert_eq!(
            AiHostError::Network("timed out".to_string()).to_string(),
            "model download failed: timed out"
        );
        assert_eq!(
            AiHostError::SidecarRejected(SidecarError {
                code: SidecarErrorCode::Busy,
                message: "another request is in flight".to_string(),
                retryable: true,
            })
            .to_string(),
            "sidecar rejected the request: busy: another request is in flight"
        );
        assert_eq!(
            AiHostError::ConsentRequired.to_string(),
            "model license consent is required"
        );
    }

    #[test]
    fn fatal_sidecar_errors_keep_the_structured_fields() {
        use vrc_photon_ai_protocol::SidecarErrorCode;

        let error = AiHostError::SidecarFatal(SidecarError {
            code: SidecarErrorCode::OutOfMemory,
            message: "allocation failed".to_string(),
            retryable: true,
        });
        let AiHostError::SidecarFatal(structured) = &error else {
            panic!("expected SidecarFatal, got {error:?}");
        };
        assert_eq!(structured.code, SidecarErrorCode::OutOfMemory);
        assert_eq!(structured.message, "allocation failed");
        assert!(structured.retryable);
    }
}
