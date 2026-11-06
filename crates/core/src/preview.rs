use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use image::RgbaImage;

use crate::cache_metadata;

/// Longest edge of a cached preview, matching the inspector and viewer.
pub const PREVIEW_MAX: u32 = 1200;
/// Longest edge kept for the Quick Look full-resolution image.
pub const FULL_IMAGE_MAX: u32 = 8192;
const PREVIEW_QUALITY: f32 = 85.0;
const PREVIEW_CACHE_MAX_FILES: usize = 2048;
const PREVIEW_CACHE_PRUNE_TO: usize = 1536;
const PRUNE_EVERY: usize = 32;
/// Temp files left behind by a crash between write and rename are pruned
/// once they are older than this; younger ones may still be in flight.
const TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

static WRITES: AtomicUsize = AtomicUsize::new(0);

/// A temp file path unique to this writer (process + sequence), so two
/// writers racing the same cache entry never share a temp file: the last
/// rename publishes a complete file, and a failed writer only ever removes
/// its own temp.
fn temp_path_for(cache: &Path) -> PathBuf {
    cache_metadata::unique_temp_path(cache)
}

pub fn previews_dir() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("previews"))
}

/// Removes every cached preview and sidecar for one photo, regardless of
/// derivative variant or temp files left by a failed writer.
///
/// Returns how many files were removed. A `replace` compression calls this so
/// the old display derivative's preview cannot be shown again.
pub fn remove_preview_files(photo_id: i64) -> std::io::Result<usize> {
    match previews_dir() {
        Some(dir) => remove_preview_files_in(&dir, photo_id),
        None => Ok(0),
    }
}

/// Same as [`remove_preview_files`] with an explicit cache directory.
pub fn remove_preview_files_in(cache_dir: &Path, photo_id: i64) -> std::io::Result<usize> {
    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(
                    %error,
                    dir = %cache_dir.display(),
                    "cannot read preview cache entry while removing a photo"
                );
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !preview_artifact_belongs_to(&name, photo_id) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) => tracing::debug!(
                %error,
                path = %path.display(),
                "cannot remove preview for photo"
            ),
        }
    }
    Ok(removed)
}

fn preview_artifact_belongs_to(name: &str, photo_id: i64) -> bool {
    let Some(stem) = preview_artifact_stem(name) else {
        return false;
    };
    let mut parts = stem.split('-');
    let Some(id) = parts.next().and_then(|id| id.parse::<i64>().ok()) else {
        return false;
    };
    let Some(variant) = parts.next().and_then(|variant| variant.parse::<i64>().ok()) else {
        return false;
    };
    parts.next().is_none() && id == photo_id && variant >= 0
}

/// Returns the `<photo>-<variant>` stem before a known preview suffix.
fn preview_artifact_stem(name: &str) -> Option<&str> {
    if let Some(stem) = name.strip_suffix(".webp") {
        return Some(stem);
    }
    if let Some(stem) = name.strip_suffix(".webp.meta") {
        return Some(stem);
    }
    if let Some((stem, suffix)) = name.split_once(".webp.meta.tmp") {
        return is_preview_temp_suffix(suffix).then_some(stem);
    }
    if let Some((stem, suffix)) = name.split_once(".webp.tmp") {
        return is_preview_temp_suffix(suffix).then_some(stem);
    }
    None
}

fn is_preview_temp_suffix(suffix: &str) -> bool {
    let Some(rest) = suffix.strip_prefix('-') else {
        return suffix.is_empty();
    };
    let mut parts = rest.split('-');
    let Some(pid) = parts.next() else {
        return false;
    };
    let Some(sequence) = parts.next() else {
        return false;
    };
    parts.next().is_none() && pid.parse::<u32>().is_ok() && sequence.parse::<u64>().is_ok()
}

pub fn decode_preview(source: &Path) -> Option<RgbaImage> {
    let image = crate::image_decode::decode_image(source)?;
    Some(image.thumbnail(PREVIEW_MAX, PREVIEW_MAX).into_rgba8())
}

