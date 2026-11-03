//! Sidecar error codes and framing errors.

use serde::{Deserialize, Serialize};

/// Error codes carried by `response` and `fatal` messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarErrorCode {
    ProtocolError,
    ImageMissing,
    ImageDecodeFailed,
    ModelLoadFailed,
    BackendUnavailable,
    Busy,
    OutOfMemory,
    Cancelled,
    Internal,
}

impl SidecarErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProtocolError => "protocol_error",
            Self::ImageMissing => "image_missing",
            Self::ImageDecodeFailed => "image_decode_failed",
            Self::ModelLoadFailed => "model_load_failed",
            Self::BackendUnavailable => "backend_unavailable",
            Self::Busy => "busy",
            Self::OutOfMemory => "out_of_memory",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }

    /// Parses a wire code. Unknown codes return `None` so a newer sidecar
    /// cannot turn into a silently wrong classification.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "protocol_error" => Some(Self::ProtocolError),
            "image_missing" => Some(Self::ImageMissing),
            "image_decode_failed" => Some(Self::ImageDecodeFailed),
            "model_load_failed" => Some(Self::ModelLoadFailed),
            "backend_unavailable" => Some(Self::BackendUnavailable),
            "busy" => Some(Self::Busy),
            "out_of_memory" => Some(Self::OutOfMemory),
            "cancelled" => Some(Self::Cancelled),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

/// Error body carried by `response` and `fatal` messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarError {
    pub code: SidecarErrorCode,
    pub message: String,
    /// The sidecar decides whether the host may retry the operation.
    pub retryable: bool,
}

/// A malformed or unreadable JSONL message on the sidecar channel.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("line exceeds the 1 MiB limit")]
    LineTooLong,
    #[error("line is not valid UTF-8")]
    InvalidUtf8,
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_error_codes_round_trip_with_the_wire_strings() {
        let cases = [
            (SidecarErrorCode::ProtocolError, "protocol_error"),
            (SidecarErrorCode::ImageMissing, "image_missing"),
            (SidecarErrorCode::ImageDecodeFailed, "image_decode_failed"),
            (SidecarErrorCode::ModelLoadFailed, "model_load_failed"),
            (SidecarErrorCode::BackendUnavailable, "backend_unavailable"),
            (SidecarErrorCode::Busy, "busy"),
            (SidecarErrorCode::OutOfMemory, "out_of_memory"),
            (SidecarErrorCode::Cancelled, "cancelled"),
            (SidecarErrorCode::Internal, "internal"),
        ];
        for (code, text) in cases {
            assert_eq!(code.as_str(), text);
            assert_eq!(SidecarErrorCode::parse(text), Some(code));
            assert_eq!(serde_json::to_value(code).unwrap(), text);
            assert_eq!(
                serde_json::from_value::<SidecarErrorCode>(serde_json::json!(text)).unwrap(),
                code
            );
        }
        assert_eq!(SidecarErrorCode::parse("future_code"), None);
    }
}
