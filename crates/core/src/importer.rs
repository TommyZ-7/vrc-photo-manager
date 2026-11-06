use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{CoreError, Result};
use crate::fs_safe::{self, MoveOutcome, PlaceOutcome};
use crate::photo_filename::is_source_photo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Copy,
    Move,
}

/// How folder imports map source paths into the destination.
///
/// The default is a flat import, matching [`import_files`] and
/// [`import_files_observed`]. `include_root` only applies when
/// `preserve_structure` is set and a source root is supplied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Recreate the source folder hierarchy below the destination.
    pub preserve_structure: bool,
    /// Prefix the hierarchy with the selected source folder's name.
    pub include_root: bool,
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
/// Files are published through [`crate::fs_safe`], which claims the final name
/// atomically and falls back to an exclusive reservation when hard links are
/// unavailable. Name collisions get a numeric suffix; an existing file is
/// never overwritten. Every source is handled independently: a per-file
/// problem is recorded in [`ImportResult::issues`] (or counted as skipped)
/// without aborting the batch. Only a destination-level failure, such as
/// creating the destination folder, returns a hard error.
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
///
/// This is the flat variant; use
/// [`import_files_observed_with_options`] to preserve a folder hierarchy.
pub fn import_files_observed<F>(
    files: &[PathBuf],
    dest: &Path,
    mode: ImportMode,
    cancel: Option<&AtomicBool>,
    observe: F,
) -> Result<ImportResult>
where
    F: FnMut(ImportProgress),
{
    import_files_observed_with_options(
        files,
        dest,
        mode,
        ImportOptions::default(),
        None,
        cancel,
        observe,
    )
}

/// Copies or moves source photos with folder-structure options.
///
/// With `preserve_structure` and a `source_root`, each target is
/// `dest/[root name]/relative(path, root)`. Relative paths are validated:
/// absolute paths, `..` components, and files outside `source_root` become
/// per-file failures and are never written. Nested parent directories are
/// created before publication, and name collisions inside a target directory
/// still receive the usual numeric suffix.
///
/// Without a `source_root` (file-only imports) the layout stays flat.
pub fn import_files_observed_with_options<F>(
    files: &[PathBuf],
    dest: &Path,
    mode: ImportMode,
    options: ImportOptions,
    source_root: Option<&Path>,
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
        import_one(source, dest, mode, options, source_root, &mut result);
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
    // A selected root that cannot be read is a hard selection error; only
    // nested directories degrade to per-folder issues.
    if let Err(error) = fs::read_dir(&root) {
        return Err(CoreError::Io(error));
    }
    // An exclusion that genuinely does not exist cannot contain any
    // candidate, so it is safe to continue without it. This also keeps the
    // first import working when the destination folder has not been created
    // yet. Every other resolution failure is ambiguous: the path might be a
    // managed destination reached through an unavailable mount or an
    // unresolvable symlink. Refuse the scan (fail closed) instead of walking
    // the source without the exclusion and re-importing managed photos.
    let exclude = match exclude {
        Some(path) => match fs::canonicalize(path) {
            Ok(path) => Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "import exclusion does not exist; continuing without it"
                );
                None
            }
            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    %error,
                    "cannot resolve import exclusion; refusing to import"
                );
                return Err(CoreError::Invalid(format!(
                    "取り込み除外パスを解決できませんでした（安全のため取り込みを中止します）: {}: {error}",
                    path.display()
                )));
            }
        },
        None => None,
    };
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

