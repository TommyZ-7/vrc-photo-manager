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
/// `zip` only searches the tail of the file for the EOCD, and trailing bytes
/// after the EOCD comment are allowed. A generous 128 KiB window covers its
/// search window plus the largest possible comment.
const EOCD_SEARCH_BYTES: u64 = 128 * 1024;

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

/// Installs a signed plugin ZIP and keeps the replaced package's files in a
/// backup directory instead of deleting them.
///
/// Returns the previous installation directory when an installation was
/// replaced. The caller owns that directory and must either pass it to
/// [`restore_installation_backup`] or remove it once the new package is
/// committed.
pub fn install_from_zip_keeping_backup(
    zip_path: &Path,
    data_dir: &Path,
    keys: &[String],
    host_version: &str,
) -> Result<(InstalledPlugin, Option<PathBuf>)> {
    install_from_zip_impl(
        zip_path,
        data_dir,
        keys,
        host_version,
        InstallLimits::default(),
        |from, to| fs::rename(from, to),
        true,
    )
}

/// Restores the package replaced by [`install_from_zip_keeping_backup`].
///
/// Removes the newly installed target and renames the retained backup back
/// into place. The backup's now-empty `.backup-*` parent is removed too.
pub fn restore_installation_backup(target: &Path, backup: &Path) -> Result<()> {
    match fs::remove_dir_all(target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::rename(backup, target)?;
    if let Some(parent) = backup.parent()
        && parent
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".backup-"))
    {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

/// Deletes the retained previous package after the new one is committed.
///
/// Only a path inside a `.backup-*` directory is accepted, so a caller cannot
/// accidentally remove the live installation directory.
pub fn discard_installation_backup(backup: &Path) -> Result<()> {
    let parent = backup.parent().ok_or_else(|| {
        AiHostError::Integrity(format!(
            "backup path {} has no parent directory",
            backup.display()
        ))
    })?;
    if !parent
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(".backup-"))
    {
        return Err(AiHostError::Integrity(format!(
            "refusing to remove non-backup directory {}",
            parent.display()
        )));
    }
    match fs::remove_dir_all(parent) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Installs a signed plugin ZIP with explicit limits.
pub fn install_from_zip_with_limits(
    zip_path: &Path,
    data_dir: &Path,
    keys: &[String],
    host_version: &str,
    limits: InstallLimits,
) -> Result<InstalledPlugin> {
    install_from_zip_impl(
        zip_path,
        data_dir,
        keys,
        host_version,
        limits,
        |from, to| fs::rename(from, to),
        false,
    )
    .map(|(installed, _)| installed)
}

#[cfg(test)]
fn install_from_zip_with_limits_using<R>(
    zip_path: &Path,
    data_dir: &Path,
    keys: &[String],
    host_version: &str,
    limits: InstallLimits,
    rename: R,
) -> Result<InstalledPlugin>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    install_from_zip_impl(
        zip_path,
        data_dir,
        keys,
        host_version,
        limits,
        rename,
        false,
    )
    .map(|(installed, _)| installed)
}

