//! Signed update checks, downloads, and application for VRC Photo Manager.
//!
//! The crate is intentionally independent of the UI, the daemon, and SQLite.
//! The UI drives checks and downloads; the helper binary applies them.

pub mod check;
pub mod download;
pub mod error;
pub mod fetch;
pub mod helper;
pub mod install;
pub mod manifest;
pub mod verify;

pub use check::{FeedConfig, check};
pub use download::download;
pub use error::{IoOperation, Result, UpdateError};
pub use fetch::{HttpClient, UreqClient};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub use helper::{HealthMarker, LastResult, LastStatus};
pub use install::{
    APPLY_SCHEMA, ApplyJob, ApplyKind, InstallKind, PackageFamily, UpdatePaths, install_kind,
    prepare_apply, start_apply,
};
pub use manifest::{
    AvailableUpdate, MANIFEST_SCHEMA, MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES,
    Manifest, PlatformAsset, UpdateFormat,
};
pub use verify::{
    decode_signature_base64, effective_public_keys, embedded_public_keys, verify_bytes, verify_file,
};