fn import_one(
    source: &Path,
    dest: &Path,
    mode: ImportMode,
    options: ImportOptions,
    source_root: Option<&Path>,
    result: &mut ImportResult,
) {
    let target_dir = match target_directory(source, dest, options, source_root) {
        Ok(target_dir) => target_dir,
        Err(message) => {
            result
                .issues
                .push(failed_issue(source, None, message, false));
            return;
        }
    };
    match fs::metadata(source) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(metadata) if metadata.is_dir() => {
            result.issues.push(failed_issue(
                source,
                None,
                "フォルダは取り込めません".to_string(),
                false,
            ));
            return;
        }
        Ok(_) => {
            result.issues.push(failed_issue(
                source,
                None,
                "ファイルではないため取り込めません".to_string(),
                false,
            ));
            return;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            result.issues.push(failed_issue(
                source,
                None,
                "ファイルが見つかりません".to_string(),
                false,
            ));
            return;
        }
        Err(error) => {
            result.issues.push(failed_issue(
                source,
                None,
                format!("ファイルの情報を読み取れませんでした: {error}"),
                false,
            ));
            return;
        }
    }
    if !is_source_photo(source) {
        result.skipped += 1;
        return;
    }
    if let Err(error) = fs::create_dir_all(&target_dir) {
        result.issues.push(failed_issue(
            source,
            Some(target_candidate(&target_dir, source, 0)),
            format!("取り込み先フォルダを作成できませんでした: {error}"),
            true,
        ));
        return;
    }
    match mode {
        ImportMode::Copy => copy_file(source, &target_dir, result),
        ImportMode::Move => move_file(source, &target_dir, result),
    }
}

/// Resolves the directory that should receive `source`.
///
/// Only the preserve-with-root case nests; every other combination keeps the
/// historical flat layout. A path that cannot be safely mapped below `dest`
/// returns a message instead of a target directory.
fn target_directory(
    source: &Path,
    dest: &Path,
    options: ImportOptions,
    source_root: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    if !options.preserve_structure {
        return Ok(dest.to_path_buf());
    }
    let Some(root) = source_root else {
        return Ok(dest.to_path_buf());
    };
    let relative = source
        .strip_prefix(root)
        .map_err(|_| "取り込み元フォルダの外にあるファイルは取り込めません".to_string())?;
    if relative.as_os_str().is_empty() {
        return Err("取り込み元フォルダ自体はファイルとして取り込めません".to_string());
    }
    validate_relative_path(relative)?;

    let mut target_dir = dest.to_path_buf();
    if options.include_root
        && let Some(name) = root.file_name()
    {
        target_dir.push(name);
    }
    if let Some(parent) = relative.parent()
        && !parent.as_os_str().is_empty()
    {
        target_dir.push(parent);
    }
    Ok(target_dir)
}

/// Rejects relative paths that could escape the destination directory.
fn validate_relative_path(relative: &Path) -> std::result::Result<(), String> {
    if relative.is_absolute() {
        return Err("取り込み元からの相対パスが絶対パスになっています".to_string());
    }
    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err("取り込み元からの相対パスに `..` が含まれています".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("取り込み元からの相対パスが不正です".to_string());
            }
        }
    }
    Ok(())
}

