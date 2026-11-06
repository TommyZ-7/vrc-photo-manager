//! Model file download, offline import, readiness checks, and deletion.
//!
//! Model files live under `<data_dir>/plugins/<plugin_id>/models/<revision>/`
//! and are shared by every installed variant of a plugin. Downloads and
//! imports are verified against the signed manifest before the final file name
//! is created. A verification stamp records the size, mtime, and SHA-256 of
//! both final files, so a startup check only rehashes a multi-gigabyte model
//! when the stamp is missing or no longer matches.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;

use crate::error::{AiHostError, Result};
use crate::http::ModelHttpClient;
use crate::manifest::{ManifestModelFile, ModelFileRole, PluginManifest, model_file_role};
use crate::sidecar::ModelPaths;

const BUFFER_BYTES: usize = 64 * 1024;

/// Version of the on-disk verification stamp schema.
const VERIFICATION_SCHEMA: u32 = 1;

/// A complete model download (both files) must finish within this wall-clock
/// deadline. The per-read HTTP timeout still catches a completely stalled
/// connection; this bounds a server that keeps trickling bytes forever.
pub const DEFAULT_DOWNLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Result of checking whether a plugin's model files are usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCheck {
    /// Both files exist with the declared sizes.
    Ready(ModelPaths),
    /// At least one required model file is absent.
    Missing,
    /// The files exist but cannot be used, or the manifest is not supported.
    Invalid(String),
}

/// Directory holding the model files of one plugin revision:
/// `<data_dir>/plugins/<plugin_id>/models/<revision>`.
pub fn model_dir(data_dir: &Path, plugin_id: &str, revision: &str) -> PathBuf {
    data_dir
        .join("plugins")
        .join(plugin_id)
        .join("models")
        .join(revision)
}

/// Checks the installed model files.
///
/// Existence and size are always checked. The SHA-256 of both files is only
/// re-read when the verification stamp is absent or its size/mtime entries no
/// longer match the files on disk; after a successful rehash the stamp is
/// rewritten atomically.
pub fn check_model(manifest: &PluginManifest, data_dir: &Path) -> ModelCheck {
    let (model, mmproj) = match model_files(manifest) {
        Ok(files) => files,
        Err(error) => return ModelCheck::Invalid(error.to_string()),
    };
    let directory = match checked_model_dir(manifest, data_dir) {
        Ok(directory) => directory,
        Err(error) => return ModelCheck::Invalid(error.to_string()),
    };
    let paths = model_paths(&directory, model, mmproj);
    let mut missing = false;
    let mut metadata = [None, None];

    for (index, (file, path)) in [(model, &paths.model), (mmproj, &paths.mmproj)]
        .into_iter()
        .enumerate()
    {
        match fs::metadata(path) {
            Ok(found) => {
                if !found.is_file() {
                    return ModelCheck::Invalid(format!("{}: not a regular file", path.display()));
                }
                if found.len() != file.size {
                    return ModelCheck::Invalid(format!(
                        "{}: expected {} bytes, found {}",
                        path.display(),
                        file.size,
                        found.len()
                    ));
                }
                metadata[index] = Some(found);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => {
                return ModelCheck::Invalid(format!("{}: {error}", path.display()));
            }
        }
    }

    if missing {
        return ModelCheck::Missing;
    }

    let manifest_hash = manifest_fingerprint(manifest);
    let expected = [
        (model, paths.model.as_path(), metadata[0].as_ref()),
        (mmproj, paths.mmproj.as_path(), metadata[1].as_ref()),
    ];
    if stamp_matches(manifest, &directory, &manifest_hash, &expected) {
        return ModelCheck::Ready(paths);
    }

    for (file, path, _) in expected {
        match hash_file(path) {
            Ok(digest) if digest.eq_ignore_ascii_case(&file.sha256) => {}
            Ok(_) => {
                return ModelCheck::Invalid(format!(
                    "{}: SHA-256 does not match the manifest",
                    path.display()
                ));
            }
            Err(error) => {
                return ModelCheck::Invalid(format!("{}: {error}", path.display()));
            }
        }
    }

    match verification_stamp(manifest, &manifest_hash, model, mmproj, &paths)
        .and_then(|stamp| write_verification_stamp(&directory, manifest, &stamp))
    {
        Ok(()) => {}
        Err(error) => {
            // Failing to persist the stamp must not make a valid model
            // unusable; the next check simply hashes it again.
            tracing::warn!(
                %error,
                directory = %directory.display(),
                "cannot write the AI model verification stamp"
            );
        }
    }

    ModelCheck::Ready(paths)
}

/// Downloads both model files into `data_dir` and verifies size and SHA-256
/// before renaming each file into place.
///
/// Progress is the cumulative number of bytes received across all files.
/// Cancellation is checked before each file and for every received chunk; an
/// interrupted file never reaches its final name.
pub fn download_model(
    manifest: &PluginManifest,
    data_dir: &Path,
    client: &dyn ModelHttpClient,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<ModelPaths> {
    download_model_with_deadline(
        manifest,
        data_dir,
        client,
        progress,
        cancel,
        DEFAULT_DOWNLOAD_DEADLINE,
    )
}

/// [`download_model`] with an explicit total wall-clock deadline.
///
/// The deadline covers both files. A server that keeps sending slowly still
/// fails with [`AiHostError::DownloadTimeout`] once it is reached, and the
/// partial files are removed.
pub fn download_model_with_deadline(
    manifest: &PluginManifest,
    data_dir: &Path,
    client: &dyn ModelHttpClient,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
    deadline: Duration,
) -> Result<ModelPaths> {
    let (model, mmproj) = model_files(manifest)?;
    let directory = checked_model_dir(manifest, data_dir)?;
    let total = total_size(model, mmproj)?;
    let started = Instant::now();
    let deadline = started
        .checked_add(deadline)
        .unwrap_or_else(|| started + Duration::from_secs(24 * 60 * 60));
    let control = DownloadControl {
        cancel,
        deadline,
        started,
    };

    if cancel.load(Ordering::SeqCst) {
        return Err(AiHostError::Cancelled);
    }
    control.ensure_active()?;
    fs::create_dir_all(&directory)?;

    let model_destination = directory.join(&model.path);
    let mmproj_destination = directory.join(&mmproj.path);
    let model_part = part_path(&model_destination);
    let mmproj_part = part_path(&mmproj_destination);

    // Stage and verify both files before committing either one. A failure in
    // the second file must not leave the first file at its final name.
    let mut downloaded = 0u64;
    let staged = (|| -> Result<()> {
        for (file, destination, part) in [
            (model, &model_destination, &model_part),
            (mmproj, &mmproj_destination, &mmproj_part),
        ] {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            download_one(
                file,
                part,
                client,
                &mut downloaded,
                total,
                progress,
                &control,
            )?;
        }
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&model_part);
        let _ = fs::remove_file(&mmproj_part);
        return Err(error);
    }
    if let Err(error) = control.ensure_active() {
        let _ = fs::remove_file(&model_part);
        let _ = fs::remove_file(&mmproj_part);
        return Err(error);
    }

    commit_verified([
        (&model_part, &model_destination),
        (&mmproj_part, &mmproj_destination),
    ])?;

    let paths = model_paths(&directory, model, mmproj);
    record_verified_model(manifest, &directory, model, mmproj, &paths);
    Ok(paths)
}

/// Imports full model files selected by the user.
///
/// Selected files are matched to the manifest by file name. Each file is
/// verified against its declared size and SHA-256 while it is copied to a
/// private part file, and only the verified copy is renamed into place. Using
/// a copy instead of a hard link keeps the source from changing the verified
/// inode after the hash was taken.
pub fn import_model_files(
    manifest: &PluginManifest,
    selected: &[PathBuf],
    data_dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<ModelPaths> {
    let (model, mmproj) = model_files(manifest)?;
    let directory = checked_model_dir(manifest, data_dir)?;
    let total = total_size(model, mmproj)?;

    let model_source = selected_file(selected, &model.path)?;
    let mmproj_source = selected_file(selected, &mmproj.path)?;

    // Fail before touching the model directory if either selection is
    // unusable.
    for (file, source) in [(model, model_source), (mmproj, mmproj_source)] {
        let metadata = fs::metadata(source).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AiHostError::MissingEntry(source.display().to_string())
            } else {
                AiHostError::Io(error)
            }
        })?;
        if !metadata.is_file() {
            return Err(AiHostError::MissingEntry(source.display().to_string()));
        }
        if metadata.len() != file.size {
            return Err(AiHostError::Integrity(format!(
                "{}: expected {} bytes, found {} in {}",
                file.path,
                file.size,
                metadata.len(),
                source.display()
            )));
        }
    }

    fs::create_dir_all(&directory)?;
    let model_destination = directory.join(&model.path);
    let mmproj_destination = directory.join(&mmproj.path);
    let model_part = part_path(&model_destination);
    let mmproj_part = part_path(&mmproj_destination);

    // Stage and verify both files before committing either one. A failure in
    // the second file must not leave the first file at its final name.
    let mut imported = 0u64;
    let staged = (|| -> Result<()> {
        stage_import(
            model,
            model_source,
            &model_destination,
            &model_part,
            &mut imported,
            total,
            progress,
        )?;
        stage_import(
            mmproj,
            mmproj_source,
            &mmproj_destination,
            &mmproj_part,
            &mut imported,
            total,
            progress,
        )?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&model_part);
        let _ = fs::remove_file(&mmproj_part);
        return Err(error);
    }

    commit_verified([
        (&model_part, &model_destination),
        (&mmproj_part, &mmproj_destination),
    ])?;

    let paths = model_paths(&directory, model, mmproj);
    record_verified_model(manifest, &directory, model, mmproj, &paths);
    Ok(paths)
}

