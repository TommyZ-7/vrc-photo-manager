use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{CoreError, Result};

pub const THUMBNAIL_MAX: u32 = 256;
const THUMBNAIL_QUALITY: f32 = 80.0;
const ACCESS_REFRESH_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThumbnailCacheUsage {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThumbnailPruneReport {
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub remaining_files: usize,
    pub remaining_bytes: u64,
}

struct ThumbnailEntry {
    modified: SystemTime,
    bytes: u64,
    path: PathBuf,
}

pub fn thumbnails_dir() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("thumbnails"))
}

pub fn thumbnail_path(photo_id: i64) -> Option<PathBuf> {
    thumbnails_dir().map(|dir| dir.join(format!("{photo_id}.webp")))
}

/// Total size of the thumbnail cache, counting only `<u64>.webp` files.
pub fn thumbnail_cache_usage_in(cache_dir: &Path) -> Result<ThumbnailCacheUsage> {
    let entries = match collect_thumbnail_entries(cache_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ThumbnailCacheUsage::default());
        }
        Err(error) => return Err(CoreError::Io(error)),
    };
    Ok(ThumbnailCacheUsage {
        files: entries.len(),
        bytes: entries
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.bytes)),
    })
}

/// Removes the least recently modified thumbnails until the cache fits in
/// `max_bytes`. A limit of zero means unlimited.
pub fn prune_thumbnails_in(cache_dir: &Path, max_bytes: u64) -> Result<ThumbnailPruneReport> {
    let mut entries = match collect_thumbnail_entries(cache_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ThumbnailPruneReport::default());
        }
        Err(error) => return Err(CoreError::Io(error)),
    };
    let scanned_files = entries.len();
    let scanned_bytes = entries
        .iter()
        .fold(0u64, |total, entry| total.saturating_add(entry.bytes));
    let mut report = ThumbnailPruneReport {
        scanned_files,
        scanned_bytes,
        remaining_files: scanned_files,
        remaining_bytes: scanned_bytes,
        ..ThumbnailPruneReport::default()
    };
    if max_bytes == 0 || scanned_bytes <= max_bytes {
        return Ok(report);
    }

    entries.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut remaining = scanned_bytes;
    for entry in entries {
        if remaining <= max_bytes {
            break;
        }
        match fs::remove_file(&entry.path) {
            Ok(()) => {
                report.removed_files += 1;
                report.removed_bytes = report.removed_bytes.saturating_add(entry.bytes);
                remaining = remaining.saturating_sub(entry.bytes);
            }
            Err(error) => tracing::debug!(
                %error,
                path = %entry.path.display(),
                "cannot remove thumbnail cache entry"
            ),
        }
    }
    report.remaining_files = scanned_files.saturating_sub(report.removed_files);
    report.remaining_bytes = remaining;
    Ok(report)
}

fn collect_thumbnail_entries(cache_dir: &Path) -> std::io::Result<Vec<ThumbnailEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(cache_dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(
                    %error,
                    dir = %cache_dir.display(),
                    "cannot read thumbnail cache entry"
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("webp") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem.parse::<u64>().is_err() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        entries.push(ThumbnailEntry {
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            bytes: metadata.len(),
            path,
        });
    }
    Ok(entries)
}

/// Marks a cached thumbnail as recently used by refreshing its mtime.
///
/// Display paths call this on a cache hit. The daemon deliberately does not:
/// its startup pass over every photo would otherwise promote the whole cache.
pub fn refresh_thumbnail_access(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return;
    };
    if age < ACCESS_REFRESH_AFTER {
        return;
    }
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => {
            if let Err(error) = file.set_modified(SystemTime::now()) {
                tracing::debug!(
                    %error,
                    path = %path.display(),
                    "cannot refresh thumbnail access time"
                );
            }
        }
        Err(error) => tracing::debug!(
            %error,
            path = %path.display(),
            "cannot open thumbnail to refresh access time"
        ),
    }
}

/// Creates a 256px WebP thumbnail if it does not exist yet.
///
/// The file is written atomically. Returns `None` when the source is missing
/// or cannot be decoded, so callers can keep a placeholder.
pub fn ensure_thumbnail(photo_id: i64, source: &Path) -> Result<Option<PathBuf>> {
    let Some(cache_dir) = thumbnails_dir() else {
        return Ok(None);
    };
    ensure_thumbnail_in(photo_id, source, &cache_dir)
}

