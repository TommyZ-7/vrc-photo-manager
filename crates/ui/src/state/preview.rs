//! Preview cache identity and the single-decode request queue used by the
//! viewer.

use std::path::PathBuf;

use vrc_photo_core::model::Photo;

/// Identifies a cached preview by photo and displayed file variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreviewKey {
    pub(crate) photo_id: i64,
    pub(crate) variant: i64,
}

pub(crate) fn preview_key(photo: &Photo) -> PreviewKey {
    PreviewKey {
        photo_id: photo.id,
        variant: photo.display_derivative_id.unwrap_or(0),
    }
}

/// Keeps one preview decode in flight and remembers the latest request.
///
/// A selection change while a decode is running only replaces `wanted`; the
/// running decode finishes and is dropped by the caller when stale, then the
/// latest key starts. This bounds background CPU during fast navigation.
#[derive(Debug, Default)]
pub(crate) struct PreviewQueue {
    in_flight: Option<PreviewKey>,
    wanted: Option<PreviewKey>,
}

impl PreviewQueue {
    /// Records `key` as the latest request. Returns true when the caller
    /// should start a decode immediately.
    pub(crate) fn want(&mut self, key: PreviewKey) -> bool {
        if self.in_flight == Some(key) {
            self.wanted = None;
            return false;
        }
        self.wanted = Some(key);
        self.in_flight.is_none()
    }

    /// Takes the queued key and marks it in flight. Returns None while another
    /// decode is still running.
    pub(crate) fn begin(&mut self) -> Option<PreviewKey> {
        if self.in_flight.is_some() {
            return None;
        }
        let key = self.wanted.take()?;
        self.in_flight = Some(key);
        Some(key)
    }

    /// Clears `key` if it is still the in-flight decode.
    pub(crate) fn finish(&mut self, key: PreviewKey) {
        if self.in_flight == Some(key) {
            self.in_flight = None;
        }
    }

    pub(crate) fn cancel_wanted(&mut self) {
        self.wanted = None;
    }

    pub(crate) fn pending_for(&self, photo_id: i64) -> bool {
        self.in_flight.is_some_and(|key| key.photo_id == photo_id)
            || self.wanted.is_some_and(|key| key.photo_id == photo_id)
    }
}

/// Whether a live refresh should request `key`.
///
/// A key is only requested once: after a failed attempt, `preview_attempt`
/// suppresses the 3-second retry loop until the user clicks the photo again.
pub(crate) fn should_request_preview(
    loaded: Option<PreviewKey>,
    attempt: Option<PreviewKey>,
    pending: bool,
    key: PreviewKey,
) -> bool {
    loaded != Some(key) && attempt != Some(key) && !pending
}

/// Chooses the preview cache directory. Demo mode may only use its own cache;
/// if the demo assets are unavailable, it must not touch the real data dir.
pub(crate) fn preview_cache_dir(
    demo: bool,
    demo_dir: Option<PathBuf>,
    real_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if demo { demo_dir } else { real_dir }
}