/// Copies `source` to the first destination name that can be claimed
/// atomically. The suffix sequence stays `name.ext`, `name-1.ext`, ... but no
/// `exists()` check decides the winner: a collision reported by the publish
/// helper simply advances to the next name.
fn copy_file(source: &Path, dest: &Path, result: &mut ImportResult) {
    let mut number = 0;
    loop {
        let target = target_candidate(dest, source, number);
        if source == target.as_path() {
            result.skipped += 1;
            return;
        }
        clear_legacy_temp(&target);
        match copy_atomic(source, &target) {
            Ok(PlaceOutcome::Placed) => {
                record_imported(source, target, result);
                return;
            }
            Ok(PlaceOutcome::DestinationExists) => number += 1,
            Err(error) => {
                result.issues.push(failed_issue(
                    source,
                    Some(target),
                    format!("コピーに失敗しました: {error}"),
                    true,
                ));
                return;
            }
        }
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

fn move_file(source: &Path, dest: &Path, result: &mut ImportResult) {
    move_file_with(source, dest, result, fs_safe::move_no_clobber);
}

/// Moves a file to the first destination name that can be claimed atomically,
/// reporting a partial move when the destination is complete but the source
/// removal fails. The move operation is a parameter so tests can inject
/// failures deterministically.
fn move_file_with(
    source: &Path,
    dest: &Path,
    result: &mut ImportResult,
    place: fn(&Path, &Path) -> std::io::Result<MoveOutcome>,
) {
    let mut number = 0;
    loop {
        let target = target_candidate(dest, source, number);
        if source == target.as_path() {
            result.skipped += 1;
            return;
        }
        match place(source, &target) {
            Ok(MoveOutcome::Moved) => {
                record_imported(source, target, result);
                return;
            }
            Ok(MoveOutcome::SourceNotRemoved(error)) => {
                result.issues.push(ImportIssue {
                    source: source.to_path_buf(),
                    target: Some(target.clone()),
                    message: format!(
                        "移動先へのコピーは成功しましたが、元ファイルを削除できませんでした: {error}"
                    ),
                    kind: ImportIssueKind::PartialMove,
                    retryable: false,
                });
                // The copy exists in the destination, so the destination scan
                // can index it even though the original remains behind.
                record_imported(source, target, result);
                return;
            }
            Ok(MoveOutcome::DestinationExists) => number += 1,
            Err(error) => {
                result.issues.push(failed_issue(
                    source,
                    Some(target),
                    format!("移動に失敗しました: {error}"),
                    true,
                ));
                return;
            }
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

fn target_candidate(dest: &Path, source: &Path, number: u64) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("photo");
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png");
    if number == 0 {
        dest.join(format!("{stem}.{extension}"))
    } else {
        dest.join(format!("{stem}-{number}.{extension}"))
    }
}

fn copy_atomic(source: &Path, target: &Path) -> Result<PlaceOutcome> {
    fs_safe::copy_no_clobber(source, target).map_err(CoreError::Io)
}

/// Copies `source` to a uniquely named temporary file next to `target` and
/// publishes it without clobbering. The copy operation is a parameter so tests
/// can inject failures.
#[cfg(test)]
fn copy_atomic_with(
    source: &Path,
    target: &Path,
    copy: fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<PlaceOutcome> {
    fs_safe::copy_no_clobber_with(source, target, copy).map_err(CoreError::Io)
}

/// Removes the deterministic temporary name used by versions before the
/// no-clobber helper. New copies use unique temp names, which never block a
/// destination, but an upgrade may still find one of these stale files.
fn clear_legacy_temp(target: &Path) {
    let extension = target
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png");
    let legacy = target.with_extension(format!("{extension}.tmp"));
    if legacy.is_file()
        && let Err(error) = fs::remove_file(&legacy)
    {
        tracing::debug!(
            %error,
            path = %legacy.display(),
            "cannot remove stale import temp"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::tempdir;

    fn write_png(path: &Path) {
        write_png_color(path, [10, 20, 30, 255]);
    }

    fn write_png_color(path: &Path, color: [u8; 4]) {
        RgbaImage::from_pixel(4, 3, Rgba(color)).save(path).unwrap();
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
    fn copy_failure_is_retryable_and_reports_the_target() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let missing = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        let mut result = ImportResult::default();

        copy_file(&missing, &dest, &mut result);

        assert!(result.imported.is_empty());
        assert_eq!(result.issues.len(), 1);
        let issue = &result.issues[0];
        assert_eq!(issue.kind, ImportIssueKind::Failed);
        assert!(issue.retryable);
        assert_eq!(issue.source, missing);
        assert_eq!(
            issue.target.as_deref(),
            Some(dest.join(missing.file_name().unwrap()).as_path())
        );
        assert!(
            issue.message.contains("コピーに失敗しました"),
            "unexpected message: {}",
            issue.message
        );
    }

    #[test]
    fn stale_temp_file_does_not_block_the_import() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        // A temp file left behind by an interrupted import of the same name.
        let stale_temp = dest.join("VRChat_2026-10-21_21-14-03.221_4x3.png.tmp");
        fs::write(&stale_temp, b"partial garbage").unwrap();

        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();

        assert_eq!(result.imported.len(), 1);
        assert!(result.issues.is_empty());
        assert!(!stale_temp.exists(), "the stale temp must be gone");
        assert_eq!(
            fs::metadata(&result.imported[0]).unwrap().len(),
            fs::metadata(&source).unwrap().len()
        );
    }

    #[test]
    fn failed_copy_removes_the_partial_temp() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("photo.png");
        write_png(&source);
        let target = dir.path().join("dest.png");

        let result = copy_atomic_with(&source, &target, |_from, to| {
            fs::write(to, b"partial")?;
            Err(std::io::Error::other("injected copy failure"))
        });

        assert!(result.is_err());
        assert!(!target.exists());
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the source must remain");
        assert_eq!(entries[0].as_ref().unwrap().path(), source);
    }

    #[test]
    fn directory_source_is_not_reported_as_missing() {
        let dir = tempdir().unwrap();
        // A folder wearing a photo's name must not be reported as missing.
        let folder = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        fs::create_dir_all(&folder).unwrap();
        let dest = dir.path().join("dest");

        let result = import_files(std::slice::from_ref(&folder), &dest, ImportMode::Copy).unwrap();

        assert!(result.imported.is_empty());
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].kind, ImportIssueKind::Failed);
        assert!(
            result.issues[0].message.contains("フォルダ"),
            "message should say the source is a folder: {}",
            result.issues[0].message
        );
    }

    #[test]
    fn move_failure_is_reported_with_the_underlying_reason() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let target = dest.join(source.file_name().unwrap());

        let mut result = ImportResult::default();
        move_file_with(&source, &dest, &mut result, |_, _| {
            Err(std::io::Error::other("injected move failure"))
        });

        assert!(result.imported.is_empty());
        assert_eq!(result.issues.len(), 1);
        let issue = &result.issues[0];
        assert_eq!(issue.kind, ImportIssueKind::Failed);
        assert!(issue.retryable);
        assert_eq!(issue.target.as_deref(), Some(target.as_path()));
        assert!(
            issue.message.contains("移動に失敗しました"),
            "unexpected message: {}",
            issue.message
        );
        assert!(
            issue.message.contains("injected move failure"),
            "message must keep the move reason: {}",
            issue.message
        );
    }

    #[test]
    fn move_reports_partial_move_with_paths_and_error() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let target = dest.join(source.file_name().unwrap());
        // The destination is already complete when only removing the source
        // fails.
        fs::copy(&source, &target).unwrap();

        let mut result = ImportResult::default();
        move_file_with(&source, &dest, &mut result, |_, _| {
            Ok(MoveOutcome::SourceNotRemoved(std::io::Error::other(
                "injected removal failure",
            )))
        });

        assert!(target.is_file(), "the destination must exist");
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
    fn two_moves_to_the_same_destination_keep_both_files() {
        let dir = tempdir().unwrap();
        let first_dir = dir.path().join("first");
        let second_dir = dir.path().join("second");
        fs::create_dir_all(&first_dir).unwrap();
        fs::create_dir_all(&second_dir).unwrap();
        let file_name = "VRChat_2026-10-21_21-14-03.221_4x3.png";
        let first = first_dir.join(file_name);
        let second = second_dir.join(file_name);
        write_png_color(&first, [255, 0, 0, 255]);
        write_png_color(&second, [0, 0, 255, 255]);
        let first_bytes = fs::read(&first).unwrap();
        let second_bytes = fs::read(&second).unwrap();
        let dest = dir.path().join("dest");

        let result =
            import_files(&[first.clone(), second.clone()], &dest, ImportMode::Move).unwrap();

        assert!(result.issues.is_empty());
        assert_eq!(result.imported.len(), 2);
        assert_ne!(result.imported[0], result.imported[1]);
        assert!(!first.exists());
        assert!(!second.exists());
        let mut contents = vec![
            fs::read(&result.imported[0]).unwrap(),
            fs::read(&result.imported[1]).unwrap(),
        ];
        contents.sort();
        let mut expected = vec![first_bytes, second_bytes];
        expected.sort();
        assert_eq!(contents, expected, "both source files must be preserved");
    }

    #[test]
    fn concurrent_moves_to_the_same_destination_keep_both_files() {
        let dir = tempdir().unwrap();
        let first_dir = dir.path().join("first");
        let second_dir = dir.path().join("second");
        fs::create_dir_all(&first_dir).unwrap();
        fs::create_dir_all(&second_dir).unwrap();
        let file_name = "VRChat_2026-10-21_21-14-03.221_4x3.png";
        let first = first_dir.join(file_name);
        let second = second_dir.join(file_name);
        write_png_color(&first, [255, 0, 0, 255]);
        write_png_color(&second, [0, 0, 255, 255]);
        let first_bytes = fs::read(&first).unwrap();
        let second_bytes = fs::read(&second).unwrap();
        let dest = dir.path().join("dest");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|source| {
                let dest = dest.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    import_files(&[source], &dest, ImportMode::Move).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert!(results.iter().all(|result| result.issues.is_empty()));
        let mut paths: Vec<_> = results
            .iter()
            .flat_map(|result| result.imported.clone())
            .collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 2, "each move must keep its own file");
        let mut contents: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
        contents.sort();
        let mut expected = vec![first_bytes, second_bytes];
        expected.sort();
        assert_eq!(contents, expected);
    }

    #[test]
    fn an_existing_destination_is_never_truncated() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let existing = dest.join(source.file_name().unwrap());
        let sentinel = b"do not overwrite this file";
        fs::write(&existing, sentinel).unwrap();

        let copy = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();
        assert!(copy.issues.is_empty());
        assert_ne!(copy.imported[0], existing);
        assert_eq!(fs::read(&existing).unwrap(), sentinel);

        let moved = import_files(std::slice::from_ref(&source), &dest, ImportMode::Move).unwrap();
        assert!(moved.issues.is_empty());
        assert_ne!(moved.imported[0], existing);
        assert!(!source.exists());
        assert_eq!(fs::read(&existing).unwrap(), sentinel);
    }

    #[test]
    fn missing_import_exclusion_is_non_fatal() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        let photo = root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&photo);

        let scan =
            collect_folder_sources(&root, Some(&dir.path().join("not-created-yet"))).unwrap();

        assert_eq!(scan.files, vec![photo]);
        assert!(scan.issues.is_empty());
    }

    #[test]
    fn unresolvable_import_exclusion_fails_closed() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_png(&root.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        // A regular file used as a path component makes canonicalize fail with
        // something other than NotFound, even when the tests run as root.
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"blocker").unwrap();

        let result = collect_folder_sources(&root, Some(&blocker.join("child")));

        assert!(
            result.is_err(),
            "an ambiguous exclusion must refuse the scan (fail closed)"
        );
    }

    #[test]
    fn folder_scan_is_recursive_and_filters_compressed_symlinks_and_destination() {
        let dir = tempdir().unwrap();
        let raw_root = dir.path().join("photos");
        fs::create_dir_all(raw_root.join("nested")).unwrap();
        fs::create_dir_all(raw_root.join("compressed")).unwrap();
        fs::create_dir_all(raw_root.join("imported")).unwrap();
        let root = fs::canonicalize(&raw_root).unwrap();
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
        let raw_root = dir.path().join("photos");
        let raw_locked = raw_root.join("locked");
        fs::create_dir_all(&raw_locked).unwrap();
        let root = fs::canonicalize(&raw_root).unwrap();
        let locked = root.join("locked");
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

    #[cfg(unix)]
    #[test]
    fn folder_scan_rejects_an_unreadable_root() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        let probe = fs::read_dir(&root);
        let result = collect_folder_sources(&root, None);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        if probe.is_ok() {
            eprintln!("skipping unreadable-root assertions: running with elevated permissions");
            return;
        }
        assert!(result.is_err());
    }

    #[test]
    fn progress_counts_a_failed_source_and_completes() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        // Never created on disk, but still a candidate source photo.
        let missing = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        let good = source_dir.join("VRChat_2026-10-21_21-14-04.221_4x3.png");
        write_png(&good);
        let dest = dir.path().join("dest");

        let mut events = Vec::new();
        let result = import_files_observed(
            &[missing, good],
            &dest,
            ImportMode::Copy,
            None,
            |progress| events.push(progress),
        )
        .unwrap();

        let last = events.last().unwrap();
        assert_eq!(last.processed, 2);
        assert_eq!(last.total, 2);
        assert_eq!(last.imported, 1);
        assert_eq!(last.failed, 1);
        assert_eq!(result.processed, 2);
        assert!(!result.cancelled);
    }

    #[test]
    fn unique_names_are_not_reported_as_renames() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let dest = dir.path().join("dest");

        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();

        assert_eq!(result.imported.len(), 1);
        assert!(result.renamed.is_empty());
    }

    fn make_folder_tree(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let root = dir.join("Trip");
        fs::create_dir_all(root.join("Day1")).unwrap();
        fs::create_dir_all(root.join("Day2")).unwrap();
        let day1 = root.join("Day1/a.png");
        let day2 = root.join("Day2/b.png");
        write_png(&day1);
        write_png(&day2);
        (root, day1, day2)
    }

    fn import_with_options(
        files: &[PathBuf],
        dest: &Path,
        mode: ImportMode,
        options: ImportOptions,
        root: Option<&Path>,
    ) -> ImportResult {
        import_files_observed_with_options(files, dest, mode, options, root, None, |_| {}).unwrap()
    }

    #[test]
    fn preserve_off_keeps_the_flat_layout_even_with_a_source_root() {
        let dir = tempdir().unwrap();
        let (root, day1, day2) = make_folder_tree(dir.path());
        let dest = dir.path().join("dest");
        let options = ImportOptions {
            preserve_structure: false,
            include_root: true,
        };

        let result = import_with_options(
            &[day1.clone(), day2.clone()],
            &dest,
            ImportMode::Copy,
            options,
            Some(&root),
        );

        assert_eq!(
            result.imported,
            vec![
                dest.join(day1.file_name().unwrap()),
                dest.join(day2.file_name().unwrap()),
            ]
        );
        assert!(result.issues.is_empty());
        assert!(result.renamed.is_empty());
    }

    #[test]
    fn preserve_structure_without_a_source_root_stays_flat() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("a.png");
        write_png(&source);
        let dest = dir.path().join("dest");
        let options = ImportOptions {
            preserve_structure: true,
            include_root: true,
        };

        let result = import_files_observed_with_options(
            std::slice::from_ref(&source),
            &dest,
            ImportMode::Copy,
            options,
            None,
            None,
            |_| {},
        )
        .unwrap();

        assert_eq!(result.imported, vec![dest.join("a.png")]);
    }

    #[test]
    fn preserve_structure_nests_subfolders_without_the_root_name() {
        let dir = tempdir().unwrap();
        let (root, day1, day2) = make_folder_tree(dir.path());
        let dest = dir.path().join("dest");
        let options = ImportOptions {
            preserve_structure: true,
            include_root: false,
        };

        let result =
            import_with_options(&[day1, day2], &dest, ImportMode::Copy, options, Some(&root));

        assert_eq!(
            result.imported,
            vec![dest.join("Day1/a.png"), dest.join("Day2/b.png")]
        );
        assert!(result.issues.is_empty());
        assert!(result.renamed.is_empty());
    }

    #[test]
    fn preserve_structure_can_include_the_selected_root_name() {
        let dir = tempdir().unwrap();
        let (root, day1, day2) = make_folder_tree(dir.path());
        let dest = dir.path().join("dest");
        let options = ImportOptions {
            preserve_structure: true,
            include_root: true,
        };

        let result =
            import_with_options(&[day1, day2], &dest, ImportMode::Copy, options, Some(&root));

        assert_eq!(
            result.imported,
            vec![dest.join("Trip/Day1/a.png"), dest.join("Trip/Day2/b.png"),]
        );
        assert!(result.issues.is_empty());
        assert!(result.renamed.is_empty());
    }

    #[test]
    fn nested_same_named_files_in_different_subfolders_do_not_collide() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Trip");
        fs::create_dir_all(root.join("Day1")).unwrap();
        fs::create_dir_all(root.join("Day2")).unwrap();
        let first = root.join("Day1/same.png");
        let second = root.join("Day2/same.png");
        write_png(&first);
        write_png(&second);
        let dest = dir.path().join("dest");

        let result = import_with_options(
            &[first, second],
            &dest,
            ImportMode::Copy,
            ImportOptions {
                preserve_structure: true,
                include_root: false,
            },
            Some(&root),
        );

        assert_eq!(
            result.imported,
            vec![dest.join("Day1/same.png"), dest.join("Day2/same.png")]
        );
        assert!(result.renamed.is_empty());
    }

    #[test]
    fn collisions_inside_a_nested_target_still_get_a_suffix() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Trip");
        fs::create_dir_all(root.join("Day1")).unwrap();
        let source = root.join("Day1/same.png");
        write_png(&source);
        let dest = dir.path().join("dest");
        let existing = dest.join("Day1/same.png");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        write_png(&existing);

        let result = import_with_options(
            std::slice::from_ref(&source),
            &dest,
            ImportMode::Copy,
            ImportOptions {
                preserve_structure: true,
                include_root: false,
            },
            Some(&root),
        );

        let expected = dest.join("Day1/same-1.png");
        assert_eq!(result.imported, vec![expected.clone()]);
        assert_eq!(result.renamed.len(), 1);
        assert_eq!(result.renamed[0].source, source);
        assert_eq!(result.renamed[0].target, expected);
        assert!(existing.is_file(), "the existing file must not be replaced");
    }

    #[test]
    fn files_outside_the_source_root_are_per_file_failures() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Trip");
        fs::create_dir_all(&root).unwrap();
        let inside = root.join("a.png");
        write_png(&inside);
        let outside = dir.path().join("outside.png");
        write_png(&outside);
        let traversal = root.join("..").join("outside.png");
        let dest = dir.path().join("dest");

        let result = import_with_options(
            &[outside.clone(), traversal.clone(), inside],
            &dest,
            ImportMode::Copy,
            ImportOptions {
                preserve_structure: true,
                include_root: false,
            },
            Some(&root),
        );

        assert_eq!(result.imported, vec![dest.join("a.png")]);
        assert_eq!(result.issues.len(), 2);
        assert!(result.issues.iter().all(|issue| {
            issue.kind == ImportIssueKind::Failed && !issue.retryable && issue.target.is_none()
        }));
        assert_eq!(result.issues[0].source, outside);
        assert_eq!(result.issues[1].source, traversal);
        assert!(
            result.issues[0].message.contains("外"),
            "unexpected message: {}",
            result.issues[0].message
        );
        assert!(
            result.issues[1].message.contains(".."),
            "unexpected message: {}",
            result.issues[1].message
        );
        assert!(!dest.join("outside.png").exists());
        assert!(
            !dir.path().join("outside-1.png").exists(),
            "nothing may be written outside the destination"
        );
    }

    #[test]
    fn nested_parent_creation_failure_reports_the_nested_target() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Trip");
        fs::create_dir_all(root.join("Day1")).unwrap();
        let source = root.join("Day1/a.png");
        write_png(&source);
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        // A file where the nested directory should be makes create_dir_all fail.
        fs::write(dest.join("Day1"), b"blocker").unwrap();

        let result = import_with_options(
            &[source],
            &dest,
            ImportMode::Copy,
            ImportOptions {
                preserve_structure: true,
                include_root: false,
            },
            Some(&root),
        );

        assert!(result.imported.is_empty());
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].kind, ImportIssueKind::Failed);
        assert_eq!(
            result.issues[0].target.as_deref(),
            Some(dest.join("Day1/a.png").as_path())
        );
        assert!(result.issues[0].retryable);
    }

    #[test]
    fn moving_a_folder_import_preserves_structure() {
        let dir = tempdir().unwrap();
        let (root, day1, day2) = make_folder_tree(dir.path());
        let dest = dir.path().join("dest");

        let result = import_with_options(
            &[day1.clone(), day2.clone()],
            &dest,
            ImportMode::Move,
            ImportOptions {
                preserve_structure: true,
                include_root: true,
            },
            Some(&root),
        );

        assert_eq!(
            result.imported,
            vec![dest.join("Trip/Day1/a.png"), dest.join("Trip/Day2/b.png"),]
        );
        assert!(!day1.exists());
        assert!(!day2.exists());
        assert!(result.imported.iter().all(|path| path.is_file()));
    }
}
