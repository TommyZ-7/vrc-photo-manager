use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use image::RgbaImage;

/// Longest edge of a cached preview, matching the inspector and viewer.
pub const PREVIEW_MAX: u32 = 1200;
const PREVIEW_QUALITY: f32 = 85.0;
const PREVIEW_CACHE_MAX_FILES: usize = 2048;
const PREVIEW_CACHE_PRUNE_TO: usize = 1536;
const PRUNE_EVERY: usize = 32;

static WRITES: AtomicUsize = AtomicUsize::new(0);

pub fn previews_dir() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("previews"))
}

pub fn decode_preview(source: &Path) -> Option<RgbaImage> {
    let image = crate::image_decode::decode_image(source)?;
    Some(image.thumbnail(PREVIEW_MAX, PREVIEW_MAX).to_rgba8())
}

pub fn ensure_preview(photo_id: i64, variant: i64, source: &Path) -> Option<RgbaImage> {
    let dir = previews_dir()?;
    ensure_preview_in(photo_id, variant, source, &dir)
}

/// Loads a 1200 px preview from `cache_dir`, regenerating it from `source`
/// when the cache is missing, stale, or unreadable.
///
/// A cache write failure does not fail the preview: the decoded image is
/// returned anyway so the UI can display it for this selection.
pub fn ensure_preview_in(
    photo_id: i64,
    variant: i64,
    source: &Path,
    cache_dir: &Path,
) -> Option<RgbaImage> {
    let cache = cache_dir.join(format!("{photo_id}-{variant}.webp"));
    if cache_is_fresh(&cache, source) {
        if let Some(image) = read_cache(&cache) {
            return Some(image);
        }
        if let Err(error) = fs::remove_file(&cache) {
            tracing::debug!(
                %error,
                photo_id = photo_id,
                path = %cache.display(),
                "cannot remove corrupt preview cache"
            );
        }
    }
    let image = match decode_preview(source) {
        Some(image) => image,
        None => {
            tracing::warn!(
                photo_id = photo_id,
                path = %source.display(),
                "cannot decode preview source"
            );
            return None;
        }
    };
    if write_cache(&cache, &image) {
        maybe_prune(cache_dir);
    }
    Some(image)
}

fn cache_is_fresh(cache: &Path, source: &Path) -> bool {
    let Ok(cache_meta) = fs::metadata(cache) else {
        return false;
    };
    if cache_meta.len() == 0 {
        return false;
    }
    let Ok(source_meta) = fs::metadata(source) else {
        // A missing source is not a reason to discard a usable preview.
        return true;
    };
    let (Ok(cache_time), Ok(source_time)) = (cache_meta.modified(), source_meta.modified()) else {
        return true;
    };
    source_time <= cache_time
}

fn read_cache(cache: &Path) -> Option<RgbaImage> {
    let image = crate::image_decode::decode_image(cache)?;
    Some(image.to_rgba8())
}

fn write_cache(cache: &Path, image: &RgbaImage) -> bool {
    let Some(parent) = cache.parent() else {
        return false;
    };
    if let Err(error) = fs::create_dir_all(parent) {
        tracing::warn!(
            %error,
            path = %parent.display(),
            "cannot create preview cache directory"
        );
        return false;
    }
    let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
    let encoded = encoder.encode(PREVIEW_QUALITY);
    let temp = cache.with_extension("webp.tmp");
    if let Err(error) = fs::write(&temp, &*encoded) {
        tracing::warn!(
            %error,
            path = %temp.display(),
            "cannot write preview cache"
        );
        return false;
    }
    if let Err(error) = fs::rename(&temp, cache) {
        tracing::warn!(
            %error,
            path = %cache.display(),
            "cannot replace preview cache"
        );
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::debug!(
                %cleanup,
                path = %temp.display(),
                "cannot remove temporary preview after rename failure"
            );
        }
        return false;
    }
    true
}

fn maybe_prune(cache_dir: &Path) {
    let writes = WRITES.fetch_add(1, Ordering::Relaxed) + 1;
    if !writes.is_multiple_of(PRUNE_EVERY) {
        return;
    }
    match prune_previews_in(cache_dir, PREVIEW_CACHE_MAX_FILES, PREVIEW_CACHE_PRUNE_TO) {
        Ok(0) => {}
        Ok(removed) => {
            tracing::debug!(removed = removed, dir = %cache_dir.display(), "pruned preview cache");
        }
        Err(error) => {
            tracing::warn!(%error, dir = %cache_dir.display(), "cannot prune preview cache");
        }
    }
}

