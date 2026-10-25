use std::collections::BTreeMap;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Result, UpdateError};
use crate::install::InstallKind;

pub const MANIFEST_SCHEMA: u32 = 1;
pub const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
pub const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 300 * 1024 * 1024;

const LINUX_PLATFORM: &str = "linux-x86_64";
const WINDOWS_PLATFORM: &str = "windows-x86_64";

/// The signed update manifest served from the latest GitHub Release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    #[serde(default)]
    pub pub_date: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
    pub platforms: BTreeMap<String, PlatformAsset>,
}

/// One downloadable artifact in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformAsset {
    pub url: String,
    pub format: UpdateFormat,
    pub size: u64,
    pub sha256: String,
    /// Base64 of the complete minisign `.minisig` file contents.
    pub signature: String,
}

/// Supported update artifact formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateFormat {
    #[serde(rename = "appimage")]
    AppImage,
    #[serde(rename = "nsis")]
    Nsis,
}

/// A newer release selected for the current installation.
#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub version: Version,
    pub notes_url: Option<String>,
    pub asset: Option<PlatformAsset>,
}

impl Manifest {
    /// Parses the manifest and enforces the schema, platform, URL, size, and
    /// hash constraints. The caller must verify the detached signature over
    /// the raw bytes first.
    pub fn parse_and_validate(bytes: &[u8], allow_http: bool) -> Result<(Self, Version)> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(UpdateError::InvalidManifest(
                "manifest is larger than 512 KiB".to_string(),
            ));
        }

        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;

        if manifest.schema != MANIFEST_SCHEMA {
            return Err(UpdateError::InvalidManifest(format!(
                "unsupported manifest schema {}",
                manifest.schema
            )));
        }

        let version = parse_version(&manifest.version)?;

        for (platform, asset) in &manifest.platforms {
            if platform != LINUX_PLATFORM && platform != WINDOWS_PLATFORM {
                return Err(UpdateError::InvalidManifest(format!(
                    "unknown platform key {platform}"
                )));
            }
            asset.validate(platform, allow_http)?;
        }

        if let Some(notes_url) = &manifest.notes_url {
            validate_url(notes_url, allow_http, "notes_url")?;
        }

        if let Some(pub_date) = &manifest.pub_date
            && chrono::DateTime::parse_from_rfc3339(pub_date).is_err()
        {
            return Err(UpdateError::InvalidManifest(format!(
                "pub_date is not RFC 3339: {pub_date}"
            )));
        }

        Ok((manifest, version))
    }

    /// Returns the artifact for an install kind that can apply automatically.
    pub fn asset_for(&self, kind: &InstallKind) -> Option<&PlatformAsset> {
        let (platform, format) = match kind {
            InstallKind::AppImage { .. } => (LINUX_PLATFORM, UpdateFormat::AppImage),
            InstallKind::WindowsCurrentUser { .. } => (WINDOWS_PLATFORM, UpdateFormat::Nsis),
            _ => return None,
        };
        self.platforms
            .get(platform)
            .filter(|asset| asset.format == format)
    }
}

impl PlatformAsset {
    fn validate(&self, platform: &str, allow_http: bool) -> Result<()> {
        if self.size == 0 || self.size > MAX_ARTIFACT_BYTES {
            return Err(UpdateError::InvalidManifest(format!(
                "{platform}: artifact size {} is out of range",
                self.size
            )));
        }

        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(UpdateError::InvalidManifest(format!(
                "{platform}: sha256 must be 64 lowercase hex characters"
            )));
        }

        if self.signature.trim().is_empty() {
            return Err(UpdateError::InvalidManifest(format!(
                "{platform}: signature is empty"
            )));
        }
        let signature = crate::verify::decode_signature_base64(&self.signature).map_err(|_| {
            UpdateError::InvalidManifest(format!("{platform}: signature is not valid base64"))
        })?;
        minisign_verify::Signature::decode(&signature).map_err(|_| {
            UpdateError::InvalidManifest(format!(
                "{platform}: signature is not a minisign signature"
            ))
        })?;

