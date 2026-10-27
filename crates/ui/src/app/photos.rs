//! App-side photo browser state: selection, thumbnails, previews, and the
//! active grid accessor.
//!
//! Rendering stays in `ui::library`; these methods only touch App fields and
//! background decode queues.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::*;
use vrc_photo_core::model::Photo;
use vrc_photo_core::{preview, thumbnail};

use crate::app::cli::log_timing;
use crate::app::demo::demo_thumb_path;
use crate::app::{App, Screen};
use crate::state::photo_browser::PhotoBrowser;
use crate::state::preview::{preview_cache_dir, preview_key, should_request_preview};
use crate::state::thumbnail::{decode_thumbnail, make_render_image, preview_render_image};
use crate::ui::theme::{BG, scene_color, selection_range, toggle_selected_id};

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
                this.notify_thumbnail_loaded(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Coalesces bursts of thumbnail decodes into one UI notification. A
    /// 16 ms window keeps scrolling responsive without one full-app render per
    /// decoded thumbnail.
    fn notify_thumbnail_loaded(&mut self, cx: &mut Context<Self>) {
        if self.thumbnail_notify_pending {
            return;
        }
        self.thumbnail_notify_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            this.update(cx, |this, cx| {
                this.thumbnail_notify_pending = false;
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

    pub(crate) fn handle_photo_press(
        &mut self,
        id: i64,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        if modifiers.shift {
            self.select_photo_range(id, modifiers.secondary(cx), cx);
        } else if modifiers.secondary(cx) {
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

    /// Selects every photo in the active grid. The current primary photo stays
    /// primary when it is still present so the inspector does not jump.
    pub(crate) fn select_all_photos(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<i64> = self
            .photo_browser()
            .index
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect();
        if ids.is_empty() {
            self.clear_selection(cx);
            return;
        }
        let primary = self
            .selected
            .filter(|selected| ids.contains(selected))
            .unwrap_or(ids[0]);
        self.selection_anchor = Some(ids[0]);
        self.selected_ids = ids;
        self.set_primary_photo(primary, cx);
        cx.notify();
    }

    /// IDs the ribbon's bulk actions operate on. The primary selection is the
    /// fallback while the multi-selection list is empty.
    pub(crate) fn selected_action_ids(&self) -> Vec<i64> {
        if self.selected_ids.is_empty() {
            self.selected.into_iter().collect()
        } else {
            self.selected_ids.clone()
        }
    }

    pub(crate) fn color_for(&self, photo: &Photo) -> u32 {
        self.thumbnails
            .color(photo.id)
            .unwrap_or_else(|| scene_color(photo.world_name.as_deref().unwrap_or("")))
    }

    pub(crate) fn selected_color(&self) -> u32 {
        self.selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(BG)
    }

    pub(crate) fn begin_note_edit(
        &mut self,
        id: i64,
        note: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_edit_id = Some(id);
        let text = note.unwrap_or_default();
        self.note_edit_input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        let focus = self.note_edit_input.read(cx).focus_handle(cx);
        // The textarea mounts on the next frame, after the editor state flips.
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id == Some(id)
            {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn save_note(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.note_edit_input.read(cx).value();
        let note = normalize_note(&value);
        if let Err(error) = self.db.set_note(id, note) {
            tracing::warn!(%error, photo_id = id, "cannot save photo note");
            self.message = Some(format!("メモを保存できません: {error}"));
            cx.notify();
            return;
        }
        self.note_edit_id = None;
        self.load_selected_meta(id);
        let focus = self.quick_look_note_edit_focus.clone();
        // The edit action mounts after the textarea disappears.
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id.is_none() {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn cancel_note_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_edit_id.take().is_none() {
            return;
        }
        let focus = self.quick_look_note_edit_focus.clone();
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id.is_none() {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn trash_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        let title = if count == 1 {
            "写真をゴミ箱へ移動"
        } else {
            "選択した写真をゴミ箱へ移動"
        };
        let description = if count == 1 {
            self.selected_photo
                .as_ref()
                .map(|photo| {
                    format!(
                        "「{}」をOSゴミ箱へ移動します。この操作は元に戻せません。",
                        photo.captured_at_local
                    )
                })
                .unwrap_or_else(|| "写真をOSゴミ箱へ移動します。".to_string())
        } else {
            format!("{count} 枚をOSゴミ箱へ移動します。この操作は元に戻せません。")
        };
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let ids = ids.clone();
            alert
                .title(title)
                .description(description.clone())
                .confirm()
                .ok_text("ゴミ箱へ")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("キャンセル")
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        for id in &ids {
                            this.trash_photo(*id, cx);
                        }
                    });
                    true
                })
        });
    }

    fn trash_photo(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(photo) = self.db.photo_by_id(id).ok().flatten() else {
            return;
        };
        let effective_path = photo.effective_path().to_string();
        let replaced = photo.is_replaced();

        // A replaced photo's "original" is already gone; the effective path is
        // the compressed derivative that the card is currently displaying.
        let derivative_to_promote = if replaced {
            None
        } else {
            self.db
                .derivatives_for_photo(id)
                .ok()
                .and_then(|derivatives| {
                    derivatives
                        .into_iter()
                        .find(|derivative| Path::new(&derivative.path).is_file())
                })
        };

        if let Some(derivative) = &derivative_to_promote
            && let Err(error) = self.db.set_replaced(id, derivative.id)
        {
            self.message = Some(format!("圧縮版を表示に設定できません: {error}"));
            cx.notify();
            return;
        }

        match trash::delete(&effective_path) {
            Ok(_) => {
                if derivative_to_promote.is_some() {
                    // The row stays visible with the compressed derivative as
                    // its display asset.
                    let _ = self.db.bump_library_revision();
                    self.reload_library(true, cx);
                    self.message =
                        Some("元画像をゴミ箱へ移動しました（圧縮版を表示中）".to_string());
                } else {
                    if let Err(error) = self.db.mark_missing(id) {
                        tracing::warn!(%error, photo_id = id, "cannot mark trashed photo missing");
                    }
                    let _ = self.db.clear_display_derivative(id);
                    let _ = self.db.bump_library_revision();
                    self.library.remove_photo(id);
                    self.search_view.remove_photo(id);
                    self.thumbnails.unpin(id);
                    if self.selected == Some(id) {
                        self.selected = None;
                    }
                    self.selected_ids.retain(|selected| *selected != id);
                    self.selection_anchor = None;
                    if self
                        .selected_photo
                        .as_ref()
                        .is_some_and(|photo| photo.id == id)
                    {
                        self.selected_photo = None;
                        self.selected_players.clear();
                        self.selected_tags.clear();
                        self.selected_derivatives.clear();
                        self.preview = None;
                    }
                    self.message = Some("写真をゴミ箱へ移動しました".to_string());
                    self.reload_aux(cx);
                }
            }
            Err(error) => {
                if derivative_to_promote.is_some() {
                    let _ = self.db.restore_present(id);
                }
                self.message = Some(format!("ゴミ箱へ移動できません: {error}"));
            }
        }
        cx.notify();
    }

    fn commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>) {
        if let Err(error) = self.db.set_photo_tags(photo_id, &tags) {
            tracing::warn!(%error, photo_id, "cannot save photo tags");
            self.message = Some(format!("タグを保存できません: {error}"));
            cx.notify();
            return;
        }
        self.load_selected_meta(photo_id);
        // A successful write is authoritative even if the metadata read failed.
        if self.selected_tags.is_empty() && !tags.is_empty() {
            self.selected_tags = tags;
        }
        self.refresh_search_after_tag_edit(cx);
        cx.notify();
    }

    pub(crate) fn add_tag(&mut self, photo_id: i64, tag: String, cx: &mut Context<Self>) {
        let tags = match self.db.photo_tags(photo_id) {
            Ok(tags) => tags,
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot load photo tags");
                self.message = Some(format!("タグを読み込めません: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(tags) = with_tag(&tags, &tag) else {
            return;
        };
        self.commit_tags(photo_id, tags, cx);
    }

    pub(crate) fn remove_tag(&mut self, photo_id: i64, tag: &str, cx: &mut Context<Self>) {
        let tags = match self.db.photo_tags(photo_id) {
            Ok(tags) => tags,
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot load photo tags");
                self.message = Some(format!("タグを読み込めません: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(tags) = without_tag(&tags, tag) else {
            return;
        };
        self.commit_tags(photo_id, tags, cx);
    }
}

/// Trims the outer whitespace of a note and treats an empty result as "no note".
fn normalize_note(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Returns the tags left after removing `tag`, or `None` when it is absent.
fn without_tag(tags: &[String], tag: &str) -> Option<Vec<String>> {
    if !tags.iter().any(|existing| existing == tag) {
        return None;
    }
    Some(
        tags.iter()
            .filter(|existing| existing.as_str() != tag)
            .cloned()
            .collect(),
    )
}

/// Returns the tags with `tag` appended, or `None` when it is already present.
fn with_tag(tags: &[String], tag: &str) -> Option<Vec<String>> {
    if tags.iter().any(|existing| existing == tag) {
        return None;
    }
    let mut tags = tags.to_vec();
    tags.push(tag.to_string());
    Some(tags)
}

#[cfg(test)]
mod tests {
    use super::{normalize_note, with_tag, without_tag};

    #[test]
    fn notes_normalize_empty_to_none_and_trim_text() {
        assert_eq!(normalize_note(""), None);
        assert_eq!(normalize_note("  \n\t "), None);
        assert_eq!(normalize_note(" hello \n"), Some("hello"));
        assert_eq!(normalize_note("line1\nline2"), Some("line1\nline2"));
    }

    #[test]
    fn tag_removal_filters_only_the_named_tag() {
        let tags = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(without_tag(&tags, "alpha"), Some(vec!["beta".to_string()]));
        assert_eq!(without_tag(&tags, "missing"), None);
    }

    #[test]
    fn tag_addition_rejects_duplicates() {
        let tags = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(
            with_tag(&tags, "gamma"),
            Some(vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string()
            ])
        );
        assert_eq!(with_tag(&tags, "alpha"), None);
    }
}
