//! Thumbnail LRU cache and the background decode / image conversion helpers
//! shared by the photo grid and the preview viewer.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::RenderImage;
use smallvec::SmallVec;
use vrc_photo_core::thumbnail;

use crate::ui::theme::average_bgra;

const THUMB_CACHE_ENTRIES: usize = 192;
const THUMB_IN_FLIGHT: usize = 4;

/// A flush is due once a batch reaches either bound. Small batches are flushed
/// by a timer instead so a quiet gallery still reports its generated files.
pub(crate) const THUMBNAIL_NOTICE_BATCH_FILES: u64 = 32;
pub(crate) const THUMBNAIL_NOTICE_BATCH_BYTES: u64 = 1024 * 1024;

/// Accumulates thumbnails the UI generated on its own before telling the
/// daemon about them. One notice per thumbnail would double the IPC traffic,
/// so the caller flushes on a threshold or a short timer.
#[derive(Debug, Default)]
pub(crate) struct ThumbnailNoticeBatch {
    files: u64,
    bytes: u64,
}

impl ThumbnailNoticeBatch {
    /// Records one generated file and returns whether the batch is now full.
    pub(crate) fn add(&mut self, bytes: u64) -> bool {
        self.files += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.files >= THUMBNAIL_NOTICE_BATCH_FILES || self.bytes >= THUMBNAIL_NOTICE_BATCH_BYTES
    }

    /// Returns the accumulated `(files, bytes)` and clears the batch.
    pub(crate) fn take(&mut self) -> Option<(u64, u64)> {
        if self.files == 0 {
            return None;
        }
        let notice = (self.files, self.bytes);
        self.files = 0;
        self.bytes = 0;
        Some(notice)
    }
}

/// Bounded LRU cache for grid thumbnails.
///
/// Decoding runs on the background executor; the UI thread only turns the
/// decoded RGBA buffer into a `RenderImage`. `pending` and `failed` keep a
/// photo from being requested more than once while it is on screen.
#[derive(Default)]
pub(crate) struct ThumbnailCache {
    images: HashMap<i64, Arc<RenderImage>>,
    colors: HashMap<i64, u32>,
    /// Monotonic access tick per cached id. `get` updates a single entry
    /// instead of scanning an order list for every visible card.
    recency: HashMap<i64, u64>,
    clock: u64,
    /// Last time a disk access refresh was queued for a cached id. Throttles
    /// the mtime update to once per `ACCESS_REFRESH_AFTER`.
    last_disk_refresh: HashMap<i64, Instant>,
    /// Ids whose disk access time should be refreshed in the background.
    disk_refresh_due: HashSet<i64>,
    pending: HashSet<i64>,
    failed: HashSet<i64>,
    pub(crate) pinned: Option<i64>,
    in_flight: usize,
}

impl ThumbnailCache {
    pub(crate) fn get(&mut self, id: i64) -> Option<Arc<RenderImage>> {
        let image = self.images.get(&id)?.clone();
        self.touch(id);
        let now = Instant::now();
        let stale = self
            .last_disk_refresh
            .get(&id)
            .is_none_or(|at| now.duration_since(*at) >= thumbnail::ACCESS_REFRESH_AFTER);
        if stale {
            // Record the queue time so later frames in the same throttle
            // window do not queue the same id again.
            self.last_disk_refresh.insert(id, now);
            self.disk_refresh_due.insert(id);
        }
        Some(image)
    }

    pub(crate) fn color(&self, id: i64) -> Option<u32> {
        self.colors.get(&id).copied()
    }

    /// Drains the ids whose cached file should be marked as recently used.
    pub(crate) fn take_access_refreshes(&mut self) -> Vec<i64> {
        self.disk_refresh_due.drain().collect()
    }

    pub(crate) fn begin(&mut self, id: i64) -> bool {
        if self.images.contains_key(&id)
            || self.pending.contains(&id)
            || self.failed.contains(&id)
            || self.in_flight >= THUMB_IN_FLIGHT
        {
            return false;
        }
        self.pending.insert(id);
        self.in_flight += 1;
        true
    }

