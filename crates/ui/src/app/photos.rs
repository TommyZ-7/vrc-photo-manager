//! App-side photo browser state: selection, thumbnails, previews, and the
//! active grid accessor.
//!
//! Rendering stays in `ui::library`; these methods only touch App fields and
//! background decode queues.

use std::path::PathBuf;
use std::time::Instant;

use gpui_kit::*;
use vrc_photo_core::model::Photo;
use vrc_photo_core::{preview, thumbnail};

use crate::app::cli::log_timing;
use crate::app::{App, Screen};
use crate::demo_thumb_path;
use crate::state::photo_browser::PhotoBrowser;
use crate::state::preview::{preview_cache_dir, preview_key, should_request_preview};
use crate::state::thumbnail::{decode_thumbnail, make_render_image, preview_render_image};
use crate::ui::theme::{selection_range, toggle_selected_id};

impl App {
    /// Reloads the metadata for a photo without decoding its 1200 px preview.
    /// Favorite and tag edits use this so the preview does not flash.
    pub(crate) fn load_selected_meta(&mut self, id: i64) {
        self.selected = Some(id);
        self.selected_photo = match self.db.photo_by_id(id) {
            Ok(photo) => photo,
            Err(error) => {
                tracing::warn!(%error, photo_id = id, "cannot load selected photo");
                None
            }
        };
        self.selected_players = self.db.photo_players(id).unwrap_or_else(|error| {
            tracing::warn!(%error, photo_id = id, "cannot load selected photo players");
            Vec::new()
        });
        self.selected_tags = self.db.photo_tags(id).unwrap_or_else(|error| {
            tracing::warn!(%error, photo_id = id, "cannot load selected photo tags");
            Vec::new()
        });
        self.selected_derivatives = self.db.derivatives_for_photo(id).unwrap_or_else(|error| {
            tracing::warn!(%error, photo_id = id, "cannot load photo derivatives");
            Vec::new()
        });
        if self.note_edit_id.is_some_and(|edit_id| edit_id != id) {
            self.note_edit_id = None;
        }
    }

