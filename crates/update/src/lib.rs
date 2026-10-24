//! Signed update checks, downloads, and application for VRC Photo Manager.
//!
//! The crate is intentionally independent of the UI, the daemon, and SQLite.
//! The UI drives checks and downloads; the helper binary applies them.

pub mod error;
pub mod manifest;

pub use error::{Result, UpdateError};
pub use manifest::{
    AvailableUpdate, MANIFEST_SCHEMA, MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES,
    Manifest, PlatformAsset, UpdateFormat,
};