/// Deletes the model directory of the manifest's revision.
///
/// Other revisions and the plugin installation itself are preserved. Deleting
/// an already-missing directory succeeds.
pub fn delete_model(manifest: &PluginManifest, data_dir: &Path) -> Result<()> {
    let directory = checked_model_dir(manifest, data_dir)?;
    remove_model_directory(&directory)
}

/// Deletes one model revision without needing an installed manifest.
///
/// Used when only `plugin_state.model_revision` remains, notably after the
/// plugin itself was uninstalled. Both path components are validated so this
/// cannot escape `<data_dir>/plugins/<plugin_id>/models/`.
pub fn delete_model_revision(data_dir: &Path, plugin_id: &str, revision: &str) -> Result<()> {
    validate_component("plugin_id", plugin_id)?;
    validate_component("model.revision", revision)?;
    remove_model_directory(&model_dir(data_dir, plugin_id, revision))
}

fn remove_model_directory(directory: &Path) -> Result<()> {
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn model_files(manifest: &PluginManifest) -> Result<(&ManifestModelFile, &ManifestModelFile)> {
    let mut model = None;
    let mut mmproj = None;

    for file in &manifest.model.files {
        if !file.parts.is_empty() {
            return Err(AiHostError::UnsupportedParts);
        }
        crate::manifest::validate_relative_path("model.files[].path", &file.path)?;
        match model_file_role(file)? {
            ModelFileRole::Model => set_role(&mut model, file, "model")?,
            ModelFileRole::Mmproj => set_role(&mut mmproj, file, "mmproj")?,
        }
    }

    let model = model.ok_or_else(|| {
        AiHostError::ManifestInvalid("manifest declares no model file".to_string())
    })?;
    let mmproj = mmproj.ok_or_else(|| {
        AiHostError::ManifestInvalid("manifest declares no mmproj file".to_string())
    })?;
    Ok((model, mmproj))
}

fn set_role<'a>(
    slot: &mut Option<&'a ManifestModelFile>,
    file: &'a ManifestModelFile,
    role: &str,
) -> Result<()> {
    if slot.replace(file).is_some() {
        return Err(AiHostError::ManifestInvalid(format!(
            "manifest declares more than one {role} file"
        )));
    }
    Ok(())
}

