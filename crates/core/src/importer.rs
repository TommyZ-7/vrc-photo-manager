use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRename {
    /// Source path before the collision suffix was applied.
    pub source: PathBuf,
    /// Destination path that was actually written.
    pub target: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    pub imported: Vec<PathBuf>,
    pub skipped: usize,
    pub issues: Vec<ImportIssue>,
    /// Successful imports whose destination file name differs from the source.
    pub renamed: Vec<ImportRename>,
    /// Sources attempted in this run, including skipped files and failures.
    pub processed: usize,
    /// True only when cancellation was observed before an unprocessed file.
    pub cancelled: bool,
}

/// A snapshot of import progress after the initial event or one attempted file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportProgress {
    pub processed: usize,
    pub total: usize,
    pub imported: usize,
    pub skipped: usize,
    pub failed: usize,
    pub partial: usize,
    /// Last source handled by this run; None on the initial event.
    pub last_source: Option<PathBuf>,
}

/// Copies or moves source photos into a managed library folder.
///
/// Files are written to a temporary name and renamed into place. Name
/// collisions get a numeric suffix; an existing file is never overwritten.
/// Every source is handled independently: a per-file problem is recorded in
/// [`ImportResult::issues`] (or counted as skipped) without aborting the batch.
/// Only a destination-level failure, such as creating the destination folder,
/// returns a hard error.
///
/// Use [`import_files_observed`] for progress and cancellation.
pub fn import_files(files: &[PathBuf], dest: &Path, mode: ImportMode) -> Result<ImportResult> {
    import_files_observed(files, dest, mode, None, |_| {})
}

/// Copies or moves source photos while reporting progress and honoring
/// cancellation.
///
/// The observer is called once before the first file and once after every
/// attempted file. Cancellation is checked before each source; the current
/// file finishes, already imported files stay in place, and the result reports
/// `cancelled = true` with the sources after `processed` left untouched.
pub fn import_files_observed<F>(
    files: &[PathBuf],
    dest: &Path,
    mode: ImportMode,
    cancel: Option<&AtomicBool>,
    mut observe: F,
) -> Result<ImportResult>
where
    F: FnMut(ImportProgress),
{
    fs::create_dir_all(dest)?;
    let total = files.len();
    let mut result = ImportResult::default();
    let mut failed = 0;
    let mut partial = 0;
    observe(ImportProgress {
        processed: 0,
        total,
        imported: 0,
        skipped: 0,
        failed,
        partial,
        last_source: None,
    });
    for source in files {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            result.cancelled = true;
            break;
        }
        let issues_before = result.issues.len();
        import_one(source, dest, mode, &mut result);
        result.processed += 1;
        for issue in &result.issues[issues_before..] {
            match issue.kind {
                ImportIssueKind::Failed => failed += 1,
                ImportIssueKind::PartialMove => partial += 1,
            }
        }
        observe(ImportProgress {
            processed: result.processed,
            total,
            imported: result.imported.len(),
            skipped: result.skipped,
            failed,
            partial,
            last_source: Some(source.clone()),
        });
    }
    Ok(result)
}

/// Source files and unreadable directories found under a folder.
#[derive(Debug, Clone, Default)]
pub struct FolderScan {
    pub files: Vec<PathBuf>,
    /// Unreadable directories, expressed as non-retryable import failures.
    pub issues: Vec<ImportIssue>,
}

/// Collects supported photos under `folder` recursively.
///
/// The walk skips symlinks, a directory named `compressed` (the scanner's
/// derivative folder), and the `exclude` subtree (normally the import
/// destination). Files are sorted by path so collision suffixes and progress
/// are deterministic.
pub fn collect_folder_sources(folder: &Path, exclude: Option<&Path>) -> Result<FolderScan> {
    if !folder.is_dir() {
        return Err(CoreError::Invalid(format!(
            "not a directory: {}",
            folder.display()
        )));
    }
    let root = fs::canonicalize(folder)?;
    let exclude = exclude.and_then(|path| fs::canonicalize(path).ok());
    let mut scan = FolderScan::default();
    walk_folder(&root, exclude.as_deref(), &mut scan);
    scan.files.sort();
    Ok(scan)
}

