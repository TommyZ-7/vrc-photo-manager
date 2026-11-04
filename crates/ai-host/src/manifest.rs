//! `manifest.json` types and validation for AI plugin packages.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use url::Url;
use vrc_photon_ai_protocol::PROTOCOL_VERSION;

use crate::error::{AiHostError, Result};

/// Version of the manifest format understood by this host.
pub const FORMAT_VERSION: u32 = 1;

/// Backends the host can launch a sidecar for.
pub const SUPPORTED_BACKENDS: &[&str] = &["vulkan", "cuda", "rocm", "cpu"];

/// Root metadata of an AI plugin package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub format_version: u32,
    pub plugin_id: String,
    pub plugin_version: String,
    pub protocol_version: u32,
    pub host_version_min: String,
    pub host_version_max: String,
    pub platform: String,
    pub backend: String,
    pub variant: String,
    pub catalog_version: String,
    pub entrypoint: String,
    pub files: Vec<ManifestFile>,
    pub model: ManifestModel,
}

/// One file shipped inside the plugin ZIP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Model download metadata. Model files are not stored inside the plugin ZIP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestModel {
    pub id: String,
    pub quantization: String,
    pub revision: String,
    pub files: Vec<ManifestModelFile>,
    pub license: String,
    pub license_url: String,
}

/// One model file, optionally split into offline parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestModelFile {
    pub path: String,
    pub role: Option<ModelFileRole>,
    pub size: u64,
    pub sha256: String,
    pub url: String,
    #[serde(default)]
    pub parts: Vec<ManifestPart>,
}

/// Purpose of a model file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFileRole {
    Model,
    Mmproj,
}

/// One part of an offline-split model file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPart {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Parses `manifest.json` bytes into the typed v1 schema.
pub fn parse_manifest(bytes: &[u8]) -> Result<PluginManifest> {
    let manifest: PluginManifest = serde_json::from_slice(bytes)?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(AiHostError::UnsupportedFormat(manifest.format_version));
    }
    Ok(manifest)
}

