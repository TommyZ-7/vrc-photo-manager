use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::db::{Db, PhotoScanState};
use crate::error::{CoreError, Result};
use crate::hashing;
use crate::model::{NewPhoto, Photo, PhotoStatus};
use crate::photo_filename::{
    from_modified_time, image_dimensions, is_source_photo, parse_photo_filename,
};

#[derive(Debug, Clone)]
pub struct ScannedPhoto {
    pub path: PathBuf,
    pub rel_path: String,
    pub file_name: String,
    pub captured_at_utc: i64,
    pub captured_at_local: String,
    pub tz_offset_sec: i32,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub mtime: i64,
}

impl ScannedPhoto {
    pub fn into_new_photo(self, root_id: i64) -> NewPhoto {
        NewPhoto {
            root_id,
            rel_path: self.rel_path,
            abs_path: self.path.to_string_lossy().to_string(),
            file_name: self.file_name,
            captured_at_utc: self.captured_at_utc,
            captured_at_local: self.captured_at_local,
            tz_offset_sec: self.tz_offset_sec,
            width: self.width,
            height: self.height,
            file_size: self.file_size,
            mtime: self.mtime,
            blake3: None,
        }
    }
}

/// A source photo or directory entry that could not be scanned or indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub message: String,
}

impl ScanFailure {
    fn new(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub found: usize,
    pub indexed: usize,
    pub skipped: usize,
    /// Rows inserted, updated, or rebound by this scan. Unchanged photos are
    /// counted in `indexed` but not here.
    pub modified: usize,
    pub new_ids: Vec<i64>,
    pub rebound_ids: Vec<i64>,
    pub seen_paths: Vec<String>,
    pub failures: Vec<ScanFailure>,
    /// True when the walk or indexing stopped because the caller asked to
    /// cancel. A cancelled scan must not be used to mark unseen photos missing.
    pub cancelled: bool,
}

/// The phase a scan is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhase {
    Walking,
    Indexing,
}

/// Progress reported by [`scan_and_index_observed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub processed: usize,
    pub total: Option<usize>,
    pub indexed: usize,
}

/// Recursively scans a VRChat photo root.
///
/// Only source PNG/JPEG files are returned. The `compressed` subdirectory is
/// skipped so that generated derivatives are not indexed as new photos.
/// Failures are dropped; use [`scan_root_report`] to inspect them.
pub fn scan_root(root: &Path) -> Result<Vec<ScannedPhoto>> {
    scan_root_report(root).map(|(photos, _failures)| photos)
}

/// Recursively scans a VRChat photo root, reporting unreadable entries.
///
/// A failure while reading one directory or photo does not abort the walk;
/// readable photos are still returned.
pub fn scan_root_report(root: &Path) -> Result<(Vec<ScannedPhoto>, Vec<ScanFailure>)> {
    scan_root_report_with_progress(root, |_| {})
}

fn scan_root_report_with_progress<F>(
    root: &Path,
    observe: F,
) -> Result<(Vec<ScannedPhoto>, Vec<ScanFailure>)>
where
    F: FnMut(usize),
{
    let (photos, failures, _cancelled) =
        scan_root_report_with_progress_running(root, None, observe)?;
    Ok((photos, failures))
}

fn scan_root_report_with_progress_running<F>(
    root: &Path,
    running: Option<&AtomicBool>,
    mut observe: F,
) -> Result<(Vec<ScannedPhoto>, Vec<ScanFailure>, bool)>
where
    F: FnMut(usize),
{
    if !root.is_dir() {
        return Err(CoreError::Invalid(format!(
            "photo root is not a directory: {}",
            root.display()
        )));
    }
    let mut photos = Vec::new();
    let mut failures = Vec::new();
    let cancelled = walk(
        root,
        root,
        running,
        &mut photos,
        &mut failures,
        &mut observe,
    );
    photos.sort_by_key(|photo| photo.captured_at_utc);
    Ok((photos, failures, cancelled))
}

/// Scans one file. Used for incremental notify events so a new photo does not
/// trigger a full library walk. Failures are dropped; use
/// [`scan_one_detailed`] to inspect them.
pub fn scan_one(root: &Path, path: &Path) -> Option<ScannedPhoto> {
    scan_one_detailed(root, path).and_then(|result| result.ok())
}

