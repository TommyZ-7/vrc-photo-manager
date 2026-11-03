//! Verified installation of signed plugin ZIP packages.
//!
//! Every package is extracted into a staging directory under `plugins/` and
//! only renamed into its final location after the signature, manifest, path
//! safety, size, and hash checks have all succeeded.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::Builder;
use zip::ZipArchive;

use crate::error::{AiHostError, Result};
use crate::layout::{plugin_dir, plugins_root};
use crate::manifest::{PluginManifest, parse_manifest, validate_manifest};
use crate::verify::verify_manifest_bytes;

/// Name of the manifest entry at the ZIP root.
pub const MANIFEST_FILE: &str = "manifest.json";
/// Name of the detached signature entry at the ZIP root.
pub const SIGNATURE_FILE: &str = "manifest.minisig";

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const ZIP64_EOCD_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const EOCD_MIN_BYTES: u64 = 22;
const ZIP64_EOCD_LOCATOR_BYTES: u64 = 20;
const ZIP64_EOCD_MIN_BYTES: u64 = 56;

/// Limits applied while reading a plugin ZIP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLimits {
    /// Maximum number of ZIP entries, including the manifest pair.
    pub max_entries: usize,
    /// Maximum total uncompressed bytes written by the package.
    pub max_uncompressed_bytes: u64,
    /// Maximum size of `manifest.json` (and `manifest.minisig`).
    pub max_manifest_bytes: u64,
    /// Maximum size of the ZIP file on disk before it is opened.
    pub max_zip_bytes: u64,
}

impl Default for InstallLimits {
    fn default() -> Self {
        Self {
            max_entries: 512,
            max_uncompressed_bytes: 512 * 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
            max_zip_bytes: 512 * 1024 * 1024,
        }
    }
}

/// A plugin that was verified and committed to its installation directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub plugin_id: String,
    pub variant: String,
    pub version: String,
    pub protocol_version: u32,
    pub path: PathBuf,
    pub manifest: PluginManifest,
    pub manifest_json: String,
}

/// Installs a signed plugin ZIP using the default limits.
pub fn install_from_zip(
    zip_path: &Path,
    data_dir: &Path,
    keys: &[String],
    host_version: &str,
) -> Result<InstalledPlugin> {
    install_from_zip_with_limits(
        zip_path,
        data_dir,
        keys,
        host_version,
        InstallLimits::default(),
    )
}

