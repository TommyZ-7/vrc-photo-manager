//! Helpers for tests that need signed plugin packages.
//!
//! The module is compiled for this crate's unit tests and, in downstream
//! crates, when the `test-support` feature is enabled through a
//! dev-dependency.

use std::io::{Cursor, Write};
use std::path::Path;

use minisign::KeyPair;
use tempfile::NamedTempFile;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{AiHostError, Result};
use crate::manifest::{
    ManifestFile, ManifestModel, ManifestModelFile, ModelFileRole, PluginManifest,
};

pub const TEST_PLUGIN_ID: &str = "vrc-photon-autotag";
pub const TEST_PLUGIN_VERSION: &str = "1.0.0";
pub const TEST_PLATFORM: &str = "linux";
pub const TEST_BACKEND: &str = "vulkan";
pub const TEST_VARIANT: &str = "linux-vulkan";
pub const TEST_HOST_VERSION_MIN: &str = "0.10.0";
pub const TEST_HOST_VERSION_MAX: &str = "0.99.99";
pub const TEST_CATALOG_VERSION: &str = "2026-11-03.1";
pub const TEST_ENTRYPOINT: &str = "d1-sidecar";

/// A throwaway minisign key pair for signing test packages.
#[derive(Clone, Debug)]
pub struct TestKey {
    keypair: KeyPair,
}

impl TestKey {
    /// Generates an unencrypted, in-memory key pair.
    pub fn generate() -> Self {
        let keypair =
            KeyPair::generate_unencrypted_keypair().expect("generate a test minisign key pair");
        Self { keypair }
    }

    /// The base64 public key line accepted by `minisign_verify`.
    pub fn public_key(&self) -> String {
        let public_box = self
            .keypair
            .pk
            .to_box()
            .expect("encode the test public key");
        public_box
            .to_string()
            .lines()
            .find(|line| line.starts_with("RW"))
            .expect("the public key box contains an RW line")
            .to_string()
    }

    /// Signs bytes and returns the complete `.minisig` file contents.
    pub fn sign(&self, data: &[u8]) -> String {
        minisign::sign(None, &self.keypair.sk, Cursor::new(data), None, None)
            .expect("sign test data")
            .into_string()
    }
}

/// One file written into a test ZIP.
#[derive(Clone, Debug)]
pub struct TestEntry {
    pub path: String,
    pub data: Vec<u8>,
    /// Full Unix mode including the file type bits, when the entry needs one.
    pub unix_mode: Option<u32>,
}

impl TestEntry {
    pub fn new(path: impl Into<String>, data: impl AsRef<[u8]>) -> Self {
        Self {
            path: path.into(),
            data: data.as_ref().to_vec(),
            unix_mode: None,
        }
    }

    pub fn with_mode(path: impl Into<String>, data: impl AsRef<[u8]>, mode: u32) -> Self {
        Self {
            path: path.into(),
            data: data.as_ref().to_vec(),
            unix_mode: Some(mode),
        }
    }
}

