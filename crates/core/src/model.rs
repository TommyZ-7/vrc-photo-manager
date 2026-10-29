use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhotoStatus {
    Present,
    Replaced,
    Missing,
    Deleting,
}

impl PhotoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Replaced => "replaced",
            Self::Missing => "missing",
            Self::Deleting => "deleting",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "replaced" => Self::Replaced,
            "missing" => Self::Missing,
            "deleting" => Self::Deleting,
            _ => Self::Present,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionFormat {
    Jpeg,
    Webp,
    Avif,
    JpegXl,
}

impl CompressionFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::JpegXl => "jxl",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::JpegXl => "jxl",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            "jxl" => Some(Self::JpegXl),
            _ => None,
        }
    }
}

impl std::fmt::Display for CompressionFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResizeMode {
    /// Resize to the profile bounds even when the source already fits.
    Always,
    /// Resize only when the source exceeds the profile bounds.
    #[default]
    DownscaleOnly,
}

impl ResizeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::DownscaleOnly => "downscale_only",
        }
    }
}

/// Maximum profile-name length shown in the UI. Names are display-only, so
/// this only keeps cards, menus, and job rows from breaking.
pub const MAX_PROFILE_NAME_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionProfile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub auto_enabled: bool,
    /// At most one profile system-wide may use this flag. The clipboard action
    /// compresses with it immediately, without opening the manual modal.
    #[serde(default)]
    pub clipboard: bool,
    pub format: CompressionFormat,
    pub quality: u8,
    pub lossless: bool,
    pub resize_enabled: bool,
    #[serde(default)]
    pub resize_mode: ResizeMode,
    pub width: u32,
    pub height: u32,
    pub maintain_aspect: bool,
    pub output_dir: Option<String>,
    pub suffix: String,
    /// 1 = slowest/best, 10 = fastest
    pub encode_speed: u8,
}

impl Default for CompressionProfile {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "WebP 高品質".to_string(),
            enabled: true,
            auto_enabled: true,
            clipboard: false,
            format: CompressionFormat::Webp,
            quality: 82,
            lossless: false,
            resize_enabled: false,
            resize_mode: ResizeMode::DownscaleOnly,
            width: 3840,
            height: 2160,
            maintain_aspect: true,
            output_dir: None,
            suffix: "_webp".to_string(),
            encode_speed: 6,
        }
    }
}

impl CompressionProfile {
    /// Built-in presets seeded into an empty database.
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                name: "無劣化webp".to_string(),
                auto_enabled: true,
                clipboard: false,
                format: CompressionFormat::Webp,
                lossless: true,
                resize_enabled: false,
                output_dir: Some("compressed".to_string()),
                suffix: String::new(),
                ..Self::default()
            },
            Self {
                name: "discord用webp".to_string(),
                auto_enabled: false,
                clipboard: true,
                format: CompressionFormat::Webp,
                lossless: true,
                resize_enabled: true,
                resize_mode: ResizeMode::DownscaleOnly,
                width: 2560,
                height: 1440,
                maintain_aspect: true,
                output_dir: Some("discord".to_string()),
                suffix: "_discord".to_string(),
                ..Self::default()
            },
            Self {
                name: "高画質webp".to_string(),
                auto_enabled: false,
                clipboard: false,
                format: CompressionFormat::Webp,
                quality: 90,
                lossless: false,
                resize_enabled: false,
                output_dir: Some("webp".to_string()),
                suffix: String::new(),
                ..Self::default()
            },
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Waiting,
    Pending,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

impl QueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "waiting" => Self::Waiting,
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Waiting | Self::Pending | Self::Processing)
    }
}

