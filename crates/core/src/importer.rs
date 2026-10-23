use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};
use crate::photo_filename::is_source_photo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportIssueKind {
    /// The file could not be imported.
    Failed,
    /// The file reached the destination but its source could not be removed.
    PartialMove,
}

/// A per-file import problem that did not abort the rest of the batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportIssue {
    /// Source path that produced the issue.
    pub source: PathBuf,
    /// Destination path involved in the issue, when one is known.
    pub target: Option<PathBuf>,
    /// Human-readable reason shown in the import report.
    pub message: String,
    pub kind: ImportIssueKind,
    /// Whether importing the same source again may succeed.
    pub retryable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    pub imported: Vec<PathBuf>,
    pub skipped: usize,
    pub issues: Vec<ImportIssue>,
}

/// Copies or moves source photos into a managed library folder.
///
/// Files are written to a temporary name and renamed into place. Name
/// collisions get a numeric suffix; an existing file is never overwritten.
/// Every source is handled independently: a per-file problem is recorded in
/// [`ImportResult::issues`] (or counted as skipped) without aborting the batch.
/// Only a destination-level failure, such as creating the destination folder,
/// returns a hard error.
pub fn import_files(files: &[PathBuf], dest: &Path, mode: ImportMode) -> Result<ImportResult> {
    fs::create_dir_all(dest)?;
    let mut result = ImportResult::default();
    for source in files {
        import_one(source, dest, mode, &mut result);
    }
    Ok(result)
}

fn import_one(source: &Path, dest: &Path, mode: ImportMode, result: &mut ImportResult) {
    if !source.is_file() {
        result.issues.push(failed_issue(
            source,
            None,
            "ファイルが見つかりません".to_string(),
            false,
        ));
        return;
    }
    if !is_source_photo(source) {
        result.skipped += 1;
        return;
    }
    let target = unique_target(dest, source);
    if source == target.as_path() {
        result.skipped += 1;
        return;
    }
    match mode {
        ImportMode::Copy => match copy_atomic(source, &target) {
            Ok(()) => result.imported.push(target),
            Err(error) => result.issues.push(failed_issue(
                source,
                Some(target),
                format!("コピーに失敗しました: {error}"),
                true,
            )),
        },
        ImportMode::Move => move_file(source, target, result),
    }
}

fn move_file(source: &Path, target: PathBuf, result: &mut ImportResult) {
    if fs::rename(source, &target).is_ok() {
        result.imported.push(target);
        return;
    }
    // Cross-device moves cannot be renamed, so fall back to copy + delete.
    if let Err(error) = copy_atomic(source, &target) {
        result.issues.push(failed_issue(
            source,
            Some(target),
            format!("移動に失敗しました: {error}"),
            true,
        ));
        return;
    }
    match fs::remove_file(source) {
        Ok(()) => result.imported.push(target),
        Err(error) => {
            result.issues.push(ImportIssue {
                source: source.to_path_buf(),
                target: Some(target.clone()),
                message: format!(
                    "移動先へのコピーは成功しましたが、元ファイルを削除できませんでした: {error}"
                ),
                kind: ImportIssueKind::PartialMove,
                retryable: false,
            });
            // The copy exists in the destination, so the destination scan can
            // index it even though the original remains behind.
            result.imported.push(target);
        }
    }
}

fn failed_issue(
    source: &Path,
    target: Option<PathBuf>,
    message: String,
    retryable: bool,
) -> ImportIssue {
    ImportIssue {
        source: source.to_path_buf(),
        target,
        message,
        kind: ImportIssueKind::Failed,
        retryable,
    }
}

fn unique_target(dest: &Path, source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("photo");
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png");
    let mut candidate = dest.join(format!("{stem}.{extension}"));
    let mut number = 1;
    while candidate.exists() {
        candidate = dest.join(format!("{stem}-{number}.{extension}"));
        number += 1;
    }
    candidate
}

fn copy_atomic(source: &Path, target: &Path) -> Result<()> {
    let temp = target.with_extension(format!(
        "{}.tmp",
        target
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("png")
    ));
    fs::copy(source, &temp)?;
    let source_len = fs::metadata(source)?.len();
    let copied_len = fs::metadata(&temp)?.len();
    if source_len != copied_len {
        let _ = fs::remove_file(&temp);
        return Err(CoreError::Invalid(format!(
            "copy size mismatch: {} != {}",
            source_len, copied_len
        )));
    }
    if let Err(error) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(CoreError::Io(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use tempfile::tempdir;

    fn write_png(path: &Path) {
        RgbaImage::from_pixel(4, 3, Rgba([10, 20, 30, 255]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn copies_and_renames_collisions() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        let dest = dir.path().join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);

        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();
        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.skipped, 0);
        assert!(result.issues.is_empty());
        assert!(source.exists());
        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();
        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.skipped, 0);
        assert!(result.issues.is_empty());
        assert_ne!(result.imported[0], dest.join(source.file_name().unwrap()));
    }

    #[test]
    fn moves_files_and_skips_non_images() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let text = dir.path().join("note.txt");
        fs::write(&text, "hello").unwrap();
        let dest = dir.path().join("dest");

        let result = import_files(&[source.clone(), text], &dest, ImportMode::Move).unwrap();
        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.skipped, 1);
        assert!(result.issues.is_empty());
        assert!(!source.exists());
        assert!(dest.join(source.file_name().unwrap()).exists());
    }

    #[test]
    fn missing_source_is_reported_without_aborting_the_batch() {
        let dir = tempdir().unwrap();
        let good = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&good);
        let missing = dir.path().join("VRChat_missing.png");
        let dest = dir.path().join("dest");

        let result =
            import_files(&[missing.clone(), good.clone()], &dest, ImportMode::Copy).unwrap();

        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].kind, ImportIssueKind::Failed);
        assert_eq!(result.issues[0].source, missing);
        assert!(!result.issues[0].retryable);
        assert!(dest.join(good.file_name().unwrap()).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn move_reports_partial_move_and_keeps_the_target() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        let dest = dir.path().join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);

        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Move).unwrap();
        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();

        if result.issues.is_empty() {
            // A privileged environment can delete the source even from a
            // read-only directory, so the failure cannot be simulated here.
            return;
        }

        assert!(source.is_file(), "source must remain after a failed delete");
        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].kind, ImportIssueKind::PartialMove);
        assert!(!result.issues[0].retryable);
        assert_eq!(
            result.issues[0].target.as_deref(),
            Some(result.imported[0].as_path())
        );
    }
}
