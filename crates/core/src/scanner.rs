use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::model::NewPhoto;
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
    pub new_ids: Vec<i64>,
    pub seen_paths: Vec<String>,
    pub failures: Vec<ScanFailure>,
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
    if !root.is_dir() {
        return Err(CoreError::Invalid(format!(
            "photo root is not a directory: {}",
            root.display()
        )));
    }
    let mut photos = Vec::new();
    let mut failures = Vec::new();
    walk(root, root, &mut photos, &mut failures);
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
/// `None` means the path is not a candidate source photo, `Some(Err(_))` means
/// a candidate could not be read or has no readable image dimensions, and
/// `Some(Ok(_))` is a valid photo.
pub fn scan_one_detailed(
    root: &Path,
    path: &Path,
) -> Option<std::result::Result<ScannedPhoto, ScanFailure>> {
    if !path.is_file() || !is_source_photo(path) {
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
    let metadata = std::fs::metadata(path).map_err(|error| failure(error.to_string()))?;
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

fn walk(root: &Path, dir: &Path, photos: &mut Vec<ScannedPhoto>, failures: &mut Vec<ScanFailure>) {
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
            walk(root, &path, photos, failures);
            continue;
        }
        match scan_one_detailed(root, &path) {
            Some(Ok(photo)) => photos.push(photo),
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
    let (photos, failures) = scan_root_report(root)?;
    let mut report = ScanReport {
        found: photos.len(),
        failures,
        ..Default::default()
    };
    for photo in photos {
        let path = photo.path.to_string_lossy().to_string();
        report.seen_paths.push(path.clone());
        let photo_path = photo.path.clone();
        let new_photo = photo.into_new_photo(root_id);
        let existed = db.photo_by_path(&path)?.is_some();
        match db.upsert_photo(&new_photo) {
            Ok(id) => {
                report.indexed += 1;
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
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use tempfile::tempdir;

    fn write_png(path: &Path) {
        let image = RgbaImage::from_pixel(4, 3, Rgba([10, 20, 30, 255]));
        image.save(path).unwrap();
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
}
