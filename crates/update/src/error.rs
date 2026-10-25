use std::path::PathBuf;

/// The operation whose I/O failed, used to choose a user-facing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOperation {
    Download,
    Helper,
    Job,
    Apply,
    Spawn,
    Other,
}

/// Errors produced by update checks, downloads, and the apply helper.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Network(String),
    #[error("update resource not found")]
    NotFound,
    #[error("invalid update manifest: {0}")]
    InvalidManifest(String),
    #[error("update signature verification failed")]
    Signature,
    #[error("update integrity check failed: {0}")]
    Integrity(String),
    #[error("this platform has no update artifact")]
    UnsupportedPlatform,
    #[error("update was cancelled")]
    Cancelled,
    #[error("update helper is missing")]
    HelperMissing,
    #[error("{operation:?} failed at {path}: {source}")]
    Io {
        operation: IoOperation,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid update job: {0}")]
    InvalidJob(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, UpdateError>;

impl UpdateError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::io_at(IoOperation::Other, path, source)
    }

    pub fn io_at(operation: IoOperation, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub fn with_io_operation(self, operation: IoOperation) -> Self {
        match self {
            Self::Io { path, source, .. } => Self::Io {
                operation,
                path,
                source,
            },
            other => other,
        }
    }
}