fn checked_model_dir(manifest: &PluginManifest, data_dir: &Path) -> Result<PathBuf> {
    validate_component("plugin_id", &manifest.plugin_id)?;
    validate_component("model.revision", &manifest.model.revision)?;
    Ok(model_dir(
        data_dir,
        &manifest.plugin_id,
        &manifest.model.revision,
    ))
}

/// Rejects a value that could escape the model directory when joined. The
/// manifest installer performs the same checks; this repeats them because
/// these functions accept a manifest directly.
fn validate_component(field: &str, value: &str) -> Result<()> {
    if value.contains('/') {
        return Err(AiHostError::UnsafePath(format!("{field}: {value}")));
    }
    crate::manifest::validate_relative_path(field, value)
}

fn total_size(model: &ManifestModelFile, mmproj: &ManifestModelFile) -> Result<u64> {
    model
        .size
        .checked_add(mmproj.size)
        .ok_or_else(|| AiHostError::Integrity("model size overflows u64".to_string()))
}

fn model_paths(
    directory: &Path,
    model: &ManifestModelFile,
    mmproj: &ManifestModelFile,
) -> ModelPaths {
    ModelPaths {
        model: directory.join(&model.path),
        mmproj: directory.join(&mmproj.path),
    }
}

fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    destination.with_file_name(name)
}

/// Cancellation and total wall-clock deadline shared by both files.
struct DownloadControl<'a> {
    cancel: &'a AtomicBool,
    deadline: Instant,
    started: Instant,
}