fn walk_folder(dir: &Path, exclude: Option<&Path>, scan: &mut FolderScan) {
    if exclude == Some(dir) {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(path = %dir.display(), %error, "cannot read import folder");
            scan.issues.push(failed_issue(
                dir,
                None,
                format!("フォルダを読み取れませんでした: {error}"),
                false,
            ));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "cannot read import folder entry");
                scan.issues.push(failed_issue(
                    dir,
                    None,
                    format!("フォルダを読み取れませんでした: {error}"),
                    false,
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot read import file type");
                scan.issues.push(failed_issue(
                    &path,
                    None,
                    format!("フォルダを読み取れませんでした: {error}"),
                    false,
                ));
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if entry.file_name() == "compressed" {
                continue;
            }
            walk_folder(&path, exclude, scan);
        } else if file_type.is_file() && is_source_photo(&path) {
            scan.files.push(path);
        }
    }
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
            Ok(()) => record_imported(source, target, result),
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

fn record_imported(source: &Path, target: PathBuf, result: &mut ImportResult) {
    if source.file_name() != target.file_name() {
        result.renamed.push(ImportRename {
            source: source.to_path_buf(),
            target: target.clone(),
        });
    }
    result.imported.push(target);
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
        record_imported(source, target, result);
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
        Ok(()) => record_imported(source, target, result),
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
            record_imported(source, target, result);
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
    use std::sync::atomic::{AtomicBool, Ordering};
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

    #[test]
    fn progress_reports_start_then_each_attempted_file() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let first = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        let second = source_dir.join("VRChat_2026-10-21_21-14-04.221_4x3.png");
        let text = source_dir.join("note.txt");
        write_png(&first);
        write_png(&second);
        fs::write(&text, "hello").unwrap();
        let dest = dir.path().join("dest");

        let mut events = Vec::new();
        let result = import_files_observed(
            &[first, text, second],
            &dest,
            ImportMode::Copy,
            None,
            |progress| events.push(progress),
        )
        .unwrap();

        assert_eq!(events.len(), 4, "initial event + one per attempted file");
        assert_eq!(events[0].processed, 0);
        assert_eq!(events[0].total, 3);
        assert!(events[0].last_source.is_none());
        let last = events.last().unwrap();
        assert_eq!(last.processed, 3);
        assert_eq!(last.imported, 2);
        assert_eq!(last.skipped, 1);
        assert_eq!(last.failed, 0);
        assert_eq!(result.processed, 3);
        assert!(!result.cancelled);
    }

    #[test]
    fn cancellation_before_the_first_file_imports_nothing() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let dest = dir.path().join("dest");
        let cancel = AtomicBool::new(true);

        let result = import_files_observed(
            std::slice::from_ref(&source),
            &dest,
            ImportMode::Copy,
            Some(&cancel),
            |_| {},
        )
        .unwrap();

        assert!(result.cancelled);
        assert_eq!(result.processed, 0);
        assert_eq!(result.imported.len(), 0);
        assert!(!dest.join(source.file_name().unwrap()).exists());
    }

    #[test]
    fn cancellation_stops_before_the_next_file() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let first = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        let second = source_dir.join("VRChat_2026-10-21_21-14-04.221_4x3.png");
        write_png(&first);
        write_png(&second);
        let dest = dir.path().join("dest");
        let cancel = AtomicBool::new(false);

        let result = import_files_observed(
            &[first.clone(), second.clone()],
            &dest,
            ImportMode::Copy,
            Some(&cancel),
            |progress| {
                if progress.processed == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        )
        .unwrap();

        assert!(result.cancelled);
        assert_eq!(result.processed, 1);
        assert_eq!(result.imported.len(), 1);
        assert!(dest.join(first.file_name().unwrap()).is_file());
        assert!(!dest.join(second.file_name().unwrap()).exists());
    }

    #[test]
    fn collisions_are_reported_as_renames() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let existing = dest.join(source.file_name().unwrap());
        write_png(&existing);

        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();

        assert_eq!(result.renamed.len(), 1);
        assert_eq!(result.renamed[0].source, source);
        assert_ne!(result.renamed[0].target, existing);
        assert_eq!(
            result.renamed[0].target.file_name().unwrap(),
            "VRChat_2026-10-21_21-14-03.221_4x3-1.png"
        );
        assert!(result.renamed[0].target.is_file());
    }

    #[test]
    fn same_named_sources_in_one_batch_get_distinct_targets() {
        let dir = tempdir().unwrap();
        let first_dir = dir.path().join("first");
        let second_dir = dir.path().join("second");
        fs::create_dir_all(&first_dir).unwrap();
        fs::create_dir_all(&second_dir).unwrap();
        let file_name = "VRChat_2026-10-21_21-14-03.221_4x3.png";
        let first = first_dir.join(file_name);
        let second = second_dir.join(file_name);
        write_png(&first);
        write_png(&second);
        let dest = dir.path().join("dest");

        let result = import_files(&[first, second], &dest, ImportMode::Copy).unwrap();

        assert_eq!(result.imported.len(), 2);
        assert_ne!(result.imported[0], result.imported[1]);
        assert_eq!(result.renamed.len(), 1);
        assert_eq!(
            result.imported[1].file_name().unwrap(),
            "VRChat_2026-10-21_21-14-03.221_4x3-1.png"
        );
        assert_eq!(result.renamed[0].target, result.imported[1]);
    }

    #[test]
    fn folder_scan_is_recursive_and_filters_compressed_symlinks_and_destination() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(root.join("compressed")).unwrap();
        fs::create_dir_all(root.join("imported")).unwrap();
        let top = root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        let nested = root.join("nested/VRChat_2026-10-21_21-14-04.221_4x3.png");
        let compressed = root.join("compressed/VRChat_2026-10-21_21-14-05.221_4x3.png");
        let excluded = root.join("imported/VRChat_2026-10-21_21-14-06.221_4x3.png");
        write_png(&top);
        write_png(&nested);
        write_png(&compressed);
        write_png(&excluded);
        fs::write(root.join("nested/note.txt"), "not a photo").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("nested"), root.join("link")).unwrap();

        let scan = collect_folder_sources(&root, Some(&root.join("imported"))).unwrap();

        assert_eq!(scan.files, vec![top, nested]);
        assert!(scan.issues.is_empty());
    }

    #[test]
    fn folder_scan_rejects_a_file_path() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&file);

        assert!(collect_folder_sources(&file, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn folder_scan_reports_unreadable_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let probe = fs::read_dir(&locked);
        let scan = collect_folder_sources(&root, None).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        if probe.is_ok() {
            eprintln!(
                "skipping unreadable-directory assertions: running with elevated permissions"
            );
            return;
        }
        assert_eq!(scan.issues.len(), 1);
        assert_eq!(scan.issues[0].source, locked);
        assert!(!scan.issues[0].retryable);
        assert_eq!(scan.issues[0].kind, ImportIssueKind::Failed);
        assert!(
            scan.issues[0]
                .message
                .contains("フォルダを読み取れませんでした"),
            "unexpected message: {}",
            scan.issues[0].message
        );
    }
}