/// Scans one file, distinguishing "not a source photo" from "could not scan".
///
/// `None` means the path is not a candidate source photo or no longer exists,
/// `Some(Err(_))` means a candidate could not be stat'ed or read, and
/// `Some(Ok(_))` is a valid photo.
pub fn scan_one_detailed(
    root: &Path,
    path: &Path,
) -> Option<std::result::Result<ScannedPhoto, ScanFailure>> {
    if !is_source_photo(path) {
        return None;
    }
    let rel_path = match contained_rel_path(root, path) {
        Ok(rel_path) => rel_path,
        // A path outside the configured root is not a photo for this root.
        Err(ContainmentError::Outside) => return None,
        // A symlink that resolves outside the root is a real anomaly, not a
        // normal "not a candidate" event; report it so the incremental indexer
        // can surface it.
        Err(ContainmentError::EscapesRoot) => {
            return Some(Err(ScanFailure::new(
                path,
                "撮影ルートの外を指すパスのため読み込みません".to_string(),
            )));
        }
        Err(ContainmentError::Unverifiable(message)) => {
            return Some(Err(ScanFailure::new(path, message)));
        }
    };
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        // A candidate that vanished between read_dir and stat is genuinely
        // gone; anything else (for example a directory without the execute
        // bit) must stay visible as a failure so a scan cannot mark its
        // siblings missing.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(Err(ScanFailure::new(
                path,
                format!("ファイルの情報を読み取れませんでした: {error}"),
            )));
        }
    };
    if !metadata.is_file() {
        return None;
    }
    Some(read_photo(path, rel_path))
}

enum ContainmentError {
    /// The path is lexically outside the root; it is simply not a candidate.
    Outside,
    /// The path is lexically inside, but symlink resolution leaves the root.
    EscapesRoot,
    /// Containment could not be verified (permissions, I/O, unresolvable
    /// root). Fail closed and surface the reason.
    Unverifiable(String),
}

/// Verifies that `path` is inside `root` and returns a relative path suitable
/// for the database.
///
/// The check has two parts:
///
/// 1. Lexical normalization removes `.` and `..` without touching the
///    filesystem, so a path that traverses out of the root is rejected before
///    any file is opened.
/// 2. Symlink resolution compares canonical paths, so a link (or a link in a
///    parent directory) that points outside the root is rejected.
///
/// The returned path is always a non-empty relative component. It can never
/// be absolute, unlike the previous `strip_prefix(...).unwrap_or(path)`.
fn contained_rel_path(root: &Path, path: &Path) -> std::result::Result<String, ContainmentError> {
    let cwd = std::env::current_dir().map_err(|error| {
        ContainmentError::Unverifiable(format!("現在のディレクトリを取得できませんでした: {error}"))
    })?;
    let root_abs = absolute_lexical(root, &cwd);
    let path_abs = absolute_lexical(path, &cwd);

    let relative = match path_abs.strip_prefix(&root_abs) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_path_buf(),
        _ => return Err(ContainmentError::Outside),
    };

    let canonical_root = std::fs::canonicalize(&root_abs).map_err(|error| {
        ContainmentError::Unverifiable(format!("撮影ルートを解決できませんでした: {error}"))
    })?;
    match std::fs::canonicalize(&path_abs) {
        Ok(canonical_path) => {
            if !canonical_path.starts_with(&canonical_root) {
                return Err(ContainmentError::EscapesRoot);
            }
        }
        // The candidate vanished. Its lexical location is already inside the
        // root, but verify the parent in case a symlinked directory points
        // outside while the target itself is gone.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path_abs.parent() {
                match std::fs::canonicalize(parent) {
                    Ok(canonical_parent) if canonical_parent.starts_with(&canonical_root) => {}
                    Ok(_) => return Err(ContainmentError::EscapesRoot),
                    Err(parent_error) if parent_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(parent_error) => {
                        return Err(ContainmentError::Unverifiable(format!(
                            "ファイルの親ディレクトリを解決できませんでした: {parent_error}"
                        )));
                    }
                }
            }
        }
        Err(error) => {
            return Err(ContainmentError::Unverifiable(format!(
                "ファイルパスを解決できませんでした: {error}"
            )));
        }
    }

    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn absolute_lexical(path: &Path, cwd: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    lexical_normalize(&path)
}