/// Installs a signed plugin ZIP with explicit limits.
pub fn install_from_zip_with_limits(
    zip_path: &Path,
    data_dir: &Path,
    keys: &[String],
    host_version: &str,
    limits: InstallLimits,
) -> Result<InstalledPlugin> {
    let zip_size = fs::metadata(zip_path)?.len();
    if zip_size > limits.max_zip_bytes {
        return Err(AiHostError::LimitExceeded(format!(
            "ZIP is {zip_size} bytes (limit {})",
            limits.max_zip_bytes
        )));
    }

    let mut file = File::open(zip_path)?;
    check_zip_entry_count(&mut file, zip_size, limits.max_entries)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    if archive.has_duplicate_names() {
        return Err(AiHostError::DuplicateEntry(
            "ZIP central directory contains duplicate file names".to_string(),
        ));
    }
    if archive.len() > limits.max_entries {
        return Err(AiHostError::LimitExceeded(format!(
            "ZIP has {} entries (limit {})",
            archive.len(),
            limits.max_entries
        )));
    }

    // First pass: validate every entry name and type before reading anything.
    let mut files = HashMap::new();
    let mut paths = HashSet::new();
    let mut manifest_entry = None;
    let mut signature_entry = None;

    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry
            .name()
            .map_err(|_| AiHostError::UnsafePath("<non-UTF-8 ZIP entry name>".to_string()))?
            .into_owned();

        if entry.is_symlink() {
            return Err(AiHostError::UnsafePath(name));
        }
        if entry.is_dir() {
            let directory = name.trim_end_matches('/');
            if !directory.is_empty() {
                validate_zip_path(&name, directory)?;
            }
            continue;
        }
        validate_zip_path(&name, &name)?;
        if entry.enclosed_name().is_none() {
            return Err(AiHostError::UnsafePath(name));
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 {
                return Err(AiHostError::UnsafePath(name));
            }
        }
        if !paths.insert(name.clone()) {
            return Err(AiHostError::DuplicateEntry(name));
        }

        let info = ZipEntryInfo {
            index,
            size: entry.size(),
            unix_mode: entry.unix_mode(),
        };
        if name == MANIFEST_FILE {
            manifest_entry = Some(info);
        } else if name == SIGNATURE_FILE {
            signature_entry = Some(info);
        } else {
            files.insert(name, info);
        }
    }

    let manifest_entry =
        manifest_entry.ok_or_else(|| AiHostError::MissingEntry(MANIFEST_FILE.to_string()))?;
    let signature_entry =
        signature_entry.ok_or_else(|| AiHostError::MissingEntry(SIGNATURE_FILE.to_string()))?;

    let manifest_bytes = read_entry(&mut archive, &manifest_entry, limits.max_manifest_bytes)?;
    let signature_bytes = read_entry(&mut archive, &signature_entry, limits.max_manifest_bytes)?;
    let manifest_len = manifest_bytes.len() as u64;
    let signature_len = signature_bytes.len() as u64;

    let signature = String::from_utf8(signature_bytes).map_err(|_| {
        AiHostError::ManifestInvalid("manifest.minisig is not valid UTF-8".to_string())
    })?;
    verify_manifest_bytes(&manifest_bytes, &signature, keys)?;

    let manifest_json = String::from_utf8(manifest_bytes).map_err(|_| {
        AiHostError::ManifestInvalid("manifest.json is not valid UTF-8".to_string())
    })?;
    let manifest = parse_manifest(manifest_json.as_bytes())?;
    validate_manifest(&manifest, host_version, std::env::consts::OS)?;

    let listed: HashSet<&str> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    for path in files.keys() {
        if !listed.contains(path.as_str()) {
            return Err(AiHostError::UnlistedEntry(path.clone()));
        }
    }
    for file in &manifest.files {
        if !files.contains_key(file.path.as_str()) {
            return Err(AiHostError::MissingEntry(file.path.clone()));
        }
    }

    let mut total_uncompressed = manifest_len + signature_len;
    for file in &manifest.files {
        total_uncompressed = total_uncompressed
            .checked_add(file.size)
            .ok_or_else(|| AiHostError::LimitExceeded("uncompressed size overflow".to_string()))?;
        if file.size > limits.max_uncompressed_bytes
            || total_uncompressed > limits.max_uncompressed_bytes
        {
            return Err(AiHostError::LimitExceeded(format!(
                "uncompressed size {total_uncompressed} exceeds {}",
                limits.max_uncompressed_bytes
            )));
        }
        let entry = files
            .get(file.path.as_str())
            .expect("listed files were checked above");
        if entry.size != file.size {
            return Err(AiHostError::Integrity(format!(
                "{}: ZIP entry is {} bytes but manifest declares {}",
                file.path, entry.size, file.size
            )));
        }
    }

    let root = plugins_root(data_dir);
    reject_install_directory_collisions(
        &root,
        &manifest.plugin_id,
        &manifest.variant,
        &manifest.plugin_version,
    )?;
    fs::create_dir_all(&root)?;
    let staging = Builder::new().prefix(".staging-").tempdir_in(&root)?;

    for file in &manifest.files {
        let entry = files
            .get(file.path.as_str())
            .expect("listed files were checked above");
        extract_entry(
            &mut archive,
            entry,
            staging.path(),
            file,
            file.path == manifest.entrypoint,
        )?;
    }

    let target = plugin_dir(
        data_dir,
        &manifest.plugin_id,
        &manifest.variant,
        &manifest.plugin_version,
    );
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    replace_installation(staging.path(), &target, &root)?;

    tracing::debug!(
        plugin_id = %manifest.plugin_id,
        variant = %manifest.variant,
        version = %manifest.plugin_version,
        "installed AI plugin package"
    );

    Ok(InstalledPlugin {
        plugin_id: manifest.plugin_id.clone(),
        variant: manifest.variant.clone(),
        version: manifest.plugin_version.clone(),
        protocol_version: manifest.protocol_version,
        path: target,
        manifest,
        manifest_json,
    })
}