/// Same as [`ensure_thumbnail`] but with an explicit cache directory. Used by
/// tests and by callers that keep the cache outside the user data directory.
pub fn ensure_thumbnail_in(
    photo_id: i64,
    source: &Path,
    cache_dir: &Path,
) -> Result<Option<PathBuf>> {
    let output = cache_dir.join(format!("{photo_id}.webp"));
    if output.is_file()
        && fs::metadata(&output)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    {
        return Ok(Some(output));
    }
    if !source.is_file() {
        return Ok(None);
    }
    let image = match crate::image_decode::decode_image(source) {
        Some(image) => image.thumbnail(THUMBNAIL_MAX, THUMBNAIL_MAX).to_rgba8(),
        None => {
            tracing::warn!(path = %source.display(), "cannot decode thumbnail source");
            return Ok(None);
        }
    };
    let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
    let encoded = encoder.encode(THUMBNAIL_QUALITY);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = output.with_extension("webp.tmp");
    fs::write(&temp, &*encoded)?;
    if let Err(error) = fs::rename(&temp, &output) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::debug!(
                %cleanup,
                photo_id,
                path = %temp.display(),
                "cannot remove temporary thumbnail after rename failure"
            );
        }
        return Err(CoreError::Io(error));
    }
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn write_cache_entry(path: &Path, bytes: usize, modified: SystemTime) {
        fs::write(path, vec![0u8; bytes]).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn prune_thumbnails_removes_oldest_until_under_limit() {
        let dir = tempdir().unwrap();
        let now = SystemTime::now();
        write_cache_entry(
            &dir.path().join("1.webp"),
            1000,
            now - Duration::from_secs(30),
        );
        write_cache_entry(
            &dir.path().join("2.webp"),
            1000,
            now - Duration::from_secs(20),
        );
        write_cache_entry(
            &dir.path().join("3.webp"),
            1000,
            now - Duration::from_secs(10),
        );
        write_cache_entry(&dir.path().join("4.webp"), 1000, now);

        let report = prune_thumbnails_in(dir.path(), 2500).unwrap();

        assert_eq!(report.scanned_files, 4);
        assert_eq!(report.scanned_bytes, 4000);
        assert_eq!(report.removed_files, 2);
        assert_eq!(report.removed_bytes, 2000);
        assert_eq!(report.remaining_files, 2);
        assert_eq!(report.remaining_bytes, 2000);
        assert!(!dir.path().join("1.webp").exists());
        assert!(!dir.path().join("2.webp").exists());
        assert!(dir.path().join("3.webp").exists());
        assert!(dir.path().join("4.webp").exists());
    }

    #[test]
    fn thumbnail_cache_usage_counts_numeric_webp_only() {
        let dir = tempdir().unwrap();
        write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());
        write_cache_entry(&dir.path().join("2.webp"), 2000, SystemTime::now());
        write_cache_entry(&dir.path().join("keep.webp"), 3000, SystemTime::now());
        write_cache_entry(&dir.path().join("3.webp.tmp"), 4000, SystemTime::now());

        let usage = thumbnail_cache_usage_in(dir.path()).unwrap();

        assert_eq!(usage.files, 2);
        assert_eq!(usage.bytes, 3000);
    }

    #[test]
    fn prune_thumbnails_keeps_files_when_under_limit() {
        let dir = tempdir().unwrap();
        write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());

        let report = prune_thumbnails_in(dir.path(), 2000).unwrap();

        assert_eq!(report.removed_files, 0);
        assert_eq!(report.remaining_bytes, 1000);
        assert!(dir.path().join("1.webp").exists());
    }

    #[test]
    fn prune_thumbnails_zero_limit_is_unlimited() {
        let dir = tempdir().unwrap();
        write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());

        let report = prune_thumbnails_in(dir.path(), 0).unwrap();

        assert_eq!(report.removed_files, 0);
        assert_eq!(report.remaining_files, 1);
        assert!(dir.path().join("1.webp").exists());
    }

    #[test]
    fn prune_thumbnails_ignores_non_thumbnail_files() {
        let dir = tempdir().unwrap();
        let now = SystemTime::now();
        write_cache_entry(
            &dir.path().join("1.webp"),
            5000,
            now - Duration::from_secs(10),
        );
        write_cache_entry(
            &dir.path().join("keep.webp"),
            5000,
            now - Duration::from_secs(20),
        );
        write_cache_entry(
            &dir.path().join("2.webp.tmp"),
            5000,
            now - Duration::from_secs(30),
        );
        fs::write(dir.path().join("notes.txt"), vec![0u8; 5000]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();

        let report = prune_thumbnails_in(dir.path(), 1000).unwrap();

        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.removed_files, 1);
        assert!(!dir.path().join("1.webp").exists());
        assert!(dir.path().join("keep.webp").exists());
        assert!(dir.path().join("2.webp.tmp").exists());
        assert!(dir.path().join("notes.txt").exists());
        assert!(dir.path().join("sub").is_dir());
    }

    #[test]
    fn prune_thumbnails_missing_directory_is_empty() {
        let dir = tempdir().unwrap();

        let report = prune_thumbnails_in(&dir.path().join("missing"), 1024).unwrap();

        assert_eq!(report.scanned_files, 0);
        assert_eq!(report.removed_files, 0);
    }

    #[test]
    fn prune_thumbnails_removes_every_entry_when_limit_is_below_one_file() {
        let dir = tempdir().unwrap();
        write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());

        let report = prune_thumbnails_in(dir.path(), 1).unwrap();

        assert_eq!(report.removed_files, 1);
        assert_eq!(report.remaining_bytes, 0);
        assert!(!dir.path().join("1.webp").exists());
    }

    #[test]
    fn refresh_thumbnail_access_rewrites_a_stale_mtime() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.webp");
        let stale = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        write_cache_entry(&path, 10, stale);
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        refresh_thumbnail_access(&path);

        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after > before);
    }

    #[test]
    fn refresh_thumbnail_access_leaves_a_recent_mtime_alone() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.webp");
        let recent = SystemTime::now() - Duration::from_secs(5);
        write_cache_entry(&path, 10, recent);
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        refresh_thumbnail_access(&path);

        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn creates_and_reuses_thumbnail() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        RgbaImage::from_pixel(640, 360, Rgba([10, 20, 30, 255]))
            .save(&source)
            .unwrap();
        let first = ensure_thumbnail_in(999_999, &source, dir.path())
            .unwrap()
            .expect("thumbnail");
        assert!(first.is_file());
        let size = image::image_dimensions(&first).unwrap();
        assert!(size.0 <= THUMBNAIL_MAX && size.1 <= THUMBNAIL_MAX);
        let second = ensure_thumbnail_in(999_999, &source, dir.path())
            .unwrap()
            .expect("cached");
        assert_eq!(first, second);
    }

    #[test]
    fn missing_source_is_not_an_error() {
        let dir = tempdir().unwrap();
        assert!(
            ensure_thumbnail_in(999_998, Path::new("/no/such/file.png"), dir.path())
                .unwrap()
                .is_none()
        );
    }
}