/// Validates a manifest against the host version, the current platform, and
/// the package layout rules.
pub fn validate_manifest(
    manifest: &PluginManifest,
    host_version: &str,
    platform: &str,
) -> Result<()> {
    if manifest.format_version != FORMAT_VERSION {
        return Err(AiHostError::UnsupportedFormat(manifest.format_version));
    }
    if manifest.protocol_version != PROTOCOL_VERSION {
        return Err(AiHostError::UnsupportedProtocol(manifest.protocol_version));
    }

    validate_path_component("plugin_id", &manifest.plugin_id)?;
    validate_path_component("variant", &manifest.variant)?;
    validate_path_component("plugin_version", &manifest.plugin_version)?;
    validate_path_component("model.revision", &manifest.model.revision)?;

    semver::Version::parse(&manifest.plugin_version).map_err(|error| {
        AiHostError::ManifestInvalid(format!("plugin_version is not semver: {error}"))
    })?;

    let host = semver::Version::parse(host_version)
        .map_err(|error| AiHostError::ManifestInvalid(format!("invalid host version: {error}")))?;
    let min = semver::Version::parse(&manifest.host_version_min).map_err(|error| {
        AiHostError::ManifestInvalid(format!("host_version_min is not semver: {error}"))
    })?;
    let max = semver::Version::parse(&manifest.host_version_max).map_err(|error| {
        AiHostError::ManifestInvalid(format!("host_version_max is not semver: {error}"))
    })?;
    if min > max {
        return Err(AiHostError::ManifestInvalid(
            "host_version_min is greater than host_version_max".to_string(),
        ));
    }
    if host < min || host > max {
        return Err(AiHostError::ManifestInvalid(format!(
            "host version {host_version} is outside {}..={}",
            manifest.host_version_min, manifest.host_version_max
        )));
    }

    if manifest.platform != platform {
        return Err(AiHostError::ManifestInvalid(format!(
            "package platform {} does not match host platform {platform}",
            manifest.platform
        )));
    }
    if manifest.backend.trim().is_empty() {
        return Err(AiHostError::ManifestInvalid("backend is empty".to_string()));
    }
    if !SUPPORTED_BACKENDS.contains(&manifest.backend.as_str()) {
        return Err(AiHostError::ManifestInvalid(format!(
            "unsupported backend: {}",
            manifest.backend
        )));
    }
    let expected_variant = format!("{}-{}", manifest.platform, manifest.backend);
    if manifest.variant != expected_variant {
        return Err(AiHostError::ManifestInvalid(format!(
            "variant {} does not match platform/backend ({expected_variant})",
            manifest.variant
        )));
    }
    if manifest.catalog_version.trim().is_empty() {
        return Err(AiHostError::ManifestInvalid(
            "catalog_version is empty".to_string(),
        ));
    }

    validate_relative_path("entrypoint", &manifest.entrypoint)?;

    let mut paths = HashSet::new();
    for file in &manifest.files {
        validate_relative_path("files[].path", &file.path)?;
        validate_sha256("files[].sha256", &file.sha256)?;
        if !paths.insert(file.path.as_str()) {
            return Err(AiHostError::DuplicateEntry(file.path.clone()));
        }
    }
    if manifest.files.is_empty() {
        return Err(AiHostError::ManifestInvalid(
            "manifest lists no files".to_string(),
        ));
    }
    if !paths.contains(manifest.entrypoint.as_str()) {
        return Err(AiHostError::ManifestInvalid(format!(
            "entrypoint {} is not listed in files",
            manifest.entrypoint
        )));
    }
    reject_file_prefix_conflicts(&paths)?;

    if manifest.model.files.is_empty() {
        return Err(AiHostError::ManifestInvalid(
            "model lists no files".to_string(),
        ));
    }
    let mut model_paths = HashSet::new();
    let mut models = 0usize;
    let mut mmprojs = 0usize;
    for file in &manifest.model.files {
        validate_relative_path("model.files[].path", &file.path)?;
        validate_sha256("model.files[].sha256", &file.sha256)?;
        validate_https_url("model.files[].url", &file.url)?;
        if !file.parts.is_empty() {
            return Err(AiHostError::UnsupportedParts);
        }
        if !model_paths.insert(file.path.as_str()) {
            return Err(AiHostError::DuplicateEntry(file.path.clone()));
        }
        match model_file_role(file)? {
            ModelFileRole::Model => models += 1,
            ModelFileRole::Mmproj => mmprojs += 1,
        }
    }
    if models != 1 || mmprojs != 1 {
        return Err(AiHostError::ManifestInvalid(format!(
            "model needs exactly one model and one mmproj file (found {models} and {mmprojs})"
        )));
    }

    Ok(())
}

/// Returns the declared role, falling back to the `mmproj` filename prefix.
pub fn model_file_role(file: &ManifestModelFile) -> Result<ModelFileRole> {
    if let Some(role) = file.role {
        return Ok(role);
    }
    let name = file.path.rsplit('/').next().unwrap_or(&file.path);
    if name.starts_with("mmproj") {
        Ok(ModelFileRole::Mmproj)
    } else {
        Ok(ModelFileRole::Model)
    }
}

/// Rejects absolute paths, `..`, Windows aliases/device names, and empty
/// components. Every component is validated with the same cross-platform
/// rules so a package cannot rely on Windows normalization to reach a
/// different directory.
pub(crate) fn validate_relative_path(field: &str, path: &str) -> Result<()> {
    let unsafe_path = || AiHostError::UnsafePath(format!("{field}: {path}"));
    if path.is_empty() {
        return Err(unsafe_path());
    }
    for component in path.split('/') {
        if !is_safe_component(component) {
            return Err(unsafe_path());
        }
    }
    Ok(())
}

fn validate_path_component(field: &str, value: &str) -> Result<()> {
    if value.contains('/') || !is_safe_component(value) {
        return Err(AiHostError::UnsafePath(format!("{field}: {value}")));
    }
    Ok(())
}