#[derive(Debug, Clone, Copy)]
struct ZipEntryInfo {
    index: usize,
    size: u64,
    unix_mode: Option<u32>,
}

/// Reads the EOCD (and ZIP64 EOCD when present) and rejects an excessive
/// declared entry count before `ZipArchive::new` allocates from the central
/// directory metadata.
fn check_zip_entry_count(file: &mut File, file_len: u64, max_entries: usize) -> Result<()> {
    let tail_len = file_len.min(EOCD_MIN_BYTES + u64::from(u16::MAX));
    if tail_len < EOCD_MIN_BYTES {
        return Err(invalid_zip("file is too small to be a ZIP archive"));
    }
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(file_len - tail_len))?;
    file.read_exact(&mut tail)?;

    let signature = EOCD_SIGNATURE.to_le_bytes();
    let mut declared_entries = 0u64;
    let mut found = false;

    for position in 0..=tail.len() - EOCD_MIN_BYTES as usize {
        if tail[position..position + 4] != signature {
            continue;
        }
        let comment_length = usize::from(read_u16(&tail, position + 20));
        if position + EOCD_MIN_BYTES as usize + comment_length != tail.len() {
            continue;
        }
        found = true;
        declared_entries = declared_entries
            .max(u64::from(read_u16(&tail, position + 8)))
            .max(u64::from(read_u16(&tail, position + 10)));

        if let Some(locator_position) = position.checked_sub(ZIP64_EOCD_LOCATOR_BYTES as usize)
            && tail[locator_position..locator_position + 4]
                == ZIP64_EOCD_LOCATOR_SIGNATURE.to_le_bytes()
        {
            let zip64_offset = read_u64(&tail, locator_position + 8);
            let (on_disk, total) = read_zip64_entry_count(file, file_len, zip64_offset)?;
            declared_entries = declared_entries.max(on_disk).max(total);
        }
    }

    if !found {
        return Err(invalid_zip("ZIP end-of-central-directory record not found"));
    }
    if declared_entries > max_entries as u64 {
        return Err(AiHostError::LimitExceeded(format!(
            "ZIP declares {declared_entries} entries (limit {max_entries})"
        )));
    }
    Ok(())
}

fn read_zip64_entry_count(file: &mut File, file_len: u64, offset: u64) -> Result<(u64, u64)> {
    let header_end = offset
        .checked_add(ZIP64_EOCD_MIN_BYTES)
        .ok_or_else(|| invalid_zip("ZIP64 EOCD offset overflows"))?;
    if header_end > file_len {
        return Err(invalid_zip("ZIP64 EOCD record is outside the file"));
    }

    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; ZIP64_EOCD_MIN_BYTES as usize];
    file.read_exact(&mut header)?;
    if read_u32(&header, 0) != ZIP64_EOCD_SIGNATURE {
        return Err(invalid_zip("ZIP64 EOCD signature not found"));
    }

    let record_size = read_u64(&header, 4);
    let record_end = offset
        .checked_add(12)
        .and_then(|value| value.checked_add(record_size))
        .ok_or_else(|| invalid_zip("ZIP64 EOCD size overflows"))?;
    if record_size < 44 || record_end > file_len {
        return Err(invalid_zip("invalid ZIP64 EOCD record size"));
    }

    Ok((read_u64(&header, 24), read_u64(&header, 32)))
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("a four-byte field"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("an eight-byte field"),
    )
}

fn invalid_zip(message: &'static str) -> AiHostError {
    AiHostError::Zip(zip::result::ZipError::InvalidArchive(
        std::borrow::Cow::Borrowed(message),
    ))
}

