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
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::*;
use gpui_kit::*;
use vrc_photon_core::model::{DeletionJobSummary, DeletionReason, Photo};
use vrc_photon_core::{CoreError, preview, thumbnail};
use vrc_photon_i18n::{tr, tr_args};
use vrc_photon_ipc::Request as IpcRequest;

use crate::app::cli::log_timing;
use crate::app::demo::demo_thumb_path;
use crate::app::{App, Screen};
use crate::state::photo_browser::{PhotoBrowser, plan_page_retry, row_for_photo};
use crate::state::preview::{
    preview_cache_dir, preview_key, should_apply_preview, should_request_preview,
};
use crate::state::thumbnail::{decode_thumbnail, make_render_image, rgba_to_bgra};
use crate::ui::components::photo_scene_key;

/// Delay before a partial thumbnail notice batch is reported. Keeps a quiet
/// gallery's generated files visible to the daemon without one request each.
const THUMBNAIL_NOTICE_DELAY: Duration = Duration::from_secs(1);

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
        self.load_selected_ai(id);
        if self.note_edit_id.is_some_and(|edit_id| edit_id != id) {
            self.note_edit_id = None;
        }
    }

    /// Starts a background decode for a grid thumbnail when the cache has a
    /// free slot. Cached, pending, and previously failed photos are skipped.
    pub(crate) fn request_thumbnail(&mut self, photo: &Photo, cx: &mut Context<Self>) {
        let Some(ticket) = self.thumbnails.borrow_mut().begin_ticket(photo.id) else {
            return;
        };
        let id = photo.id;
        let source = PathBuf::from(photo.effective_path());
        let spec = self.settings.thumbnail_spec();
        let cache_path = match &self.demo_assets {
            Some(assets) => demo_thumb_path(assets, id),
            None => thumbnail::thumbnail_path(id, spec).unwrap_or_default(),
        };
        let cache_dir = if self.demo_assets.is_none() {
            thumbnail::thumbnails_dir()
        } else {
            None
        };
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    let decode_start = Instant::now();
                    let decoded =
                        decode_thumbnail(id, &cache_path, &source, cache_dir.as_deref(), spec);
                    log_timing("thumbnail decode", decode_start);
                    decoded
                })
                .await;
            this.update(cx, |this, cx| {
                match decoded {
                    Some(decoded) => {
                        let inserted = this.thumbnails.borrow_mut().insert_if_ticket(
                            ticket,
                            id,
                            make_render_image(decoded.image),
                            decoded.color,
                        );
                        if inserted && let Some(bytes) = decoded.generated_bytes {
                            this.note_generated_thumbnail(bytes, cx);
                        }
                    }
                    None => this.thumbnails.borrow_mut().fail_if_ticket(ticket, id),
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

    /// Marks the cached thumbnail files as recently used after the grid reads
    /// them from the memory cache. One background task handles the whole
    /// batch; demo assets have no real cache to touch.
    pub(crate) fn refresh_thumbnail_access(&self, ids: Vec<i64>, cx: &mut Context<Self>) {
        if ids.is_empty() || self.demo_assets.is_some() {
            return;
        }
        let spec = self.settings.thumbnail_spec();
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .spawn(async move {
                    for id in ids {
                        if let Some(path) = thumbnail::thumbnail_path(id, spec) {
                            thumbnail::refresh_thumbnail_access(&path);
                        }
                    }
                })
                .await;
        })
        .detach();
    }

    /// Records a thumbnail the UI generated itself. The daemon uses the same
    /// notice to track cache growth, so the files are reported in batches
    /// rather than as one request per decode. Demo assets are not part of the
    /// real cache and stay silent.
    fn note_generated_thumbnail(&mut self, bytes: u64, cx: &mut Context<Self>) {
        if self.demo_assets.is_some() {
            return;
        }
        if self.thumbnail_notice.add(bytes) {
            self.flush_thumbnail_notice(cx);
            return;
        }
        if self.thumbnail_notice_scheduled {
            return;
        }
        self.thumbnail_notice_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(THUMBNAIL_NOTICE_DELAY).await;
            this.update(cx, |this, cx| {
                this.thumbnail_notice_scheduled = false;
                this.flush_thumbnail_notice(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Sends the accumulated generation notice, if any, and only while the
    /// daemon connection is up. A batch held across a disconnect is kept for
    /// the reconnect path to flush.
    pub(crate) fn flush_thumbnail_notice(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            return;
        }
        let Some((files, bytes)) = self.thumbnail_notice.take() else {
            return;
        };
        self.wake_daemon(IpcRequest::ThumbnailsGenerated { files, bytes }, cx);
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
        // Selecting the photo is an explicit reload request: clear a recent
        // thumbnail failure so it is not hidden until the retry TTL.
        self.thumbnails.borrow_mut().retry(id);
        if changed {
            let previous_color = self
                .selected_photo
                .as_ref()
                .map(|photo| self.color_for(photo));
            self.previous_preview = self.preview.take();
            self.previous_color = previous_color;
            self.preview_key = None;
            self.preview_attempt = None;
            self.reset_full_preview();
            self.zoom = Default::default();
            self.zoom_drag = None;
            self.load_selected_meta(id);
            self.request_preview(cx);
            if self.quick_look_open {
                self.request_full_preview(cx);
            }
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
                    let selected = this.selected_photo.as_ref().map(preview_key);
                    if should_apply_preview(selected, key)
                        && let Some(rgba) = decoded
                    {
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

    /// Requests the original display file for Quick Look. This queue is
    /// separate from the 1200 px preview queue so a large decode never delays
    /// the next photo's instant preview.
    pub(crate) fn request_full_preview(&mut self, cx: &mut Context<Self>) {
        if !self.quick_look_open {
            self.full_preview_queue.cancel_wanted();
            return;
        }
        let Some(photo) = self.selected_photo.as_ref() else {
            return;
        };
        let key = preview_key(photo);
        if self.full_preview_key == Some(key) {
            self.full_preview_queue.cancel_wanted();
            return;
        }
        if self.full_preview_queue.want(key) {
            self.pump_full_preview(cx);
        }
    }

    fn pump_full_preview(&mut self, cx: &mut Context<Self>) {
        loop {
            let Some(key) = self.full_preview_queue.begin() else {
                return;
            };
            let matches = self
                .selected_photo
                .as_ref()
                .is_some_and(|photo| preview_key(photo) == key);
            if !matches || self.full_preview_key == Some(key) {
                self.full_preview_queue.finish(key);
                continue;
            }
            let source = PathBuf::from(
                self.selected_photo
                    .as_ref()
                    .expect("selected photo")
                    .effective_path(),
            );
            cx.spawn(async move |this, cx| {
                let decoded = cx
                    .background_executor()
                    .spawn(async move {
                        let start = Instant::now();
                        let decoded = preview::decode_full_image(&source);
                        log_timing("full preview decode", start);
                        decoded.map(rgba_to_bgra)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.full_preview_queue.finish(key);
                    let selected = this.selected_photo.as_ref().map(preview_key);
                    if this.quick_look_open
                        && should_apply_preview(selected, key)
                        && let Some(rgba) = decoded
                    {
                        this.full_preview = Some(make_render_image(rgba));
                        this.full_preview_key = Some(key);
                    }
                    this.pump_full_preview(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
    }

    pub(crate) fn reset_full_preview(&mut self) {
        self.full_preview = None;
        self.full_preview_key = None;
        self.full_preview_queue.cancel_wanted();
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

    /// Resets every piece of preview state: the decoded preview, its key
    /// and attempt bookkeeping, the queued decode request, and the
    /// crossfade's previous frame. Selection clears, forced viewer closes,
    /// and removals all go through here so no path can leave a stale
    /// preview (or a decode for one) behind.
    pub(crate) fn reset_preview_state(&mut self) {
        self.preview = None;
        self.preview_key = None;
        self.preview_attempt = None;
        self.preview_queue.cancel_wanted();
        self.reset_full_preview();
        self.zoom = Default::default();
        self.zoom_drag = None;
        self.previous_preview = None;
        self.previous_color = None;
    }

    /// Drops the in-memory thumbnail and preview for a photo whose display
    /// source changed (a completed `replace` compression). The next display
    /// request regenerates both from the derivative.
    pub(crate) fn invalidate_photo_media(&mut self, id: i64, cx: &mut Context<Self>) {
        self.thumbnails.borrow_mut().invalidate(id);
        if self.selected == Some(id) {
            self.preview = None;
            self.preview_key = None;
            self.preview_attempt = None;
            self.previous_preview = None;
            self.previous_color = None;
            self.reset_full_preview();
            self.preview_queue.cancel_wanted();
            cx.notify();
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
        self.selected_ai = None;
        self.selected_ai_tags.clear();
        self.selected_derivatives.clear();
        self.reset_preview_state();
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

    /// Marks one failed page as retrying and immediately loads only it again.
    ///
    /// `expected_generation` is the grid generation that rendered the error
    /// panel. If the snapshot or page cache changed in the meantime the click
    /// is ignored instead of reviving a stale page. Loaded cards, selection,
    /// and scroll position are untouched; the inline panel stays mounted with
    /// its loading label until the request succeeds or fails.
    pub(crate) fn retry_failed_page(
        &mut self,
        search: bool,
        page: usize,
        expected_generation: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(retry) = plan_page_retry(&mut self.browser_mut(search), expected_generation, page)
        else {
            return;
        };
        self.load_pages(search, retry.generation, vec![retry.page], cx);
        cx.notify();
    }

    /// Returns the active grid to its first row without clearing selection.
    pub(crate) fn scroll_grid_to_first_page(&mut self, search: bool, cx: &mut Context<Self>) {
        self.browser_mut(search)
            .scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
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

    /// Whether the ribbon's bulk actions have anything to operate on,
    /// without cloning the selection. Equivalent to
    /// `!selected_action_ids().is_empty()`; rendering uses this, the clone
    /// stays on click paths.
    pub(crate) fn has_action_selection(&self) -> bool {
        !self.selected_ids.is_empty() || self.selected.is_some()
    }

    pub(crate) fn color_for(&self, photo: &Photo) -> u32 {
        self.thumbnails
            .borrow()
            .color(photo.id)
            .unwrap_or_else(|| self.palette.scene_color(photo_scene_key(photo)))
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
            self.toast_error(tr_args!("photos.note.save_error", error = error), cx);
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

    /// Moves the grid selection by `delta` photos in index order and
    /// scrolls the target row into view. Bound to the arrow keys in the
    /// `photo_grid` key context, so it only runs while the grid itself has
    /// focus; text inputs elsewhere never dispatch it.
    pub(crate) fn move_grid_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.move_grid_selection_by(delta, cx);
    }

    /// Vertical arrow movement: one row is `columns` photos.
    pub(crate) fn move_grid_selection_vertical(&mut self, rows: isize, cx: &mut Context<Self>) {
        let columns = self.library.borrow().columns.max(1) as isize;
        self.move_grid_selection_by(rows * columns, cx);
    }

    fn move_grid_selection_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        let target = {
            let browser = self.library.borrow();
            let entries = &browser.index.entries;
            let current = self.selected.and_then(|id| browser.index.row_of_id(id));
            grid_move_target(current, delta, entries.len()).map(|pos| entries[pos].id)
        };
        let Some(target) = target else {
            return;
        };
        if Some(target) == self.selected {
            return;
        }
        self.select_photo(target, cx);
        let browser = self.library.borrow();
        let row = row_for_photo(&browser.index, target, browser.columns);
        browser
            .scroll
            .scroll_to_item_strict(row, ScrollStrategy::Nearest);
    }

    pub(crate) fn trash_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            return;
        }
        let confirmation = DeletionConfirmation::from_result(self.db.deletion_job_summary(&ids));
        self.open_deletion_confirmation(ids, confirmation, window, cx);
    }

    /// Opens the delete confirmation for a freshly checked summary. A retry
    /// re-enters here, so success follows the normal path and a repeated
    /// failure stays in the unavailable state.
    fn open_deletion_confirmation(
        &mut self,
        ids: Vec<i64>,
        confirmation: DeletionConfirmation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = ids.len();
        if count == 0 {
            return;
        }
        let description = self.deletion_confirmation_description(count);
        let panel_deep = self.palette.panel_deep;
        match confirmation {
            DeletionConfirmation::Simple => {
                let entity = cx.entity();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let entity = entity.clone();
                    let ids = ids.clone();
                    alert
                        .title(tr!("photos.delete.title"))
                        .bg(rgb(panel_deep))
                        .description(description.clone())
                        .confirm()
                        .ok_text(tr!("photos.delete.ok"))
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(tr!("common.cancel"))
                        .on_ok(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.request_trash_photos(ids.clone(), cx);
                            });
                            true
                        })
                });
            }
            DeletionConfirmation::Warning(summary) => {
                let warning_lines = deletion_warning_lines(&summary);
                let heading = deletion_warning_heading(summary.affected_photos, count);
                let entity = cx.entity();
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
                        .title(tr!("photos.jobs.title"))
                        .bg(rgb(panel_deep))
                        .description(content)
                        .confirm()
                        .ok_text(tr!("photos.delete.ok"))
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(tr!("common.cancel"))
                        .on_ok(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.request_trash_photos(ids.clone(), cx);
                            });
                            true
                        })
                });
            }
            DeletionConfirmation::Unavailable(reason) => {
                self.open_deletion_unavailable(ids, count, reason, window, cx);
            }
        }
    }

    fn deletion_confirmation_description(&self, count: usize) -> String {
        if count == 1 {
            self.selected_photo
                .as_ref()
                .map(|photo| tr_args!("photos.delete.confirm_one", name = photo.captured_at_local))
                .unwrap_or_else(|| tr!("photos.delete.confirm_none"))
        } else {
            tr_args!("photos.delete.confirm_count", count = count)
        }
    }

    /// The job-count query failed, so absence of jobs is unproven. Keep the
    /// delete action disabled and offer only Cancel and a user-triggered
    /// re-check; never fall through to `request_trash_photos`.
    fn open_deletion_unavailable(
        &mut self,
        ids: Vec<i64>,
        count: usize,
        reason: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::warn!(reason = %reason, count, "cannot check deletion jobs");
        let entity = cx.entity();
        let p = self.palette;
        let panel_deep = p.panel_deep;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let retry_entity = entity.clone();
            let retry_ids = ids.clone();
            let unknown = tr!("photos.delete.unavailable.count_unknown");
            let stat = |label: String| {
                v_flex()
                    .flex_1()
                    .p(px(9.))
                    .gap(px(2.))
                    .rounded(px(9.))
                    .border_1()
                    .border_color(rgb(p.line_soft))
                    .bg(rgb(p.panel_2))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(unknown.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted_2))
                            .child(label),
                    )
            };
            let title = if count == 1 {
                tr!("photos.delete.unavailable.title_one")
            } else {
                tr_args!("photos.delete.unavailable.title_count", count = count)
            };
            alert
                .title(title)
                .bg(rgb(panel_deep))
                .description(
                    v_flex()
                        .gap(px(10.))
                        .child(
                            h_flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(p.muted))
                                        .child(tr!("photos.delete.unavailable.intro")),
                                )
                                .child(
                                    div()
                                        .px(px(8.))
                                        .py(px(1.))
                                        .rounded_full()
                                        .border_1()
                                        .border_color(rgb(p.amber))
                                        .bg(rgb(p.amber_soft))
                                        .text_size(px(9.))
                                        .text_color(rgb(p.amber))
                                        .child(tr!("photos.delete.unavailable.badge")),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .child(stat(tr!("photos.jobs.compression")))
                                .child(stat(tr!("photos.jobs.send")))
                                .child(stat(tr!("photos.jobs.ai_tagging"))),
                        )
                        .child(
                            v_flex()
                                .gap(px(2.))
                                .p(px(10.))
                                .rounded(px(9.))
                                .border_1()
                                .border_color(rgb(p.amber))
                                .bg(rgb(p.amber_soft))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .font_semibold()
                                        .text_color(rgb(p.amber))
                                        .child(tr!("photos.delete.unavailable.headline")),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(p.muted))
                                        .child(tr!("photos.delete.unavailable.body")),
                                ),
                        ),
                )
                .footer(
                    DialogFooter::new()
                        .items_center()
                        .child(
                            div()
                                .mr_auto()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(tr!("photos.delete.unavailable.hint")),
                        )
                        .child(
                            Button::new("deletion-unavailable-cancel")
                                .label(tr!("common.cancel"))
                                .ghost()
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("deletion-unavailable-retry")
                                .label(tr!("photos.delete.unavailable.retry"))
                                .primary()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let entity = retry_entity.clone();
                                    let ids = retry_ids.clone();
                                    window.defer(cx, move |window, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.retry_deletion_confirmation(ids, window, cx);
                                        });
                                    });
                                }),
                        )
                        .child(
                            Button::new("deletion-unavailable-disabled")
                                .label(tr!("photos.delete.unavailable.disabled_action"))
                                .danger()
                                .disabled(true),
                        ),
                )
        });
    }

    fn retry_deletion_confirmation(
        &mut self,
        ids: Vec<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty() {
            return;
        }
        let confirmation = DeletionConfirmation::from_result(self.db.deletion_job_summary(&ids));
        self.open_deletion_confirmation(ids, confirmation, window, cx);
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
            let mut message = tr_args!("photos.trash.moved", count = moved);
            if !promoted.is_empty() {
                message.push_str(&tr_args!("photos.trash.promoted", count = promoted.len()));
            }
            self.toast_success(message, cx);
        }

        if !failures.is_empty() {
            let detail = if failures.len() == 1 {
                format!(": {}", failures[0])
            } else {
                tr_args!("photos.trash.failures", count = failures.len())
            };
            self.toast_error(tr_args!("photos.trash.error", detail = detail), cx);
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
                self.toast_error(tr_args!("photos.delete.register_error", error = error), cx);
                cx.notify();
                return;
            }
        };
        if requested.is_empty() {
            self.toast_warning(tr!("photos.delete.none"), cx);
            cx.notify();
            return;
        }
        let removed: HashSet<i64> = requested.iter().copied().collect();
        self.remove_photos_from_ui(&removed, false, cx);
        if self.daemon_active {
            self.toast_success(
                tr_args!("photos.delete.registered", count = requested.len()),
                cx,
            );
        } else {
            self.toast_warning(
                tr_args!(
                    "photos.delete.registered_after_start",
                    count = requested.len()
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
            self.selected_ai = None;
            self.selected_ai_tags.clear();
            self.selected_derivatives.clear();
            self.reset_preview_state();
        }
        if self.note_edit_id.is_some_and(|id| removed.contains(&id)) {
            self.note_edit_id = None;
        }
        if let Some(revision) = revision {
            // The local snapshot already reflects the deletions; claim the
            // revision so the next poll does not re-query 100k rows.
            self.last_library_revision = revision;
        }
        // Stats, compressed ids, and the job queue are collected by the
        // background live snapshot; the visible index was already updated
        // above.
        self.refresh_live_async(cx);
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
            return TrashOutcome::Failed(tr_args!("photos.trash.promote_error", error = error));
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
                TrashOutcome::Failed(tr_args!("photos.trash.move_error", error = error))
            }
        }
    }

    fn commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>) {
        if let Err(error) = self.db.set_photo_tags(photo_id, &tags) {
            tracing::warn!(%error, photo_id, "cannot save photo tags");
            self.toast_error(tr_args!("photos.tags.save_error", error = error), cx);
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
                self.toast_error(tr_args!("photos.tags.load_error", error = error), cx);
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
                self.toast_error(tr_args!("photos.tags.load_error", error = error), cx);
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

/// The outcome of checking pending jobs before a manual deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DeletionConfirmation {
    /// No jobs to warn about; the simple confirmation still deletes.
    Simple,
    /// Jobs are still pending; warn and keep the danger action available.
    Warning(DeletionJobSummary),
    /// The query failed, so "no jobs" is unproven. Deletion must stay blocked.
    Unavailable(String),
}

impl DeletionConfirmation {
    fn from_result(result: Result<DeletionJobSummary, CoreError>) -> Self {
        match result {
            Ok(summary) if summary.needs_warning() => Self::Warning(summary),
            Ok(_) => Self::Simple,
            Err(error) => Self::Unavailable(error.to_string()),
        }
    }

    /// The action the primary button takes. An unverified summary can only
    /// re-run the query; it must never reach the trash request.
    #[cfg(test)]
    fn primary_action(&self) -> DeletionPrimaryAction {
        match self {
            Self::Simple | Self::Warning(_) => DeletionPrimaryAction::RequestTrash,
            Self::Unavailable(_) => DeletionPrimaryAction::RetrySummary,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeletionPrimaryAction {
    RequestTrash,
    RetrySummary,
}

/// Warning heading for a manual deletion that still has jobs. The first count
/// is the number of selected photos with jobs, not the whole selection.
fn deletion_warning_heading(affected: i64, selected: usize) -> String {
    let selected = selected as i64;
    if affected < selected {
        tr_args!(
            "photos.jobs.affected_selected",
            affected = affected,
            selected = selected
        )
    } else {
        tr_args!("photos.jobs.affected", selected = selected)
    }
}

/// Warning body for a manual deletion that still has jobs. An empty vector
/// means the simple confirmation is enough.
fn deletion_warning_lines(summary: &DeletionJobSummary) -> Vec<String> {
    if !summary.needs_warning() {
        return Vec::new();
    }
    vec![
        job_status_line(tr!("photos.jobs.compression"), summary.compression),
        job_status_line(tr!("photos.jobs.send"), summary.send),
        job_status_line(tr!("photos.jobs.ai_tagging"), summary.tagging),
        tr!("photos.jobs.note_all"),
        tr!("photos.jobs.note_send_cancelled"),
        tr!("photos.jobs.note_failed"),
    ]
}

fn job_status_line(
    label: impl Into<String>,
    counts: vrc_photon_core::model::JobStatusCounts,
) -> String {
    let label: String = label.into();
    if counts.total() == 0 {
        return tr_args!("photos.jobs.none", label = label);
    }
    let mut parts = Vec::new();
    if counts.pending > 0 {
        parts.push(tr_args!("photos.jobs.pending", count = counts.pending));
    }
    if counts.processing > 0 {
        parts.push(tr_args!(
            "photos.jobs.processing",
            count = counts.processing
        ));
    }
    if counts.waiting > 0 {
        parts.push(tr_args!("photos.jobs.waiting", count = counts.waiting));
    }
    if counts.failed > 0 {
        parts.push(tr_args!("photos.jobs.failed", count = counts.failed));
    }
    if counts.unknown > 0 {
        parts.push(tr_args!("photos.jobs.unknown", count = counts.unknown));
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

/// Target position for a keyboard move in the photo grid. `current` is the
/// position of the primary selection in index order; with no selection the
/// first photo is the target. The result is clamped to the list, and `None`
/// means the list is empty.
fn grid_move_target(current: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(current) = current else {
        return Some(0);
    };
    let target = current as isize + delta;
    Some(target.clamp(0, len as isize - 1) as usize)
}

#[cfg(test)]
mod tests {
    use super::{
        DeletionConfirmation, DeletionPrimaryAction, deletion_warning_heading,
        deletion_warning_lines, grid_move_target, normalize_note, with_tag, without_tag,
    };
    use vrc_photon_core::CoreError;
    use vrc_photon_core::model::{DeletionJobSummary, JobStatusCounts};

    #[test]
    fn grid_move_target_steps_and_clamps() {
        assert_eq!(grid_move_target(Some(3), 1, 10), Some(4));
        assert_eq!(grid_move_target(Some(3), -1, 10), Some(2));
        assert_eq!(grid_move_target(Some(3), 4, 10), Some(7));
        assert_eq!(grid_move_target(Some(0), -1, 10), Some(0));
        assert_eq!(grid_move_target(Some(9), 1, 10), Some(9));
        assert_eq!(grid_move_target(Some(8), 4, 10), Some(9));
    }

    #[test]
    fn grid_move_target_without_selection_or_photos() {
        assert_eq!(grid_move_target(None, 1, 10), Some(0));
        assert_eq!(grid_move_target(None, -1, 10), Some(0));
        assert_eq!(grid_move_target(Some(0), 1, 0), None);
        assert_eq!(grid_move_target(None, 1, 0), None);
    }

    #[test]
    fn deletion_confirmation_never_forgets_a_failed_job_query() {
        let unavailable =
            DeletionConfirmation::from_result(Err(CoreError::Invalid("database is locked".into())));
        assert!(
            matches!(&unavailable, DeletionConfirmation::Unavailable(reason) if reason.contains("database is locked")),
            "{unavailable:?}"
        );
        assert!(!matches!(
            &unavailable,
            DeletionConfirmation::Simple | DeletionConfirmation::Warning(_)
        ));
        assert_eq!(
            unavailable.primary_action(),
            DeletionPrimaryAction::RetrySummary,
            "a failed query must only re-check, never request the trash"
        );
        assert_ne!(
            unavailable.primary_action(),
            DeletionPrimaryAction::RequestTrash
        );
    }

    #[test]
    fn deletion_confirmation_keeps_successful_summaries_on_the_trash_path() {
        assert_eq!(
            DeletionConfirmation::from_result(Ok(DeletionJobSummary::default())),
            DeletionConfirmation::Simple
        );
        let with_jobs = DeletionJobSummary {
            compression: JobStatusCounts {
                pending: 1,
                ..JobStatusCounts::default()
            },
            ..DeletionJobSummary::default()
        };
        assert_eq!(
            DeletionConfirmation::from_result(Ok(with_jobs)),
            DeletionConfirmation::Warning(with_jobs)
        );
        assert_eq!(
            DeletionConfirmation::Simple.primary_action(),
            DeletionPrimaryAction::RequestTrash
        );
        assert_eq!(
            DeletionConfirmation::Warning(with_jobs).primary_action(),
            DeletionPrimaryAction::RequestTrash
        );
    }

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
                unknown: 0,
            },
            send: JobStatusCounts::default(),
            tagging: JobStatusCounts::default(),
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
    fn deletion_warning_lines_include_unknown_compression_statuses() {
        let summary = DeletionJobSummary {
            compression: JobStatusCounts {
                unknown: 2,
                ..JobStatusCounts::default()
            },
            send: JobStatusCounts::default(),
            tagging: JobStatusCounts::default(),
            affected_photos: 1,
        };
        let lines = deletion_warning_lines(&summary);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("圧縮") && line.contains("不明 2")),
            "{lines:?}"
        );
    }

    #[test]
    fn deletion_warning_lines_list_send_counts() {
        let summary = DeletionJobSummary {
            compression: JobStatusCounts::default(),
            send: JobStatusCounts {
                waiting: 1,
                pending: 2,
                processing: 0,
                failed: 1,
                unknown: 0,
            },
            tagging: JobStatusCounts::default(),
            affected_photos: 3,
        };
        let lines = deletion_warning_lines(&summary);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("送信") && line.contains("失敗 1"))
        );
    }

    #[test]
    fn deletion_warning_lines_include_ai_tagging() {
        let summary = DeletionJobSummary {
            compression: JobStatusCounts::default(),
            send: JobStatusCounts::default(),
            tagging: JobStatusCounts {
                waiting: 1,
                pending: 1,
                processing: 1,
                failed: 0,
                unknown: 0,
            },
            affected_photos: 1,
        };
        let lines = deletion_warning_lines(&summary);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("AIタグ付け") && line.contains("処理中 1")),
            "AI tagging jobs must be listed with the shared 処理中 label: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("AIタグ付け") && line.contains("未実行 1"))
        );
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