    pub(crate) fn insert(&mut self, id: i64, image: Arc<RenderImage>, color: u32) {
        if self.pending.remove(&id) {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.images.insert(id, image);
        self.colors.insert(id, color);
        self.touch(id);
        self.last_disk_refresh.insert(id, Instant::now());
        self.disk_refresh_due.remove(&id);
        self.evict();
    }

    pub(crate) fn fail(&mut self, id: i64) {
        if self.pending.remove(&id) {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.failed.insert(id);
    }

    pub(crate) fn pin(&mut self, id: i64) {
        self.pinned = Some(id);
    }

    /// Drops the pin when the selected photo is deleted or deselected.
    pub(crate) fn unpin(&mut self, id: i64) {
        if self.pinned == Some(id) {
            self.pinned = None;
        }
    }

    fn touch(&mut self, id: i64) {
        self.clock = self.clock.wrapping_add(1);
        self.recency.insert(id, self.clock);
    }

    fn evict(&mut self) {
        while self.images.len() > THUMB_CACHE_ENTRIES {
            let Some((&victim, _)) = self
                .recency
                .iter()
                .filter(|(id, _)| Some(**id) != self.pinned)
                .min_by_key(|(_, tick)| **tick)
            else {
                break;
            };
            self.images.remove(&victim);
            self.colors.remove(&victim);
            self.recency.remove(&victim);
            self.last_disk_refresh.remove(&victim);
            self.disk_refresh_due.remove(&victim);
        }
    }
}

/// A thumbnail decoded on a background thread.
#[derive(Debug)]
pub(crate) struct DecodedThumbnail {
    /// Average color in RGB.
    pub(crate) color: u32,
    /// RGBA buffer with channels already swapped to BGRA.
    pub(crate) image: image::RgbaImage,
    /// On-disk size when this decode wrote the file; `None` for a cache hit.
    pub(crate) generated_bytes: Option<u64>,
}

/// Decodes a thumbnail on a background thread.
///
/// With `cache_dir`, the core resolves or regenerates the file and a cache hit
/// is marked as recently used for LRU eviction. Without it, `cache_path` is
/// read-only (demo mode or an explicit asset path).
pub(crate) fn decode_thumbnail(
    id: i64,
    cache_path: &Path,
    source: &Path,
    cache_dir: Option<&Path>,
) -> Option<DecodedThumbnail> {
    decode_thumbnail_with(
        id,
        cache_path,
        source,
        cache_dir,
        thumbnail::ensure_thumbnail_in_with_status,
        decode_thumbnail_file,
    )
}

fn decode_thumbnail_with<Ensure, Decode>(
    id: i64,
    cache_path: &Path,
    source: &Path,
    cache_dir: Option<&Path>,
    ensure_thumbnail: Ensure,
    decode_thumbnail_file: Decode,
) -> Option<DecodedThumbnail>
where
    Ensure:
        Fn(i64, &Path, &Path) -> vrc_photo_core::error::Result<Option<thumbnail::EnsuredThumbnail>>,
    Decode: Fn(i64, &Path) -> Option<(u32, image::RgbaImage)>,
{
    let Some(cache_dir) = cache_dir else {
        if !cache_path.is_file() {
            return None;
        }
        return match decode_thumbnail_file(id, cache_path) {
            Some((color, image)) => Some(DecodedThumbnail {
                color,
                image,
                generated_bytes: None,
            }),
            None => {
                tracing::warn!(
                    photo_id = id,
                    path = %cache_path.display(),
                    "cannot decode thumbnail"
                );
                None
            }
        };
    };

    let ensured = match ensure_thumbnail(id, source, cache_dir) {
        Ok(Some(ensured)) => ensured,
        Ok(None) => {
            tracing::debug!(
                photo_id = id,
                path = %source.display(),
                "thumbnail source is missing or cannot be decoded"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                %error,
                photo_id = id,
                path = %source.display(),
                "cannot generate thumbnail"
            );
            return None;
        }
    };
    let mut path = ensured.path;
    let mut generated_bytes = ensured.created.then_some(ensured.bytes);
    match decode_thumbnail_file(id, &path) {
        Some((color, image)) => {
            if !ensured.created {
                thumbnail::refresh_thumbnail_access(&path);
            }
            return Some(DecodedThumbnail {
                color,
                image,
                generated_bytes,
            });
        }
        None if !path.is_file() => {
            tracing::debug!(
                photo_id = id,
                path = %path.display(),
                "thumbnail cache entry disappeared during decode; regenerating"
            );
        }
        None => {
            tracing::warn!(
                photo_id = id,
                path = %path.display(),
                "cannot decode thumbnail"
            );
            return None;
        }
    }

    match ensure_thumbnail(id, source, cache_dir) {
        Ok(Some(ensured)) => {
            path = ensured.path;
            if ensured.created {
                generated_bytes = Some(ensured.bytes);
            }
        }
        Ok(None) => {
            tracing::debug!(
                photo_id = id,
                path = %source.display(),
                "thumbnail source disappeared during decode"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                %error,
                photo_id = id,
                path = %source.display(),
                "cannot regenerate thumbnail"
            );
            return None;
        }
    }
    match decode_thumbnail_file(id, &path) {
        Some((color, image)) => Some(DecodedThumbnail {
            color,
            image,
            generated_bytes,
        }),
        None => {
            tracing::warn!(
                photo_id = id,
                path = %path.display(),
                "cannot decode regenerated thumbnail"
            );
            None
        }
    }
}

fn decode_thumbnail_file(_id: i64, path: &Path) -> Option<(u32, image::RgbaImage)> {
    let buffer = vrc_photo_core::image_decode::decode_image(path)?.to_rgba8();
    let buffer = rgba_to_bgra(buffer);
    let color = average_bgra(buffer.as_raw());
    Some((color, buffer))
}

/// Builds a GPUI image from a decoded buffer. Must run on the UI thread.
pub(crate) fn make_render_image(rgba: image::RgbaImage) -> Arc<RenderImage> {
    let frame = image::Frame::new(rgba);
    Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1)))
}

