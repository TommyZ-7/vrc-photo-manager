//! Verification, installation, and model management for AI plugins.
//!
//! This crate is used by the daemon only. It does not depend on the database,
//! the UI, or GPUI. Plugin packages are signed with minisign and contain a
//! `manifest.json` whose `files` list covers every other file in the ZIP.

pub mod error;
pub mod install;
pub mod layout;
pub mod manifest;
pub mod verify;

#[cfg(any(feature = "test-support", test))]
pub mod test_support;

pub use error::{AiHostError, Result};
pub use install::{InstallLimits, InstalledPlugin, install_from_zip, install_from_zip_with_limits};
pub use layout::{plugin_dir, plugins_root};
pub use manifest::{
    FORMAT_VERSION, ManifestFile, ManifestModel, ManifestModelFile, ManifestPart, ModelFileRole,
    PluginManifest, model_file_role, parse_manifest, validate_manifest,
};
pub use verify::{effective_plugin_keys, parse_public_keys, verify_manifest_bytes};
