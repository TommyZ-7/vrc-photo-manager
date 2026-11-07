//! Persistent status enums shared by the database and IPC.

use serde::{Deserialize, Serialize};

/// Lifecycle of a tagging job (`tagging_jobs.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaggingJobStatus {
    Waiting,
    Pending,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

impl TaggingJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "waiting" => Some(Self::Waiting),
            "pending" => Some(Self::Pending),
            "processing" => Some(Self::Processing),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Pipeline stage of a running tagging job (`tagging_jobs.stage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaggingStage {
    Presence,
    PeopleCount,
    PlaceClass,
    PlaceTags,
    AtmosphereTags,
    Write,
}

impl TaggingStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Presence => "presence",
            Self::PeopleCount => "people_count",
            Self::PlaceClass => "place_class",
            Self::PlaceTags => "place_tags",
            Self::AtmosphereTags => "atmosphere_tags",
            Self::Write => "write",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "presence" => Some(Self::Presence),
            "people_count" => Some(Self::PeopleCount),
            "place_class" => Some(Self::PlaceClass),
            "place_tags" => Some(Self::PlaceTags),
            "atmosphere_tags" => Some(Self::AtmosphereTags),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// Error code stored on a failed tagging job (`tagging_jobs.error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaggingErrorCode {
    PluginNotReady,
    ModelMissing,
    ModelHashMismatch,
    SidecarExited,
    SidecarTimeout,
    SidecarProtocol,
    ImageMissing,
    ImageDecodeFailed,
    BackendUnavailable,
    Busy,
    OutOfMemory,
    Cancelled,
    PhotoMissing,
    Internal,
}

impl TaggingErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PluginNotReady => "plugin_not_ready",
            Self::ModelMissing => "model_missing",
            Self::ModelHashMismatch => "model_hash_mismatch",
            Self::SidecarExited => "sidecar_exited",
            Self::SidecarTimeout => "sidecar_timeout",
            Self::SidecarProtocol => "sidecar_protocol",
            Self::ImageMissing => "image_missing",
            Self::ImageDecodeFailed => "image_decode_failed",
            Self::BackendUnavailable => "backend_unavailable",
            Self::Busy => "busy",
            Self::OutOfMemory => "out_of_memory",
            Self::Cancelled => "cancelled",
            Self::PhotoMissing => "photo_missing",
            Self::Internal => "internal",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "plugin_not_ready" => Some(Self::PluginNotReady),
            "model_missing" => Some(Self::ModelMissing),
            "model_hash_mismatch" => Some(Self::ModelHashMismatch),
            "sidecar_exited" => Some(Self::SidecarExited),
            "sidecar_timeout" => Some(Self::SidecarTimeout),
            "sidecar_protocol" => Some(Self::SidecarProtocol),
            "image_missing" => Some(Self::ImageMissing),
            "image_decode_failed" => Some(Self::ImageDecodeFailed),
            "backend_unavailable" => Some(Self::BackendUnavailable),
            "busy" => Some(Self::Busy),
            "out_of_memory" => Some(Self::OutOfMemory),
            "cancelled" => Some(Self::Cancelled),
            "photo_missing" => Some(Self::PhotoMissing),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

/// Model lifecycle stored in `plugin_state.model_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Missing,
    Downloading,
    Verifying,
    Ready,
    Error,
}

impl ModelState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Ready => "ready",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "missing" => Some(Self::Missing),
            "downloading" => Some(Self::Downloading),
            "verifying" => Some(Self::Verifying),
            "ready" => Some(Self::Ready),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Progress of a plugin ZIP installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginInstallStatus {
    Running,
    Installed,
    Failed,
}

impl PluginInstallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Installed => "installed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "installed" => Some(Self::Installed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagging_status_round_trips() {
        let jobs = [
            TaggingJobStatus::Waiting,
            TaggingJobStatus::Pending,
            TaggingJobStatus::Processing,
            TaggingJobStatus::Completed,
            TaggingJobStatus::Failed,
            TaggingJobStatus::Cancelled,
        ];
        for status in jobs {
            assert_eq!(TaggingJobStatus::parse(status.as_str()), Some(status));
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
        assert_eq!(TaggingJobStatus::parse("unknown"), None);

        let stages = [
            TaggingStage::Presence,
            TaggingStage::PeopleCount,
            TaggingStage::PlaceClass,
            TaggingStage::PlaceTags,
            TaggingStage::AtmosphereTags,
            TaggingStage::Write,
        ];
        for stage in stages {
            assert_eq!(TaggingStage::parse(stage.as_str()), Some(stage));
            assert_eq!(serde_json::to_value(stage).unwrap(), stage.as_str());
        }
        assert_eq!(TaggingStage::parse("unknown"), None);

        let errors = [
            TaggingErrorCode::PluginNotReady,
            TaggingErrorCode::ModelMissing,
            TaggingErrorCode::ModelHashMismatch,
            TaggingErrorCode::SidecarExited,
            TaggingErrorCode::SidecarTimeout,
            TaggingErrorCode::SidecarProtocol,
            TaggingErrorCode::ImageMissing,
            TaggingErrorCode::ImageDecodeFailed,
            TaggingErrorCode::BackendUnavailable,
            TaggingErrorCode::Busy,
            TaggingErrorCode::OutOfMemory,
            TaggingErrorCode::Cancelled,
            TaggingErrorCode::PhotoMissing,
            TaggingErrorCode::Internal,
        ];
        for error in errors {
            assert_eq!(TaggingErrorCode::parse(error.as_str()), Some(error));
            assert_eq!(serde_json::to_value(error).unwrap(), error.as_str());
        }
        assert_eq!(TaggingErrorCode::parse("unknown"), None);

        let model_states = [
            ModelState::Missing,
            ModelState::Downloading,
            ModelState::Verifying,
            ModelState::Ready,
            ModelState::Error,
        ];
        for state in model_states {
            assert_eq!(ModelState::parse(state.as_str()), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
        assert_eq!(ModelState::parse("unknown"), None);

        let install_states = [
            PluginInstallStatus::Running,
            PluginInstallStatus::Installed,
            PluginInstallStatus::Failed,
        ];
        for status in install_states {
            assert_eq!(PluginInstallStatus::parse(status.as_str()), Some(status));
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
        assert_eq!(PluginInstallStatus::parse("unknown"), None);
    }
}