/// Resolves `.` and `..` using path components only. Leading `..` components
/// of a relative path are preserved; for an absolute path they stop at the
/// root, as the kernel would.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => normalized.push(".."),
            },
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn read_photo(path: &Path, rel_path: String) -> std::result::Result<ScannedPhoto, ScanFailure> {
    let failure = |message: String| ScanFailure::new(path, message);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| failure(format!("ファイル名が不正です: {}", path.display())))?
        .to_string();
    let metadata = std::fs::metadata(path)
        .map_err(|error| failure(format!("ファイルの情報を読み取れませんでした: {error}")))?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);

    let mut parsed = parse_photo_filename(&file_name).unwrap_or_else(|| {
        from_modified_time(metadata.modified().unwrap_or(std::time::SystemTime::now()))
    });
    match image_dimensions(path) {
        Some((width, height)) if width > 0 && height > 0 => {
            parsed.width = width;
            parsed.height = height;
        }
        _ => return Err(failure("画像の寸法を読み取れませんでした".to_string())),
    }

    Ok(ScannedPhoto {
        path: path.to_path_buf(),
        rel_path,
        file_name,
        captured_at_utc: parsed.captured_at_utc,
        captured_at_local: parsed.captured_at_local,
        tz_offset_sec: parsed.tz_offset_sec,
        width: parsed.width,
        height: parsed.height,
        file_size: metadata.len(),
        mtime,
    })
}

