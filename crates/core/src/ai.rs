//! Storage types for AI tagging plugins and their tag catalogs.

pub use vrc_photon_ai_protocol::ModelState;

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