/// Decodes the display file for Quick Look without the 1200 px preview cap.
/// Images under [`FULL_IMAGE_MAX`] keep their original pixels; larger ones are
/// downscaled once so the memory ceiling is bounded.
pub fn decode_full_image(source: &Path) -> Option<RgbaImage> {
    decode_limited(source, FULL_IMAGE_MAX)
}

fn decode_limited(source: &Path, max_edge: u32) -> Option<RgbaImage> {
    let image = crate::image_decode::decode_image(source)?;
    let (width, height) = (image.width(), image.height());
    if width.max(height) <= max_edge {
        Some(image.into_rgba8())
    } else {
        Some(image.thumbnail(max_edge, max_edge).into_rgba8())
    }
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
        if let Err(error) = remove_cache_entry(&cache) {
            tracing::debug!(
                %error,
                photo_id = photo_id,
                path = %cache.display(),
                "cannot remove corrupt preview cache"
            );
        }
    }
    let stamp = cache_metadata::source_stamp(source);
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
    if write_cache(&cache, &image, stamp) {
        maybe_prune(cache_dir);
    }
    Some(image)
}

fn cache_is_fresh(cache: &Path, source: &Path) -> bool {
    cache_metadata::cache_is_fresh(cache, source)
}

fn remove_cache_entry(cache: &Path) -> std::io::Result<()> {
    let result = match fs::remove_file(cache) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    cache_metadata::remove_stamp(cache);
    result
}

fn read_cache(cache: &Path) -> Option<RgbaImage> {
    let image = crate::image_decode::decode_image(cache)?;
    Some(image.into_rgba8())
}