#[derive(Debug, Clone)]
pub struct LibraryRoot {
    pub id: i64,
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default)]
pub struct NewPhoto {
    pub root_id: i64,
    pub rel_path: String,
    pub abs_path: String,
    pub file_name: String,
    pub captured_at_utc: i64,
    pub captured_at_local: String,
    pub tz_offset_sec: i32,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub mtime: i64,
    pub blake3: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Photo {
    pub id: i64,
    pub root_id: i64,
    pub rel_path: String,
    pub abs_path: String,
    pub file_name: String,
    pub captured_at_utc: i64,
    pub captured_at_local: String,
    pub tz_offset_sec: i32,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub mtime: i64,
    pub blake3: Option<String>,
    pub status: PhotoStatus,
    pub is_favorite: bool,
    pub note: Option<String>,
    /// Derivative used instead of the original file after a successful
    /// `replace` compression or after the original was deleted while a
    /// completed derivative existed.
    pub display_derivative_id: Option<i64>,
    pub display_path: Option<String>,
    pub display_format: Option<String>,
    pub display_file_size: Option<u64>,
    pub display_width: Option<u32>,
    pub display_height: Option<u32>,
    pub world_name: Option<String>,
    pub instance_id: Option<String>,
    pub link_method: Option<String>,
    pub confidence: Option<f64>,
    pub author: Option<String>,
}

impl Photo {
    /// Path the UI and compression workers should read. Replaced photos use
    /// their primary derivative because the original file no longer exists.
    pub fn effective_path(&self) -> &str {
        self.display_path.as_deref().unwrap_or(&self.abs_path)
    }

    pub fn is_replaced(&self) -> bool {
        self.status == PhotoStatus::Replaced
    }

    pub fn effective_file_size(&self) -> u64 {
        self.display_file_size.unwrap_or(self.file_size)
    }