/// Rejects a new install directory whose name differs only by case from an
/// existing sibling. This keeps Linux behaviour consistent with Windows,
/// where both names would resolve to the same directory.
fn reject_install_directory_collisions(
    root: &Path,
    plugin_id: &str,
    variant: &str,
    version: &str,
) -> Result<()> {
    reject_case_collision(root, plugin_id)?;
    let plugin_path = root.join(plugin_id);
    reject_case_collision(&plugin_path, variant)?;
    let variant_path = plugin_path.join(variant);
    reject_case_collision(&variant_path, version)?;
    Ok(())
}

fn reject_case_collision(parent: &Path, name: &str) -> Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let existing = entry.file_name();
        let Some(existing) = existing.to_str() else {
            continue;
        };
        if existing != name && existing.eq_ignore_ascii_case(name) {
            return Err(AiHostError::UnsafePath(format!(
                "install directory {name:?} collides case-insensitively with existing {existing:?}"
            )));
        }
    }
    Ok(())
}

fn validate_zip_path(original: &str, path: &str) -> Result<()> {
    crate::manifest::validate_relative_path("ZIP entry", path).map_err(|error| match error {
        AiHostError::UnsafePath(_) => AiHostError::UnsafePath(original.to_string()),
        other => other,
    })
}

fn read_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    info: &ZipEntryInfo,
    limit: u64,
) -> Result<Vec<u8>> {
    let mut entry = archive.by_index(info.index)?;
    if entry.size() > limit {
        return Err(AiHostError::LimitExceeded(format!(
            "{} is {} bytes (limit {limit})",
            entry.name().unwrap_or_default(),
            entry.size()
        )));
    }
    let mut bytes = Vec::new();
    entry
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(AiHostError::LimitExceeded(format!(
            "entry exceeds the {limit}-byte limit"
        )));
    }
    Ok(bytes)
}

