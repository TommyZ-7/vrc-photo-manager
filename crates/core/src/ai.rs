//! Storage types for AI tagging plugins, their tag catalogs, results, and jobs.

pub use vrc_photon_ai_protocol::ModelState;
use vrc_photon_ai_protocol::{TaggingJobStatus, TaggingStage};

/// The only AI tagging plugin supported by the host.
pub const AI_PLUGIN_ID: &str = "vrc-photon-autotag";

/// One installed plugin package: a plugin version for a platform variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallation {
    pub plugin_id: String,
    /// Platform-backend pair such as `linux-vulkan`.
    pub variant: String,
    pub version: String,
    pub protocol_version: u32,
    pub path: String,
    pub manifest_json: String,
    pub catalog_version: Option<String>,
    pub active: bool,
    pub installed_at: String,
    pub updated_at: String,
}

/// Per-plugin model download state, progress, and cached probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginState {
    pub plugin_id: String,
    pub model_state: ModelState,
    pub model_revision: Option<String>,
    pub model_bytes_downloaded: i64,
    pub model_bytes_total: Option<i64>,
    pub model_error: Option<String>,
    /// Raw JSON returned by the sidecar's `--probe` mode.
    pub probe_json: Option<String>,
    pub updated_at: String,
}

/// One tag in the catalog shipped by an installed plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct AiTagDefinition {
    pub plugin_id: String,
    pub tag_key: String,
    /// `indoor` or `outdoor`.
    pub branch: String,
    pub name: String,
    pub name_en: Option<String>,
    pub description: String,
    pub default_threshold: f32,
    pub sort_order: i64,
    pub catalog_version: String,
    pub updated_at: String,
}

/// A user override of one tag's default threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct AiTagThreshold {
    pub plugin_id: String,
    pub tag_key: String,
    pub threshold: f32,
    pub updated_at: String,
}

/// The stored result of one tagging run for a photo.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoAi {
    pub photo_id: i64,
    pub plugin_id: String,
    pub variant: String,
    pub sidecar_version: String,
    pub model_version: String,
    pub model_revision: String,
    pub catalog_version: String,
    pub source: String,
    pub presence_threshold: f32,
    pub character_present: bool,
    pub presence_probability: f32,
    pub people_min: Option<u32>,
    pub people_max: Option<u32>,
    pub people_median: Option<u32>,
    pub place_branch: Option<String>,
    pub place_probabilities_json: Option<String>,
    pub tagged_at: String,
}

/// One judged tag emitted by a tagging run.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoAiTag {
    pub photo_id: i64,
    pub plugin_id: String,
    pub tag_key: String,
    pub probability: f32,
    pub applied: bool,
    pub model_version: String,
    pub catalog_version: String,
    pub tagged_at: String,
}

/// A tag row joined with its catalog display names for the inspector.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoAiTagView {
    pub tag_key: String,
    pub name: String,
    pub name_en: Option<String>,
    pub probability: f32,
    pub applied: bool,
}

/// A complete result ready to be written for one photo.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPhotoAi {
    pub variant: String,
    pub sidecar_version: String,
    pub model_version: String,
    pub model_revision: String,
    pub catalog_version: String,
    pub source: String,
    pub presence_threshold: f32,
    pub character_present: bool,
    pub presence_probability: f32,
    pub people_min: Option<u32>,
    pub people_max: Option<u32>,
    pub people_median: Option<u32>,
    pub place_branch: Option<String>,
    pub place_probabilities_json: Option<String>,
    pub tags: Vec<(String, f32)>,
}

/// Maximum number of user retries for one tagging job.
pub const MAX_TAGGING_RETRIES: i64 = 2;

/// One durable tagging job.
#[derive(Debug, Clone, PartialEq)]
pub struct TaggingJob {
    pub id: String,
    pub photo_id: i64,
    pub plugin_id: String,
    pub variant: Option<String>,
    pub status: TaggingJobStatus,
    pub progress: f64,
    pub stage: Option<TaggingStage>,
    pub error: Option<String>,
    pub retry_count: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