/// Swaps RGBA to the BGRA order GPUI's `RenderImage` expects. Call this on the
/// background executor so a full preview does not spend its pixel loop on the
/// UI thread.
pub(crate) fn rgba_to_bgra(mut rgba: image::RgbaImage) -> image::RgbaImage {
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn test_render_image() -> Arc<RenderImage> {
        let frame = image::Frame::new(image::RgbaImage::new(1, 1));
        Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1)))
    }

    #[test]
    fn rgba_to_bgra_swaps_the_red_and_blue_channels() {
        let source = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 40]));
        let converted = rgba_to_bgra(source);
        assert_eq!(converted.get_pixel(0, 0).0, [30, 20, 10, 40]);
    }

    #[test]
    fn thumbnail_cache_get_refreshes_recency() {
        let mut cache = ThumbnailCache::default();
        for id in 1..=THUMB_CACHE_ENTRIES as i64 {
            assert!(cache.begin(id));
            cache.insert(id, test_render_image(), 0);
        }
        // Touch the oldest entry, then overflow the cache. The entry after it
        // must be evicted instead.
        assert!(cache.get(1).is_some());
        assert!(cache.begin(999));
        cache.insert(999, test_render_image(), 0);
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn thumbnail_cache_limits_in_flight_requests() {
        let mut cache = ThumbnailCache::default();
        for id in 0..THUMB_IN_FLIGHT as i64 {
            assert!(cache.begin(id));
        }
        assert!(!cache.begin(999));
    }

    #[test]
    fn thumbnail_cache_evicts_least_recently_used_but_keeps_pinned() {
        let mut cache = ThumbnailCache::default();
        cache.pin(1);
        for id in 1..=THUMB_CACHE_ENTRIES as i64 {
            assert!(cache.begin(id));
            cache.insert(id, test_render_image(), 0x112233);
        }
        // Do not touch id 1: it is the least recently used entry, so only the
        // pin can keep it alive when the next insert overflows the cache.
        assert!(cache.begin(999));
        cache.insert(999, test_render_image(), 0x445566);
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn thumbnail_cache_unpins_a_deleted_selection() {
        let mut cache = ThumbnailCache::default();
        cache.pin(7);
        cache.unpin(7);
        assert_eq!(cache.pinned, None);

        cache.pin(8);
        cache.unpin(7);
        assert_eq!(cache.pinned, Some(8));
    }

    #[test]
    fn thumbnail_cache_frees_a_slot_for_a_pinned_retry() {
        let mut cache = ThumbnailCache::default();
        for id in 0..THUMB_IN_FLIGHT as i64 {
            assert!(cache.begin(id));
        }
        cache.pin(99);
        // The selected thumbnail is refused while every slot is busy.
        assert!(!cache.begin(99));
        // A completed decode frees a slot; the pinned retry can now start.
        cache.insert(0, test_render_image(), 0);
        assert!(cache.begin(99));
    }

    #[test]
    fn thumbnail_cache_does_not_retry_failed_or_pending_ids() {
        let mut cache = ThumbnailCache::default();
        assert!(cache.begin(1));
        assert!(!cache.begin(1));
        cache.fail(1);
        assert!(!cache.begin(1));
    }

    #[test]
    fn thumbnail_notice_batch_flushes_at_file_and_byte_thresholds() {
        let mut batch = ThumbnailNoticeBatch::default();
        for _ in 0..(THUMBNAIL_NOTICE_BATCH_FILES - 1) {
            assert!(!batch.add(1));
        }
        assert!(batch.add(1));
        assert_eq!(
            batch.take(),
            Some((THUMBNAIL_NOTICE_BATCH_FILES, THUMBNAIL_NOTICE_BATCH_FILES))
        );
        assert_eq!(batch.take(), None);

        let mut batch = ThumbnailNoticeBatch::default();
        assert!(batch.add(THUMBNAIL_NOTICE_BATCH_BYTES));
        assert_eq!(batch.take(), Some((1, THUMBNAIL_NOTICE_BATCH_BYTES)));
    }

    #[test]
    fn decode_thumbnail_reports_bytes_only_when_it_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("thumbnails");
        let source = dir.path().join("source.png");
        image::RgbaImage::from_pixel(128, 80, image::Rgba([200, 100, 50, 255]))
            .save(&source)
            .unwrap();
        let cache_path = cache.join("5.webp");

        let first = decode_thumbnail(5, &cache_path, &source, Some(&cache)).expect("generated");
        assert!(first.generated_bytes.is_some_and(|bytes| bytes > 0));
        let second = decode_thumbnail(5, &cache_path, &source, Some(&cache)).expect("cached");
        assert_eq!(second.generated_bytes, None);
    }

    #[test]
    fn decode_thumbnail_regenerates_a_removed_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("thumbnails");
        let source = dir.path().join("source.png");
        image::RgbaImage::from_pixel(128, 80, image::Rgba([200, 100, 50, 255]))
            .save(&source)
            .unwrap();
        let cache_path = cache.join("7.webp");

        let first = decode_thumbnail(7, &cache_path, &source, Some(&cache)).expect("generated");
        assert!(cache_path.is_file());
        std::fs::remove_file(&cache_path).unwrap();
        let second = decode_thumbnail(7, &cache_path, &source, Some(&cache)).expect("regenerated");
        assert!(cache_path.is_file());
        assert_eq!(first.color, second.color);
    }

    #[test]
    fn decode_thumbnail_refreshes_cached_access_time() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("thumbnails");
        let source = dir.path().join("source.png");
        image::RgbaImage::from_pixel(32, 32, image::Rgba([10, 20, 30, 255]))
            .save(&source)
            .unwrap();
        let cache_path = cache.join("8.webp");
        decode_thumbnail(8, &cache_path, &source, Some(&cache)).expect("generated");
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&cache_path)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        let before = std::fs::metadata(&cache_path).unwrap().modified().unwrap();

        decode_thumbnail(8, &cache_path, &source, Some(&cache)).expect("cached");

        assert!(std::fs::metadata(&cache_path).unwrap().modified().unwrap() > before);
    }

    #[test]
    fn decode_thumbnail_retries_once_after_the_cache_file_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.webp");
        let regenerated = dir.path().join("regenerated.webp");
        std::fs::write(&regenerated, b"not decoded here").unwrap();
        let calls = std::cell::Cell::new(0);
        let ensure = |_id: i64, _source: &Path, _cache: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(Some(thumbnail::EnsuredThumbnail {
                    path: missing.clone(),
                    created: false,
                    bytes: 10,
                }))
            } else {
                Ok(Some(thumbnail::EnsuredThumbnail {
                    path: regenerated.clone(),
                    created: true,
                    bytes: 10,
                }))
            }
        };
        let decode = |_id: i64, path: &Path| {
            if path == missing {
                None
            } else {
                Some((0x112233, image::RgbaImage::new(1, 1)))
            }
        };

        let decoded = decode_thumbnail_with(
            1,
            &missing,
            Path::new("/no/such/source.png"),
            Some(dir.path()),
            ensure,
            decode,
        )
        .expect("retried thumbnail");

        assert_eq!(decoded.color, 0x112233);
        assert_eq!(decoded.generated_bytes, Some(10));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn decode_thumbnail_does_not_regenerate_an_undecodable_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("corrupt.webp");
        std::fs::write(&cache_path, b"not an image").unwrap();
        let calls = std::cell::Cell::new(0);
        let ensure = |_id: i64, _source: &Path, _cache: &Path| {
            calls.set(calls.get() + 1);
            Ok(Some(thumbnail::EnsuredThumbnail {
                path: cache_path.clone(),
                created: false,
                bytes: 10,
            }))
        };
        let decode = |_id: i64, _path: &Path| None;

        let decoded = decode_thumbnail_with(
            2,
            &cache_path,
            Path::new("/no/such/source.png"),
            Some(dir.path()),
            ensure,
            decode,
        );

        assert!(decoded.is_none());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn decode_thumbnail_does_not_generate_without_a_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("existing.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&cache_path)
            .unwrap();

        let decoded =
            decode_thumbnail(6, &cache_path, &dir.path().join("missing-source.png"), None)
                .expect("cached");

        assert_eq!(decoded.generated_bytes, None);
    }

    #[test]
    fn decode_thumbnail_without_cache_dir_does_not_generate() {
        let dir = tempfile::tempdir().unwrap();

        assert!(
            decode_thumbnail(
                99,
                &dir.path().join("missing.png"),
                Path::new("/no/such/source.png"),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn thumbnail_cache_queues_one_disk_refresh_per_hour() {
        let mut cache = ThumbnailCache::default();
        cache.insert(1, test_render_image(), 0);
        cache.last_disk_refresh.insert(
            1,
            Instant::now() - thumbnail::ACCESS_REFRESH_AFTER - Duration::from_secs(1),
        );
        assert!(cache.get(1).is_some());
        assert_eq!(cache.take_access_refreshes(), vec![1]);
        assert!(cache.get(1).is_some());
        assert!(cache.take_access_refreshes().is_empty());
    }

    #[test]
    fn thumbnail_cache_insert_cancels_a_pending_disk_refresh() {
        let mut cache = ThumbnailCache::default();
        cache.insert(1, test_render_image(), 0);
        cache.last_disk_refresh.insert(
            1,
            Instant::now() - thumbnail::ACCESS_REFRESH_AFTER - Duration::from_secs(1),
        );
        assert!(cache.get(1).is_some());
        cache.insert(1, test_render_image(), 0);
        assert!(cache.take_access_refreshes().is_empty());
    }

    #[test]
    fn decode_thumbnail_uses_the_cached_file_and_swaps_channels() {
        let dir = std::env::temp_dir().join(format!("vrc-photo-ui-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache_path = dir.join("thumb.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .save(&cache_path)
            .unwrap();
        let decoded =
            decode_thumbnail(7, &cache_path, &dir.join("missing-source.png"), None).unwrap();
        assert_eq!(decoded.color, 0x0a141e);
        assert_eq!(&decoded.image.as_raw()[..4], &[30, 20, 10, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
