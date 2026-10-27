//! App-side viewer state: open/close, photo stepping, details panel, and the
//! focus trap used by the quick-look overlay.
//!
//! Rendering stays in `ui::viewer`; these methods only touch App fields and the
//! viewer animation state.

use std::time::{Duration, Instant};

use gpui_kit::*;

use crate::app::{App, Screen};
use crate::state::viewer::{
    ViewerBoxState, ViewerMorph, ViewerOpenSource, ViewerPanelState, current_tagged_bounds,
    preferred_bounds,
};
use crate::ui::theme::{
    MOTION_BASE, MOTION_MORPH, MOTION_SLOW, VIEWER_DETAILS_WIDTH, centered_viewer_rect,
    eased_progress, next_focus_index, step_index, viewer_image_rect,
};

impl App {
    pub(crate) fn open_quick_look(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        source: ViewerOpenSource,
    ) {
        if self.overlay_open() || self.selected_photo.is_none() {
            return;
        }
        if self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| morph.closing)
        {
            self.viewer_morph = None;
            self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
        }
        self.message = None;
        self.delete_armed = None;
        self.note_edit_id = None;
        self.quick_look_return_focus = window.focused(cx);
        self.quick_look_photo_ids = self.viewer_source_ids();
        self.quick_look_index = self
            .quick_look_photo_ids
            .iter()
            .position(|id| Some(*id) == self.selected)
            .unwrap_or(0);
        let id = self.selected.expect("selected photo");
        let aspect = self
            .selected_photo
            .as_ref()
            .map(|photo| {
                let (width, height) = photo.effective_dimensions();
                if height > 0 {
                    width as f32 / height as f32
                } else {
                    16.0 / 9.0
                }
            })
            .unwrap_or(16.0 / 9.0);
        let viewport = window.viewport_size();
        let to = viewer_image_rect(viewport, self.quick_look_details_open, aspect);
        let now = Instant::now();
        self.viewer_nav_epoch = 0;
        self.viewer_nav_direction = 1.0;
        self.viewer_nav_started_at = now;
        self.viewer_open_started_at = now;
        let details_width = if self.quick_look_details_open {
            VIEWER_DETAILS_WIDTH
        } else {
            0.0
        };
        let from_size = (f32::from(to.size.width), f32::from(to.size.height));
        self.viewer_box = ViewerBoxState {
            epoch: self.viewer_box.epoch.wrapping_add(1),
            started_at: now,
            from_size,
            to_aspect: aspect,
            to_details_width: details_width,
            duration: Duration::ZERO,
        };
        self.viewer_panel = ViewerPanelState {
            epoch: self.viewer_panel.epoch.wrapping_add(1),
            started_at: now,
            from_width: details_width,
            to_width: details_width,
            duration: Duration::ZERO,
        };
        let resolved = self.actual_source(id, source);
        if let Some((actual_source, _)) = resolved {
            self.viewer_return_source = Some(actual_source);
        }
        if !cx.reduce_motion()
            && let Some((actual_source, from)) = resolved
        {
            let thumbnail = self.thumbnails.get(id);
            let preview = self.preview.clone();
            if thumbnail.is_some() || preview.is_some() {
                self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
                let epoch = self.viewer_morph_epoch;
                let color = self
                    .selected_photo
                    .as_ref()
                    .map(|photo| self.color_for(photo))
                    .unwrap_or(0);
                self.viewer_morph = Some(ViewerMorph {
                    epoch,
                    started_at: Instant::now(),
                    from,
                    to,
                    thumbnail,
                    preview,
                    color,
                    source: actual_source,
                    closing: false,
                    fade_only: false,
                    backdrop_from: 1.0,
                });
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(MOTION_MORPH + Duration::from_millis(60))
                        .await;
                    this.update(cx, |this, cx| {
                        if this.viewer_morph_epoch == epoch
                            && this
                                .viewer_morph
                                .as_ref()
                                .is_some_and(|morph| !morph.closing)
                        {
                            this.viewer_morph = None;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
        self.quick_look_open = true;
        self.quick_look_epoch = self.quick_look_epoch.wrapping_add(1);
        self.quick_look_focus_index = 0;
        // Remember the last details state for the session, but render it
        // statically: only an explicit toggle animates the panel.
        self.quick_look_details_epoch = 0;
        let close_focus = self.quick_look_close_focus.clone();
        window.focus(&close_focus, cx);
        // The close button mounts with the overlay on this frame, so make the
        // focus stick once the focus tree contains it. Guard the retry so a
        // close (or close+reopen) cannot steal focus back to a stale overlay.
        let epoch = self.quick_look_epoch;
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_epoch == epoch {
                window.focus(&close_focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn close_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open {
            return;
        }
        let opening = self
            .viewer_morph
            .as_ref()
            .filter(|morph| !morph.closing)
            .map(|morph| (morph.current_bounds(Instant::now()), morph.source));
        let viewer_rect = if let Some((bounds, _)) = opening {
            bounds
        } else {
            let viewport = window.viewport_size();
            let (width, height) = self.viewer_box.current_size(viewport);
            centered_viewer_rect(viewport, self.viewer_panel.current_width(), width, height)
        };
        let from = viewer_rect;
        let source = opening
            .map(|(_, source)| source)
            .or(self.viewer_return_source)
            .unwrap_or(ViewerOpenSource::Auto);
        let id = self.selected.unwrap_or_default();
        let target = self.actual_source(id, source);
        let (to, target_kind, fade_only) = match target {
            Some((kind, bounds)) => (bounds, kind, false),
            None => (from, source, true),
        };
        let backdrop_from = eased_progress(
            self.viewer_open_started_at,
            Duration::from_millis(240),
            Instant::now(),
        );
        let thumbnail = self.thumbnails.get(id);
        let preview = self.preview.clone();
        let color = self
            .selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(0);

        if !cx.reduce_motion() && (thumbnail.is_some() || preview.is_some()) {
            self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
            let epoch = self.viewer_morph_epoch;
            self.viewer_morph = Some(ViewerMorph {
                epoch,
                started_at: Instant::now(),
                from,
                to,
                thumbnail,
                preview,
                color,
                source: target_kind,
                closing: true,
                fade_only,
                backdrop_from,
            });
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(MOTION_MORPH + Duration::from_millis(60))
                    .await;
                this.update(cx, |this, cx| {
                    if this.viewer_morph_epoch == epoch
                        && this
                            .viewer_morph
                            .as_ref()
                            .is_some_and(|morph| morph.closing)
                    {
                        this.viewer_morph = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        } else {
            self.viewer_morph = None;
        }
        self.viewer_return_source = None;
        self.quick_look_open = false;
        self.delete_armed = None;
        self.note_edit_id = None;
        let focus = self
            .quick_look_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes a viewer whose backing photo vanished (for example, an external
    /// delete picked up by the three-second refresh) and restores focus.
    pub(crate) fn guard_viewer_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open || self.selected_photo.is_some() {
            return;
        }
        self.quick_look_open = false;
        self.delete_armed = None;
        self.note_edit_id = None;
        self.viewer_morph = None;
        self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
        self.viewer_return_source = None;
        if let Some(id) = self.selected.take() {
            self.thumbnails.unpin(id);
        }
        self.selected_ids.clear();
        self.selection_anchor = None;
        self.quick_look_photo_ids.clear();
        self.quick_look_index = 0;
        self.preview = None;
        self.previous_preview = None;
        self.previous_color = None;
        let focus = self
            .quick_look_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        cx.defer_in(window, move |this, window, cx| {
            if !this.quick_look_open {
                window.focus(&focus, cx);
            }
        });
    }

    fn viewer_source_ids(&self) -> Vec<i64> {
        let browser = if self.screen == Screen::Search && self.search_active {
            &self.search_view
        } else {
            &self.library
        };
        let source: Vec<i64> = browser.index.entries.iter().map(|entry| entry.id).collect();
        match self.selected {
            Some(id) if source.contains(&id) => source,
            Some(id) => vec![id],
            None => Vec::new(),
        }
    }

    pub(crate) fn step_viewer_photo(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.quick_look_open || self.quick_look_photo_ids.is_empty() {
            return;
        }
        if self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing)
        {
            self.viewer_morph = None;
        }
        let next = step_index(
            self.quick_look_index,
            self.quick_look_photo_ids.len(),
            delta,
        );
        if next == self.quick_look_index {
            return;
        }
        self.quick_look_index = next;
        let focused = window.focused(cx);
        let prev_disabled = next == 0 && focused.as_ref() == Some(&self.quick_look_prev_focus);
        let next_disabled = next + 1 == self.quick_look_photo_ids.len()
            && focused.as_ref() == Some(&self.quick_look_next_focus);
        if prev_disabled || next_disabled {
            self.quick_look_focus_index = 0;
            window.focus(&self.quick_look_close_focus, cx);
        }
        // Keep the top-bar navigation focused; only the library selection and
        // the displayed photo change behind the overlay.
        let viewport = window.viewport_size();
        let from_size = self.viewer_box.current_size(viewport);
        let target_details_width = self.viewer_panel.to_width;
        self.select_photo(self.quick_look_photo_ids[next], cx);
        let new_aspect = self
            .selected_photo
            .as_ref()
            .map(|photo| {
                let (width, height) = photo.effective_dimensions();
                if height > 0 {
                    width as f32 / height as f32
                } else {
                    16.0 / 10.0
                }
            })
            .unwrap_or(16.0 / 10.0);
        let now = Instant::now();
        self.viewer_nav_epoch = self.viewer_nav_epoch.wrapping_add(1);
        self.viewer_nav_direction = if delta > 0 { 1.0 } else { -1.0 };
        self.viewer_nav_started_at = now;
        self.viewer_box = ViewerBoxState {
            epoch: self.viewer_box.epoch.wrapping_add(1),
            started_at: now,
            from_size,
            to_aspect: new_aspect,
            to_details_width: target_details_width,
            duration: MOTION_BASE,
        };
        cx.notify();
    }

    pub(crate) fn toggle_quick_look_details(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing)
        {
            self.viewer_morph = None;
        }
        let current_width = self.viewer_panel.current_width();
        let viewport = window.viewport_size();
        let from_size = self.viewer_box.current_size(viewport);
        self.quick_look_details_open = !self.quick_look_details_open;
        if !self.quick_look_details_open {
            self.note_edit_id = None;
        }
        let target_width = if self.quick_look_details_open {
            VIEWER_DETAILS_WIDTH
        } else {
            0.0
        };
        let aspect = self
            .selected_photo
            .as_ref()
            .map(|photo| {
                let (width, height) = photo.effective_dimensions();
                if height > 0 {
                    width as f32 / height as f32
                } else {
                    16.0 / 10.0
                }
            })
            .unwrap_or(16.0 / 10.0);
        let now = Instant::now();
        self.viewer_panel = ViewerPanelState {
            epoch: self.viewer_panel.epoch.wrapping_add(1),
            started_at: now,
            from_width: current_width,
            to_width: target_width,
            duration: MOTION_SLOW,
        };
        self.viewer_box = ViewerBoxState {
            epoch: self.viewer_box.epoch.wrapping_add(1),
            started_at: now,
            from_size,
            to_aspect: aspect,
            to_details_width: target_width,
            duration: MOTION_SLOW,
        };
        self.quick_look_details_epoch = self.quick_look_details_epoch.wrapping_add(1);
        let index = self
            .quick_look_focus_order(cx)
            .iter()
            .position(|handle| *handle == self.quick_look_details_focus)
            .unwrap_or(0);
        self.quick_look_focus_index = index;
        window.focus(&self.quick_look_details_focus, cx);
        cx.notify();
    }

    fn quick_look_focus_order(&self, cx: &gpui_kit::App) -> Vec<FocusHandle> {
        let index = self
            .quick_look_index
            .min(self.quick_look_photo_ids.len().saturating_sub(1));
        let mut order = vec![self.quick_look_close_focus.clone()];
        if index > 0 {
            order.push(self.quick_look_prev_focus.clone());
        }
        if index + 1 < self.quick_look_photo_ids.len() {
            order.push(self.quick_look_next_focus.clone());
        }
        order.push(self.quick_look_details_focus.clone());
        if self.quick_look_details_open {
            order.push(self.quick_look_favorite_focus.clone());
            order.push(self.tag_edit_input.read(cx).focus_handle(cx));
            order.push(self.quick_look_add_tag_focus.clone());
            if self.note_edit_id.is_some() {
                order.push(self.note_edit_input.read(cx).focus_handle(cx));
                order.push(self.quick_look_note_save_focus.clone());
                order.push(self.quick_look_note_cancel_focus.clone());
            } else {
                order.push(self.quick_look_note_edit_focus.clone());
            }
            order.push(self.quick_look_compress_focus.clone());
            order.push(self.quick_look_trash_focus.clone());
        }
        order
    }

    pub(crate) fn cycle_quick_look_focus(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.quick_look_focus_order(cx);
        if order.is_empty() {
            return;
        }
        // Clicks move focus without touching `quick_look_focus_index`, so
        // derive the position from the actual focus before stepping.
        let current = window
            .focused(cx)
            .and_then(|focused| order.iter().position(|handle| *handle == focused))
            .unwrap_or(self.quick_look_focus_index);
        let next = next_focus_index(current, order.len(), reverse);
        self.quick_look_focus_index = next;
        window.focus(&order[next], cx);
    }

    pub(crate) fn handle_viewer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_edit_id.is_some() {
            self.cancel_note_edit(window, cx);
        } else {
            self.close_quick_look(window, cx);
        }
    }

    fn actual_source(
        &self,
        id: i64,
        source: ViewerOpenSource,
    ) -> Option<(ViewerOpenSource, Bounds<Pixels>)> {
        let card = current_tagged_bounds(
            self.card_image_bounds.borrow().get(&id).copied(),
            self.render_epoch,
        );
        let inspector =
            current_tagged_bounds(*self.inspector_preview_bounds.borrow(), self.render_epoch);
        let prefer_card = source != ViewerOpenSource::Inspector;
        let chosen = preferred_bounds(prefer_card, card, inspector)?;
        if card == Some(chosen) {
            Some((ViewerOpenSource::Card, chosen))
        } else {
            Some((ViewerOpenSource::Inspector, chosen))
        }
    }
}
