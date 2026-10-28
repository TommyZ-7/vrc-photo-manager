//! Thumbnail LRU cache and the background decode / image conversion helpers
//! shared by the photo grid and the preview viewer.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use gpui_kit::RenderImage;
use smallvec::SmallVec;
use vrc_photo_core::thumbnail;

use crate::ui::theme::average_bgra;

const THUMB_CACHE_ENTRIES: usize = 192;
const THUMB_IN_FLIGHT: usize = 4;

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
    pending: HashSet<i64>,
    failed: HashSet<i64>,
    pub(crate) pinned: Option<i64>,
    in_flight: usize,
}

impl ThumbnailCache {
    pub(crate) fn get(&mut self, id: i64) -> Option<Arc<RenderImage>> {
        let image = self.images.get(&id)?.clone();
        self.touch(id);
        Some(image)
    }

    pub(crate) fn color(&self, id: i64) -> Option<u32> {
        self.colors.get(&id).copied()
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
        }
    }
}

/// Decodes a thumbnail on a background thread. Returns the average color in
/// RGB and the RGBA buffer with channels already swapped to BGRA.
pub(crate) fn decode_thumbnail(
    id: i64,
    cache_path: &Path,
    source: &Path,
) -> Option<(u32, image::RgbaImage)> {
    let cached = cache_path.is_file()
        && std::fs::metadata(cache_path)
            .map(|meta| meta.len() > 0)
            .unwrap_or_else(|error| {
                tracing::debug!(
                    %error,
                    path = %cache_path.display(),
                    "cannot read thumbnail cache metadata"
                );
                false
            });
    let path = if cached {
        cache_path.to_path_buf()
    } else {
        match thumbnail::ensure_thumbnail(id, source) {
            Ok(Some(path)) => path,
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
        }
    };
    let buffer = match vrc_photo_core::image_decode::decode_image(&path) {
        Some(image) => image.to_rgba8(),
        None => {
            tracing::warn!(
                photo_id = id,
                path = %path.display(),
                "cannot decode thumbnail"
            );
            return None;
        }
    };
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
    fn decode_thumbnail_uses_the_cached_file_and_swaps_channels() {
        let dir = std::env::temp_dir().join(format!("vrc-photo-ui-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache_path = dir.join("thumb.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .save(&cache_path)
            .unwrap();
        let (color, rgba) =
            decode_thumbnail(7, &cache_path, &dir.join("missing-source.png")).unwrap();
        assert_eq!(color, 0x0a141e);
        assert_eq!(&rgba.as_raw()[..4], &[30, 20, 10, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