impl DownloadControl<'_> {
    fn ensure_active(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            Err(AiHostError::DownloadTimeout(self.started.elapsed()))
        } else {
            Ok(())
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// On-disk record that both final model files were hashed successfully.
///
/// `manifest` is a fingerprint of the model section of the signed manifest;
/// a different manifest (even for the same revision directory) cannot reuse
/// this stamp. `mtime` entries let [`check_model`] trust a file only when it
/// is byte-for-byte the one that was hashed, without reading gigabytes again.
#[derive(Debug, Serialize, Deserialize)]
struct VerificationStamp {
    schema: u32,
    revision: String,
    manifest: String,
    files: Vec<VerifiedFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct VerifiedFile {
    path: String,
    size: u64,
    sha256: String,
    mtime_secs: i64,
    mtime_nanos: u32,
}

type ExpectedModelFile<'a> = (
    &'a ManifestModelFile,
    &'a Path,
    Option<&'a std::fs::Metadata>,
);

fn verification_stamp_path(directory: &Path, revision: &str) -> PathBuf {
    directory.join(format!(".verified-{revision}.json"))
}

fn manifest_fingerprint(manifest: &PluginManifest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(manifest.plugin_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(manifest.model.revision.as_bytes());
    hasher.update([0u8]);
    match serde_json::to_vec(&manifest.model) {
        Ok(model) => hasher.update(model),
        Err(_) => hasher.update(b"unserializable-model"),
    }
    hex::encode(hasher.finalize())
}

fn stamp_matches(
    manifest: &PluginManifest,
    directory: &Path,
    manifest_hash: &str,
    expected: &[ExpectedModelFile<'_>; 2],
) -> bool {
    let stamp_path = verification_stamp_path(directory, &manifest.model.revision);
    let Ok(bytes) = fs::read(&stamp_path) else {
        return false;
    };
    let Ok(stamp) = serde_json::from_slice::<VerificationStamp>(&bytes) else {
        return false;
    };
    if stamp.schema != VERIFICATION_SCHEMA
        || stamp.revision != manifest.model.revision
        || stamp.manifest != manifest_hash
        || stamp.files.len() != expected.len()
    {
        return false;
    }

    expected.iter().all(|(file, _, metadata)| {
        let Some(metadata) = metadata else {
            return false;
        };
        let Some(entry) = stamp.files.iter().find(|entry| entry.path == file.path) else {
            return false;
        };
        if entry.size != file.size || !entry.sha256.eq_ignore_ascii_case(&file.sha256) {
            return false;
        }
        match metadata.modified().ok().map(mtime_parts) {
            Some((secs, nanos)) => entry.mtime_secs == secs && entry.mtime_nanos == nanos,
            None => false,
        }
    })
}

fn verification_stamp(
    manifest: &PluginManifest,
    manifest_hash: &str,
    model: &ManifestModelFile,
    mmproj: &ManifestModelFile,
    paths: &ModelPaths,
) -> Result<VerificationStamp> {
    let mut files = Vec::with_capacity(2);
    for (file, path) in [(model, &paths.model), (mmproj, &paths.mmproj)] {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() != file.size {
            return Err(AiHostError::Integrity(format!(
                "{}: expected {} bytes, found {}",
                path.display(),
                file.size,
                metadata.len()
            )));
        }
        let (mtime_secs, mtime_nanos) = metadata
            .modified()
            .map(mtime_parts)
            .map_err(AiHostError::Io)?;
        files.push(VerifiedFile {
            path: file.path.clone(),
            size: file.size,
            sha256: file.sha256.to_ascii_lowercase(),
            mtime_secs,
            mtime_nanos,
        });
    }

    Ok(VerificationStamp {
        schema: VERIFICATION_SCHEMA,
        revision: manifest.model.revision.clone(),
        manifest: manifest_hash.to_string(),
        files,
    })
}

fn write_verification_stamp(
    directory: &Path,
    manifest: &PluginManifest,
    stamp: &VerificationStamp,
) -> Result<()> {
    let bytes = serde_json::to_vec(stamp)?;
    let mut temp = Builder::new()
        .prefix(".verified-")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    temp.write_all(&bytes)?;
    temp.flush()?;
    let destination = verification_stamp_path(directory, &manifest.model.revision);
    #[cfg(windows)]
    if destination.exists() {
        // Windows `rename` does not replace an existing file.
        let _ = fs::remove_file(&destination);
    }
    temp.persist(&destination)
        .map_err(|error| AiHostError::Io(error.error))?;
    Ok(())
}

/// Best-effort stamp refresh after a download or import already verified the
/// bytes. A failure only costs one extra hash on the next check.
fn record_verified_model(
    manifest: &PluginManifest,
    directory: &Path,
    model: &ManifestModelFile,
    mmproj: &ManifestModelFile,
    paths: &ModelPaths,
) {
    let manifest_hash = manifest_fingerprint(manifest);
    let result = verification_stamp(manifest, &manifest_hash, model, mmproj, paths)
        .and_then(|stamp| write_verification_stamp(directory, manifest, &stamp));
    if let Err(error) = result {
        tracing::warn!(
            %error,
            directory = %directory.display(),
            "cannot write the AI model verification stamp"
        );
    }
}

fn mtime_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos()),
        Err(error) => {
            let duration = error.duration();
            (-(duration.as_secs() as i64), duration.subsec_nanos())
        }
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn download_one(
    file: &ManifestModelFile,
    part: &Path,
    client: &dyn ModelHttpClient,
    downloaded: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
    control: &DownloadControl<'_>,
) -> Result<()> {
    control.ensure_active()?;
    if control.is_cancelled() {
        return Err(AiHostError::Cancelled);
    }

    let mut output = create_part(part)?;
    let mut hasher = Sha256::new();
    let mut file_bytes = 0u64;
    let result = {
        let mut on_chunk = |chunk: &[u8]| -> Result<()> {
            control.ensure_active()?;
            if control.is_cancelled() {
                return Err(AiHostError::Cancelled);
            }
            file_bytes = file_bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| AiHostError::Integrity("model size overflows u64".to_string()))?;
            if file_bytes > file.size {
                return Err(AiHostError::Integrity(format!(
                    "{}: download exceeds the declared {} bytes",
                    file.path, file.size
                )));
            }
            hasher.update(chunk);
            output.write_all(chunk)?;
            *downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| AiHostError::Integrity("progress overflows u64".to_string()))?;
            progress(*downloaded, Some(total));
            Ok(())
        };
        client.download(&file.url, &mut on_chunk)
    };
    output.flush()?;
    drop(output);
    result?;

    if file_bytes != file.size {
        return Err(AiHostError::Integrity(format!(
            "{}: expected {} bytes, downloaded {file_bytes}",
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
    Ok(())
}

fn stage_import(
    file: &ManifestModelFile,
    source: &Path,
    destination: &Path,
    part: &Path,
    imported: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    remove_file_if_exists(part)?;

    // Always copy into the private part file: the copied bytes are what get
    // hashed and renamed into place, so changing the selected source after the
    // hash cannot alter the installed model.
    let mut input = File::open(source)?;
    let mut output = create_part(part)?;
    let verified = import_stream(
        &mut input,
        Some(&mut output),
        file,
        imported,
        total,
        progress,
    )
    .and_then(|()| {
        output.flush()?;
        Ok(())
    });
    drop(output);

    if let Err(error) = verified {
        let _ = fs::remove_file(part);
        return Err(error);
    }
    Ok(())
}

fn import_stream(
    input: &mut File,
    mut output: Option<&mut File>,
    file: &ManifestModelFile,
    imported: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; BUFFER_BYTES];

    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| AiHostError::Integrity("model size overflows u64".to_string()))?;
        if bytes > file.size {
            return Err(AiHostError::Integrity(format!(
                "{}: selected file exceeds the declared {} bytes",
                file.path, file.size
            )));
        }
        hasher.update(&buffer[..read]);
        if let Some(output) = output.as_deref_mut() {
            output.write_all(&buffer[..read])?;
        }
        *imported = imported
            .checked_add(read as u64)
            .ok_or_else(|| AiHostError::Integrity("progress overflows u64".to_string()))?;
        progress(*imported, Some(total));
    }

    if bytes != file.size {
        return Err(AiHostError::Integrity(format!(
            "{}: expected {} bytes, read {bytes}",
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
    Ok(())
}

fn selected_file<'a>(selected: &'a [PathBuf], manifest_path: &str) -> Result<&'a Path> {
    let Some(wanted) = Path::new(manifest_path).file_name() else {
        return Err(AiHostError::ManifestInvalid(format!(
            "model path has no file name: {manifest_path}"
        )));
    };

    let mut found = None;
    for path in selected {
        if path.file_name() != Some(wanted) {
            continue;
        }
        if found.is_some() {
            return Err(AiHostError::ManifestInvalid(format!(
                "more than one selected file is named {}",
                wanted.to_string_lossy()
            )));
        }
        found = Some(path.as_path());
    }

    found.ok_or_else(|| AiHostError::MissingEntry(manifest_path.to_string()))
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Creates a fresh part file without following an existing symlink.
fn create_part(path: &Path) -> Result<File> {
    remove_file_if_exists(path)?;
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?)
}

/// Commits verified part files into their final names.
///
/// Both existing finals are moved aside before either part is committed. A
/// failure after the first rename restores the old pair and removes every
/// final created by this call, so a partial commit can never replace a
/// previously installed model.
fn commit_verified(pairs: [(&Path, &Path); 2]) -> Result<()> {
    commit_verified_with(pairs, commit_part)
}

fn commit_verified_with<F>(pairs: [(&Path, &Path); 2], mut commit: F) -> Result<()>
where
    F: FnMut(&Path, &Path) -> Result<()>,
{
    let mut backups: Vec<Option<(tempfile::TempDir, PathBuf)>> = Vec::with_capacity(pairs.len());

    // Back up every existing final before touching the staged files.
    for &(_, destination) in &pairs {
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                let parent = destination.parent().unwrap_or_else(|| Path::new("."));
                let backup_dir = match Builder::new().prefix(".backup-").tempdir_in(parent) {
                    Ok(directory) => directory,
                    Err(error) => {
                        remove_parts(&pairs);
                        restore_backups(pairs, backups);
                        return Err(error.into());
                    }
                };
                let backup_path = backup_dir.path().join("previous");
                match fs::rename(destination, &backup_path) {
                    Ok(()) => backups.push(Some((backup_dir, backup_path))),
                    Err(error) => {
                        remove_parts(&pairs);
                        restore_backups(pairs, backups);
                        return Err(error.into());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => backups.push(None),
            Err(error) => {
                remove_parts(&pairs);
                restore_backups(pairs, backups);
                return Err(error.into());
            }
        }
    }

    for &(part, destination) in &pairs {
        if let Err(error) = commit(part, destination) {
            // Every pre-existing final is in a backup directory by now, so
            // anything at a destination belongs to this failed operation.
            for &(_, path) in &pairs {
                let _ = fs::remove_file(path);
            }
            remove_parts(&pairs);
            restore_backups(pairs, backups);
            return Err(error);
        }
    }

    // Success: dropping the backup directories removes the old files.
    drop(backups);
    Ok(())
}

fn remove_parts(pairs: &[(&Path, &Path); 2]) {
    for &(part, _) in pairs {
        let _ = fs::remove_file(part);
    }
}

fn restore_backups(pairs: [(&Path, &Path); 2], backups: Vec<Option<(tempfile::TempDir, PathBuf)>>) {
    for (&(_, destination), backup) in pairs.iter().zip(backups).rev() {
        let Some((directory, backup_path)) = backup else {
            continue;
        };
        match fs::rename(&backup_path, destination) {
            Ok(()) => drop(directory),
            Err(error) => {
                let kept = directory.keep();
                tracing::error!(
                    backup = %kept.display(),
                    destination = %destination.display(),
                    error = %error,
                    "failed to restore the previous model file"
                );
            }
        }
    }
}

/// Renames a verified part file into place. On Windows an existing file must
/// be removed first because `rename` does not replace it.
fn commit_part(part: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    if destination.is_file() {
        fs::remove_file(destination)?;
    }
    fs::rename(part, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;
    use crate::error::AiHostError;
    use crate::http::ModelHttpClient;
    use crate::manifest::{ManifestPart, ModelFileRole, PluginManifest};
    use crate::test_support::sample_manifest;

    const MODEL_PATH: &str = "d1-2B-Q8_0.gguf";
    const MMPROJ_PATH: &str = "mmproj-d1-2B-Q8_0.gguf";
    const MODEL_BYTES: &[u8] = b"model";
    const MMPROJ_BYTES: &[u8] = b"mmproj";

    struct FakeClient {
        files: HashMap<String, Vec<u8>>,
        chunk_size: usize,
        chunk_delay: Duration,
        cancel_after_chunk: Option<(Arc<AtomicBool>, usize)>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeClient {
        fn new(manifest: &PluginManifest, model: &[u8], mmproj: &[u8]) -> Self {
            let files = manifest
                .model
                .files
                .iter()
                .map(|file| {
                    let bytes = match model_file_role(file).unwrap() {
                        ModelFileRole::Model => model.to_vec(),
                        ModelFileRole::Mmproj => mmproj.to_vec(),
                    };
                    (file.url.clone(), bytes)
                })
                .collect();
            Self {
                files,
                chunk_size: 64 * 1024,
                chunk_delay: Duration::ZERO,
                cancel_after_chunk: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_chunk_size(mut self, chunk_size: usize) -> Self {
            self.chunk_size = chunk_size;
            self
        }

        fn with_chunk_delay(mut self, delay: Duration) -> Self {
            self.chunk_delay = delay;
            self
        }

        fn cancel_after_chunk(mut self, chunk: usize, cancel: Arc<AtomicBool>) -> Self {
            self.cancel_after_chunk = Some((cancel, chunk));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ModelHttpClient for FakeClient {
        fn download(&self, url: &str, on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
            self.calls.lock().unwrap().push(url.to_string());
            let bytes = self.files.get(url).expect("fake data for every model URL");
            for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
                if let Some((cancel, after)) = &self.cancel_after_chunk
                    && index == *after
                {
                    cancel.store(true, Ordering::SeqCst);
                }
                if !self.chunk_delay.is_zero() {
                    std::thread::sleep(self.chunk_delay);
                }
                on_chunk(chunk)?;
            }
            Ok(())
        }
    }

    fn write_sources(directory: &Path) -> (PathBuf, PathBuf) {
        let model = directory.join(MODEL_PATH);
        let mmproj = directory.join(MMPROJ_PATH);
        fs::write(&model, MODEL_BYTES).unwrap();
        fs::write(&mmproj, MMPROJ_BYTES).unwrap();
        (model, mmproj)
    }

    fn model_directory(manifest: &PluginManifest, data_dir: &Path) -> PathBuf {
        model_dir(data_dir, &manifest.plugin_id, &manifest.model.revision)
    }

    #[test]
    fn model_dir_follows_the_documented_layout() {
        assert_eq!(
            model_dir(Path::new("/data"), "vrc-photon-autotag", "rev"),
            Path::new("/data/plugins/vrc-photon-autotag/models/rev")
        );
    }

    #[test]
    fn download_writes_both_files_and_reports_cumulative_progress() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let client = FakeClient::new(&manifest, MODEL_BYTES, MMPROJ_BYTES).with_chunk_size(2);
        let mut progress = Vec::new();

        let paths = download_model(
            &manifest,
            data_dir.path(),
            &client,
            &mut |bytes, total| progress.push((bytes, total)),
            &AtomicBool::new(false),
        )
        .unwrap();

        let directory = model_directory(&manifest, data_dir.path());
        assert_eq!(paths.model, directory.join(MODEL_PATH));
        assert_eq!(paths.mmproj, directory.join(MMPROJ_PATH));
        assert_eq!(fs::read(&paths.model).unwrap(), MODEL_BYTES);
        assert_eq!(fs::read(&paths.mmproj).unwrap(), MMPROJ_BYTES);
        assert!(
            progress.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "progress must be cumulative: {progress:?}"
        );
        assert_eq!(progress.last(), Some(&(11, Some(11))));
        assert_eq!(
            client.calls(),
            vec![
                manifest.model.files[0].url.clone(),
                manifest.model.files[1].url.clone()
            ]
        );
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
        assert!(!directory.join(format!("{MMPROJ_PATH}.part")).exists());
        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(found) if found == paths
        ));
    }

    #[test]
    fn download_verifies_size_and_sha256_before_rename() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        // The model is good; the mmproj has the declared size but wrong
        // content. Neither final may be committed until both are verified.
        let client = FakeClient::new(&manifest, MODEL_BYTES, b"xxxxxx");

        let error = download_model(
            &manifest,
            data_dir.path(),
            &client,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::Integrity(_)));
        let directory = model_directory(&manifest, data_dir.path());
        assert!(!directory.join(MODEL_PATH).exists());
        assert!(!directory.join(MMPROJ_PATH).exists());
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
        assert!(!directory.join(format!("{MMPROJ_PATH}.part")).exists());
    }

    #[test]
    fn download_rejects_a_size_mismatch_and_removes_the_partial_file() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let client = FakeClient::new(&manifest, b"mod", MMPROJ_BYTES);

        let error = download_model(
            &manifest,
            data_dir.path(),
            &client,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::Integrity(_)));
        let directory = model_directory(&manifest, data_dir.path());
        assert!(!directory.join(MODEL_PATH).exists());
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
    }

    #[test]
    fn cancel_stops_download_and_removes_the_partial_file() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let client = FakeClient::new(&manifest, MODEL_BYTES, MMPROJ_BYTES)
            .with_chunk_size(2)
            .cancel_after_chunk(0, Arc::clone(&cancel));

        let error = download_model(&manifest, data_dir.path(), &client, &mut |_, _| {}, &cancel)
            .unwrap_err();

        assert!(matches!(error, AiHostError::Cancelled));
        let directory = model_directory(&manifest, data_dir.path());
        assert!(!directory.join(MODEL_PATH).exists());
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
    }

    #[test]
    fn cancel_before_start_does_not_call_the_client() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let client = FakeClient::new(&manifest, MODEL_BYTES, MMPROJ_BYTES);

        let error = download_model(
            &manifest,
            data_dir.path(),
            &client,
            &mut |_, _| {},
            &AtomicBool::new(true),
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::Cancelled));
        assert!(client.calls().is_empty());
        assert!(!model_directory(&manifest, data_dir.path()).exists());
    }

    #[test]
    fn download_rejects_manifests_with_parts() {
        let mut manifest = sample_manifest();
        manifest.model.files[0].parts.push(ManifestPart {
            path: "d1-2B-Q8_0.gguf.part1".to_string(),
            size: 1,
            sha256: "0".repeat(64),
        });
        let data_dir = tempdir().unwrap();
        let client = FakeClient::new(&manifest, MODEL_BYTES, MMPROJ_BYTES);

        let error = download_model(
            &manifest,
            data_dir.path(),
            &client,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::UnsupportedParts));
        assert!(!model_directory(&manifest, data_dir.path()).exists());
    }

    #[test]
    fn import_accepts_full_files_and_reports_cumulative_progress() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let sources = tempdir().unwrap();
        let (model, mmproj) = write_sources(sources.path());
        let mut progress = Vec::new();

        let paths = import_model_files(
            &manifest,
            &[mmproj.clone(), model.clone()],
            data_dir.path(),
            &mut |bytes, total| progress.push((bytes, total)),
        )
        .unwrap();

        let directory = model_directory(&manifest, data_dir.path());
        assert_eq!(paths.model, directory.join(MODEL_PATH));
        assert_eq!(paths.mmproj, directory.join(MMPROJ_PATH));
        assert_eq!(fs::read(&paths.model).unwrap(), MODEL_BYTES);
        assert_eq!(fs::read(&paths.mmproj).unwrap(), MMPROJ_BYTES);
        // The installed copy must not share an inode with the user's source.
        fs::write(&model, b"changed").unwrap();
        assert_eq!(fs::read(&paths.model).unwrap(), MODEL_BYTES);
        assert_eq!(progress.last(), Some(&(11, Some(11))));
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
        assert!(!directory.join(format!("{MMPROJ_PATH}.part")).exists());
        assert!(
            verification_stamp_path(&directory, &manifest.model.revision).is_file(),
            "an import must write the verification stamp"
        );
    }

    #[test]
    fn import_rejects_wrong_hash_and_leaves_no_file() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let sources = tempdir().unwrap();
        let (model, mmproj) = write_sources(sources.path());
        // Same size as declared, different content.
        fs::write(&mmproj, b"xxxxxx").unwrap();

        let error =
            import_model_files(&manifest, &[model, mmproj], data_dir.path(), &mut |_, _| {})
                .unwrap_err();

        assert!(matches!(error, AiHostError::Integrity(_)));
        let directory = model_directory(&manifest, data_dir.path());
        assert!(!directory.join(MODEL_PATH).exists());
        assert!(!directory.join(MMPROJ_PATH).exists());
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
        assert!(!directory.join(format!("{MMPROJ_PATH}.part")).exists());
    }

    #[test]
    fn import_rejects_wrong_size_before_writing() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let sources = tempdir().unwrap();
        let (model, mmproj) = write_sources(sources.path());
        fs::write(&model, b"mod").unwrap();

        let error =
            import_model_files(&manifest, &[model, mmproj], data_dir.path(), &mut |_, _| {})
                .unwrap_err();

        assert!(matches!(error, AiHostError::Integrity(_)));
        assert!(!model_directory(&manifest, data_dir.path()).exists());
    }

    #[test]
    fn import_rejects_missing_mmproj() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let sources = tempdir().unwrap();
        let (model, _) = write_sources(sources.path());

        let error =
            import_model_files(&manifest, &[model], data_dir.path(), &mut |_, _| {}).unwrap_err();

        assert!(matches!(error, AiHostError::MissingEntry(_)));
        assert!(error.to_string().contains(MMPROJ_PATH));
        assert!(!model_directory(&manifest, data_dir.path()).exists());
    }

    #[test]
    fn import_rejects_parts() {
        let mut manifest = sample_manifest();
        manifest.model.files[1].parts.push(ManifestPart {
            path: "mmproj.part1".to_string(),
            size: 1,
            sha256: "0".repeat(64),
        });
        let data_dir = tempdir().unwrap();
        let sources = tempdir().unwrap();
        let (model, mmproj) = write_sources(sources.path());

        let error =
            import_model_files(&manifest, &[model, mmproj], data_dir.path(), &mut |_, _| {})
                .unwrap_err();

        assert!(matches!(error, AiHostError::UnsupportedParts));
        assert!(!model_directory(&manifest, data_dir.path()).exists());
    }

    #[test]
    fn commit_replaces_existing_finals_and_removes_backups() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        let model_destination = directory.join(MODEL_PATH);
        let mmproj_destination = directory.join(MMPROJ_PATH);
        fs::write(&model_destination, b"old model").unwrap();
        fs::write(&mmproj_destination, b"old mmproj").unwrap();
        let model_part = part_path(&model_destination);
        let mmproj_part = part_path(&mmproj_destination);
        fs::write(&model_part, MODEL_BYTES).unwrap();
        fs::write(&mmproj_part, MMPROJ_BYTES).unwrap();

        commit_verified([
            (&model_part, &model_destination),
            (&mmproj_part, &mmproj_destination),
        ])
        .unwrap();

        assert_eq!(fs::read(&model_destination).unwrap(), MODEL_BYTES);
        assert_eq!(fs::read(&mmproj_destination).unwrap(), MMPROJ_BYTES);
        assert!(!model_part.exists());
        assert!(!mmproj_part.exists());
        assert!(backup_entries(&directory).is_empty());
    }

    #[test]
    fn commit_restores_both_existing_finals_when_the_second_commit_fails() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        let model_destination = directory.join(MODEL_PATH);
        let mmproj_destination = directory.join(MMPROJ_PATH);
        fs::write(&model_destination, b"old model").unwrap();
        fs::write(&mmproj_destination, b"old mmproj").unwrap();
        let model_part = part_path(&model_destination);
        let mmproj_part = part_path(&mmproj_destination);
        fs::write(&model_part, MODEL_BYTES).unwrap();
        fs::write(&mmproj_part, MMPROJ_BYTES).unwrap();

        let mut commits = 0;
        let error = commit_verified_with(
            [
                (&model_part, &model_destination),
                (&mmproj_part, &mmproj_destination),
            ],
            |part, destination| {
                commits += 1;
                if commits == 2 {
                    return Err(AiHostError::Io(std::io::Error::other(
                        "injected second commit failure",
                    )));
                }
                fs::rename(part, destination).map_err(AiHostError::from)
            },
        )
        .unwrap_err();

        assert!(matches!(error, AiHostError::Io(_)), "{error:?}");
        assert_eq!(fs::read(&model_destination).unwrap(), b"old model");
        assert_eq!(fs::read(&mmproj_destination).unwrap(), b"old mmproj");
        assert!(!model_part.exists());
        assert!(!mmproj_part.exists());
        assert!(backup_entries(&directory).is_empty());
    }

    #[test]
    fn backup_entries_are_hidden_siblings_in_the_model_directory() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        assert!(backup_entries(&directory).is_empty());
        fs::create_dir_all(directory.join(".backup-test")).unwrap();
        assert_eq!(backup_entries(&directory).len(), 1);
    }

    fn backup_entries(directory: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".backup-"))
            })
            .collect()
    }

    #[test]
    fn check_model_reports_missing_until_both_files_exist() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());

        assert_eq!(check_model(&manifest, data_dir.path()), ModelCheck::Missing);

        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(MODEL_PATH), MODEL_BYTES).unwrap();
        assert_eq!(check_model(&manifest, data_dir.path()), ModelCheck::Missing);

        fs::write(directory.join(MMPROJ_PATH), MMPROJ_BYTES).unwrap();
        match check_model(&manifest, data_dir.path()) {
            ModelCheck::Ready(paths) => {
                assert_eq!(paths.model, directory.join(MODEL_PATH));
                assert_eq!(paths.mmproj, directory.join(MMPROJ_PATH));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn check_model_rejects_wrong_content_and_size() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        // Correct size, wrong content: the first check has no stamp, so the
        // file is hashed and the mismatch is caught.
        fs::write(directory.join(MODEL_PATH), b"xxxxx").unwrap();
        fs::write(directory.join(MMPROJ_PATH), MMPROJ_BYTES).unwrap();
        match check_model(&manifest, data_dir.path()) {
            ModelCheck::Invalid(message) => {
                assert!(
                    message.contains(MODEL_PATH) && message.contains("SHA-256"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }

        fs::write(directory.join(MODEL_PATH), MODEL_BYTES).unwrap();
        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(_)
        ));

        fs::write(directory.join(MMPROJ_PATH), b"short").unwrap();
        match check_model(&manifest, data_dir.path()) {
            ModelCheck::Invalid(message) => {
                assert!(
                    message.contains(MMPROJ_PATH),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn check_model_detects_tampering_after_a_verified_stamp() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        let model = directory.join(MODEL_PATH);
        let mmproj = directory.join(MMPROJ_PATH);
        fs::write(&model, MODEL_BYTES).unwrap();
        fs::write(&mmproj, MMPROJ_BYTES).unwrap();

        // The first successful check creates the stamp.
        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(_)
        ));
        assert!(
            verification_stamp_path(&directory, &manifest.model.revision).is_file(),
            "a successful hash must write the verification stamp"
        );

        // Same-size content change: mtime no longer matches, so the file is
        // rehashed and the mismatch is reported.
        fs::write(&model, b"xxxxx").unwrap();
        match check_model(&manifest, data_dir.path()) {
            ModelCheck::Invalid(message) => {
                assert!(
                    message.contains(MODEL_PATH) && message.contains("SHA-256"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }

        // A size change is still caught by the cheap metadata check.
        fs::write(&model, b"too long").unwrap();
        match check_model(&manifest, data_dir.path()) {
            ModelCheck::Invalid(message) => {
                assert!(message.contains("expected 5 bytes"), "{message}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn matching_stamp_is_trusted_without_rehashing() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        let model = directory.join(MODEL_PATH);
        let mmproj = directory.join(MMPROJ_PATH);
        fs::write(&model, MODEL_BYTES).unwrap();
        fs::write(&mmproj, MMPROJ_BYTES).unwrap();
        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(_)
        ));

        // Corrupt the content but restore the recorded mtime. The stamp must
        // be trusted solely on size + mtime, so this check does not rehash.
        let modified = fs::metadata(&model).unwrap().modified().unwrap();
        fs::write(&model, b"xxxxx").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&model).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);

        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(_)
        ));
    }

    #[test]
    fn stamp_from_a_different_manifest_is_not_trusted() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        let model = directory.join(MODEL_PATH);
        let mmproj = directory.join(MMPROJ_PATH);
        fs::write(&model, MODEL_BYTES).unwrap();
        fs::write(&mmproj, MMPROJ_BYTES).unwrap();
        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(_)
        ));

        // Same revision directory, different signed manifest: the stamp must
        // not hide the changed hash.
        let mut changed = manifest.clone();
        changed.model.files[0].sha256 = "00".repeat(32);
        match check_model(&changed, data_dir.path()) {
            ModelCheck::Invalid(message) => {
                assert!(
                    message.contains(MODEL_PATH) && message.contains("SHA-256"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn download_global_deadline_fails_and_removes_the_partial_file() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        // The client keeps sending one byte at a time; the total deadline is
        // shorter than the time needed to finish.
        let client = FakeClient::new(&manifest, MODEL_BYTES, MMPROJ_BYTES)
            .with_chunk_size(1)
            .with_chunk_delay(Duration::from_millis(20));

        let error = download_model_with_deadline(
            &manifest,
            data_dir.path(),
            &client,
            &mut |_, _| {},
            &AtomicBool::new(false),
            Duration::from_millis(40),
        )
        .unwrap_err();

        assert!(
            matches!(error, AiHostError::DownloadTimeout(_)),
            "{error:?}"
        );
        let directory = model_directory(&manifest, data_dir.path());
        assert!(!directory.join(MODEL_PATH).exists());
        assert!(!directory.join(MMPROJ_PATH).exists());
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
        assert!(!directory.join(format!("{MMPROJ_PATH}.part")).exists());
    }

    #[test]
    fn download_and_import_write_a_verification_stamp() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let client = FakeClient::new(&manifest, MODEL_BYTES, MMPROJ_BYTES);
        download_model(
            &manifest,
            data_dir.path(),
            &client,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        let stamp = verification_stamp_path(&directory, &manifest.model.revision);
        assert!(stamp.is_file());
        let parsed: VerificationStamp = serde_json::from_slice(&fs::read(&stamp).unwrap()).unwrap();
        assert_eq!(parsed.schema, VERIFICATION_SCHEMA);
        assert_eq!(parsed.revision, manifest.model.revision);
        assert_eq!(parsed.files.len(), 2);

        // A later check reuses it instead of hashing again.
        assert!(matches!(
            check_model(&manifest, data_dir.path()),
            ModelCheck::Ready(_)
        ));
    }

    #[test]
    fn check_model_rejects_parts() {
        let mut manifest = sample_manifest();
        manifest.model.files[0].parts.push(ManifestPart {
            path: "d1-2B-Q8_0.gguf.part1".to_string(),
            size: 1,
            sha256: "0".repeat(64),
        });

        assert!(matches!(
            check_model(&manifest, Path::new("/data")),
            ModelCheck::Invalid(_)
        ));
    }

    #[test]
    fn delete_model_rejects_unsafe_manifest_components() {
        let mut manifest = sample_manifest();
        manifest.plugin_id = "../escape".to_string();
        let data_dir = tempdir().unwrap();

        let error = delete_model(&manifest, data_dir.path()).unwrap_err();

        assert!(matches!(error, AiHostError::UnsafePath(_)));
    }

    #[test]
    fn delete_model_revision_removes_an_explicit_revision_without_a_manifest() {
        let data_dir = tempdir().unwrap();
        let revision = "5ebcce3bab1c0521f2cab68742a647c7d038adbe";
        let directory = model_dir(data_dir.path(), "vrc-photon-autotag", revision);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(MODEL_PATH), MODEL_BYTES).unwrap();

        let other = model_dir(data_dir.path(), "vrc-photon-autotag", "other-revision");
        fs::create_dir_all(&other).unwrap();

        delete_model_revision(data_dir.path(), "vrc-photon-autotag", revision).unwrap();

        assert!(!directory.exists());
        assert!(other.exists());
        // Deleting an already-missing revision is not an error.
        delete_model_revision(data_dir.path(), "vrc-photon-autotag", revision).unwrap();

        let error = delete_model_revision(data_dir.path(), "../escape", revision).unwrap_err();
        assert!(matches!(error, AiHostError::UnsafePath(_)));
    }

    #[test]
    fn delete_model_removes_only_its_dir() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(MODEL_PATH), MODEL_BYTES).unwrap();

        let other_revision = model_dir(data_dir.path(), &manifest.plugin_id, "other-revision");
        fs::create_dir_all(&other_revision).unwrap();
        fs::write(other_revision.join("keep.gguf"), b"keep").unwrap();

        let plugin_install = data_dir
            .path()
            .join("plugins")
            .join(&manifest.plugin_id)
            .join("linux-vulkan")
            .join("1.0.0");
        fs::create_dir_all(&plugin_install).unwrap();
        fs::write(plugin_install.join("d1-sidecar"), b"binary").unwrap();

        delete_model(&manifest, data_dir.path()).unwrap();

        assert!(!directory.exists());
        assert!(other_revision.join("keep.gguf").is_file());
        assert!(plugin_install.join("d1-sidecar").is_file());
        // Deleting an already-missing model directory is not an error.
        delete_model(&manifest, data_dir.path()).unwrap();
    }
}