    pub fn effective_dimensions(&self) -> (u32, u32) {
        (
            self.display_width.unwrap_or(self.width),
            self.display_height.unwrap_or(self.height),
        )
    }
}

#[derive(Debug, Clone)]
pub struct PhotoContext {
    pub photo_id: i64,
    pub session_id: Option<i64>,
    pub world_id: Option<i64>,
    pub world_name: Option<String>,
    pub instance_id: Option<String>,
    pub link_method: String,
    pub confidence: f64,
    pub author: Option<String>,
    pub linked_at: String,
}

#[derive(Debug, Clone)]
pub struct PhotoPlayer {
    pub user_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct Derivative {
    pub id: i64,
    pub photo_id: i64,
    pub path: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub profile_id: String,
    pub policy: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct World {
    pub id: i64,
    pub world_id: String,
    pub name: Option<String>,
    pub name_source: Option<String>,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub log_file: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub world_id: i64,
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionPlayer {
    pub id: i64,
    pub session_id: i64,
    pub user_id: String,
    pub display_name: String,
    pub joined_at: i64,
    pub left_at: Option<i64>,
}

/// Maximum number of user retries per compression job.
pub const MAX_JOB_RETRIES: i64 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct CompressionJob {
    pub id: String,
    pub photo_id: i64,
    pub profile_id: String,
    pub status: QueueStatus,
    pub progress: f64,
    pub error: Option<String>,
    pub output_path: Option<String>,
    pub policy: String,
    /// Per-job output directory override. `None` uses the compression
    /// profile's configured directory (automatic jobs always do this).
    pub output_dir: Option<String>,
    /// Number of user retries already performed for this job.
    pub retry_count: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionReason {
    AutoReplace,
    Manual,
    /// A reason written by a newer version. Never safe to execute.
    Unknown,
}

impl DeletionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoReplace => "auto_replace",
            Self::Manual => "manual",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto_replace" => Some(Self::AutoReplace),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionStatus {
    Pending,
    Blocked,
    Running,
    Completed,
    Cancelled,
}

impl DeletionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Blocked => "blocked",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "blocked" => Self::Blocked,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Blocked | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionRequest {
    pub id: i64,
    pub photo_id: i64,
    pub photo_name: String,
    pub reason: DeletionReason,
    pub status: DeletionStatus,
    pub force: bool,
    pub blocked_reason: Option<String>,
    pub error: Option<String>,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletionBlockers {
    pub active: i64,
    pub failed: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobStatusCounts {
    pub waiting: i64,
    pub pending: i64,
    pub processing: i64,
    pub failed: i64,
}

impl JobStatusCounts {
    pub fn unfinished(self) -> i64 {
        self.waiting + self.pending + self.processing
    }

    pub fn total(self) -> i64 {
        self.unfinished() + self.failed
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletionJobSummary {
    pub compression: JobStatusCounts,
    pub send: JobStatusCounts,
    /// Distinct selected photos with at least one unfinished or failed job.
    pub affected_photos: i64,
}

impl DeletionJobSummary {
    pub fn needs_warning(self) -> bool {
        self.compression.total() + self.send.total() > 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletionCounts {
    pub pending: i64,
    pub blocked: i64,
    pub running: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DaemonOperation {
    Scan,
    Log,
    Link,
}

impl DaemonOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Log => "log",
            Self::Link => "link",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "scan" => Some(Self::Scan),
            "log" => Some(Self::Log),
            "link" => Some(Self::Link),
            _ => None,
        }
    }

    pub fn retry_command(self) -> DaemonCommand {
        match self {
            Self::Scan => DaemonCommand::Rescan,
            Self::Log => DaemonCommand::Relog,
            Self::Link => DaemonCommand::Relink,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DaemonCommand {
    Rescan,
    Relog,
    Relink,
}

impl DaemonCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rescan => "rescan",
            Self::Relog => "relog",
            Self::Relink => "relink",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rescan" => Some(Self::Rescan),
            "relog" => Some(Self::Relog),
            "relink" => Some(Self::Relink),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonError {
    pub operation: DaemonOperation,
    pub message: String,
    pub detail: Option<String>,
    pub occurred_at: String,
    pub attempts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoSort {
    Newest,
    Oldest,
    Largest,
    World,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoIndexEntry {
    pub id: i64,
    pub captured_at_utc: i64,
    /// Local capture month encoded as YYYYMM, e.g. 202504.
    pub captured_month: i32,
}

#[derive(Debug, Clone, Default)]
pub struct PhotoTextFilter {
    pub world: Option<String>,
    pub player: Option<String>,
    pub tag: Option<String>,
    pub favorite: bool,
    pub from_utc: Option<i64>,
    pub to_utc: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct PhotoFilter {
    pub world_ids: Vec<i64>,
    pub player_user_ids: Vec<String>,
    pub from_utc: Option<i64>,
    pub to_utc: Option<i64>,
    pub tags: Vec<String>,
    pub favorite: Option<bool>,
    pub unlinked: Option<bool>,
    pub limit: i64,
    pub offset: i64,
    pub sort: Option<PhotoSort>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryStats {
    pub photo_count: i64,
    pub total_bytes: u64,
    pub compressed_count: i64,
    pub derivative_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_status_roundtrips_waiting() {
        assert_eq!(QueueStatus::Waiting.as_str(), "waiting");
        assert_eq!(QueueStatus::parse("waiting"), QueueStatus::Waiting);
        assert!(QueueStatus::Waiting.is_active());
        assert!(QueueStatus::Pending.is_active());
        assert!(QueueStatus::Processing.is_active());
        assert!(!QueueStatus::Completed.is_active());
        assert!(!QueueStatus::Failed.is_active());
        assert!(!QueueStatus::Cancelled.is_active());
    }

    #[test]
    fn legacy_profile_json_defaults_to_downscale_only() {
        let json = r#"{
            "id": "legacy",
            "name": "Legacy",
            "enabled": true,
            "auto_enabled": true,
            "format": "webp",
            "quality": 82,
            "lossless": false,
            "resize_enabled": true,
            "width": 1920,
            "height": 1080,
            "maintain_aspect": true,
            "output_dir": null,
            "suffix": "_webp",
            "encode_speed": 6
        }"#;

        let profile: CompressionProfile = serde_json::from_str(json).unwrap();

        assert_eq!(profile.resize_mode, ResizeMode::DownscaleOnly);
    }

    #[test]
    fn resize_mode_round_trips() {
        let profile = CompressionProfile {
            resize_mode: ResizeMode::Always,
            ..CompressionProfile::default()
        };

        let json = serde_json::to_string(&profile).unwrap();
        let decoded: CompressionProfile = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.resize_mode, ResizeMode::Always);
    }

    #[test]
    fn deletion_status_and_reason_round_trip() {
        assert_eq!(DeletionStatus::parse("blocked"), DeletionStatus::Blocked);
        assert!(DeletionStatus::Running.is_active());
        assert!(!DeletionStatus::Completed.is_active());
        assert_eq!(
            DeletionReason::parse("auto_replace"),
            Some(DeletionReason::AutoReplace)
        );
        assert_eq!(DeletionReason::Manual.as_str(), "manual");
    }
}