    /// Starts a background decode for a grid thumbnail when the cache has a
    /// free slot. Cached, pending, and previously failed photos are skipped.
    pub(crate) fn request_thumbnail(&mut self, photo: &Photo, cx: &mut Context<Self>) {
        if !self.thumbnails.begin(photo.id) {
            return;
        }
        let id = photo.id;
        let source = PathBuf::from(photo.effective_path());
        let cache_path = match &self.demo_assets {
            Some(assets) => demo_thumb_path(assets, id),
            None => thumbnail::thumbnail_path(id).unwrap_or_default(),
        };
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    let decode_start = Instant::now();
                    let decoded = decode_thumbnail(id, &cache_path, &source);
                    log_timing("thumbnail decode", decode_start);
                    decoded
                })
                .await;
            this.update(cx, |this, cx| {
                match decoded {
                    Some((color, rgba)) => {
                        this.thumbnails.insert(id, make_render_image(rgba), color);
                    }
                    None => this.thumbnails.fail(id),
                }
                // The selected photo may have been refused earlier because
                // every in-flight slot was busy; a freed slot is its retry.
                this.retry_pinned_thumbnail(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-requests the pinned (selected) thumbnail if it is still missing.
    /// `begin` ignores cached, pending, failed, and over-cap requests, so this
    /// is safe to call after every decode completion.
    fn retry_pinned_thumbnail(&mut self, cx: &mut Context<Self>) {
        let Some(pinned) = self.thumbnails.pinned else {
            return;
        };
        if self.thumbnails.get(pinned).is_some() {
            return;
        }
        let Some(photo) = self
            .selected_photo
            .clone()
            .filter(|photo| photo.id == pinned)
        else {
            return;
        };
        self.request_thumbnail(&photo, cx);
    }

    pub(crate) fn select_photo(&mut self, id: i64, cx: &mut Context<Self>) {
        let select_start = Instant::now();
        self.selected_ids.clear();
        self.selected_ids.push(id);
        self.selection_anchor = Some(id);
        self.set_primary_photo(id, cx);
        log_timing("select_photo", select_start);
        cx.notify();
    }

    /// Makes `id` the inspector/viewer photo without changing the selection
    /// list. Multi-selection toggles call this when the primary changes.
    fn set_primary_photo(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.delete_armed != Some(id) {
            self.delete_armed = None;
        }
        let changed = self.selected != Some(id) || self.selected_photo.is_none();
        if changed {
            let previous_color = self
                .selected_photo
                .as_ref()
                .map(|photo| self.color_for(photo));
            self.previous_preview = self.preview.take();
            self.previous_color = previous_color;
            self.preview_key = None;
            self.preview_attempt = None;
            self.selection_epoch = self.selection_epoch.wrapping_add(1);
            self.selection_changed_at = Some(Instant::now());
            self.load_selected_meta(id);
            self.request_preview(cx);
        } else {
            let current_key = self.selected_photo.as_ref().map(preview_key);
            let loaded = current_key.is_some() && self.preview_key == current_key;
            if !loaded && !self.preview_queue.pending_for(id) {
                // An explicit click retries a failed or dropped preview.
                self.preview_attempt = None;
                self.request_preview(cx);
            }
        }
        self.thumbnails.pin(id);
        if self.thumbnails.get(id).is_none()
            && let Some(photo) = self.selected_photo.clone()
        {
            self.request_thumbnail(&photo, cx);
        }
    }

    /// Requests the preview for the current selection, coalescing rapid changes
    /// so only one background decode runs at a time.
    fn request_preview(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = self.selected_photo.as_ref() else {
            return;
        };
        let key = preview_key(photo);
        if self.preview_key == Some(key) {
            self.preview_queue.cancel_wanted();
            return;
        }
        if self.preview_queue.want(key) {
            self.pump_preview(cx);
        }
    }

    fn pump_preview(&mut self, cx: &mut Context<Self>) {
        loop {
            let Some(key) = self.preview_queue.begin() else {
                return;
            };
            let matches = self
                .selected_photo
                .as_ref()
                .is_some_and(|photo| preview_key(photo) == key);
            if !matches || self.preview_key == Some(key) {
                self.preview_queue.finish(key);
                continue;
            }
            let source = PathBuf::from(
                self.selected_photo
                    .as_ref()
                    .expect("selected photo")
                    .effective_path(),
            );
            let cache_dir = preview_cache_dir(
                self.demo,
                self.demo_assets
                    .as_ref()
                    .map(|assets| assets.preview_dir.clone()),
                preview::previews_dir(),
            );
            self.preview_attempt = Some(key);
            cx.spawn(async move |this, cx| {
                let decoded = cx
                    .background_executor()
                    .spawn(async move {
                        let start = Instant::now();
                        let decoded = match cache_dir {
                            Some(dir) => {
                                preview::ensure_preview_in(key.photo_id, key.variant, &source, &dir)
                            }
                            None => preview::decode_preview(&source),
                        };
                        log_timing("preview decode", start);
                        decoded
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.preview_queue.finish(key);
                    let current = this
                        .selected_photo
                        .as_ref()
                        .is_some_and(|photo| preview_key(photo) == key);
                    if current && let Some(rgba) = decoded {
                        this.preview = Some(preview_render_image(rgba));
                        this.preview_key = Some(key);
                    }
                    this.pump_preview(cx);
                    this.refresh_stale_preview(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
    }

    pub(crate) fn refresh_stale_preview(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = self.selected_photo.as_ref() else {
            return;
        };
        let key = preview_key(photo);
        let pending = self.preview_queue.pending_for(photo.id);
        if should_request_preview(self.preview_key, self.preview_attempt, pending, key) {
            self.request_preview(cx);
        }
    }

    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.take() {
            self.thumbnails.unpin(id);
        }
        self.selected_ids.clear();
        self.selection_anchor = None;
        self.selected_photo = None;
        self.selected_players.clear();
        self.selected_tags.clear();
        self.selected_derivatives.clear();
        self.preview = None;
        self.preview_key = None;
        self.preview_attempt = None;
        self.preview_queue.cancel_wanted();
        self.previous_preview = None;
        self.previous_color = None;
        self.delete_armed = None;
        self.note_edit_id = None;
        cx.notify();
    }

    fn toggle_photo_selection(&mut self, id: i64, cx: &mut Context<Self>) {
        if toggle_selected_id(&mut self.selected_ids, id) {
            self.selection_anchor = Some(id);
            self.set_primary_photo(id, cx);
        } else if self.selection_anchor == Some(id) {
            self.selection_anchor = self.selected_ids.last().copied();
        }
        if self.selected == Some(id) && !self.selected_ids.contains(&id) {
            if let Some(next) = self.selected_ids.last().copied() {
                self.set_primary_photo(next, cx);
            } else {
                self.clear_selection(cx);
                return;
            }
        }
        cx.notify();
    }

    fn photo_browser(&self) -> &PhotoBrowser {
        if self.screen == Screen::Search {
            &self.search_view
        } else {
            &self.library
        }
    }

    fn select_photo_range(&mut self, id: i64, add: bool, cx: &mut Context<Self>) {
        let ids: Vec<i64> = self
            .photo_browser()
            .index
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect();
        let Some(anchor) = self.selection_anchor else {
            self.select_photo(id, cx);
            return;
        };
        let Some(range) = selection_range(&ids, anchor, id) else {
            self.select_photo(id, cx);
            return;
        };
        if add {
            for selected in range {
                if !self.selected_ids.contains(&selected) {
                    self.selected_ids.push(selected);
                }
            }
        } else {
            self.selected_ids = range;
        }
        self.set_primary_photo(id, cx);
        cx.notify();
    }

    pub(crate) fn handle_photo_click(
        &mut self,
        id: i64,
        event: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.select_photo_range(id, modifiers.secondary(), cx);
        } else if modifiers.secondary() {
            self.toggle_photo_selection(id, cx);
        } else {
            self.select_photo(id, cx);
        }
    }

    pub(crate) fn browser_mut(&mut self, search: bool) -> &mut PhotoBrowser {
        if search {
            &mut self.search_view
        } else {
            &mut self.library
        }
    }
}