pub(crate) fn preview_placeholder_label(pending: bool) -> &'static str {
    if pending {
        "プレビューを読み込み中…"
    } else {
        "プレビューを読み込めません"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vrc_photo_core::model::PhotoStatus;

    fn test_photo(id: i64) -> Photo {
        Photo {
            id,
            root_id: 1,
            rel_path: format!("p/{id}.png"),
            abs_path: format!("/p/{id}.png"),
            file_name: format!("{id}.png"),
            captured_at_utc: id,
            captured_at_local: "2026-10-21 21:14:03".to_string(),
            tz_offset_sec: 0,
            width: 1,
            height: 1,
            file_size: 1,
            mtime: id,
            blake3: None,
            status: PhotoStatus::Present,
            is_favorite: false,
            note: None,
            display_derivative_id: None,
            display_path: None,
            display_format: None,
            display_file_size: None,
            display_width: None,
            display_height: None,
            world_name: None,
            instance_id: None,
            link_method: None,
            confidence: None,
            author: None,
        }
    }

    #[test]
    fn preview_queue_runs_one_decode_and_keeps_the_latest_wanted() {
        let mut queue = PreviewQueue::default();
        assert!(queue.want(PreviewKey {
            photo_id: 1,
            variant: 0
        }));
        assert_eq!(
            queue.begin(),
            Some(PreviewKey {
                photo_id: 1,
                variant: 0
            })
        );
        assert!(!queue.want(PreviewKey {
            photo_id: 2,
            variant: 0
        }));
        assert_eq!(queue.begin(), None);
        queue.finish(PreviewKey {
            photo_id: 1,
            variant: 0,
        });
        assert_eq!(
            queue.begin(),
            Some(PreviewKey {
                photo_id: 2,
                variant: 0
            })
        );
    }

    #[test]
    fn preview_queue_re_requesting_the_in_flight_key_drops_the_queued_key() {
        let mut queue = PreviewQueue::default();
        assert!(queue.want(PreviewKey {
            photo_id: 1,
            variant: 0
        }));
        queue.begin();
        assert!(!queue.want(PreviewKey {
            photo_id: 2,
            variant: 0
        }));
        assert!(!queue.want(PreviewKey {
            photo_id: 1,
            variant: 0
        }));
        queue.finish(PreviewKey {
            photo_id: 1,
            variant: 0,
        });
        assert_eq!(queue.begin(), None);
    }

    #[test]
    fn preview_queue_pending_for_covers_queued_and_in_flight() {
        let mut queue = PreviewQueue::default();
        queue.want(PreviewKey {
            photo_id: 1,
            variant: 0,
        });
        assert!(queue.pending_for(1));
        queue.begin();
        assert!(queue.pending_for(1));
        queue.finish(PreviewKey {
            photo_id: 1,
            variant: 0,
        });
        assert!(!queue.pending_for(1));
    }

    #[test]
    fn preview_key_uses_the_display_derivative() {
        let mut photo = test_photo(7);
        assert_eq!(
            preview_key(&photo),
            PreviewKey {
                photo_id: 7,
                variant: 0
            }
        );
        photo.display_derivative_id = Some(42);
        assert_eq!(
            preview_key(&photo),
            PreviewKey {
                photo_id: 7,
                variant: 42
            }
        );
    }

    #[test]
    fn stale_refresh_requests_a_new_variant_once() {
        let loaded_old = PreviewKey {
            photo_id: 7,
            variant: 0,
        };
        let wanted = PreviewKey {
            photo_id: 7,
            variant: 42,
        };
        assert!(!should_request_preview(Some(wanted), None, false, wanted));
        assert!(should_request_preview(
            Some(loaded_old),
            None,
            false,
            wanted
        ));
        assert!(!should_request_preview(
            Some(loaded_old),
            Some(wanted),
            false,
            wanted
        ));
        assert!(!should_request_preview(None, None, true, wanted));
        assert!(should_request_preview(None, None, false, wanted));
    }

    #[test]
    fn preview_cache_dir_never_uses_the_real_cache_in_demo() {
        let demo_dir = PathBuf::from("/demo/previews");
        let real_dir = PathBuf::from("/real/previews");
        assert_eq!(
            preview_cache_dir(true, Some(demo_dir.clone()), Some(real_dir.clone())),
            Some(demo_dir)
        );
        assert_eq!(preview_cache_dir(true, None, Some(real_dir.clone())), None);
        assert_eq!(
            preview_cache_dir(false, None, Some(real_dir.clone())),
            Some(real_dir)
        );
    }

    #[test]
    fn preview_placeholder_label_distinguishes_loading_from_failure() {
        assert_eq!(preview_placeholder_label(true), "プレビューを読み込み中…");
        assert_eq!(
            preview_placeholder_label(false),
            "プレビューを読み込めません"
        );
    }
}
