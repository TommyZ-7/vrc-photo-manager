//! Quick-look viewer overlay and the thumbnail-to-viewer morph transition.

use std::time::Duration;

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{App, effective_motion_fps};
use crate::state::preview::preview_placeholder_label;
use crate::state::viewer::{ViewerMorph, morph_image_plan};
use crate::ui::components::{ButtonState, ButtonTone};
use crate::ui::theme::*;

/// Opacity and horizontal offset for the outgoing and incoming photos during a
/// viewer step. Returned as `(previous_opacity, previous_offset,
/// current_opacity, current_offset)`.
pub(crate) fn viewer_nav_frame(t: f32, direction: f32) -> (f32, f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (
        1.0 - t,
        -direction * VIEWER_NAV_SLIDE * t,
        t,
        direction * VIEWER_NAV_SLIDE * (1.0 - t),
    )
}

impl App {
    pub(crate) fn quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let Some(photo) = self.selected_photo.clone() else {
            return div().into_any_element();
        };
        let color = self.color_for(&photo);
        let details_open = self.quick_look_details_open;
        let photo_count = self.quick_look_photo_ids.len();
        let index = self.quick_look_index.min(photo_count.saturating_sub(1));
        let prev_enabled = index > 0;
        let next_enabled = index + 1 < photo_count;

        let close_button = self.focusable_text_button(
            "viewer-close",
            "閉じる Esc",
            self.quick_look_close_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            |this, window, cx| this.close_quick_look(window, cx),
        );
        let prev_button = self.focusable_text_button(
            "viewer-prev",
            "前へ",
            self.quick_look_prev_focus.clone(),
            ButtonState {
                tone: if prev_enabled {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: prev_enabled,
            },
            cx,
            |this, window, cx| this.step_viewer_photo(-1, window, cx),
        );
        let next_button = self.focusable_text_button(
            "viewer-next",
            "次へ",
            self.quick_look_next_focus.clone(),
            ButtonState {
                tone: if next_enabled {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: next_enabled,
            },
            cx,
            |this, window, cx| this.step_viewer_photo(1, window, cx),
        );
        let details_button = self.focusable_text_button(
            "viewer-details",
            if details_open {
                "詳細を閉じる"
            } else {
                "詳細"
            },
            self.quick_look_details_focus.clone(),
            ButtonState {
                tone: if details_open {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: true,
            },
            cx,
            |this, window, cx| this.toggle_quick_look_details(window, cx),
        );

        let top_bar = h_flex()
            .h(px(VIEWER_TOP_BAR))
            .flex_shrink_0()
            .px(px(20.))
            .gap(px(10.))
            .items_center()
            .bg(rgba(with_alpha(p.panel_header, 0x99)))
            .border_b_1()
            .border_color(rgba(p.line_soft))
            .key_context("viewer_nav")
            .child(close_button)
            .child(prev_button)
            .child(next_button)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(photo.file_name.clone()),
            )
            .child(details_button);

        let viewport = content_viewport(window);
        let box_state = self.viewer_box;
        let nav_epoch = self.viewer_nav_epoch;
        let nav_direction = self.viewer_nav_direction;
        let nav_duration = if nav_epoch == 0 || cx.reduce_motion() {
            Duration::ZERO
        } else {
            MOTION_BASE
        };
        let previous_image = self.previous_preview.clone();
        let current_image = self
            .preview
            .clone()
            .or_else(|| self.thumbnails.borrow_mut().get(photo.id));
        let (start_w, start_h) = box_state.size_at(0.0, viewport);

        let mut image_frame = div()
            .id("viewer-image")
            .relative()
            .flex_shrink_0()
            .w(px(start_w))
            .h(px(start_h))
            .overflow_hidden()
            .rounded(px(10.))
            .bg(rgb(color))
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()));

        if nav_epoch > 0
            && let Some(previous) = previous_image
        {
            let previous = gpui_kit::img(previous)
                .object_fit(ObjectFit::Contain)
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded(px(10.))
                .with_animation(
                    ("viewer-nav-previous", nav_epoch),
                    Animation::new(nav_duration)
                        .with_easing(ease_motion)
                        .with_max_fps(self.motion_fps(window, cx)),
                    move |el, t| {
                        let (opacity, offset, _, _) = viewer_nav_frame(t, nav_direction);
                        el.opacity(opacity).left(px(offset))
                    },
                );
            image_frame = image_frame.child(previous);
        }

