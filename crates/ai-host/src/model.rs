//! Model file download, offline import, readiness checks, and deletion.
//!
//! Model files live under `<data_dir>/plugins/<plugin_id>/models/<revision>/`
//! and are shared by every installed variant of a plugin. Downloads and
//! imports are verified against the signed manifest before the final file name
//! is created. Readiness checks deliberately inspect only existence and size:
//! hashing a multi-gigabyte model on every startup would be too expensive.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::error::{AiHostError, Result};
use crate::http::ModelHttpClient;
use crate::manifest::{ManifestModelFile, ModelFileRole, PluginManifest, model_file_role};
use crate::sidecar::ModelPaths;

const BUFFER_BYTES: usize = 64 * 1024;

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

/// Checks the installed model files without hashing them.
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

    for (file, path) in [(model, &paths.model), (mmproj, &paths.mmproj)] {
        match fs::metadata(path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return ModelCheck::Invalid(format!("{}: not a regular file", path.display()));
                }
                if metadata.len() != file.size {
                    return ModelCheck::Invalid(format!(
                        "{}: expected {} bytes, found {}",
                        path.display(),
                        file.size,
                        metadata.len()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => {
                return ModelCheck::Invalid(format!("{}: {error}", path.display()));
            }
        }
    }

    if missing {
        ModelCheck::Missing
    } else {
        ModelCheck::Ready(paths)
    }
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
    let (model, mmproj) = model_files(manifest)?;
    let directory = checked_model_dir(manifest, data_dir)?;
    let total = total_size(model, mmproj)?;

    if cancel.load(Ordering::SeqCst) {
        return Err(AiHostError::Cancelled);
    }
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
            download_one(file, part, client, &mut downloaded, total, progress, cancel)?;
        }
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

    Ok(model_paths(&directory, model, mmproj))
}

/// Imports full model files selected by the user.
///
/// Selected files are matched to the manifest by file name. Each file is
/// verified against its declared size and SHA-256 before a hard link (when the
/// file system supports it) or a copy is committed to the model directory.
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

    Ok(model_paths(&directory, model, mmproj))
}

/// Deletes the model directory of the manifest's revision.
///
/// Other revisions and the plugin installation itself are preserved. Deleting
/// an already-missing directory succeeds.
pub fn delete_model(manifest: &PluginManifest, data_dir: &Path) -> Result<()> {
    let directory = checked_model_dir(manifest, data_dir)?;
    match fs::remove_dir_all(&directory) {
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

fn download_one(
    file: &ManifestModelFile,
    part: &Path,
    client: &dyn ModelHttpClient,
    downloaded: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<()> {
    if cancel.load(Ordering::SeqCst) {
        return Err(AiHostError::Cancelled);
    }

    let mut output = create_part(part)?;
    let mut hasher = Sha256::new();
    let mut file_bytes = 0u64;
    let result = {
        let mut on_chunk = |chunk: &[u8]| -> Result<()> {
            if cancel.load(Ordering::SeqCst) {
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

    // A hard link avoids copying multi-gigabyte files when the selection and
    // the data directory live on the same file system. The linked file is
    // still hashed before any file is committed.
    let hard_linked = fs::hard_link(source, part).is_ok();
    let verified = if hard_linked {
        let mut input = File::open(part)?;
        import_stream(&mut input, None, file, imported, total, progress)
    } else {
        let mut input = File::open(source)?;
        let mut output = create_part(part)?;
        let result = import_stream(
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
        result
    };

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
/// A commit failure removes the parts that were not renamed and any finals
/// created by this call. Finals that already existed before the call are left
/// untouched.
fn commit_verified(pairs: [(&Path, &Path); 2]) -> Result<()> {
    let mut created: Vec<&Path> = Vec::new();
    for (index, &(part, destination)) in pairs.iter().enumerate() {
        let existed = fs::symlink_metadata(destination).is_ok();
        if let Err(error) = commit_part(part, destination) {
            for &(remaining, _) in &pairs[index..] {
                let _ = fs::remove_file(remaining);
            }
            for path in created {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
        if !existed {
            created.push(destination);
        }
    }
    Ok(())
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
                cancel_after_chunk: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_chunk_size(mut self, chunk_size: usize) -> Self {
            self.chunk_size = chunk_size;
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
        assert_eq!(progress.last(), Some(&(11, Some(11))));
        assert!(!directory.join(format!("{MODEL_PATH}.part")).exists());
        assert!(!directory.join(format!("{MMPROJ_PATH}.part")).exists());
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
    fn check_model_detects_size_mismatch_without_hashing() {
        let manifest = sample_manifest();
        let data_dir = tempdir().unwrap();
        let directory = model_directory(&manifest, data_dir.path());
        fs::create_dir_all(&directory).unwrap();
        // Correct size, wrong content: readiness must not hash.
        fs::write(directory.join(MODEL_PATH), b"xxxxx").unwrap();
        fs::write(directory.join(MMPROJ_PATH), MMPROJ_BYTES).unwrap();
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
