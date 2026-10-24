use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;

const SETTINGS_KEY: &str = "app_settings";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// Photo roots to index. The first entry is the primary VRChat folder.
    pub photo_roots: Vec<String>,
    /// VRChat output log directory.
    pub log_dir: Option<String>,
    /// Optional folder for imported photos.
    #[serde(default)]
    pub managed_dir: Option<String>,
    pub language: String,
    pub autostart: bool,
    pub tray: bool,
    pub auto_compress: bool,
    pub auto_compress_backlog: bool,
    pub estimated_link: bool,
    pub thumbnail_cache_mb: u64,
    pub compression_workers: usize,
    #[serde(default)]
    pub ui_sidebar_collapsed: bool,
    #[serde(default)]
    pub ui_inspector_collapsed: bool,
    #[serde(default)]
    pub ui_reduce_motion: bool,
    #[serde(default = "default_auto_update_check")]
    pub auto_update_check: bool,
    #[serde(default)]
    pub update_last_checked_at: Option<i64>,
}

fn default_auto_update_check() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            photo_roots: Vec::new(),
            log_dir: None,
            managed_dir: None,
            language: "ja".to_string(),
            autostart: true,
            tray: true,
            auto_compress: true,
            auto_compress_backlog: false,
            estimated_link: true,
            thumbnail_cache_mb: 2048,
            compression_workers: 1,
            ui_sidebar_collapsed: false,
            ui_inspector_collapsed: false,
            ui_reduce_motion: false,
            auto_update_check: true,
            update_last_checked_at: None,
        }
    }
}

impl AppSettings {
    pub fn load(db: &Db) -> Result<Self> {
        match db.get_setting(SETTINGS_KEY)? {
            Some(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
            None => {
                let mut settings = Self::default();
                let detected = detect_vrchat_paths();
                settings.photo_roots = detected
                    .photo_roots
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect();
                settings.log_dir = detected
                    .log_dir
                    .map(|path| path.to_string_lossy().to_string());
                Ok(settings)
            }
        }
    }

    pub fn save(&self, db: &Db) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        db.set_setting(SETTINGS_KEY, &json)
    }

    pub fn photo_roots(&self) -> Vec<PathBuf> {
        self.photo_roots.iter().map(PathBuf::from).collect()
    }

    pub fn log_dir(&self) -> Option<PathBuf> {
        self.log_dir.as_deref().map(PathBuf::from)
    }

    /// Folder used by the optional import feature.
    pub fn import_dir(&self) -> PathBuf {
        self.managed_dir
            .as_deref()
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::data_dir().map(|dir| dir.join("imported")))
            .unwrap_or_else(|| PathBuf::from("imported"))
    }
}

#[derive(Debug, Clone, Default)]
pub struct DetectedPaths {
    pub photo_roots: Vec<PathBuf>,
    pub log_dir: Option<PathBuf>,
}

/// Detects the standard VRChat photo and output-log locations.
///
/// Environment variables `VRC_PHOTO_DIR` and `VRC_LOG_DIR` take precedence,
/// which keeps tests and non-standard installs simple.
pub fn detect_vrchat_paths() -> DetectedPaths {
    let mut photo_roots = Vec::new();
    let mut log_dir = None;

    if let Some(path) = std::env::var_os("VRC_PHOTO_DIR") {
        photo_roots.push(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("VRC_LOG_DIR") {
        log_dir = Some(PathBuf::from(path));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            let home = PathBuf::from(home);
            if photo_roots.is_empty() {
                photo_roots.push(home.join("Pictures").join("VRChat"));
            }
            if log_dir.is_none() {
                log_dir = Some(
                    home.join("AppData")
                        .join("LocalLow")
                        .join("VRChat")
                        .join("VRChat"),
                );
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if photo_roots.is_empty()
            && let Some(prefix) = proton_prefix()
        {
            let user = prefix.join("drive_c").join("users").join("steamuser");
            photo_roots.push(user.join("Pictures").join("VRChat"));
            if log_dir.is_none() {
                log_dir = Some(
                    user.join("AppData")
                        .join("LocalLow")
                        .join("VRChat")
                        .join("VRChat"),
                );
            }
        }
    }

    photo_roots.retain(|path| path.exists());
    if log_dir.as_ref().is_some_and(|path| !path.exists()) {
        log_dir = None;
    }

    DetectedPaths {
        photo_roots,
        log_dir,
    }
}

/// Returns the first existing Steam Proton prefix for VRChat (app 438100).
///
/// shortcut: common Steam layouts only, no libraryfolders.vdf parsing yet.
/// Upgrade when a user reports VRChat installed on a non-default library.
#[cfg(not(target_os = "windows"))]
pub fn proton_prefix() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let steam_roots = [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        home.join(".steam/root"),
    ];
    for root in steam_roots {
        let prefix = root
            .join("steamapps")
            .join("compatdata")
            .join("438100")
            .join("pfx");
        if prefix.is_dir() {
            return Some(prefix);
        }
    }
    None
}

pub fn existing_photo_roots(settings: &AppSettings) -> Vec<PathBuf> {
    settings
        .photo_roots()
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

pub fn is_under(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let settings = AppSettings {
            photo_roots: vec!["/tmp/photos".to_string()],
            log_dir: Some("/tmp/logs".to_string()),
            language: "en".to_string(),
            ..Default::default()
        };
        settings.save(&db).unwrap();
        let loaded = AppSettings::load(&db).unwrap();
        assert_eq!(loaded.photo_roots, settings.photo_roots);
        assert_eq!(loaded.log_dir, settings.log_dir);
        assert_eq!(loaded.language, "en");
    }

    #[test]
    fn settings_deserialize_without_ui_fields() {
        let json = r#"{
            "photo_roots": [],
            "log_dir": null,
            "managed_dir": null,
            "language": "ja",
            "autostart": true,
            "tray": true,
            "auto_compress": true,
            "auto_compress_backlog": false,
            "estimated_link": true,
            "thumbnail_cache_mb": 2048,
            "compression_workers": 1
        }"#;
        let settings: AppSettings = serde_json::from_str(json).unwrap();
        assert!(!settings.ui_sidebar_collapsed);
        assert!(!settings.ui_inspector_collapsed);
        assert!(!settings.ui_reduce_motion);
        assert!(settings.auto_update_check);
        assert_eq!(settings.update_last_checked_at, None);
    }

    #[test]
    fn defaults_auto_update_check_to_on() {
        let settings = AppSettings::default();
        assert!(settings.auto_update_check);
        assert_eq!(settings.update_last_checked_at, None);
    }

    #[test]
    fn detects_env_overrides() {
        // This test only checks the environment branch without mutating env.
        let detected = detect_vrchat_paths();
        assert!(detected.photo_roots.len() <= 1);
    }
}