fn extract_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    info: &ZipEntryInfo,
    staging: &Path,
    file: &crate::manifest::ManifestFile,
    is_entrypoint: bool,
) -> Result<()> {
    let mut entry = archive.by_index(info.index)?;
    let destination = staging.join(&file.path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = File::create(&destination)?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = entry.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > file.size {
            return Err(AiHostError::Integrity(format!(
                "{}: entry is larger than the declared {} bytes",
                file.path, file.size
            )));
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    output.flush()?;

    if written != file.size {
        return Err(AiHostError::Integrity(format!(
            "{}: expected {} bytes, extracted {written}",
            file.path, file.size
        )));
    }
    let digest = hex::encode(hasher.finalize());
    if !digest.eq_ignore_ascii_case(&file.sha256) {
        return Err(AiHostError::Integrity(format!(
            "{}: SHA-256 does not match the manifest",
            file.path
        )));
    }

    set_file_mode(&destination, info.unix_mode, is_entrypoint)?;
    Ok(())
}

fn set_file_mode(path: &Path, zip_mode: Option<u32>, is_entrypoint: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut mode = zip_mode.map(|mode| mode & 0o777).unwrap_or(0o644);
        if is_entrypoint && mode & 0o111 == 0 {
            mode = 0o755;
        }
        if mode == 0 {
            mode = 0o644;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, zip_mode, is_entrypoint);
    }
    Ok(())
}

fn replace_installation(staging: &Path, target: &Path, backup_root: &Path) -> Result<()> {
    let target_exists = match fs::symlink_metadata(target) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };

    let backup = if target_exists {
        let directory = Builder::new().prefix(".backup-").tempdir_in(backup_root)?;
        let path = directory.path().join("previous");
        fs::rename(target, &path)?;
        Some((directory, path))
    } else {
        None
    };

    if let Err(error) = fs::rename(staging, target) {
        if let Some((backup_directory, backup_path)) = backup {
            match fs::rename(&backup_path, target) {
                Ok(()) => drop(backup_directory),
                Err(restore_error) => {
                    let kept = backup_directory.keep();
                    tracing::error!(
                        backup = %kept.display(),
                        error = %restore_error,
                        "failed to restore the previous AI plugin installation"
                    );
                }
            }
        }
        return Err(error.into());
    }

    drop(backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::test_support::{
        TEST_ENTRYPOINT, TEST_PLUGIN_ID, TEST_VARIANT, TestEntry, TestKey, TestPackage,
        write_temp_zip, zip_entries, zip_with_manifest,
    };

    fn install(package: &TestPackage, data_dir: &Path) -> Result<InstalledPlugin> {
        let zip = package.temp_zip()?;
        install_from_zip(zip.path(), data_dir, &[package.public_key()], "0.10.1")
    }

    fn prefixed_entries(data_dir: &Path, prefix: &str) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(plugins_root(data_dir)) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
            })
            .collect()
    }

    fn staging_entries(data_dir: &Path) -> Vec<PathBuf> {
        prefixed_entries(data_dir, ".staging-")
    }

    fn backup_entries(data_dir: &Path) -> Vec<PathBuf> {
        prefixed_entries(data_dir, ".backup-")
    }

    fn replace_entry_data(package: &mut TestPackage, path: &str, data: &[u8]) {
        let entry = package
            .entries_mut()
            .iter_mut()
            .find(|entry| entry.path == path)
            .expect("test entry exists");
        entry.data = data.to_vec();
    }

    fn replace_bytes(bytes: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
        assert_eq!(needle.len(), replacement.len());
        let mut patched = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index..].starts_with(needle) {
                patched.extend_from_slice(replacement);
                index += needle.len();
            } else {
                patched.push(bytes[index]);
                index += 1;
            }
        }
        patched
    }

    fn patch_eocd_entry_count(bytes: &[u8], entries: u16) -> Vec<u8> {
        let signature = [0x50, 0x4b, 0x05, 0x06];
        let position = bytes
            .windows(signature.len())
            .rposition(|window| window == signature)
            .expect("ZIP contains an EOCD record");
        let mut patched = bytes.to_vec();
        patched[position + 8..position + 10].copy_from_slice(&entries.to_le_bytes());
        patched[position + 10..position + 12].copy_from_slice(&entries.to_le_bytes());
        patched
    }

    #[test]
    fn install_accepts_a_valid_plugin() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let installed = install(&package, data_dir.path()).unwrap();

        assert_eq!(installed.plugin_id, TEST_PLUGIN_ID);
        assert_eq!(installed.variant, TEST_VARIANT);
        assert_eq!(installed.version, crate::test_support::TEST_PLUGIN_VERSION);
        assert_eq!(installed.protocol_version, 1);
        assert_eq!(
            installed.path,
            plugin_dir(
                data_dir.path(),
                TEST_PLUGIN_ID,
                TEST_VARIANT,
                crate::test_support::TEST_PLUGIN_VERSION
            )
        );
        assert_eq!(installed.manifest, *package.manifest());
        assert!(installed.manifest_json.contains("\"plugin_id\""));
        assert_eq!(
            fs::read(installed.path.join("catalog.json")).unwrap(),
            br#"{"version":"2026-11-03.1"}"#
        );
        assert!(installed.path.join(TEST_ENTRYPOINT).is_file());
        assert!(staging_entries(data_dir.path()).is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(installed.path.join(TEST_ENTRYPOINT))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn install_replaces_an_existing_version() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let installed = install(&package, data_dir.path()).unwrap();
        fs::write(installed.path.join("stale.txt"), b"stale").unwrap();

        install(&package, data_dir.path()).unwrap();
        assert!(!installed.path.join("stale.txt").exists());
        assert!(staging_entries(data_dir.path()).is_empty());
        assert!(backup_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn default_install_limits_match_the_design() {
        let limits = InstallLimits::default();
        assert_eq!(limits.max_entries, 512);
        assert_eq!(limits.max_uncompressed_bytes, 512 * 1024 * 1024);
        assert_eq!(limits.max_manifest_bytes, 1024 * 1024);
        assert_eq!(limits.max_zip_bytes, 512 * 1024 * 1024);
    }

    #[test]
    fn install_rejects_an_excessive_eocd_entry_count_before_opening_the_archive() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let bytes = patch_eocd_entry_count(&package.zip_bytes().unwrap(), u16::MAX);
        let zip = write_temp_zip(&bytes).unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::LimitExceeded(_)));
        assert!(
            fs::read_dir(data_dir.path()).unwrap().next().is_none(),
            "the data directory must not be touched before the entry-count check"
        );
    }

    #[test]
    fn zip64_eocd_entry_count_is_checked_before_opening_the_archive() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("zip64.zip");
        let mut bytes = Vec::new();

        // ZIP64 EOCD record at offset 0.
        bytes.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&44u64.to_le_bytes());
        bytes.extend_from_slice(&45u16.to_le_bytes());
        bytes.extend_from_slice(&45u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1_000_000u64.to_le_bytes());
        bytes.extend_from_slice(&1_000_000u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());

        // ZIP64 EOCD locator at offset 56.
        bytes.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());

        // ZIP32 EOCD with sentinel counts at offset 76.
        bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());

        fs::write(&path, &bytes).unwrap();
        let mut file = File::open(&path).unwrap();
        let error = check_zip_entry_count(&mut file, bytes.len() as u64, 512).unwrap_err();

        assert!(matches!(error, AiHostError::LimitExceeded(_)));
    }

    #[test]
    fn install_rejects_case_insensitive_plugin_id_collisions() {
        let data_dir = tempdir().unwrap();
        let first = TestPackage::new();
        install(&first, data_dir.path()).unwrap();

        let mut second = TestPackage::new();
        second.manifest_mut().plugin_id = "VRC-Photon-AutoTag".to_string();
        let error = install(&second, data_dir.path()).unwrap_err();

        assert!(matches!(error, AiHostError::UnsafePath(_)));
        assert!(
            !plugin_dir(
                data_dir.path(),
                "VRC-Photon-AutoTag",
                TEST_VARIANT,
                crate::test_support::TEST_PLUGIN_VERSION
            )
            .exists()
        );
        assert!(staging_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_rejects_case_insensitive_variant_collisions() {
        let data_dir = tempdir().unwrap();
        let first = TestPackage::new();
        install(&first, data_dir.path()).unwrap();

        let mut second = TestPackage::new();
        second.manifest_mut().backend = "VULKAN".to_string();
        second.manifest_mut().variant = "linux-VULKAN".to_string();
        let error = install(&second, data_dir.path()).unwrap_err();

        assert!(matches!(error, AiHostError::UnsafePath(_)));
        assert!(staging_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_rejects_case_insensitive_version_collisions() {
        let data_dir = tempdir().unwrap();
        let mut first = TestPackage::new();
        first.manifest_mut().plugin_version = "1.0.0-rc1".to_string();
        install(&first, data_dir.path()).unwrap();

        let mut second = TestPackage::new();
        second.manifest_mut().plugin_version = "1.0.0-RC1".to_string();
        let error = install(&second, data_dir.path()).unwrap_err();

        assert!(matches!(error, AiHostError::UnsafePath(_)));
        assert!(staging_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_rejects_a_zip_larger_than_the_limit_without_writing() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let zip = package.temp_zip().unwrap();
        let zip_size = fs::metadata(zip.path()).unwrap().len();
        assert!(zip_size > 0);

        let error = install_from_zip_with_limits(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
            InstallLimits {
                max_zip_bytes: zip_size - 1,
                ..InstallLimits::default()
            },
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::LimitExceeded(_)));
        assert!(
            fs::read_dir(data_dir.path()).unwrap().next().is_none(),
            "the data directory must not be touched before the ZIP size check"
        );
    }

    #[test]
    fn install_rejects_tampered_manifest() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let mut manifest = package.manifest_bytes();
        let signature = package.key().sign(&manifest);
        manifest.push(b' ');
        let zip =
            write_temp_zip(&zip_with_manifest(&manifest, &signature, package.entries()).unwrap())
                .unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::Signature));
        assert!(staging_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_rejects_a_package_signed_by_a_different_key() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let other_key = TestKey::generate();
        let zip = package.temp_zip().unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[other_key.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::Signature));
    }

    #[test]
    fn install_rejects_an_empty_key_list() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let zip = package.temp_zip().unwrap();

        let error = install_from_zip(zip.path(), data_dir.path(), &[], "0.10.1").unwrap_err();
        assert!(matches!(error, AiHostError::Signature));
    }

    #[test]
    fn install_rejects_file_hash_mismatch() {
        let data_dir = tempdir().unwrap();
        let mut package = TestPackage::new();
        let original_len = package
            .entries()
            .iter()
            .find(|entry| entry.path == "catalog.json")
            .unwrap()
            .data
            .len();
        replace_entry_data(
            &mut package,
            "catalog.json",
            "x".repeat(original_len).as_bytes(),
        );

        let error = install(&package, data_dir.path()).unwrap_err();
        assert!(matches!(error, AiHostError::Integrity(_)));
        assert!(
            !plugin_dir(
                data_dir.path(),
                TEST_PLUGIN_ID,
                TEST_VARIANT,
                crate::test_support::TEST_PLUGIN_VERSION
            )
            .exists()
        );
        assert!(staging_entries(data_dir.path()).is_empty());
        assert!(backup_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_rejects_file_size_mismatch() {
        let data_dir = tempdir().unwrap();
        let mut package = TestPackage::new();
        replace_entry_data(&mut package, "catalog.json", b"short");

        let error = install(&package, data_dir.path()).unwrap_err();
        assert!(matches!(error, AiHostError::Integrity(_)));
    }

    #[test]
    fn install_rejects_duplicate_entries() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        // `zip` refuses to write duplicate names, so write two same-length names
        // and rename the second entry in both headers afterwards.
        let entries = vec![
            TestEntry::new("duplicate.txt", b"first"),
            TestEntry::new("duplicatf.txt", b"second"),
        ];
        let bytes =
            zip_with_manifest(&package.manifest_bytes(), &package.signature(), &entries).unwrap();
        let bytes = replace_bytes(&bytes, b"duplicatf.txt", b"duplicate.txt");
        let zip = write_temp_zip(&bytes).unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(
            matches!(error, AiHostError::DuplicateEntry(_)),
            "expected a duplicate-entry error, got {error:?}"
        );
    }

    #[test]
    fn install_rejects_traversal_and_absolute_entries() {
        for path in [
            "../escape.txt",
            "a/../../escape.txt",
            "/etc/passwd",
            "C:/evil.txt",
            "a\\b",
        ] {
            let data_dir = tempdir().unwrap();
            let package = TestPackage::new();
            let mut entries = package.entries().to_vec();
            entries.push(TestEntry::new(path, b"x"));
            let zip = write_temp_zip(
                &zip_with_manifest(&package.manifest_bytes(), &package.signature(), &entries)
                    .unwrap(),
            )
            .unwrap();

            let error = install_from_zip(
                zip.path(),
                data_dir.path(),
                &[package.public_key()],
                "0.10.1",
            )
            .unwrap_err();
            assert!(
                matches!(error, AiHostError::UnsafePath(_)),
                "{path:?} should be rejected as unsafe, got {error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlink_entries() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let mut entries = package.entries().to_vec();
        entries.push(TestEntry::with_mode("link", b"", 0o120777));
        let zip = write_temp_zip(
            &zip_with_manifest(&package.manifest_bytes(), &package.signature(), &entries).unwrap(),
        )
        .unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::UnsafePath(_)));
    }

    #[test]
    fn install_rejects_unlisted_entries() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let mut entries = package.entries().to_vec();
        entries.push(TestEntry::new("extra.txt", b"extra"));
        let zip = write_temp_zip(
            &zip_with_manifest(&package.manifest_bytes(), &package.signature(), &entries).unwrap(),
        )
        .unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::UnlistedEntry(_)));
    }

    #[test]
    fn install_rejects_missing_listed_files() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let entries: Vec<_> = package
            .entries()
            .iter()
            .filter(|entry| entry.path != "catalog.json")
            .cloned()
            .collect();
        let zip = write_temp_zip(
            &zip_with_manifest(&package.manifest_bytes(), &package.signature(), &entries).unwrap(),
        )
        .unwrap();

        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::MissingEntry(_)));
    }

    #[test]
    fn install_requires_a_manifest_and_signature() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();

        let entries = zip_entries(package.entries()).unwrap();
        let zip = write_temp_zip(&entries).unwrap();
        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::MissingEntry(_)));

        let mut entries = vec![TestEntry::new(MANIFEST_FILE, package.manifest_bytes())];
        entries.extend(package.entries().iter().cloned());
        let zip = write_temp_zip(&zip_entries(&entries).unwrap()).unwrap();
        let error = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::MissingEntry(_)));
    }

    #[test]
    fn install_rejects_entry_and_size_limits() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let zip = package.temp_zip().unwrap();

        let error = install_from_zip_with_limits(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
            InstallLimits {
                max_entries: 2,
                ..InstallLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::LimitExceeded(_)));

        let error = install_from_zip_with_limits(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
            InstallLimits {
                max_uncompressed_bytes: 8,
                ..InstallLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::LimitExceeded(_)));

        let error = install_from_zip_with_limits(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
            InstallLimits {
                max_manifest_bytes: 8,
                ..InstallLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, AiHostError::LimitExceeded(_)));
    }

    #[test]
    fn replace_installation_swaps_in_staging_and_removes_the_backup() {
        let workspace = tempdir().unwrap();
        let plugins = plugins_root(workspace.path());
        fs::create_dir_all(&plugins).unwrap();
        let target = plugin_dir(
            workspace.path(),
            TEST_PLUGIN_ID,
            TEST_VARIANT,
            crate::test_support::TEST_PLUGIN_VERSION,
        );
        fs::create_dir_all(target.join("libs")).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();

        let staging = Builder::new()
            .prefix(".staging-")
            .tempdir_in(&plugins)
            .unwrap();
        fs::write(staging.path().join("new.txt"), b"new").unwrap();

        replace_installation(staging.path(), &target, &plugins).unwrap();

        assert_eq!(fs::read(target.join("new.txt")).unwrap(), b"new");
        assert!(!target.join("old.txt").exists());
        assert!(prefixed_entries(workspace.path(), ".backup-").is_empty());
    }

    #[test]
    fn replace_installation_restores_the_previous_version_when_the_rename_fails() {
        let workspace = tempdir().unwrap();
        let plugins = plugins_root(workspace.path());
        fs::create_dir_all(&plugins).unwrap();
        let target = plugin_dir(
            workspace.path(),
            TEST_PLUGIN_ID,
            TEST_VARIANT,
            crate::test_support::TEST_PLUGIN_VERSION,
        );
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        let missing_staging = plugins.join(".staging-missing");

        let error = replace_installation(&missing_staging, &target, &plugins).unwrap_err();

        assert!(matches!(error, AiHostError::Io(_)));
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(prefixed_entries(workspace.path(), ".backup-").is_empty());
    }

    #[test]
    fn install_is_atomic_on_failure() {
        let data_dir = tempdir().unwrap();
        let mut package = TestPackage::new();
        let installed = install(&package, data_dir.path()).unwrap();
        let original = fs::read(installed.path.join("catalog.json")).unwrap();

        replace_entry_data(&mut package, "catalog.json", b"broken");
        let error = install(&package, data_dir.path()).unwrap_err();
        assert!(matches!(error, AiHostError::Integrity(_)));

        assert!(installed.path.join(TEST_ENTRYPOINT).is_file());
        assert_eq!(
            fs::read(installed.path.join("catalog.json")).unwrap(),
            original
        );
        assert!(staging_entries(data_dir.path()).is_empty());
        assert!(backup_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_leaves_no_target_or_staging_after_a_first_failure() {
        let data_dir = tempdir().unwrap();
        let mut package = TestPackage::new();
        replace_entry_data(&mut package, "catalog.json", b"broken");

        assert!(install(&package, data_dir.path()).is_err());
        assert!(
            !plugin_dir(
                data_dir.path(),
                TEST_PLUGIN_ID,
                TEST_VARIANT,
                crate::test_support::TEST_PLUGIN_VERSION
            )
            .exists()
        );
        assert!(staging_entries(data_dir.path()).is_empty());
        assert!(backup_entries(data_dir.path()).is_empty());
    }
}
