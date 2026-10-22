use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhotoStatus {
    Present,
    Missing,
}

impl PhotoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "missing" => Self::Missing,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionProfile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub auto_enabled: bool,
    pub format: CompressionFormat,
    pub quality: u8,
    pub lossless: bool,
    pub resize_enabled: bool,
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
            format: CompressionFormat::Webp,
            quality: 82,
            lossless: false,
            resize_enabled: false,
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
            Self::default(),
            Self {
                name: "AVIF 容量優先".to_string(),
                format: CompressionFormat::Avif,
                quality: 68,
                encode_speed: 6,
                suffix: "_avif".to_string(),
                ..Self::default()
            },
            Self {
                name: "JPEG XL ロスレス".to_string(),
                format: CompressionFormat::JpegXl,
                lossless: true,
                auto_enabled: false,
                suffix: "_jxl".to_string(),
                ..Self::default()
            },
            Self {
                name: "JPEG 互換用".to_string(),
                format: CompressionFormat::Jpeg,
                quality: 88,
                auto_enabled: false,
                suffix: "_jpg".to_string(),
                ..Self::default()
            },
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Pending,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

impl QueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
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
    pub world_name: Option<String>,
    pub instance_id: Option<String>,
    pub link_method: Option<String>,
    pub confidence: Option<f64>,
    pub author: Option<String>,
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

#[derive(Debug, Clone)]
pub struct CompressionJob {
    pub id: String,
    pub photo_id: i64,
    pub profile_id: String,
    pub status: QueueStatus,
    pub progress: f64,
    pub error: Option<String>,
    pub output_path: Option<String>,
    pub policy: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoSort {
    Newest,
    Oldest,
    Largest,
    World,
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

#[derive(Debug, Clone, Default)]
pub struct LibraryStats {
    pub photo_count: i64,
    pub total_bytes: u64,
    pub compressed_count: i64,
}
