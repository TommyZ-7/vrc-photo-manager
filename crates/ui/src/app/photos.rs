//! App-side photo browser state: selection, thumbnails, previews, and the
//! active grid accessor.
//!
//! Rendering stays in `ui::library`; these methods only touch App fields and
//! background decode queues.

use std::cell::{Ref, RefMut};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::*;
use gpui_kit::*;
use vrc_photo_core::model::{DeletionJobSummary, DeletionReason, Photo};
use vrc_photo_core::{preview, thumbnail};

use crate::app::cli::log_timing;
use crate::app::demo::demo_thumb_path;
use crate::app::{App, Screen};
use crate::state::photo_browser::PhotoBrowser;
use crate::state::preview::{preview_cache_dir, preview_key, should_request_preview};
use crate::state::thumbnail::{decode_thumbnail, make_render_image, rgba_to_bgra};

#[derive(Debug)]
enum TrashOutcome {
    Removed,
    Promoted,
    Skipped,
    Failed(String),
}

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
        if !self.thumbnails.borrow_mut().begin(photo.id) {
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
                        this.thumbnails
                            .borrow_mut()
                            .insert(id, make_render_image(rgba), color);
                    }
                    None => this.thumbnails.borrow_mut().fail(id),
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
        let Some(pinned) = self.thumbnails.borrow().pinned else {
            return;
        };
        if self.thumbnails.borrow_mut().get(pinned).is_some() {
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
        self.selected_ids.replace([id]);
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
        self.thumbnails.borrow_mut().pin(id);
        if self.thumbnails.borrow_mut().get(id).is_none()
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
                        decoded.map(rgba_to_bgra)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.preview_queue.finish(key);
                    let current = this
                        .selected_photo
                        .as_ref()
                        .is_some_and(|photo| preview_key(photo) == key);
                    if current && let Some(rgba) = decoded {
                        this.preview = Some(make_render_image(rgba));
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
            self.thumbnails.borrow_mut().unpin(id);
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
        if self.selected_ids.toggle(id) {
            self.selection_anchor = Some(id);
            self.set_primary_photo(id, cx);
        } else if self.selection_anchor == Some(id) {
            self.selection_anchor = self.selected_ids.last();
        }
        if self.selected == Some(id) && !self.selected_ids.contains(id) {
            if let Some(next) = self.selected_ids.last() {
                self.set_primary_photo(next, cx);
            } else {
                self.clear_selection(cx);
                return;
            }
        }
        cx.notify();
    }

    fn photo_browser(&self) -> Ref<'_, PhotoBrowser> {
        if self.screen == Screen::Search {
            self.search_view.borrow()
        } else {
            self.library.borrow()
        }
    }

    fn select_photo_range(&mut self, id: i64, add: bool, cx: &mut Context<Self>) {
        let index = self.photo_browser().index.clone();
        let Some(anchor) = self.selection_anchor else {
            self.select_photo(id, cx);
            return;
        };
        let Some(entries) = index.range_between(anchor, id) else {
            self.select_photo(id, cx);
            return;
        };
        if add {
            for entry in entries {
                self.selected_ids.insert(entry.id);
            }
        } else {
            self.selected_ids
                .replace(entries.iter().map(|entry| entry.id));
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

    pub(crate) fn browser_mut(&self, search: bool) -> RefMut<'_, PhotoBrowser> {
        if search {
            self.search_view.borrow_mut()
        } else {
            self.library.borrow_mut()
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
        self.selected_ids.replace(ids);
        self.set_primary_photo(primary, cx);
        cx.notify();
    }

    /// IDs the ribbon's bulk actions operate on. The primary selection is the
    /// fallback while the multi-selection list is empty.
    pub(crate) fn selected_action_ids(&self) -> Vec<i64> {
        if self.selected_ids.is_empty() {
            self.selected.into_iter().collect()
        } else {
            self.selected_ids.to_vec()
        }
    }

    pub(crate) fn color_for(&self, photo: &Photo) -> u32 {
        self.thumbnails.borrow().color(photo.id).unwrap_or_else(|| {
            self.palette
                .scene_color(photo.world_name.as_deref().unwrap_or(""))
        })
    }

    pub(crate) fn selected_color(&self) -> u32 {
        self.selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(self.palette.bg)
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
            self.toast_error(format!("メモを保存できません: {error}"), cx);
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
        let summary = self.db.deletion_job_summary(&ids).unwrap_or_default();
        let warning_lines = deletion_warning_lines(&summary);
        let heading = deletion_warning_heading(summary.affected_photos, count);
        let description = if count == 1 {
            self.selected_photo
                .as_ref()
                .map(|photo| {
                    format!(
                        "「{}」を削除待ちに追加します。デーモンがOSゴミ箱へ移動します。",
                        photo.captured_at_local
                    )
                })
                .unwrap_or_else(|| {
                    "写真を削除待ちに追加します。デーモンがOSゴミ箱へ移動します。".to_string()
                })
        } else {
            format!("{count} 枚を削除待ちに追加します。デーモンがOSゴミ箱へ移動します。")
        };
        let entity = cx.entity();
        let panel_deep = self.palette.panel_deep;

        if warning_lines.is_empty() {
            window.open_alert_dialog(cx, move |alert, _, _| {
                let entity = entity.clone();
                let ids = ids.clone();
                alert
                    .title("写真を削除")
                    .bg(rgb(panel_deep))
                    .description(description.clone())
                    .confirm()
                    .ok_text("削除待ちに追加")
                    .ok_variant(ButtonVariant::Danger)
                    .cancel_text("キャンセル")
                    .on_ok(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.request_trash_photos(ids.clone(), cx);
                        });
                        true
                    })
            });
            return;
        }

        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let ids = ids.clone();
            let lines = warning_lines.clone();
            let content = v_flex()
                .gap(px(6.))
                .child(div().text_size(px(12.)).child(heading.clone()))
                .children(
                    lines
                        .into_iter()
                        .map(|line| div().text_size(px(11.)).child(line)),
                );
            alert
                .title("未完了のジョブがあります")
                .bg(rgb(panel_deep))
                .description(content)
                .confirm()
                .ok_text("削除待ちに追加")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("キャンセル")
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.request_trash_photos(ids.clone(), cx);
                    });
                    true
                })
        });
    }

    /// Demo-only direct trash path. Real deletions go through the daemon
    /// queue so dependent jobs can drain first.
    fn trash_photos(&mut self, ids: Vec<i64>, cx: &mut Context<Self>) {
        let mut removed = HashSet::new();
        let mut promoted = HashSet::new();
        let mut failures: Vec<String> = Vec::new();
        for id in ids {
            match self.trash_photo(id) {
                TrashOutcome::Removed => {
                    removed.insert(id);
                }
                TrashOutcome::Promoted => {
                    promoted.insert(id);
                }
                TrashOutcome::Skipped => {}
                TrashOutcome::Failed(error) => failures.push(error),
            }
        }

        if !removed.is_empty() || !promoted.is_empty() {
            self.remove_photos_from_ui(&removed, !promoted.is_empty(), cx);
            let moved = removed.len() + promoted.len();
            let mut message = format!("{moved} 枚をゴミ箱へ移動しました");
            if !promoted.is_empty() {
                message.push_str(&format!("（{} 枚は圧縮版を表示中）", promoted.len()));
            }
            self.toast_success(message, cx);
        }

        if !failures.is_empty() {
            let detail = if failures.len() == 1 {
                format!(": {}", failures[0])
            } else {
                format!("（{} 件）", failures.len())
            };
            self.toast_error(format!("ゴミ箱へ移動できませんでした{detail}"), cx);
        }
        cx.notify();
    }

    /// Adds a manual deletion request for real libraries. The photo disappears
    /// immediately; the daemon trashes the files after its jobs drain.
    pub(crate) fn request_trash_photos(&mut self, ids: Vec<i64>, cx: &mut Context<Self>) {
        if self.demo {
            self.trash_photos(ids, cx);
            return;
        }
        let requested = match self
            .db
            .request_deletion(&ids, DeletionReason::Manual, false)
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%error, "cannot request photo deletion");
                self.toast_error(format!("削除を登録できません: {error}"), cx);
                cx.notify();
                return;
            }
        };
        if requested.is_empty() {
            self.toast_warning("削除できる写真がありません", cx);
            cx.notify();
            return;
        }
        let removed: HashSet<i64> = requested.iter().copied().collect();
        self.remove_photos_from_ui(&removed, false, cx);
        if self.daemon_active {
            self.toast_success(
                format!("{} 枚を削除待ちに追加しました", requested.len()),
                cx,
            );
        } else {
            self.toast_warning(
                format!(
                    "{} 枚を削除待ちに追加しました。デーモン起動後に実行されます",
                    requested.len()
                ),
                cx,
            );
        }
        self.send_wake_jobs(cx);
        cx.notify();
    }

    /// Removes photos from the mounted views without rebuilding the whole
    /// library index. The caller already changed the database.
    fn remove_photos_from_ui(
        &mut self,
        removed: &HashSet<i64>,
        promoted: bool,
        cx: &mut Context<Self>,
    ) {
        let revision = match self.db.bump_library_revision() {
            Ok(revision) => Some(revision),
            Err(error) => {
                tracing::warn!(%error, "cannot bump library revision after trash");
                None
            }
        };
        self.library.borrow_mut().remove_photos(removed);
        self.search_view.borrow_mut().remove_photos(removed);
        if promoted {
            // Kept cards now display the promoted derivative.
            self.library.borrow_mut().invalidate_pages();
            self.search_view.borrow_mut().invalidate_pages();
        }
        for id in removed {
            self.thumbnails.borrow_mut().unpin(*id);
        }
        self.selected_ids.remove_all(removed);
        if self
            .selection_anchor
            .is_some_and(|anchor| removed.contains(&anchor))
        {
            self.selection_anchor = None;
        }
        if self
            .selected
            .is_some_and(|selected| removed.contains(&selected))
        {
            self.selected = None;
        }
        if self
            .selected_photo
            .as_ref()
            .is_some_and(|photo| removed.contains(&photo.id))
        {
            self.selected_photo = None;
            self.selected_players.clear();
            self.selected_tags.clear();
            self.selected_derivatives.clear();
            self.preview = None;
        }
        if self.note_edit_id.is_some_and(|id| removed.contains(&id)) {
            self.note_edit_id = None;
        }
        self.reload_aux(cx);
        if let Some(revision) = revision {
            // The local snapshot already reflects the deletions; claim the
            // revision so the next poll does not re-query 100k rows.
            self.last_library_revision = revision;
        }
    }

    /// Moves one photo to the OS trash and updates its row. Side panels and
    /// the index are refreshed once by the batch caller.
    fn trash_photo(&mut self, id: i64) -> TrashOutcome {
        let Some(photo) = self.db.photo_by_id(id).ok().flatten() else {
            return TrashOutcome::Skipped;
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
            tracing::warn!(%error, photo_id = id, "cannot promote compressed derivative");
            return TrashOutcome::Failed(format!("圧縮版を表示に設定できません: {error}"));
        }

        match trash::delete(&effective_path) {
            Ok(_) => {
                if derivative_to_promote.is_some() {
                    // The row stays visible with the compressed derivative as
                    // its display asset.
                    TrashOutcome::Promoted
                } else {
                    if let Err(error) = self.db.mark_missing(id) {
                        tracing::warn!(%error, photo_id = id, "cannot mark trashed photo missing");
                    }
                    let _ = self.db.clear_display_derivative(id);
                    TrashOutcome::Removed
                }
            }
            Err(error) => {
                if derivative_to_promote.is_some() {
                    let _ = self.db.restore_present(id);
                }
                tracing::warn!(%error, photo_id = id, "cannot move photo to trash");
                TrashOutcome::Failed(format!("ゴミ箱へ移動できません: {error}"))
            }
        }
    }

    fn commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>) {
        if let Err(error) = self.db.set_photo_tags(photo_id, &tags) {
            tracing::warn!(%error, photo_id, "cannot save photo tags");
            self.toast_error(format!("タグを保存できません: {error}"), cx);
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
                self.toast_error(format!("タグを読み込めません: {error}"), cx);
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
                self.toast_error(format!("タグを読み込めません: {error}"), cx);
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

/// Warning heading for a manual deletion that still has jobs. The first count
/// is the number of selected photos with jobs, not the whole selection.
fn deletion_warning_heading(affected: i64, selected: usize) -> String {
    let selected = selected as i64;
    if affected < selected {
        format!("{affected} 枚に未完了のジョブがあります（選択 {selected} 枚）。")
    } else {
        format!("{selected} 枚に未完了のジョブがあります。")
    }
}

/// Warning body for a manual deletion that still has jobs. An empty vector
/// means the simple confirmation is enough.
fn deletion_warning_lines(summary: &DeletionJobSummary) -> Vec<String> {
    if !summary.needs_warning() {
        return Vec::new();
    }
    vec![
        job_status_line("圧縮", summary.compression),
        job_status_line("送信", summary.send),
        "・全ジョブの完了後に削除されます。".to_string(),
        "・送信ジョブはキャンセルされます。".to_string(),
        "・失敗したジョブがある場合、削除は保留され、通知されます。".to_string(),
    ]
}

fn job_status_line(label: &str, counts: vrc_photo_core::model::JobStatusCounts) -> String {
    if counts.total() == 0 {
        return format!("{label}: なし");
    }
    let mut parts = Vec::new();
    if counts.pending > 0 {
        parts.push(format!("実行待ち {}", counts.pending));
    }
    if counts.processing > 0 {
        parts.push(format!("処理中 {}", counts.processing));
    }
    if counts.waiting > 0 {
        parts.push(format!("未実行 {}", counts.waiting));
    }
    if counts.failed > 0 {
        parts.push(format!("失敗 {}", counts.failed));
    }
    format!("{label}: {}", parts.join(" / "))
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
    use super::{
        deletion_warning_heading, deletion_warning_lines, normalize_note, with_tag, without_tag,
    };
    use vrc_photo_core::model::{DeletionJobSummary, JobStatusCounts};

    #[test]
    fn deletion_warning_heading_counts_only_photos_with_jobs() {
        assert_eq!(
            deletion_warning_heading(2, 5),
            "2 枚に未完了のジョブがあります（選択 5 枚）。"
        );
        assert_eq!(
            deletion_warning_heading(3, 3),
            "3 枚に未完了のジョブがあります。"
        );
    }

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
    fn deletion_warning_lines_list_compression_and_send_counts() {
        let summary = DeletionJobSummary {
            compression: JobStatusCounts {
                waiting: 2,
                pending: 1,
                processing: 1,
                failed: 1,
            },
            send: JobStatusCounts::default(),
            affected_photos: 1,
        };
        let lines = deletion_warning_lines(&summary);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("圧縮") && line.contains("失敗 1"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("送信") && line.contains("なし"))
        );
        assert!(lines.iter().any(|line| line.contains("全ジョブの完了後")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("送信ジョブはキャンセル"))
        );
        assert!(lines.iter().any(|line| line.contains("保留")));
    }

    #[test]
    fn deletion_warning_lines_are_empty_without_jobs() {
        assert!(deletion_warning_lines(&DeletionJobSummary::default()).is_empty());
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