fn walk<F>(
    root: &Path,
    dir: &Path,
    running: Option<&AtomicBool>,
    photos: &mut Vec<ScannedPhoto>,
    failures: &mut Vec<ScanFailure>,
    observe: &mut F,
) -> bool
where
    F: FnMut(usize),
{
    if stopped(running) {
        return true;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(path = %dir.display(), %error, "cannot read directory");
            failures.push(ScanFailure::new(
                dir,
                format!("ディレクトリを読み取れませんでした: {error}"),
            ));
            return false;
        }
    };

    for entry in entries {
        if stopped(running) {
            return true;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "cannot read directory entry");
                failures.push(ScanFailure::new(
                    dir,
                    format!("ディレクトリ内の項目を読み取れませんでした: {error}"),
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot read file type");
                failures.push(ScanFailure::new(
                    &path,
                    format!("ファイルの種類を確認できませんでした: {error}"),
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
            if walk(root, &path, running, photos, failures, observe) {
                return true;
            }
            continue;
        }
        match scan_one_detailed(root, &path) {
            Some(Ok(photo)) => {
                photos.push(photo);
                observe(photos.len());
            }
            Some(Err(failure)) => failures.push(failure),
            None => {}
        }
    }
    false
}

fn stopped(running: Option<&AtomicBool>) -> bool {
    running.is_some_and(|flag| !flag.load(Ordering::Relaxed))
}

/// Scans and upserts all source photos under `root` into the library.
///
/// Per-photo failures are reported in [`ScanReport::failures`] and do not
/// abort the batch. The one fatal step is the preflight read of the batch's
/// existing scan states: if that read fails, the scan returns an error
/// before indexing anything.
pub fn scan_and_index(db: &Db, root_id: i64, root: &Path) -> Result<ScanReport> {
    scan_and_index_observed(db, root_id, root, |_| {})
}

/// The database operations a scan needs. Implemented for a plain `Db` (the
/// synchronous callers and tests) and for `Mutex<Db>` (the daemon, which must
/// not hold the lock while walking or hashing files).
trait ScanStore {
    fn photo_scan_states(&self, paths: &[String]) -> Result<HashMap<String, PhotoScanState>>;
    fn find_rebind_candidate(&self, photo: &NewPhoto) -> Result<Option<Photo>>;
    fn rebind_photo(&self, photo_id: i64, photo: &NewPhoto) -> Result<bool>;
    fn upsert_photo(&self, photo: &NewPhoto) -> Result<i64>;
}

impl ScanStore for Db {
    fn photo_scan_states(&self, paths: &[String]) -> Result<HashMap<String, PhotoScanState>> {
        Db::photo_scan_states(self, paths)
    }

    fn find_rebind_candidate(&self, photo: &NewPhoto) -> Result<Option<Photo>> {
        Db::find_rebind_candidate(self, photo)
    }

    fn rebind_photo(&self, photo_id: i64, photo: &NewPhoto) -> Result<bool> {
        Db::rebind_photo(self, photo_id, photo)
    }

    fn upsert_photo(&self, photo: &NewPhoto) -> Result<i64> {
        Db::upsert_photo(self, photo)
    }
}

impl ScanStore for Mutex<Db> {
    fn photo_scan_states(&self, paths: &[String]) -> Result<HashMap<String, PhotoScanState>> {
        with_db(self, |db| db.photo_scan_states(paths))
    }

    fn find_rebind_candidate(&self, photo: &NewPhoto) -> Result<Option<Photo>> {
        with_db(self, |db| db.find_rebind_candidate(photo))
    }

    fn rebind_photo(&self, photo_id: i64, photo: &NewPhoto) -> Result<bool> {
        with_db(self, |db| db.rebind_photo(photo_id, photo))
    }

    fn upsert_photo(&self, photo: &NewPhoto) -> Result<i64> {
        with_db(self, |db| db.upsert_photo(photo))
    }
}

fn with_db<T>(db: &Mutex<Db>, operation: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
    let guard = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    operation(&guard)
}

/// Scans and indexes one root, reporting walking and indexing progress.
///
/// The observer is called once per candidate photo during the walk and once
/// per processed photo during indexing. `total` is `None` while walking.
pub fn scan_and_index_observed<F>(
    db: &Db,
    root_id: i64,
    root: &Path,
    observe: F,
) -> Result<ScanReport>
where
    F: FnMut(ScanProgress),
{
    scan_and_index_with_store(db, root_id, root, None, observe)
}

/// Scans and indexes one root while taking the database mutex only for short
/// per-photo operations. The caller must not hold the mutex.
pub fn scan_and_index_shared<F>(
    db: &Mutex<Db>,
    root_id: i64,
    root: &Path,
    running: Option<&AtomicBool>,
    observe: F,
) -> Result<ScanReport>
where
    F: FnMut(ScanProgress),
{
    scan_and_index_with_store(db, root_id, root, running, observe)
}

fn scan_and_index_with_store<S, F>(
    db: &S,
    root_id: i64,
    root: &Path,
    running: Option<&AtomicBool>,
    mut observe: F,
) -> Result<ScanReport>
where
    S: ScanStore + ?Sized,
    F: FnMut(ScanProgress),
{
    let (photos, failures, walk_cancelled) =
        scan_root_report_with_progress_running(root, running, |found| {
            observe(ScanProgress {
                phase: ScanPhase::Walking,
                processed: found,
                total: None,
                indexed: 0,
            });
        })?;
    let total = photos.len();
    let mut report = ScanReport {
        found: total,
        failures,
        cancelled: walk_cancelled,
        ..Default::default()
    };
    if walk_cancelled {
        return Ok(report);
    }
    let paths: Vec<String> = photos
        .iter()
        .map(|photo| photo.path.to_string_lossy().to_string())
        .collect();
    let states = db.photo_scan_states(&paths)?;
    let mut processed = 0;
    observe(ScanProgress {
        phase: ScanPhase::Indexing,
        processed,
        total: Some(total),
        indexed: 0,
    });
    for photo in photos {
        if stopped(running) {
            report.cancelled = true;
            break;
        }
        let path = photo.path.to_string_lossy().to_string();
        report.seen_paths.push(path.clone());
        let existing = states.get(&path);
        // A row that still matches the file on disk needs no write and no
        // re-hash. `missing`, `replaced`, and `deleting` rows always fall
        // through so their state is restored or preserved. Unknown statuses
        // written by a newer version also fall through, and `upsert_photo`
        // preserves the stored status instead of rewriting it to present.
        let unchanged = existing.is_some_and(|state| {
            state.status == PhotoStatus::Present
                && state.display_derivative_id.is_none()
                && state.file_size == photo.file_size
                && state.mtime == photo.mtime
        });
        if unchanged {
            report.indexed += 1;
            processed += 1;
            observe(ScanProgress {
                phase: ScanPhase::Indexing,
                processed,
                total: Some(total),
                indexed: report.indexed,
            });
            continue;
        }
        let photo_path = photo.path.clone();
        let mut new_photo = photo.into_new_photo(root_id);
        let existed = existing.is_some();
        new_photo.blake3 = match existing {
            Some(existing)
                if existing.blake3.is_some()
                    && existing.file_size == new_photo.file_size
                    && existing.mtime == new_photo.mtime =>
            {
                existing.blake3.clone()
            }
            _ => hashing::file_blake3(Path::new(&new_photo.abs_path))
                .ok()
                .flatten(),
        };
        if stopped(running) {
            report.cancelled = true;
            break;
        }

        if !existed {
            // A database error while checking for a move is a per-photo
            // failure: whether this photo moved is unknown, so it is
            // reported instead of being indexed blind.
            match rebind_moved_photo(db, &new_photo) {
                Ok(Some(candidate_id)) => {
                    report.indexed += 1;
                    report.modified += 1;
                    report.rebound_ids.push(candidate_id);
                    processed += 1;
                    observe(ScanProgress {
                        phase: ScanPhase::Indexing,
                        processed,
                        total: Some(total),
                        indexed: report.indexed,
                    });
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(path = %new_photo.abs_path, %error, "cannot check photo move");
                    report.skipped += 1;
                    report
                        .failures
                        .push(ScanFailure::new(&photo_path, error.to_string()));
                    processed += 1;
                    observe(ScanProgress {
                        phase: ScanPhase::Indexing,
                        processed,
                        total: Some(total),
                        indexed: report.indexed,
                    });
                    continue;
                }
            }
        }

        match db.upsert_photo(&new_photo) {
            Ok(id) => {
                report.indexed += 1;
                report.modified += 1;
                if !existed {
                    report.new_ids.push(id);
                }
            }
            Err(error) => {
                tracing::warn!(path = %new_photo.abs_path, %error, "cannot index photo");
                report.skipped += 1;
                report
                    .failures
                    .push(ScanFailure::new(&photo_path, error.to_string()));
            }
        }
        processed += 1;
        observe(ScanProgress {
            phase: ScanPhase::Indexing,
            processed,
            total: Some(total),
            indexed: report.indexed,
        });
    }
    Ok(report)
}

/// Rebinds `new_photo` to the row of a photo that moved to its path.
///
/// A copy leaves the old path in place; only rebind when the old file is
/// gone, which distinguishes a move from a duplicate copy. Returns the
/// rebound row id, or `None` when there is no move candidate, the
/// candidate's old file still exists, or the rebind did not apply.
fn rebind_moved_photo<S: ScanStore + ?Sized>(db: &S, new_photo: &NewPhoto) -> Result<Option<i64>> {
    let Some(candidate) = db.find_rebind_candidate(new_photo)? else {
        return Ok(None);
    };
    if Path::new(&candidate.abs_path).exists() {
        return Ok(None);
    }
    if db.rebind_photo(candidate.id, new_photo)? {
        Ok(Some(candidate.id))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PhotoStatus;
    use image::{Rgba, RgbaImage};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn write_png(path: &Path) {
        let image = RgbaImage::from_pixel(4, 3, Rgba([10, 20, 30, 255]));
        image.save(path).unwrap();
    }

    #[test]
    fn observed_scan_reports_walking_and_indexing_progress() {
        let dir = tempdir().unwrap();
        for name in [
            "VRChat_2026-10-21_21-14-03.221_4x3.png",
            "VRChat_2026-10-21_21-14-04.221_4x3.png",
        ] {
            write_png(&dir.path().join(name));
        }
        let db = Db::open_in_memory().unwrap();
        let root_id = db
            .add_root(&dir.path().to_string_lossy(), "reference")
            .unwrap();
        let mut events = Vec::new();
        let report = scan_and_index_observed(&db, root_id, dir.path(), |progress| {
            events.push(progress);
        })
        .unwrap();
        assert_eq!(report.indexed, 2);
        assert!(
            events
                .iter()
                .any(|progress| progress.phase == ScanPhase::Walking && progress.total.is_none())
        );
        let last = events.last().unwrap();
        assert_eq!(last.phase, ScanPhase::Indexing);
        assert_eq!(last.total, Some(2));
        assert_eq!(last.processed, 2);
        assert_eq!(last.indexed, 2);
    }

    #[test]
    fn shared_scan_releases_the_db_lock_around_callbacks() {
        let dir = tempdir().unwrap();
        write_png(&dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let root_id = {
            let db = db.lock().unwrap();
            db.add_root(&dir.path().to_string_lossy(), "reference")
                .unwrap()
        };
        let probe = db.clone();
        let report = scan_and_index_shared(&db, root_id, dir.path(), None, |_| {
            assert!(
                probe.try_lock().is_ok(),
                "the scan called back while holding the database lock"
            );
        })
        .unwrap();
        assert_eq!(report.indexed, 1);
    }

    #[test]
    fn shared_scan_can_cancel_before_indexing() {
        let dir = tempdir().unwrap();
        write_png(&dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let root_id = {
            let db = db.lock().unwrap();
            db.add_root(&dir.path().to_string_lossy(), "reference")
                .unwrap()
        };
        let running = AtomicBool::new(false);
        let report =
            scan_and_index_shared(&db, root_id, dir.path(), Some(&running), |_| {}).unwrap();
        assert!(report.cancelled);
        assert_eq!(report.indexed, 0);
        assert_eq!(db.lock().unwrap().count_photos().unwrap(), 0);
    }

    #[test]
    fn scans_and_indexes_vrchat_photos() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_png(&root.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        std::fs::create_dir(root.join("compressed")).unwrap();
        write_png(
            &root
                .join("compressed")
                .join("VRChat_2026-10-21_21-14-03.221_4x3.png"),
        );

        let photos = scan_root(root).unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].width, 4);
        assert_eq!(photos[0].height, 3);

        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();
        assert_eq!(report.found, 1);
        assert_eq!(report.indexed, 1);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn scans_one_file_for_incremental_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let photo = scan_one(dir.path(), &path).expect("photo");
        assert_eq!(photo.width, 4);
        assert_eq!(photo.height, 3);
        assert_eq!(photo.rel_path, "VRChat_2026-10-21_21-14-03.221_4x3.png");
    }

    #[test]
    fn scan_one_reports_orientation_aware_dimensions() {
        let dir = tempdir().unwrap();
        let source = RgbaImage::from_pixel(200, 100, Rgba([10, 20, 30, 255]));
        for (orientation, minute) in [(6, "03"), (8, "04")] {
            let path = dir
                .path()
                .join(format!("VRChat_2026-10-21_21-14-{minute}.221_200x100.jpg"));
            crate::image_decode::test_support::write_jpeg_with_orientation(
                &path,
                &source,
                orientation,
            );

            let photo = scan_one(dir.path(), &path).expect("photo");

            assert_eq!(
                (photo.width, photo.height),
                (100, 200),
                "orientation {orientation}"
            );
        }
    }

    #[test]
    fn scan_one_accepts_a_normal_nested_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        let nested = root.join("2026").join("10");
        std::fs::create_dir_all(&nested).unwrap();
        let path = nested.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);

        let photo = scan_one(&root, &path).expect("nested photo");

        assert_eq!(
            photo.rel_path,
            "2026/10/VRChat_2026-10-21_21-14-03.221_4x3.png"
        );
        assert!(!Path::new(&photo.rel_path).is_absolute());
    }

    #[test]
    fn scan_one_normalizes_dot_dot_inside_the_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let path = root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let traversal = nested.join("..").join(path.file_name().unwrap());

        let photo = scan_one(&root, &traversal).expect("photo inside the root");

        assert_eq!(photo.rel_path, "VRChat_2026-10-21_21-14-03.221_4x3.png");
    }

    #[test]
    fn scan_one_rejects_parent_traversal_out_of_the_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&outside);
        let traversal = root.join("..").join(outside.file_name().unwrap());

        assert!(scan_one(&root, &traversal).is_none());
    }

    #[test]
    fn scan_one_rejects_an_absolute_path_outside_the_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let outside = other.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&outside);

        assert!(scan_one(&root, &outside).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn scan_one_rejects_a_symlink_that_escapes_the_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("photos");
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&outside);
        let link = root.join("linked.png");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let result = scan_one_detailed(&root, &link).expect("candidate path");
        let failure = result.expect_err("an escaping symlink must be rejected");
        assert!(
            failure.message.contains("撮影ルートの外"),
            "unexpected message: {}",
            failure.message
        );
        assert!(scan_one(&root, &link).is_none());
    }

    #[test]
    fn a_moved_photo_keeps_its_id_and_tags() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let old_path = root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&old_path);

        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();
        let photo_id = report.new_ids[0];
        db.set_photo_tags(photo_id, &["night".to_string()]).unwrap();

        let new_dir = root.join("2026");
        std::fs::create_dir(&new_dir).unwrap();
        let new_path = new_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        std::fs::rename(&old_path, &new_path).unwrap();

        let report = scan_and_index(&db, root_id, root).unwrap();
        assert_eq!(report.rebound_ids, vec![photo_id]);
        assert!(report.new_ids.is_empty());
        assert_eq!(report.modified, 1);
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert_eq!(photo.abs_path, new_path.to_string_lossy());
        assert_eq!(photo.status, PhotoStatus::Present);
        assert_eq!(db.photo_tags(photo_id).unwrap(), vec!["night".to_string()]);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn rescanning_preserves_a_photo_status_written_by_a_newer_version() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let path = root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();
        let photo_id = report.new_ids[0];
        db.connection()
            .execute(
                "UPDATE photos SET status = 'future_status' WHERE id = ?1",
                [photo_id],
            )
            .unwrap();

        scan_and_index(&db, root_id, root).unwrap();

        let status: String = db
            .connection()
            .query_row(
                "SELECT status FROM photos WHERE id = ?1",
                [photo_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "future_status");
        assert_eq!(
            db.photo_by_id(photo_id).unwrap().unwrap().status,
            PhotoStatus::Unknown
        );
    }

    #[test]
    fn a_copied_photo_is_not_merged_with_the_original() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let original = root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&original);

        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();
        let original_id = report.new_ids[0];

        let copy = root.join("copy.png");
        std::fs::copy(&original, &copy).unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();

        // The old path still exists, so this is a duplicate copy, not a move.
        assert!(report.rebound_ids.is_empty());
        assert_eq!(report.new_ids.len(), 1);
        assert_ne!(report.new_ids[0], original_id);
        assert_eq!(db.count_photos().unwrap(), 2);
    }

    #[test]
    fn corrupt_source_is_reported_but_other_photos_are_indexed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_png(&root.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        std::fs::write(root.join("broken.png"), b"not an image").unwrap();

        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();

        assert_eq!(report.indexed, 1);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].path.ends_with("broken.png"));
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn corrupt_vrchat_named_photo_is_reported_but_others_are_indexed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_png(&root.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        // A VRChat-style name whose bytes are not a readable image: the
        // name parses, but the dimensions cannot be read.
        std::fs::write(
            root.join("VRChat_2026-10-21_21-14-05.221_3840x2160.png"),
            b"not an image",
        )
        .unwrap();

        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
        let report = scan_and_index(&db, root_id, root).unwrap();

        assert_eq!(report.indexed, 1);
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0]
                .path
                .ends_with("VRChat_2026-10-21_21-14-05.221_3840x2160.png")
        );
        assert!(
            report.failures[0].message.contains("寸法"),
            "message should describe the dimension failure: {}",
            report.failures[0].message
        );
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn unchanged_photo_is_skipped_without_rehashing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let metadata = std::fs::metadata(&path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let db = Db::open_in_memory().unwrap();
        let root_id = db
            .add_root(dir.path().to_str().unwrap(), "reference")
            .unwrap();
        let id = db
            .upsert_photo(&NewPhoto {
                root_id,
                rel_path: "VRChat_2026-10-21_21-14-03.221_4x3.png".to_string(),
                abs_path: path.to_string_lossy().to_string(),
                file_name: "VRChat_2026-10-21_21-14-03.221_4x3.png".to_string(),
                captured_at_utc: 1_700_000_000,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 0,
                width: 4,
                height: 3,
                file_size: metadata.len(),
                mtime,
                blake3: None,
            })
            .unwrap();
        let before: String = db
            .connection()
            .query_row("SELECT updated_at FROM photos WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap();

        let report = scan_and_index(&db, root_id, dir.path()).unwrap();
        assert_eq!(report.indexed, 1);
        assert_eq!(report.modified, 0);
        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert!(photo.blake3.is_none());
        let after: String = db
            .connection()
            .query_row("SELECT updated_at FROM photos WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn changed_file_is_reindexed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let db = Db::open_in_memory().unwrap();
        let root_id = db
            .add_root(dir.path().to_str().unwrap(), "reference")
            .unwrap();
        scan_and_index(&db, root_id, dir.path()).unwrap();

        image::RgbaImage::from_pixel(8, 3, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let report = scan_and_index(&db, root_id, dir.path()).unwrap();
        assert_eq!(report.modified, 1);
        let photo = db.photo_by_path(path.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(photo.width, 8);
        assert_eq!(photo.file_size, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn mtime_only_change_is_reindexed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let db = Db::open_in_memory().unwrap();
        let root_id = db
            .add_root(dir.path().to_str().unwrap(), "reference")
            .unwrap();
        scan_and_index(&db, root_id, dir.path()).unwrap();
        let size_before = std::fs::metadata(&path).unwrap().len();

        // Touch only the mtime; the bytes (and therefore the size) stay the
        // same, so the skip check must fall through on the mtime mismatch.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime + std::time::Duration::from_secs(3600))
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size_before);

        let report = scan_and_index(&db, root_id, dir.path()).unwrap();
        assert_eq!(report.modified, 1);
        let photo = db.photo_by_path(path.to_str().unwrap()).unwrap().unwrap();
        let new_mtime = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(photo.mtime, new_mtime);
    }

    #[test]
    fn size_only_change_is_reindexed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let db = Db::open_in_memory().unwrap();
        let root_id = db
            .add_root(dir.path().to_str().unwrap(), "reference")
            .unwrap();
        scan_and_index(&db, root_id, dir.path()).unwrap();

        // Rewrite with different dimensions, then restore the original
        // mtime so only the size differs from the indexed row.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        image::RgbaImage::from_pixel(8, 3, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let report = scan_and_index(&db, root_id, dir.path()).unwrap();
        assert_eq!(report.modified, 1);
        let photo = db.photo_by_path(path.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(photo.width, 8);
        assert_eq!(photo.file_size, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn a_reappearing_missing_photo_becomes_present() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&path);
        let db = Db::open_in_memory().unwrap();
        let root_id = db
            .add_root(dir.path().to_str().unwrap(), "reference")
            .unwrap();
        let first = scan_and_index(&db, root_id, dir.path()).unwrap();
        db.mark_missing(first.new_ids[0]).unwrap();

        let report = scan_and_index(&db, root_id, dir.path()).unwrap();
        assert_eq!(report.modified, 1);
        let photo = db.photo_by_id(first.new_ids[0]).unwrap().unwrap();
        assert_eq!(photo.status, PhotoStatus::Present);
    }

    #[test]
    fn nested_root_scan_skips_a_photo_owned_by_another_root() {
        let dir = tempdir().unwrap();
        let parent = dir.path();
        let child = parent.join("2026");
        std::fs::create_dir(&child).unwrap();
        write_png(&child.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
        let db = Db::open_in_memory().unwrap();
        let child_id = db.add_root(child.to_str().unwrap(), "reference").unwrap();
        let first = scan_and_index(&db, child_id, &child).unwrap();
        let parent_id = db.add_root(parent.to_str().unwrap(), "reference").unwrap();

        let report = scan_and_index(&db, parent_id, parent).unwrap();
        assert_eq!(report.modified, 0);
        assert_eq!(db.count_photos().unwrap(), 1);
        assert_eq!(
            db.photo_by_path(
                child
                    .join("VRChat_2026-10-21_21-14-03.221_4x3.png")
                    .to_str()
                    .unwrap()
            )
            .unwrap()
            .unwrap()
            .id,
            first.new_ids[0]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Root ignores the permission bits, so the directory stays readable
        // and there is no failure to report.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "skipping unreadable-directory assertions: running with elevated permissions"
            );
            return;
        }

        let (photos, failures) = scan_root_report(dir.path()).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(photos.is_empty());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path, locked);
    }

    #[cfg(unix)]
    #[test]
    fn stat_failure_in_listable_directory_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let photo = locked.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&photo);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();

        // A mode-0444 directory can be listed but its children cannot be
        // stat'ed without the execute bit. Root and some filesystems ignore
        // that, so only run the assertion when the lock is effective.
        let child_type = std::fs::read_dir(&locked)
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(|entry| entry.ok())
            .and_then(|entry| entry.file_type().ok());
        if std::fs::metadata(&photo).is_ok() || !child_type.is_some_and(|kind| kind.is_file()) {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping stat lock: cannot simulate a listable but unstatable directory");
            return;
        }

        let (photos, failures) = scan_root_report(dir.path()).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(photos.is_empty(), "unreadable photo must not be returned");
        assert_eq!(failures.len(), 1, "failures: {failures:?}");
        assert_eq!(failures[0].path, photo);
        assert!(
            failures[0].message.contains("読み取れませんでした")
                || failures[0].message.contains("解決できませんでした"),
            "message should describe the stat/resolution failure: {}",
            failures[0].message
        );
    }
}
