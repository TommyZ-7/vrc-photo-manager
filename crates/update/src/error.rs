use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::install::PackageFamily;

/// A machine-readable reason for the last failed apply, stored in
/// `last-result.json` so the UI can show a localized message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastErrorCode {
    AuthCancelled,
    AuthUnavailable,
    PackageManagerLocked,
    PackageManagerFailed,
    PackageVerificationFailed,
    PackageMetadataMismatch,
    Downgrade,
    HealthCheckFailed,
    HelperFailed,
}

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
    #[error("this operation must run as an unprivileged user")]
    NotPrivileged,
    #[error("system update helper is missing")]
    SystemHelperMissing,
    #[error("staging rejected: {0}")]
    StagingRejected(String),
    #[error("the package manager database is locked")]
    PackageManagerLocked,
    #[error("the package manager is unavailable")]
    PackageManagerUnavailable,
    #[error("package metadata mismatch: {0}")]
    PackageMetadataMismatch(String),
    #[error("package verification failed: {0}")]
    PackageVerificationFailed(String),
    #[error("downgrade refused: current {current}, target {target}")]
    Downgrade { current: String, target: String },
    #[error("package manager failed: {family:?} {status}: {output}")]
    PackageManagerFailed {
        family: PackageFamily,
        status: String,
        output: String,
    },
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

    /// The exit code the privileged helper uses to report this failure to the
    /// user helper that launched it through `pkexec`.
    pub fn helper_exit_code(&self) -> i32 {
        match self {
            Self::InvalidJob(_) | Self::NotPrivileged | Self::StagingRejected(_) => 10,
            Self::Signature | Self::PackageVerificationFailed(_) => 11,
            Self::PackageMetadataMismatch(_) => 12,
            Self::Downgrade { .. } => 13,
            Self::PackageManagerLocked => 14,
            Self::PackageManagerFailed { .. } | Self::PackageManagerUnavailable => 15,
            _ => 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_exit_codes_map_the_privileged_failures() {
        assert_eq!(UpdateError::InvalidJob("x".into()).helper_exit_code(), 10);
        assert_eq!(UpdateError::NotPrivileged.helper_exit_code(), 10);
        assert_eq!(
            UpdateError::StagingRejected("x".into()).helper_exit_code(),
            10
        );
        assert_eq!(UpdateError::Signature.helper_exit_code(), 11);
        assert_eq!(
            UpdateError::PackageVerificationFailed("x".into()).helper_exit_code(),
            11
        );
        assert_eq!(
            UpdateError::PackageMetadataMismatch("x".into()).helper_exit_code(),
            12
        );
        assert_eq!(
            UpdateError::Downgrade {
                current: "0.9.2".into(),
                target: "0.9.1".into()
            }
            .helper_exit_code(),
            13
        );
        assert_eq!(UpdateError::PackageManagerLocked.helper_exit_code(), 14);
        assert_eq!(
            UpdateError::PackageManagerUnavailable.helper_exit_code(),
            15
        );
        assert_eq!(
            UpdateError::PackageManagerFailed {
                family: PackageFamily::Pacman,
                status: "exit status: 1".into(),
                output: String::new()
            }
            .helper_exit_code(),
            15
        );
        assert_eq!(UpdateError::SystemHelperMissing.helper_exit_code(), 16);
        assert_eq!(UpdateError::Other("x".into()).helper_exit_code(), 16);
    }

    #[test]
    fn last_error_codes_serialize_as_snake_case() {
        for (code, expected) in [
            (LastErrorCode::AuthCancelled, "\"auth_cancelled\""),
            (LastErrorCode::AuthUnavailable, "\"auth_unavailable\""),
            (
                LastErrorCode::PackageManagerLocked,
                "\"package_manager_locked\"",
            ),
            (
                LastErrorCode::PackageManagerFailed,
                "\"package_manager_failed\"",
            ),
            (
                LastErrorCode::PackageVerificationFailed,
                "\"package_verification_failed\"",
            ),
            (
                LastErrorCode::PackageMetadataMismatch,
                "\"package_metadata_mismatch\"",
            ),
            (LastErrorCode::Downgrade, "\"downgrade\""),
            (LastErrorCode::HealthCheckFailed, "\"health_check_failed\""),
            (LastErrorCode::HelperFailed, "\"helper_failed\""),
        ] {
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, expected);
            assert_eq!(serde_json::from_str::<LastErrorCode>(&json).unwrap(), code);
        }
    }
}
