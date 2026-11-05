//! Domain, storage, and ingestion logic for VRC PhotoN.
//!
//! The core crate is intentionally free of UI and process concerns so that the
//! daemon and the gpui application can share it.

pub mod ai;
mod cache_metadata;
pub mod compressor;
pub mod db;
pub mod discord;
pub mod error;
pub mod fs_safe;
pub mod hashing;
pub mod image_decode;
pub mod importer;
pub mod linker;
pub mod log_parser;
pub mod logging;
pub mod metadata;
pub mod model;
pub mod photo_filename;
pub mod preview;
pub mod scanner;
pub mod settings;
pub mod tagging_image;
pub mod thumbnail;
pub mod time;
pub mod world_name;

pub use db::Db;
pub use error::{CoreError, Result};

pub const APP_NAME: &str = "VRC PhotoN";
pub const DB_FILE_NAME: &str = "library.db";

/// Returns the per-user data directory for the application.
pub fn data_dir() -> Option<std::path::PathBuf> {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join("vrc-photon"))
}

/// Returns the default SQLite path.
pub fn default_db_path() -> Option<std::path::PathBuf> {
    data_dir().map(|dir| dir.join(DB_FILE_NAME))
}