        if let Some(image) = current_image {
            let current = gpui_kit::img(image)
                .object_fit(ObjectFit::Contain)
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded(px(10.))
                .with_animation(
                    ("viewer-nav-current", nav_epoch),
                    Animation::new(nav_duration)
                        .with_easing(ease_motion)
                        .with_max_fps(self.motion_fps(window, cx)),
                    move |el, t| {
                        let (_, _, opacity, offset) = viewer_nav_frame(t, nav_direction);
                        el.opacity(opacity).left(px(offset))
                    },
                );
            image_frame = image_frame.child(current);
        } else {
            image_frame = image_frame.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(preview_placeholder_label(
                        self.preview_queue.pending_for(photo.id),
                    )),
            );
        }

        let image_frame = image_frame.with_animation(
            ("viewer-box", box_state.epoch),
            Animation::new(box_state.duration)
                .with_easing(ease_motion)
                .with_max_fps(self.motion_fps(window, cx)),
            move |el, t| {
                let (width, height) = box_state.size_at(t, viewport);
                el.w(px(width)).h(px(height))
            },
        );

        let opening_morph = self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing);
        let image_area = div()
            .id("viewer-image-area")
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .p(px(VIEWER_PAD))
            .opacity(if opening_morph { 0.0 } else { 1.0 })
            .on_click(cx.listener(|this, _, window, cx| this.close_quick_look(window, cx)))
            .child(image_frame);

        let mut details_panel = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if details_open {
            let body = v_flex()
                .id("viewer-details-body")
                .h_full()
                .p(px(16.))
                .gap(px(10.))
                .overflow_y_scrollbar();
            let body = body.child(self.detail_body(&photo, cx)).with_animation(
                ("viewer-details-body", self.quick_look_details_epoch),
                Animation::new(if self.quick_look_details_epoch == 0 {
                    Duration::ZERO
                } else {
                    MOTION_FAST
                })
                .with_easing(ease_motion)
                .with_max_fps(self.motion_fps(window, cx)),
                |el, t| el.opacity(t).relative().top(px(6.0 * (1.0 - t))),
            );
            // The details column stays a fixed-width dock so the image geometry
            // is unchanged; the card inside it is the opaque foreground surface.
            details_panel = details_panel.p(px(14.)).child(
                v_flex()
                    .h_full()
                    .w_full()
                    .overflow_hidden()
                    .rounded(px(17.))
                    .border_1()
                    .border_color(rgba(p.line_soft))
                    .bg(rgb(p.panel_deep))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(18.), rgba(p.shadow).into()).blur_radius(px(44.)),
                        BoxShadow::new(px(0.), px(1.), rgba(p.highlight).into())
                            .inset()
                            .blur_radius(px(0.)),
                    ])
                    .child(body),
            );
        }
        let panel_state = self.viewer_panel;
        let details_panel = details_panel.with_animation(
            ("viewer-panel", panel_state.epoch),
            Animation::new(panel_state.duration)
                .with_easing(ease_motion)
                .with_max_fps(self.motion_fps(window, cx)),
            move |el, t| el.w(px(panel_state.width_at(t))).overflow_hidden(),
        );

        let content = h_flex()
            .flex_1()
            .min_h_0()
            .child(image_area)
            .child(details_panel);

        v_flex()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .bg(rgb(p.bg))
            .key_context("quick_look")
            .occlude()
            .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
            .child(top_bar)
            .child(content)
            .into_any_element()
    }
}

pub(crate) struct ViewerMorphView {
    morph: Option<ViewerMorph>,
    /// Configured cap; resolved against the display in `render` so a cached
    /// morph still follows the window's current refresh rate.
    max_fps: u32,
    palette: Palette,
}

impl ViewerMorphView {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            morph: None,
            max_fps: 60,
            palette,
        }
    }

    pub(crate) fn set_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        self.palette = palette;
        cx.notify();
    }

    pub(crate) fn set_morph(
        &mut self,
        morph: Option<ViewerMorph>,
        max_fps: u32,
        cx: &mut Context<Self>,
    ) {
        self.morph = morph;
        self.max_fps = max_fps;
        cx.notify();
    }
}

impl Render for ViewerMorphView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let max_fps = effective_motion_fps(self.max_fps, window.refresh_interval(cx));
        let Some(morph) = self.morph.as_ref() else {
            // An in-flow empty slot would still take a root flex gap and
            // narrow the main column by 14px.
            return div().absolute().into_any_element();
        };
        let from = morph.from;
        let to = morph.to;
        let epoch = morph.epoch;
        let color = morph.color;
        let closing = morph.closing;
        let fade_only = morph.fade_only;
        let backdrop_from = morph.backdrop_from;
        let (_old_kind, _new_kind, start_radius, end_radius) = morph_image_plan(
            closing,
            morph.source,
            morph.thumbnail.is_some(),
            morph.preview.is_some(),
        );
        let Some(image) = morph.preview.clone().or_else(|| morph.thumbnail.clone()) else {
            // Keep the empty slot out of flow for the same reason as the idle
            // return above: a root flex gap would narrow the whole shell.
            return div().absolute().into_any_element();
        };
        let frame = div()
            .absolute()
            .overflow_hidden()
            .bg(rgb(color))
            .child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full(),
            )
            .with_animation(
                ("viewer-morph-frame", epoch),
                Animation::new(MOTION_MORPH)
                    .with_easing(ease_motion)
                    .with_max_fps(max_fps),
                move |el, t| {
                    let bounds = lerp_bounds(from, to, t);
                    let radius = start_radius + (end_radius - start_radius) * t;
                    let opacity = if closing || fade_only { 1.0 - t } else { 1.0 };
                    el.opacity(opacity)
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .rounded(px(radius))
                },
            );

        let backdrop = closing.then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .size_full()
                .bg(rgb(p.bg))
                .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
                .with_animation(
                    ("viewer-morph-backdrop", epoch),
                    Animation::new(MOTION_MORPH)
                        .with_easing(ease_motion)
                        .with_max_fps(max_fps),
                    move |el, t| el.opacity(backdrop_from * (1.0 - t)),
                )
        });

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .when_some(backdrop, |this, backdrop| this.child(backdrop))
            .child(frame)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::viewer_nav_frame;
    use crate::ui::theme::VIEWER_NAV_SLIDE;

    #[test]
    fn viewer_nav_frame_hits_both_ends() {
        assert_eq!(
            viewer_nav_frame(0.0, 1.0),
            (1.0, 0.0, 0.0, VIEWER_NAV_SLIDE)
        );
        assert_eq!(
            viewer_nav_frame(1.0, 1.0),
            (0.0, -VIEWER_NAV_SLIDE, 1.0, 0.0)
        );
        assert_eq!(
            viewer_nav_frame(0.0, -1.0),
            (1.0, 0.0, 0.0, -VIEWER_NAV_SLIDE)
        );
    }
}