/// One cross-platform path component. Rejects separators/stream characters,
/// control characters, Windows trailing-dot/space normalization, and reserved
/// device names (including names followed by an extension).
fn is_safe_component(component: &str) -> bool {
    if component.is_empty() || component == "." || component == ".." {
        return false;
    }
    if component.ends_with('.') || component.ends_with(' ') {
        return false;
    }
    if component.chars().any(|character| {
        character.is_control()
            || matches!(character, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return false;
    }
    let stem = component.split('.').next().unwrap_or(component);
    !is_windows_reserved_name(stem)
}

fn is_windows_reserved_name(stem: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
        "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
        "LPT8", "LPT9",
    ];
    RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

fn validate_sha256(field: &str, value: &str) -> Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(AiHostError::ManifestInvalid(format!(
            "{field} is not a SHA-256 hex digest"
        )))
    }
}

fn validate_https_url(field: &str, value: &str) -> Result<()> {
    let url = Url::parse(value).map_err(|error| {
        AiHostError::ManifestInvalid(format!("{field} is not a valid URL: {error}"))
    })?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(AiHostError::ManifestInvalid(format!(
            "{field} must be an absolute HTTPS URL"
        )));
    }
    Ok(())
}

fn reject_file_prefix_conflicts(paths: &HashSet<&str>) -> Result<()> {
    for path in paths {
        for (index, _) in path.match_indices('/') {
            let prefix = &path[..index];
            if paths.contains(prefix) {
                return Err(AiHostError::ManifestInvalid(format!(
                    "file {prefix} is also used as a directory by {path}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::sample_manifest;

    fn assert_invalid(manifest: &PluginManifest) {
        assert!(
            validate_manifest(manifest, "0.10.1", "linux").is_err(),
            "manifest should have been rejected"
        );
    }

    #[test]
    fn validate_accepts_a_supported_manifest() {
        validate_manifest(&sample_manifest(), "0.10.1", "linux").unwrap();
    }

    #[test]
    fn validate_rejects_protocol_and_host_version_mismatch() {
        let mut manifest = sample_manifest();
        manifest.protocol_version = 2;
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsupportedProtocol(2))
        ));

        let mut manifest = sample_manifest();
        manifest.host_version_min = "0.11.0".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.host_version_max = "0.10.0".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.host_version_min = "not-a-version".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.host_version_min = "0.99.0".to_string();
        manifest.host_version_max = "0.10.0".to_string();
        assert_invalid(&manifest);
    }

    #[test]
    fn validate_rejects_platform_and_variant_mismatch() {
        let mut manifest = sample_manifest();
        manifest.platform = "windows".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.variant = "linux-cuda".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.backend = "cuda".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.backend = " ".to_string();
        manifest.variant = "linux- ".to_string();
        assert_invalid(&manifest);
    }

    #[test]
    fn validate_rejects_traversal_and_absolute_paths() {
        for path in [
            "../escape.txt",
            "a/../../b",
            "/etc/passwd",
            "C:/windows/system32",
            "C:\\windows\\system32",
            "\\\\server\\share\\file",
            "a\\b",
            "a//b",
            "./a",
            "a/./b",
            "dir/",
            "file:stream",
            "",
        ] {
            let mut manifest = sample_manifest();
            manifest.files[0].path = path.to_string();
            assert!(
                matches!(
                    validate_manifest(&manifest, "0.10.1", "linux"),
                    Err(AiHostError::UnsafePath(_))
                ),
                "path {path:?} should be unsafe"
            );
        }
    }

    #[test]
    fn validate_rejects_unsafe_plugin_id_and_revision() {
        let mut manifest = sample_manifest();
        manifest.plugin_id = "../evil".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));

        let mut manifest = sample_manifest();
        manifest.variant = "linux/vulkan".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));

        let mut manifest = sample_manifest();
        manifest.model.revision = "..".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));
    }

    #[test]
    fn validate_rejects_windows_path_aliases() {
        for value in [
            ".. ", "foo.", "foo ", "CON", "con", "CON.txt", "NUL", "COM1", "LPT9", "CONOUT$",
            "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\tb",
        ] {
            let mut manifest = sample_manifest();
            manifest.plugin_id = value.to_string();
            assert!(
                matches!(
                    validate_manifest(&manifest, "0.10.1", "linux"),
                    Err(AiHostError::UnsafePath(_))
                ),
                "plugin_id {value:?} should be rejected"
            );
        }

        let mut manifest = sample_manifest();
        manifest.model.revision = "foo.".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));

        let mut manifest = sample_manifest();
        manifest.files[0].path = "libs/CON/child".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));

        let mut manifest = sample_manifest();
        manifest.files[0].path = "libs/foo./child".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));

        // Case variants are rejected by the backend allowlist rather than by
        // the path rules, so identifier casing cannot smuggle in a new backend.
        let mut manifest = sample_manifest();
        manifest.backend = "VULKAN".to_string();
        manifest.variant = "linux-VULKAN".to_string();
        assert_invalid(&manifest);
    }

    #[test]
    fn validate_enforces_the_backend_allowlist() {
        for backend in ["vulkan", "cuda", "rocm", "cpu"] {
            let mut manifest = sample_manifest();
            manifest.backend = backend.to_string();
            manifest.variant = format!("linux-{backend}");
            validate_manifest(&manifest, "0.10.1", "linux").unwrap();
        }

        for backend in ["metal", "opencl", "roc", "Vulkan", ""] {
            let mut manifest = sample_manifest();
            manifest.backend = backend.to_string();
            manifest.variant = format!("linux-{backend}");
            assert!(
                matches!(
                    validate_manifest(&manifest, "0.10.1", "linux"),
                    Err(AiHostError::ManifestInvalid(_))
                ),
                "backend {backend:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_rejects_duplicate_file_paths() {
        let mut manifest = sample_manifest();
        let duplicate = manifest.files[0].clone();
        manifest.files.push(duplicate);
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::DuplicateEntry(_))
        ));
    }

    #[test]
    fn validate_rejects_file_used_as_a_directory() {
        let mut manifest = sample_manifest();
        manifest.files.push(ManifestFile {
            path: format!("{}/nested.txt", manifest.files[0].path),
            size: 1,
            sha256: "0".repeat(64),
        });
        assert_invalid(&manifest);
    }

    #[test]
    fn validate_requires_entrypoint_in_files() {
        let mut manifest = sample_manifest();
        manifest.entrypoint = "other-sidecar".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.entrypoint = "../d1-sidecar".to_string();
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsafePath(_))
        ));
    }

    #[test]
    fn validate_requires_https_model_urls() {
        for url in [
            "http://example.com/model.gguf",
            "file:///tmp/model.gguf",
            "/models/model.gguf",
            "ftp://example.com/model.gguf",
            "https://",
        ] {
            let mut manifest = sample_manifest();
            manifest.model.files[0].url = url.to_string();
            assert_invalid(&manifest);
        }

        let mut manifest = sample_manifest();
        manifest.model.files[0].url = "https://example.com/model.gguf".to_string();
        validate_manifest(&manifest, "0.10.1", "linux").unwrap();
    }

    #[test]
    fn validate_requires_one_model_and_one_mmproj() {
        let mut manifest = sample_manifest();
        manifest
            .model
            .files
            .retain(|file| file.role != Some(ModelFileRole::Mmproj));
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest
            .model
            .files
            .retain(|file| file.role != Some(ModelFileRole::Model));
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        let model = manifest.model.files[0].clone();
        manifest.model.files.push(model);
        assert_invalid(&manifest);
    }

    #[test]
    fn model_file_role_falls_back_to_the_filename_prefix() {
        let mut file = ManifestModelFile {
            path: "mmproj-d1-2B.gguf".to_string(),
            role: None,
            size: 1,
            sha256: "0".repeat(64),
            url: "https://example.com/mmproj.gguf".to_string(),
            parts: Vec::new(),
        };
        assert_eq!(model_file_role(&file).unwrap(), ModelFileRole::Mmproj);

        file.path = "models/mmproj-d1-2B.gguf".to_string();
        assert_eq!(model_file_role(&file).unwrap(), ModelFileRole::Mmproj);

        file.path = "d1-2B.gguf".to_string();
        assert_eq!(model_file_role(&file).unwrap(), ModelFileRole::Model);

        file.role = Some(ModelFileRole::Mmproj);
        assert_eq!(model_file_role(&file).unwrap(), ModelFileRole::Mmproj);
    }

    #[test]
    fn validate_rejects_parts_until_offline_import_exists() {
        let mut manifest = sample_manifest();
        manifest.model.files[0].parts.push(ManifestPart {
            path: "d1-2B.part1".to_string(),
            size: 1,
            sha256: "0".repeat(64),
        });
        assert!(matches!(
            validate_manifest(&manifest, "0.10.1", "linux"),
            Err(AiHostError::UnsupportedParts)
        ));
    }

    #[test]
    fn validate_rejects_invalid_hashes_and_versions() {
        let mut manifest = sample_manifest();
        manifest.files[0].sha256 = "not-a-hash".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.plugin_version = "1.0".to_string();
        assert_invalid(&manifest);

        let mut manifest = sample_manifest();
        manifest.catalog_version = " ".to_string();
        assert_invalid(&manifest);
    }

    #[test]
    fn parse_manifest_reads_the_v1_schema() {
        let manifest = sample_manifest();
        let json = serde_json::to_vec(&manifest).unwrap();
        let parsed = parse_manifest(&json).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.model.files[1].role, Some(ModelFileRole::Mmproj));
    }

    #[test]
    fn parse_manifest_accepts_absent_optional_fields() {
        let json = serde_json::json!({
            "format_version": 1,
            "plugin_id": "vrc-photon-autotag",
            "plugin_version": "1.0.0",
            "protocol_version": 1,
            "host_version_min": "0.10.0",
            "host_version_max": "0.99.99",
            "platform": "linux",
            "backend": "vulkan",
            "variant": "linux-vulkan",
            "catalog_version": "2026-11-03.1",
            "entrypoint": "d1-sidecar",
            "files": [{"path": "d1-sidecar", "size": 1, "sha256": "00"}],
            "model": {
                "id": "d1-2B",
                "quantization": "Q8_0",
                "revision": "rev",
                "files": [{
                    "path": "model.gguf",
                    "size": 1,
                    "sha256": "00",
                    "url": "https://example.com/model.gguf"
                }],
                "license": "LFM Open License v1.0",
                "license_url": "https://example.com/license"
            }
        });
        let parsed = parse_manifest(json.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.model.files[0].role, None);
        assert!(parsed.model.files[0].parts.is_empty());
    }

    #[test]
    fn parse_manifest_rejects_unsupported_formats_and_malformed_json() {
        let mut manifest = sample_manifest();
        manifest.format_version = 2;
        let json = serde_json::to_vec(&manifest).unwrap();
        assert!(matches!(
            parse_manifest(&json),
            Err(AiHostError::UnsupportedFormat(2))
        ));

        assert!(matches!(
            parse_manifest(b"not json"),
            Err(AiHostError::Json(_))
        ));

        let json = br#"{
            "format_version": 1,
            "plugin_id": "vrc-photon-autotag",
            "plugin_version": "1.0.0",
            "protocol_version": 1,
            "host_version_min": "0.10.0",
            "host_version_max": "0.99.99",
            "platform": "linux",
            "backend": "vulkan",
            "variant": "linux-vulkan",
            "catalog_version": "2026-11-03.1",
            "entrypoint": "d1-sidecar",
            "files": [],
            "model": {
                "id": "d1-2B",
                "quantization": "Q8_0",
                "revision": "rev",
                "files": [{
                    "path": "model.gguf",
                    "role": "unknown",
                    "size": 1,
                    "sha256": "00",
                    "url": "https://example.com/model.gguf"
                }],
                "license": "LFM Open License v1.0",
                "license_url": "https://example.com/license"
            }
        }"#;
        assert!(matches!(parse_manifest(json), Err(AiHostError::Json(_))));
    }
}