fn write_cache(
    cache: &Path,
    image: &RgbaImage,
    stamp: Option<cache_metadata::SourceStamp>,
) -> bool {
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
    let temp = temp_path_for(cache);
    if let Err(error) = fs::write(&temp, &*encoded) {
        tracing::warn!(
            %error,
            path = %temp.display(),
            "cannot write preview cache"
        );
        let _ = fs::remove_file(&temp);
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
    if let Some(stamp) = stamp
        && let Err(error) = cache_metadata::write_stamp(cache, stamp)
    {
        // The image is usable for this request; a missing stamp only forces a
        // regeneration next time.
        tracing::warn!(
            %error,
            path = %cache.display(),
            "cannot write preview cache metadata"
        );
    }
    true
}

/// Whether the `writes`-th cache write triggers a prune.
fn prune_due(writes: usize) -> bool {
    writes.is_multiple_of(PRUNE_EVERY)
}

fn maybe_prune(cache_dir: &Path) {
    let writes = WRITES.fetch_add(1, Ordering::Relaxed) + 1;
    if !prune_due(writes) {
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

/// Whether `name` is a preview temp file, in the current unique form
/// (`<name>.webp.tmp-<pid>-<seq>`) or the legacy shared form
/// (`<name>.webp.tmp`).
fn is_preview_temp_name(name: &str) -> bool {
    name.contains(".webp.tmp")
}

fn prune_previews_in(
    cache_dir: &Path,
    max_files: usize,
    prune_to: usize,
) -> std::io::Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;
    let mut entries = Vec::new();
    for entry in fs::read_dir(cache_dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if is_preview_temp_name(&entry.file_name().to_string_lossy()) {
            // Stale temps never count toward the cache cap; they are swept
            // purely by age so an in-flight write is never removed.
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > TEMP_MAX_AGE)
            {
                match fs::remove_file(&path) {
                    Ok(()) => removed += 1,
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            path = %path.display(),
                            "cannot remove stale preview temp file"
                        );
                    }
                }
            }
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("webp") {
            continue;
        }
        entries.push((modified, path));
    }
    if entries.len() <= max_files {
        return Ok(removed);
    }
    entries.sort_by_key(|(modified, _)| *modified);
    let remove_count = entries.len().saturating_sub(prune_to);
    for (_, path) in entries.into_iter().take(remove_count) {
        match fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                cache_metadata::remove_stamp(&path);
            }
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
    fn full_decode_preserves_small_images_pixel_for_pixel() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        write_png(&source, 37, 21, [12, 34, 56, 255]);

        let image = decode_full_image(&source).expect("full image");
        assert_eq!(image.dimensions(), (37, 21));
        assert_eq!(image.get_pixel(0, 0).0, [12, 34, 56, 255]);
    }

    #[test]
    fn full_decode_caps_only_the_long_edge() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("wide.png");
        write_png(&source, 200, 100, [1, 2, 3, 255]);
        let image = decode_limited(&source, 100).expect("limited image");
        assert_eq!(image.dimensions(), (100, 50));
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
        set_mtime(&source, older + Duration::from_secs(1));

        let refreshed = ensure_preview_in(10, 0, &source, &cache).expect("regenerated");
        let pixel = refreshed.get_pixel(0, 0).0;
        assert!(pixel[2] > 240 && pixel[0] < 16 && pixel[1] < 16);
    }

    #[test]
    fn source_size_change_regenerates_a_preview_with_the_same_variant() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [255, 0, 0, 255]);
        assert!(ensure_preview_in(10, 7, &source, &cache).is_some());
        let modified = fs::metadata(&source).unwrap().modified().unwrap();

        write_png(&source, 640, 400, [0, 255, 0, 255]);
        set_mtime(&source, modified);

        let refreshed = ensure_preview_in(10, 7, &source, &cache).expect("regenerated");
        let pixel = refreshed.get_pixel(0, 0).0;
        assert!(
            pixel[1] > 240 && pixel[0] < 16 && pixel[2] < 16,
            "preview still shows the old source: {pixel:?}"
        );
    }

    #[test]
    fn remove_preview_files_removes_every_variant_and_sidecar() {
        let dir = tempdir().unwrap();
        for name in [
            "7-0.webp",
            "7-0.webp.meta",
            "7-42.webp",
            "7-42.webp.meta",
            "7-43.webp.tmp-42-1",
            "8-0.webp",
            "8-0.webp.meta",
            "17-0.webp",
            "notes.txt",
        ] {
            fs::write(dir.path().join(name), b"data").unwrap();
        }

        let removed = remove_preview_files_in(dir.path(), 7).unwrap();

        assert_eq!(removed, 5);
        for name in [
            "7-0.webp",
            "7-0.webp.meta",
            "7-42.webp",
            "7-42.webp.meta",
            "7-43.webp.tmp-42-1",
        ] {
            assert!(!dir.path().join(name).exists(), "still present: {name}");
        }
        assert!(dir.path().join("8-0.webp").exists());
        assert!(dir.path().join("8-0.webp.meta").exists());
        assert!(dir.path().join("17-0.webp").exists());
        assert!(dir.path().join("notes.txt").exists());
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
    fn temp_paths_are_unique_per_writer() {
        let cache = Path::new("/cache/7-0.webp");
        let first = temp_path_for(cache);
        let second = temp_path_for(cache);
        assert_ne!(first, second);
        for temp in [&first, &second] {
            let name = temp.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("7-0.webp.tmp-"), "{name}");
            assert_eq!(temp.parent(), cache.parent());
        }
    }

    #[test]
    fn concurrent_writes_leave_a_decodable_cache() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [10, 200, 30, 255]);

        // The import warmer and the selection decoder can write the same
        // entry at once; every published cache file must be complete.
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = &cache;
                let source = &source;
                scope.spawn(move || {
                    for _ in 0..3 {
                        let image = ensure_preview_in(20, 0, source, cache).expect("preview");
                        assert_eq!(image.dimensions(), (1200, 750));
                    }
                });
            }
        });

        let cached = read_cache(&cache.join("20-0.webp")).expect("decodable cache");
        assert_eq!(cached.dimensions(), (1200, 750));
        // No writer leaves its temp file behind.
        for entry in fs::read_dir(&cache).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.contains(".tmp"), "leftover temp file: {name}");
        }
    }

    #[test]
    fn prune_is_due_every_32nd_write() {
        assert_eq!(PRUNE_EVERY, 32);
        assert!(!prune_due(1));
        assert!(!prune_due(31));
        assert!(prune_due(32));
        assert!(!prune_due(33));
        assert!(prune_due(64));
    }

    #[test]
    fn prune_with_production_limits_keeps_the_prune_target() {
        assert_eq!(PREVIEW_CACHE_MAX_FILES, 2048);
        assert_eq!(PREVIEW_CACHE_PRUNE_TO, 1536);
        let dir = tempdir().unwrap();
        let cache = dir.path().join("previews");
        fs::create_dir_all(&cache).unwrap();
        let total = PREVIEW_CACHE_MAX_FILES + 12;
        for index in 0..total {
            let path = cache.join(format!("{index}-0.webp"));
            fs::write(&path, b"preview").unwrap();
            set_mtime(
                &path,
                SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
            );
        }

        let removed =
            prune_previews_in(&cache, PREVIEW_CACHE_MAX_FILES, PREVIEW_CACHE_PRUNE_TO).unwrap();
        assert_eq!(removed, total - PREVIEW_CACHE_PRUNE_TO);
        let remaining = fs::read_dir(&cache).unwrap().count();
        assert_eq!(remaining, PREVIEW_CACHE_PRUNE_TO);
        // The survivors are the newest entries.
        assert!(!cache.join("0-0.webp").exists());
        assert!(cache.join(format!("{}-0.webp", total - 1)).exists());
    }

    #[test]
    fn returns_the_decoded_preview_when_the_cache_write_fails() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        write_png(&source, 320, 200, [255, 0, 0, 255]);
        // A regular file where the cache parent must be created makes
        // create_dir_all fail, so nothing can be cached.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let cache = blocker.join("previews");

        let image =
            ensure_preview_in(12, 0, &source, &cache).expect("preview despite write failure");
        assert_eq!(image.dimensions(), (1200, 750));
        assert!(!cache.join("12-0.webp").exists());
    }

    #[test]
    fn prunes_stale_temp_files_but_keeps_fresh_ones() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("previews");
        fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        let stale = now - TEMP_MAX_AGE - Duration::from_secs(60);
        // Current unique form and the legacy shared form are both swept.
        let stale_unique = cache.join("1-0.webp.tmp-4242-0");
        let stale_legacy = cache.join("2-0.webp.tmp");
        let fresh_unique = cache.join("3-0.webp.tmp-4242-1");
        let kept = cache.join("4-0.webp");
        for path in [&stale_unique, &stale_legacy, &kept] {
            fs::write(path, b"preview").unwrap();
            set_mtime(path, stale);
        }
        fs::write(&fresh_unique, b"preview").unwrap();
        set_mtime(&fresh_unique, now);

        // The cap is not exceeded, so only the stale temps are removed.
        let removed =
            prune_previews_in(&cache, PREVIEW_CACHE_MAX_FILES, PREVIEW_CACHE_PRUNE_TO).unwrap();
        assert_eq!(removed, 2);
        assert!(!stale_unique.exists());
        assert!(!stale_legacy.exists());
        assert!(fresh_unique.exists());
        // The temp sweep never touches real cache entries, however old.
        assert!(kept.exists());
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
            fs::write(cache.join(format!("{index}-0.webp.meta")), b"v1 0 0 1").unwrap();
            set_mtime(
                &path,
                SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
            );
        }

        let removed = prune_previews_in(&cache, 4, 2).unwrap();
        assert_eq!(removed, 4);
        assert!(!cache.join("0-0.webp").exists());
        assert!(!cache.join("0-0.webp.meta").exists());
        assert!(!cache.join("3-0.webp").exists());
        assert!(cache.join("4-0.webp").exists());
        assert!(cache.join("5-0.webp").exists());
        assert!(unrelated.exists());
    }
}