fn prune_previews_in(
    cache_dir: &Path,
    max_files: usize,
    prune_to: usize,
) -> std::io::Result<usize> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(cache_dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("webp") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((modified, path));
    }
    if entries.len() <= max_files {
        return Ok(0);
    }
    entries.sort_by_key(|(modified, _)| *modified);
    let remove_count = entries.len().saturating_sub(prune_to);
    let mut removed = 0;
    for (_, path) in entries.into_iter().take(remove_count) {
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::debug!(
                    %error,
                    path = %path.display(),
                    "cannot remove preview cache entry"
                );
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn write_png(path: &Path, width: u32, height: u32, color: [u8; 4]) {
        RgbaImage::from_pixel(width, height, Rgba(color))
            .save(path)
            .unwrap();
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn creates_a_scaled_preview_and_reuses_it_without_the_source() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 1600, 1000, [255, 0, 0, 255]);

        let first = ensure_preview_in(7, 0, &source, &cache).expect("preview");
        assert_eq!(first.dimensions(), (1200, 750));
        assert!(cache.join("7-0.webp").is_file());
        let pixel = first.get_pixel(0, 0).0;
        assert!(pixel[0] > 240 && pixel[1] < 16 && pixel[2] < 16);

        fs::remove_file(&source).unwrap();
        let second = ensure_preview_in(7, 0, &source, &cache).expect("cached preview");
        assert_eq!(second.dimensions(), (1200, 750));
        let pixel = second.get_pixel(0, 0).0;
        assert!(pixel[0] > 240 && pixel[1] < 16 && pixel[2] < 16);
    }

    #[test]
    fn keeps_display_variants_separate() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [0, 255, 0, 255]);

        assert!(ensure_preview_in(8, 0, &source, &cache).is_some());
        assert!(ensure_preview_in(8, 42, &source, &cache).is_some());
        assert!(cache.join("8-0.webp").is_file());
        assert!(cache.join("8-42.webp").is_file());
    }

    #[test]
    fn missing_source_and_cache_returns_none() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("missing.png");
        assert!(ensure_preview_in(9, 0, &source, &dir.path().join("previews")).is_none());
    }

    #[test]
    fn regenerates_when_the_source_is_newer_than_the_cache() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [255, 0, 0, 255]);
        assert!(ensure_preview_in(10, 0, &source, &cache).is_some());

        let cache_path = cache.join("10-0.webp");
        let older = fs::metadata(&cache_path).unwrap().modified().unwrap() - Duration::from_secs(2);
        set_mtime(&cache_path, older);
        write_png(&source, 320, 200, [0, 0, 255, 255]);

        let refreshed = ensure_preview_in(10, 0, &source, &cache).expect("regenerated");
        let pixel = refreshed.get_pixel(0, 0).0;
        assert!(pixel[2] > 240 && pixel[0] < 16 && pixel[1] < 16);
    }

    #[test]
    fn repairs_a_corrupt_cache_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [200, 100, 50, 255]);
        assert!(ensure_preview_in(11, 0, &source, &cache).is_some());

        fs::write(cache.join("11-0.webp"), b"not an image").unwrap();
        let repaired = ensure_preview_in(11, 0, &source, &cache).expect("repaired");
        let pixel = repaired.get_pixel(0, 0).0;
        assert!(pixel[0] > 180 && pixel[1] > 80 && pixel[2] > 30);
    }

    #[test]
    fn prunes_only_the_oldest_cache_files() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("previews");
        fs::create_dir_all(&cache).unwrap();
        let unrelated = cache.join("keep.txt");
        fs::write(&unrelated, b"keep").unwrap();
        for index in 0..6 {
            let path = cache.join(format!("{index}-0.webp"));
            fs::write(&path, b"preview").unwrap();
            set_mtime(
                &path,
                SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
            );
        }

        let removed = prune_previews_in(&cache, 4, 2).unwrap();
        assert_eq!(removed, 4);
        assert!(!cache.join("0-0.webp").exists());
        assert!(!cache.join("3-0.webp").exists());
        assert!(cache.join("4-0.webp").exists());
        assert!(cache.join("5-0.webp").exists());
        assert!(unrelated.exists());
    }
}