        let url = validate_url(&self.url, allow_http, "artifact url")?;
        let file_name = artifact_file_name(&url)?;
        let expected_format = match platform {
            LINUX_PLATFORM => UpdateFormat::AppImage,
            WINDOWS_PLATFORM => UpdateFormat::Nsis,
            _ => unreachable!(),
        };
        if self.format != expected_format {
            return Err(UpdateError::InvalidManifest(format!(
                "{platform}: format {:?} does not match the platform",
                self.format
            )));
        }
        if !file_name
            .to_ascii_lowercase()
            .ends_with(match expected_format {
                UpdateFormat::AppImage => ".appimage",
                UpdateFormat::Nsis => ".exe",
            })
        {
            return Err(UpdateError::InvalidManifest(format!(
                "{platform}: unexpected artifact file name {file_name}"
            )));
        }

        Ok(())
    }

    /// Returns the safe single-component file name from the artifact URL.
    pub fn file_name(&self) -> Result<String> {
        let url = Url::parse(&self.url)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
        artifact_file_name(&url)
    }
}

fn parse_version(raw: &str) -> Result<Version> {
    let trimmed = raw.strip_prefix('v').unwrap_or(raw);
    Version::parse(trimmed)
        .map_err(|error| UpdateError::InvalidManifest(format!("invalid version {raw}: {error}")))
}

pub(crate) fn validate_url(raw: &str, allow_http: bool, label: &str) -> Result<Url> {
    let url = Url::parse(raw)
        .map_err(|error| UpdateError::InvalidManifest(format!("invalid {label}: {error}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UpdateError::InvalidManifest(format!(
            "{label} must not contain credentials"
        )));
    }
    let allowed = match url.scheme() {
        "https" => true,
        "http" => allow_http && url.host_str().is_some_and(is_loopback_host),
        _ => false,
    };
    if !allowed {
        return Err(UpdateError::InvalidManifest(format!(
            "{label} must use HTTPS{}",
            if allow_http {
                " (HTTP is allowed only on loopback in debug builds)"
            } else {
                ""
            }
        )));
    }
    Ok(url)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

