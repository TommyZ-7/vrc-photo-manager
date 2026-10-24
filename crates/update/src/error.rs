use std::path::PathBuf;

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
    #[error("io error at {path}: {source}")]
    Io {
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
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