/// The files of a minimal, valid package.
pub fn sample_entries() -> Vec<TestEntry> {
    vec![
        TestEntry::with_mode(TEST_ENTRYPOINT, b"#!/bin/sh\nexit 0\n", 0o644),
        TestEntry::new("catalog.json", br#"{"version":"2026-11-03.1"}"#),
        TestEntry::new("calibration.json", br#"{"thresholds":{}}"#),
    ]
}

/// A minimal manifest whose `files` match [`sample_entries`].
pub fn sample_manifest() -> PluginManifest {
    let files = sample_entries()
        .into_iter()
        .map(|entry| ManifestFile {
            path: entry.path,
            size: entry.data.len() as u64,
            sha256: sha256_hex(&entry.data),
        })
        .collect();

    PluginManifest {
        format_version: 1,
        plugin_id: TEST_PLUGIN_ID.to_string(),
        plugin_version: TEST_PLUGIN_VERSION.to_string(),
        protocol_version: vrc_photon_ai_protocol::PROTOCOL_VERSION,
        host_version_min: TEST_HOST_VERSION_MIN.to_string(),
        host_version_max: TEST_HOST_VERSION_MAX.to_string(),
        platform: TEST_PLATFORM.to_string(),
        backend: TEST_BACKEND.to_string(),
        variant: TEST_VARIANT.to_string(),
        catalog_version: TEST_CATALOG_VERSION.to_string(),
        entrypoint: TEST_ENTRYPOINT.to_string(),
        files,
        model: ManifestModel {
            id: "d1-2B".to_string(),
            quantization: "Q8_0".to_string(),
            revision: "5ebcce3bab1c0521f2cab68742a647c7d038adbe".to_string(),
            files: vec![
                ManifestModelFile {
                    path: "d1-2B-Q8_0.gguf".to_string(),
                    role: Some(ModelFileRole::Model),
                    size: 5,
                    sha256: sha256_hex(b"model"),
                    url: "https://example.invalid/models/d1-2B-Q8_0.gguf".to_string(),
                    parts: Vec::new(),
                },
                ManifestModelFile {
                    path: "mmproj-d1-2B-Q8_0.gguf".to_string(),
                    role: Some(ModelFileRole::Mmproj),
                    size: 6,
                    sha256: sha256_hex(b"mmproj"),
                    url: "https://example.invalid/models/mmproj-d1-2B-Q8_0.gguf".to_string(),
                    parts: Vec::new(),
                },
            ],
            license: "LFM Open License v1.0".to_string(),
            license_url: "https://example.invalid/LICENSE".to_string(),
        },
    }
}

/// A valid package builder. Mutate the manifest or entries to create broken
/// packages while keeping a valid signature.
#[derive(Clone, Debug)]
pub struct TestPackage {
    key: TestKey,
    manifest: PluginManifest,
    entries: Vec<TestEntry>,
}

impl TestPackage {
    pub fn new() -> Self {
        Self {
            key: TestKey::generate(),
            manifest: sample_manifest(),
            entries: sample_entries(),
        }
    }

    pub fn with_key(key: TestKey) -> Self {
        Self {
            key,
            manifest: sample_manifest(),
            entries: sample_entries(),
        }
    }

    pub fn key(&self) -> &TestKey {
        &self.key
    }

    pub fn public_key(&self) -> String {
        self.key.public_key()
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn manifest_mut(&mut self) -> &mut PluginManifest {
        &mut self.manifest
    }

    pub fn entries(&self) -> &[TestEntry] {
        &self.entries
    }

    pub fn entries_mut(&mut self) -> &mut Vec<TestEntry> {
        &mut self.entries
    }

    pub fn manifest_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.manifest).expect("serialize the test manifest")
    }

    pub fn signature(&self) -> String {
        self.key.sign(&self.manifest_bytes())
    }

    pub fn zip_bytes(&self) -> Result<Vec<u8>> {
        signed_zip(&self.manifest_bytes(), &self.entries, &self.key)
    }

    pub fn temp_zip(&self) -> Result<NamedTempFile> {
        write_temp_zip(&self.zip_bytes()?)
    }

    pub fn write_zip(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.zip_bytes()?)?;
        Ok(())
    }
}

impl Default for TestPackage {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a valid package plus one extra entry, for negative ZIP tests.
///
/// The returned temporary file owns the ZIP and must stay alive while the
/// installer reads it.
pub fn test_zip_with_entry(
    path: &str,
    data: &[u8],
    unix_mode: Option<u32>,
) -> Result<(NamedTempFile, String)> {
    let package = TestPackage::new();
    let mut entries = package.entries().to_vec();
    entries.push(match unix_mode {
        Some(mode) => TestEntry::with_mode(path, data, mode),
        None => TestEntry::new(path, data),
    });
    let bytes = zip_with_manifest(&package.manifest_bytes(), &package.signature(), &entries)?;
    Ok((write_temp_zip(&bytes)?, package.public_key()))
}

/// Builds a ZIP with a manifest, its signature, and the given entries.
pub fn zip_with_manifest(
    manifest: &[u8],
    signature: &str,
    entries: &[TestEntry],
) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    write_entry(&mut writer, "manifest.json", manifest, None)?;
    write_entry(&mut writer, "manifest.minisig", signature.as_bytes(), None)?;
    for entry in entries {
        write_entry(&mut writer, &entry.path, &entry.data, entry.unix_mode)?;
    }
    Ok(writer.finish()?.into_inner())
}

/// Builds a ZIP without the manifest pair, used to test missing entries.
pub fn zip_entries(entries: &[TestEntry]) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for entry in entries {
        write_entry(&mut writer, &entry.path, &entry.data, entry.unix_mode)?;
    }
    Ok(writer.finish()?.into_inner())
}

/// Signs a manifest and embeds it in a ZIP with the given entries.
pub fn signed_zip(manifest: &[u8], entries: &[TestEntry], key: &TestKey) -> Result<Vec<u8>> {
    zip_with_manifest(manifest, &key.sign(manifest), entries)
}

/// Writes ZIP bytes to a temporary file that the caller must keep alive.
pub fn write_temp_zip(bytes: &[u8]) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new()?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(file)
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

fn write_entry(
    writer: &mut ZipWriter<Cursor<Vec<u8>>>,
    path: &str,
    data: &[u8],
    unix_mode: Option<u32>,
) -> Result<()> {
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let options = match unix_mode {
        Some(mode) if mode & 0o170000 != 0 => options.external_attributes(mode << 16),
        Some(mode) => options.unix_permissions(mode),
        None => options,
    };
    writer
        .start_file(path, options)
        .map_err(AiHostError::from)?;
    writer.write_all(data)?;
    Ok(())
}