fn install_from_zip_impl<R>(
    zip_path: &Path,
    data_dir: &Path,
    keys: &[String],
    host_version: &str,
    limits: InstallLimits,
    mut rename: R,
    keep_backup: bool,
) -> Result<(InstalledPlugin, Option<PathBuf>)>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
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
    let backup =
        replace_installation_with(staging.path(), &target, &root, &mut rename, keep_backup)?;

    tracing::debug!(
        plugin_id = %manifest.plugin_id,
        variant = %manifest.variant,
        version = %manifest.plugin_version,
        "installed AI plugin package"
    );

    Ok((
        InstalledPlugin {
            plugin_id: manifest.plugin_id.clone(),
            variant: manifest.variant.clone(),
            version: manifest.plugin_version.clone(),
            protocol_version: manifest.protocol_version,
            path: target,
            manifest,
            manifest_json,
        },
        backup,
    ))
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
///
/// Backwards EOCD scanners may accept a candidate whose comment ends before
/// EOF and then fall back to an earlier candidate when the last one does not
/// describe a readable central directory. To cover that, every plausible
/// EOCD candidate in the tail is considered and the maximum declared count
/// wins.
fn check_zip_entry_count<R: Read + Seek>(
    file: &mut R,
    file_len: u64,
    max_entries: usize,
) -> Result<()> {
    let tail_len = file_len.min(EOCD_SEARCH_BYTES);
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
        // `zip` accepts trailing bytes after the comment, so do not require
        // the record to end exactly at EOF.
        let comment_length = usize::from(read_u16(&tail, position + 20));
        if position + EOCD_MIN_BYTES as usize + comment_length > tail.len() {
            continue;
        }
        found = true;

        let entries_on_disk = u64::from(read_u16(&tail, position + 8));
        let total_entries = u64::from(read_u16(&tail, position + 10));
        let directory_size = u64::from(read_u32(&tail, position + 12));
        let directory_offset = u64::from(read_u32(&tail, position + 16));
        // `zip` only consults the ZIP64 locator when a ZIP32 field uses its
        // sentinel value. Mirror that so a valid ZIP64 archive is not
        // rejected for its sentinel, while the values actually used for the
        // allocation are always checked.
        let may_be_zip64 = entries_on_disk == u64::from(u16::MAX)
            || total_entries == u64::from(u16::MAX)
            || directory_size == u64::from(u32::MAX)
            || directory_offset == u64::from(u32::MAX);

        let (candidate_on_disk, candidate_total) = if may_be_zip64 {
            match zip64_locator_counts(file, file_len, file_len - tail_len, &tail, position)? {
                Some(counts) => counts,
                None => (entries_on_disk, total_entries),
            }
        } else {
            (entries_on_disk, total_entries)
        };
        declared_entries = declared_entries.max(candidate_on_disk).max(candidate_total);
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

/// Validates the EOCD record(s) of an in-memory ZIP image.
///
/// This mirrors the entry-count gate that runs before `ZipArchive::new`, but
/// accepts a byte slice so the standalone fuzz targets can exercise the
/// parser without filesystem setup. It is only compiled for `fuzz-support`.
#[cfg(feature = "fuzz-support")]
#[doc(hidden)]
pub fn scan_zip_entry_count(bytes: &[u8], max_entries: usize) -> Result<()> {
    let mut cursor = std::io::Cursor::new(bytes);
    check_zip_entry_count(&mut cursor, bytes.len() as u64, max_entries)
}

fn zip64_locator_counts<R: Read + Seek>(
    file: &mut R,
    file_len: u64,
    tail_start: u64,
    tail: &[u8],
    eocd_position: usize,
) -> Result<Option<(u64, u64)>> {
    let Some(locator_position) = eocd_position.checked_sub(ZIP64_EOCD_LOCATOR_BYTES as usize)
    else {
        return Ok(None);
    };
    if tail[locator_position..locator_position + 4] != ZIP64_EOCD_LOCATOR_SIGNATURE.to_le_bytes() {
        return Ok(None);
    }
    if read_u32(tail, locator_position + 16) > 1 {
        return Err(invalid_zip("multi-disk ZIP64 archives are not supported"));
    }

    let locator_disk_with_central_directory = read_u32(tail, locator_position + 4);
    let zip64_offset = read_u64(tail, locator_position + 8);
    let locator_file_offset = tail_start + locator_position as u64;
    let counts = read_zip64_entry_count(
        file,
        file_len,
        zip64_offset,
        locator_file_offset,
        locator_disk_with_central_directory,
    )?;
    Ok(Some(counts))
}

fn read_zip64_entry_count<R: Read + Seek>(
    file: &mut R,
    file_len: u64,
    offset: u64,
    locator_offset: u64,
    locator_disk_with_central_directory: u32,
) -> Result<(u64, u64)> {
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
    let expected_length = locator_offset
        .checked_sub(offset)
        .ok_or_else(|| invalid_zip("ZIP64 EOCD offset is after its locator"))?;
    if record_size < 44
        || record_size
            .checked_add(12)
            .is_none_or(|length| length != expected_length)
    {
        return Err(invalid_zip("invalid ZIP64 EOCD record length"));
    }

    let entries_on_disk = read_u64(&header, 24);
    let total_entries = read_u64(&header, 32);
    if read_u32(&header, 16) != 0 || entries_on_disk != total_entries {
        return Err(invalid_zip("multi-disk ZIP64 archives are not supported"));
    }
    if read_u32(&header, 20) != locator_disk_with_central_directory {
        return Err(invalid_zip("ZIP64 EOCD disagrees with its locator"));
    }

    // `zip` rejects an EOCD64 that starts before the minimum possible end of
    // the central directory. Mirror this so its later cache-based search can
    // never fall through to a different EOCD64 record.
    let central_directory_offset = read_u64(&header, 48);
    let minimum_offset = total_entries
        .checked_mul(46)
        .and_then(|value| value.checked_add(central_directory_offset))
        .ok_or_else(|| invalid_zip("ZIP64 EOCD offset overflows"))?;
    if offset < minimum_offset {
        return Err(invalid_zip(
            "ZIP64 EOCD is positioned before its central directory",
        ));
    }

    Ok((entries_on_disk, total_entries))
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

fn replace_installation_with<R>(
    staging: &Path,
    target: &Path,
    backup_root: &Path,
    mut rename: R,
    keep_backup: bool,
) -> Result<Option<PathBuf>>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let target_exists = match fs::symlink_metadata(target) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };

    let backup = if target_exists {
        let directory = Builder::new().prefix(".backup-").tempdir_in(backup_root)?;
        let path = directory.path().join("previous");
        rename(target, &path)?;
        Some((directory, path))
    } else {
        None
    };

    if let Err(error) = rename(staging, target) {
        if let Some((backup_directory, backup_path)) = backup {
            match rename(&backup_path, target) {
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

    match backup {
        Some((directory, path)) if keep_backup => {
            // Persist the temporary directory so the caller can restore the
            // previous package after a later probe or catalog failure.
            let _kept = directory.keep();
            Ok(Some(path))
        }
        Some(_) => Ok(None),
        None => Ok(None),
    }
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

    fn insert_decoy_eocd(bytes: &[u8], entries: u16) -> Vec<u8> {
        let signature = [0x50, 0x4b, 0x05, 0x06];
        let position = bytes
            .windows(signature.len())
            .rposition(|window| window == signature)
            .expect("ZIP contains an EOCD record");
        let mut decoy = Vec::with_capacity(22);
        decoy.extend_from_slice(&signature);
        decoy.extend_from_slice(&0u16.to_le_bytes());
        decoy.extend_from_slice(&0u16.to_le_bytes());
        decoy.extend_from_slice(&entries.to_le_bytes());
        decoy.extend_from_slice(&entries.to_le_bytes());
        decoy.extend_from_slice(&0u32.to_le_bytes());
        decoy.extend_from_slice(&0u32.to_le_bytes());
        decoy.extend_from_slice(&0u16.to_le_bytes());

        let mut patched = bytes[..position].to_vec();
        // Keep the decoy's preceding 20 bytes non-locator-shaped so the test
        // exercises the plain ZIP32 candidate path.
        patched.extend_from_slice(&[0u8; 20]);
        patched.extend_from_slice(&decoy);
        patched.extend_from_slice(&bytes[position..]);
        patched
    }

    fn zip64_eocd_fixture(entries: u64) -> Vec<u8> {
        // Keep the EOCD64 after the minimum possible central-directory end,
        // matching `zip`'s consistency check.
        let padding = (entries * 46 + 8).max(512) as usize;
        let mut bytes = vec![0u8; padding];

        // ZIP64 EOCD record at `padding`.
        bytes.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&44u64.to_le_bytes());
        bytes.extend_from_slice(&45u16.to_le_bytes());
        bytes.extend_from_slice(&45u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());

        // ZIP64 EOCD locator immediately after it.
        bytes.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(padding as u64).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());

        // ZIP32 EOCD with sentinel counts immediately after the locator.
        bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
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
    fn install_rejects_an_earlier_excessive_eocd_behind_an_eof_decoy() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let bytes = insert_decoy_eocd(&package.zip_bytes().unwrap(), u16::MAX);
        let zip = write_temp_zip(&bytes).unwrap();

        // The real EOCD at EOF is valid and `zip` would use it; only the
        // precheck sees the earlier candidate claiming 65535 entries.
        let archive = ZipArchive::new(BufReader::new(File::open(zip.path()).unwrap())).unwrap();
        assert_eq!(archive.len(), 5);

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
    fn install_accepts_trailing_bytes_after_the_eocd() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let mut bytes = package.zip_bytes().unwrap();
        bytes.extend_from_slice(b"trailing bytes accepted by zip");
        let zip = write_temp_zip(&bytes).unwrap();

        let installed = install_from_zip(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap();

        assert!(installed.path.join(TEST_ENTRYPOINT).is_file());
    }

    #[test]
    fn zip64_eocd_entry_count_is_checked_before_opening_the_archive() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("zip64.zip");
        let bytes = zip64_eocd_fixture(513);
        fs::write(&path, &bytes).unwrap();

        let mut file = File::open(&path).unwrap();
        let error = check_zip_entry_count(&mut file, bytes.len() as u64, 512).unwrap_err();

        assert!(matches!(error, AiHostError::LimitExceeded(_)));
    }

    #[test]
    fn zip64_sentinel_counts_use_the_zip64_values() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("zip64.zip");
        let bytes = zip64_eocd_fixture(5);
        fs::write(&path, &bytes).unwrap();

        let mut file = File::open(&path).unwrap();
        check_zip_entry_count(&mut file, bytes.len() as u64, 512).unwrap();
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
    fn install_rejects_an_uppercase_backend_before_the_collision_check() {
        let data_dir = tempdir().unwrap();
        let first = TestPackage::new();
        install(&first, data_dir.path()).unwrap();

        let mut second = TestPackage::new();
        second.manifest_mut().backend = "VULKAN".to_string();
        second.manifest_mut().variant = "linux-VULKAN".to_string();
        let error = install(&second, data_dir.path()).unwrap_err();

        assert!(
            matches!(error, AiHostError::ManifestInvalid(_)),
            "{error:?}"
        );
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

        replace_installation_with(
            staging.path(),
            &target,
            &plugins,
            |from, to| fs::rename(from, to),
            false,
        )
        .unwrap();

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

        let error = replace_installation_with(
            &missing_staging,
            &target,
            &plugins,
            |from, to| fs::rename(from, to),
            false,
        )
        .unwrap_err();

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

        // Same length as the signed entry, so extraction reaches the SHA-256
        // check after the staging directory has been populated.
        replace_entry_data(
            &mut package,
            "catalog.json",
            br#"{"version":"2026-11-03.2"}"#,
        );
        assert_eq!(
            package
                .entries()
                .iter()
                .find(|entry| entry.path == "catalog.json")
                .unwrap()
                .data
                .len(),
            original.len()
        );
        let error = install(&package, data_dir.path()).unwrap_err();
        assert!(
            matches!(&error, AiHostError::Integrity(message) if message.contains("SHA-256")),
            "{error:?}"
        );

        assert!(installed.path.join(TEST_ENTRYPOINT).is_file());
        assert_eq!(
            fs::read(installed.path.join("catalog.json")).unwrap(),
            original
        );
        assert!(staging_entries(data_dir.path()).is_empty());
        assert!(backup_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_restores_the_previous_files_when_the_final_rename_fails() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let installed = install(&package, data_dir.path()).unwrap();
        let old_catalog = fs::read(installed.path.join("catalog.json")).unwrap();
        let old_entrypoint = fs::read(installed.path.join(TEST_ENTRYPOINT)).unwrap();
        let zip = package.temp_zip().unwrap();

        let mut renames = 0;
        let error = install_from_zip_with_limits_using(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
            InstallLimits::default(),
            |from: &Path, to: &Path| {
                renames += 1;
                if renames == 2 {
                    return Err(std::io::Error::other("injected final rename failure"));
                }
                fs::rename(from, to)
            },
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::Io(_)), "{error:?}");
        assert_eq!(
            fs::read(installed.path.join("catalog.json")).unwrap(),
            old_catalog
        );
        assert_eq!(
            fs::read(installed.path.join(TEST_ENTRYPOINT)).unwrap(),
            old_entrypoint
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

    #[test]
    fn install_keeping_backup_can_restore_the_previous_package() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let installed = install(&package, data_dir.path()).unwrap();
        fs::write(installed.path.join("stale.txt"), b"stale").unwrap();

        let zip = package.temp_zip().unwrap();
        let (reinstalled, backup) = crate::install_from_zip_keeping_backup(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap();
        let backup = backup.expect("replacing a package must retain its files");
        assert!(!reinstalled.path.join("stale.txt").exists());
        assert_eq!(fs::read(backup.join("stale.txt")).unwrap(), b"stale");

        crate::restore_installation_backup(&reinstalled.path, &backup).unwrap();
        assert_eq!(
            fs::read(reinstalled.path.join("stale.txt")).unwrap(),
            b"stale"
        );
        assert!(!backup.exists());
        assert!(backup_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_keeping_backup_reports_no_backup_on_a_first_install() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let zip = package.temp_zip().unwrap();

        let (_installed, backup) = crate::install_from_zip_keeping_backup(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap();

        assert!(backup.is_none());
        assert!(backup_entries(data_dir.path()).is_empty());
    }

    #[test]
    fn install_keeping_backup_can_be_discarded_after_commit() {
        let data_dir = tempdir().unwrap();
        let package = TestPackage::new();
        let installed = install(&package, data_dir.path()).unwrap();
        fs::write(installed.path.join("stale.txt"), b"stale").unwrap();

        let zip = package.temp_zip().unwrap();
        let (reinstalled, backup) = crate::install_from_zip_keeping_backup(
            zip.path(),
            data_dir.path(),
            &[package.public_key()],
            "0.10.1",
        )
        .unwrap();
        let backup = backup.expect("replacing a package must retain its files");
        let backup_directory = backup.parent().unwrap().to_path_buf();

        crate::discard_installation_backup(&backup).unwrap();

        assert!(!backup_directory.exists());
        assert!(reinstalled.path.join(TEST_ENTRYPOINT).is_file());
        assert!(!reinstalled.path.join("stale.txt").exists());
        assert!(backup_entries(data_dir.path()).is_empty());
    }
}
