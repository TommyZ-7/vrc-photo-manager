//! Domain, storage, and ingestion logic for VRC Photo Manager.
//!
//! The core crate is intentionally free of UI and process concerns so that the
//! daemon and the gpui application can share it.

pub mod db;
pub mod error;
pub mod linker;
pub mod log_parser;
pub mod model;
pub mod photo_filename;
pub mod scanner;
pub mod time;

pub use db::Db;
pub use error::{CoreError, Result};

pub const APP_NAME: &str = "VRC Photo Manager";
pub const DB_FILE_NAME: &str = "library.db";

/// Returns the per-user data directory for the application.
pub fn data_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|dir| dir.join("vrc-photo-manager"))
}

/// Returns the default SQLite path.
pub fn default_db_path() -> Option<std::path::PathBuf> {
    data_dir().map(|dir| dir.join(DB_FILE_NAME))
}