fn artifact_file_name(url: &Url) -> Result<String> {
    let raw = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| UpdateError::InvalidManifest("artifact URL has no file name".to_string()))?;

    // Artifact names are ASCII and contain no escapes. Rejecting `%` avoids
    // encoded separators and traversal on any platform.
    if raw == "."
        || raw == ".."
        || raw.contains('/')
        || raw.contains('\\')
        || raw.contains('%')
        || raw.contains('\0')
    {
        return Err(UpdateError::InvalidManifest(format!(
            "unsafe artifact file name {raw}"
        )));
    }
    if Path::new(raw).components().count() != 1 {
        return Err(UpdateError::InvalidManifest(format!(
            "unsafe artifact file name {raw}"
        )));
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::UpdateError;
    use serde_json::Value;

    fn valid_manifest() -> Value {
        serde_json::json!({
            "schema": 1,
            "version": "0.4.0",
            "pub_date": "2026-10-24T12:00:00Z",
            "notes_url": "https://github.com/TommyZ-7/vrc-photo-manager/releases/tag/v0.4.0",
            "platforms": {
                "linux-x86_64": {
                    "url": "https://github.com/x/y/releases/download/v0.4.0/vrc-photo-manager_0.4.0_x86_64.AppImage",
                    "format": "appimage",
                    "size": 1024,
                    "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "signature": crate::verify::test_signature()
                }
            }
        })
    }

    fn parse(value: &Value, allow_http: bool) -> Result<(Manifest, semver::Version)> {
        let bytes = serde_json::to_vec(value).unwrap();
        Manifest::parse_and_validate(&bytes, allow_http)
    }

    #[test]
    fn validates_a_complete_manifest() {
        let (manifest, version) = parse(&valid_manifest(), false).unwrap();
        assert_eq!(version, semver::Version::new(0, 4, 0));
        assert_eq!(manifest.platforms.len(), 1);
        assert_eq!(
            manifest.platforms["linux-x86_64"].format,
            UpdateFormat::AppImage
        );
    }

    #[test]
    fn accepts_a_leading_v_in_the_version() {
        let mut value = valid_manifest();
        value["version"] = serde_json::json!("v0.4.0");
        let (_, version) = parse(&value, false).unwrap();
        assert_eq!(version, semver::Version::new(0, 4, 0));
    }

    #[test]
    fn rejects_unknown_schema() {
        let mut value = valid_manifest();
        value["schema"] = serde_json::json!(2);
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_unparsable_version() {
        let mut value = valid_manifest();
        value["version"] = serde_json::json!("latest");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_zero_and_oversized_artifacts() {
        let mut zero = valid_manifest();
        zero["platforms"]["linux-x86_64"]["size"] = serde_json::json!(0);
        assert!(matches!(
            parse(&zero, false),
            Err(UpdateError::InvalidManifest(_))
        ));

        let mut huge = valid_manifest();
        huge["platforms"]["linux-x86_64"]["size"] = serde_json::json!(MAX_ARTIFACT_BYTES + 1);
        assert!(matches!(
            parse(&huge, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_short_or_uppercase_sha256() {
        let mut short = valid_manifest();
        short["platforms"]["linux-x86_64"]["sha256"] = serde_json::json!("a".repeat(63));
        assert!(matches!(
            parse(&short, false),
            Err(UpdateError::InvalidManifest(_))
        ));

        let mut upper = valid_manifest();
        upper["platforms"]["linux-x86_64"]["sha256"] = serde_json::json!("A".repeat(64));
        assert!(matches!(
            parse(&upper, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_plain_http_unless_it_is_loopback_and_allowed() {
        let mut value = valid_manifest();
        value["platforms"]["linux-x86_64"]["url"] =
            serde_json::json!("http://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
        assert!(matches!(
            parse(&value, true),
            Err(UpdateError::InvalidManifest(_))
        ));

        value["platforms"]["linux-x86_64"]["url"] =
            serde_json::json!("http://127.0.0.1:8000/vrc-photo-manager_0.4.0_x86_64.AppImage");
        assert!(parse(&value, true).is_ok());
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_unknown_platform_keys() {
        let mut value = valid_manifest();
        value["platforms"]["macos-aarch64"] = value["platforms"]["linux-x86_64"].clone();
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_unsafe_or_missing_file_names() {
        let mut encoded = valid_manifest();
        encoded["platforms"]["linux-x86_64"]["url"] =
            serde_json::json!("https://example.com/dir/%2e%2e%2fevil.AppImage");
        assert!(matches!(
            parse(&encoded, false),
            Err(UpdateError::InvalidManifest(_))
        ));

        let mut missing = valid_manifest();
        missing["platforms"]["linux-x86_64"]["url"] = serde_json::json!("https://example.com/dir/");
        assert!(matches!(
            parse(&missing, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_non_https_notes_url() {
        let mut value = valid_manifest();
        value["notes_url"] = serde_json::json!("javascript:alert(1)");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_invalid_pub_date() {
        let mut value = valid_manifest();
        value["pub_date"] = serde_json::json!("yesterday");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_empty_signature() {
        let mut value = valid_manifest();
        value["platforms"]["linux-x86_64"]["signature"] = serde_json::json!("");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_a_signature_that_is_not_base64() {
        let mut value = valid_manifest();
        value["platforms"]["linux-x86_64"]["signature"] = serde_json::json!("not base64!");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn rejects_a_signature_that_is_not_minisign() {
        let mut value = valid_manifest();
        // base64("nope")
        value["platforms"]["linux-x86_64"]["signature"] = serde_json::json!("bm9wZQ==");
        assert!(matches!(
            parse(&value, false),
            Err(UpdateError::InvalidManifest(_))
        ));
    }
}
