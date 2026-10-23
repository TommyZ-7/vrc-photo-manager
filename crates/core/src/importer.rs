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
    fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }
    fn remove(path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }
    move_file_with(source, target, result, rename, remove);
}

/// Moves a file, reporting a partial move when the copy succeeds but the source
/// removal fails. The filesystem operations are parameters so tests can inject
/// failures deterministically.
fn move_file_with(
    source: &Path,
    target: PathBuf,
    result: &mut ImportResult,
    rename: fn(&Path, &Path) -> std::io::Result<()>,
    remove_source: fn(&Path) -> std::io::Result<()>,
) {
    if rename(source, &target).is_ok() {
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
    match remove_source(source) {
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
        if let Err(error) = fs::remove_file(&temp) {
            tracing::debug!(
                %error,
                path = %temp.display(),
                "cannot remove temporary copy after size mismatch"
            );
        }
        return Err(CoreError::Invalid(format!(
            "copy size mismatch: {} != {}",
            source_len, copied_len
        )));
    }
    if let Err(error) = fs::rename(&temp, target) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::debug!(
                %cleanup,
                path = %temp.display(),
                "cannot remove temporary copy after rename failure"
            );
        }
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

    #[test]
    fn copy_failure_is_retryable_and_does_not_abort_the_batch() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let bad = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&bad);
        // copy_atomic writes to "<name>.png.tmp" first; an existing directory
        // there makes fs::copy fail deterministically, even when privileged.
        let blocked_temp = dest.join("VRChat_2026-10-21_21-14-03.221_4x3.png.tmp");
        fs::create_dir_all(&blocked_temp).unwrap();
        let good = dir.path().join("VRChat_2026-10-22_10-00-00.000_4x3.png");
        write_png(&good);

        let result = import_files(&[bad.clone(), good.clone()], &dest, ImportMode::Copy).unwrap();

        assert_eq!(result.imported, vec![dest.join(good.file_name().unwrap())]);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.issues.len(), 1);
        let issue = &result.issues[0];
        assert_eq!(issue.kind, ImportIssueKind::Failed);
        assert!(issue.retryable);
        assert_eq!(issue.source, bad);
        assert_eq!(
            issue.target.as_deref(),
            Some(dest.join(bad.file_name().unwrap()).as_path())
        );
        assert!(
            issue.message.contains("コピーに失敗しました"),
            "unexpected message: {}",
            issue.message
        );
    }

    #[test]
    fn move_fallback_reports_partial_move_with_paths_and_error() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let target = dest.join(source.file_name().unwrap());

        let mut result = ImportResult::default();
        move_file_with(
            &source,
            target.clone(),
            &mut result,
            |_, _| Err(std::io::Error::other("injected rename failure")),
            |_| Err(std::io::Error::other("injected removal failure")),
        );

        assert!(target.is_file(), "the fallback copy must exist");
        assert!(source.is_file(), "the failed removal keeps the source");
        assert_eq!(result.imported, vec![target.clone()]);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.issues.len(), 1);
        let issue = &result.issues[0];
        assert_eq!(issue.kind, ImportIssueKind::PartialMove);
        assert_eq!(issue.source, source);
        assert_eq!(issue.target.as_deref(), Some(target.as_path()));
        assert!(!issue.retryable);
        assert!(
            issue.message.contains("injected removal failure"),
            "message must carry the removal error: {}",
            issue.message
        );
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
        let sentinel = source_dir.join("delete-probe");
        fs::write(&sentinel, "probe").unwrap();

        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = fs::remove_file(&sentinel);
        if probe.is_ok() {
            // A privileged environment can delete from a read-only directory,
            // so the partial-move failure cannot be simulated here. The
            // injected test above still covers this branch deterministically.
            fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "skipping partial-move assertions: this environment can delete from a read-only directory"
            );
            return;
        }
        let probe_error = probe.unwrap_err();
        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Move).unwrap();
        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(source.is_file(), "source must remain after a failed delete");
        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.issues.len(), 1);
        let issue = &result.issues[0];
        assert_eq!(issue.kind, ImportIssueKind::PartialMove);
        assert_eq!(issue.source, source);
        assert!(!issue.retryable);
        assert_eq!(issue.target.as_deref(), Some(result.imported[0].as_path()));
        assert!(
            issue.message.contains(&probe_error.to_string()),
            "message must carry the removal error: {}",
            issue.message
        );
    }
}
