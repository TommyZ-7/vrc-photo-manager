use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::hashing;
use crate::model::{NewPhoto, PhotoStatus};
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
    mut observe: F,
) -> Result<(Vec<ScannedPhoto>, Vec<ScanFailure>)>
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
    walk(root, root, &mut photos, &mut failures, &mut observe);
    photos.sort_by_key(|photo| photo.captured_at_utc);
    Ok((photos, failures))
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
                format!("cannot read file metadata: {error}"),
            )));
        }
    };
    if !metadata.is_file() {
        return None;
    }
    Some(read_photo(root, path))
}

fn read_photo(root: &Path, path: &Path) -> std::result::Result<ScannedPhoto, ScanFailure> {
    let failure = |message: String| ScanFailure::new(path, message);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| failure(format!("invalid file name: {}", path.display())))?
        .to_string();
    let metadata = std::fs::metadata(path)
        .map_err(|error| failure(format!("cannot read file metadata: {error}")))?;
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
        _ => return Err(failure("cannot read image dimensions".to_string())),
    }

    let rel_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");

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
    photos: &mut Vec<ScannedPhoto>,
    failures: &mut Vec<ScanFailure>,
    observe: &mut F,
) where
    F: FnMut(usize),
{
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(path = %dir.display(), %error, "cannot read directory");
            failures.push(ScanFailure::new(dir, error.to_string()));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "cannot read directory entry");
                failures.push(ScanFailure::new(dir, error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot read file type");
                failures.push(ScanFailure::new(&path, error.to_string()));
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
            walk(root, &path, photos, failures, observe);
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
}

/// Scans and upserts all source photos under `root` into the library.
///
/// Per-photo failures are reported in [`ScanReport::failures`] and do not
/// abort the batch.
pub fn scan_and_index(db: &Db, root_id: i64, root: &Path) -> Result<ScanReport> {
    scan_and_index_observed(db, root_id, root, |_| {})
}

/// Scans and indexes one root, reporting walking and indexing progress.
///
/// The observer is called once per candidate photo during the walk and once
/// per processed photo during indexing. `total` is `None` while walking.
pub fn scan_and_index_observed<F>(
    db: &Db,
    root_id: i64,
    root: &Path,
    mut observe: F,
) -> Result<ScanReport>
where
    F: FnMut(ScanProgress),
{
    let (photos, failures) = scan_root_report_with_progress(root, |found| {
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
        ..Default::default()
    };
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
        let path = photo.path.to_string_lossy().to_string();
        report.seen_paths.push(path.clone());
        let existing = states.get(&path);
        // A row that still matches the file on disk needs no write and no
        // re-hash. `missing`, `replaced`, and `deleting` rows always fall
        // through so their state is restored or preserved.
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

        if !existed && let Some(candidate) = db.find_rebind_candidate(&new_photo)? {
            // A copy leaves the old path in place; only rebind when the old
            // file is gone, which distinguishes a move from a duplicate copy.
            let old_path_exists = std::path::Path::new(&candidate.abs_path).exists();
            if !old_path_exists && db.rebind_photo(candidate.id, &new_photo)? {
                report.indexed += 1;
                report.modified += 1;
                report.rebound_ids.push(candidate.id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PhotoStatus;
    use image::{Rgba, RgbaImage};
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
            failures[0].message.contains("metadata"),
            "message should describe the stat failure: {}",
            failures[0].message
        );
    }
}
