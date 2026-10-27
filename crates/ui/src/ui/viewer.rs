//! Quick-look viewer overlay and the thumbnail-to-viewer morph transition.

use std::time::{Duration, Instant};

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::App;
use crate::state::preview::preview_placeholder_label;
use crate::state::viewer::{MorphImageKind, morph_image_plan};
use crate::ui::components::{ButtonState, ButtonTone};
use crate::ui::theme::*;

impl App {
    pub(crate) fn quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
            .bg(rgba(0x05070a99))
            .border_b_1()
            .border_color(rgba(0xffffff12))
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
                    .text_color(rgb(MUTED))
                    .child(photo.file_name.clone()),
            )
            .child(details_button)
            .with_animation(
                ("viewer-topbar", self.quick_look_epoch),
                Animation::new(MOTION_BASE)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.relative().top(px(-8.0 * (1.0 - t))),
            );

        let viewport = window.viewport_size();
        let box_state = self.viewer_box;
        let nav_epoch = self.viewer_nav_epoch;
        let nav_direction = self.viewer_nav_direction;
        let nav_t = if nav_epoch == 0 {
            1.0
        } else {
            viewer_nav_progress(
                self.viewer_nav_started_at,
                Instant::now(),
                cx.reduce_motion(),
            )
        };
        let previous_preview = self.previous_preview.clone();
        let current_preview = self.preview.clone();
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
            && nav_t < 1.0
            && let Some(previous) = previous_preview
        {
            image_frame = image_frame.child(
                gpui_kit::img(previous)
                    .object_fit(ObjectFit::Contain)
                    .size_full()
                    .rounded(px(10.))
                    .opacity(1.0 - nav_t)
                    .relative()
                    .left(px(-nav_direction * VIEWER_NAV_SLIDE * nav_t)),
            );
        }

        let current_offset = if nav_epoch == 0 {
            0.0
        } else {
            nav_direction * VIEWER_NAV_SLIDE * (1.0 - nav_t)
        };
        if let Some(preview) = current_preview {
            image_frame = image_frame.child(
                gpui_kit::img(preview)
                    .object_fit(ObjectFit::Contain)
                    .size_full()
                    .rounded(px(10.))
                    .opacity(if nav_epoch == 0 { 1.0 } else { nav_t })
                    .relative()
                    .left(px(current_offset)),
            );
        } else {
            image_frame = image_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(preview_placeholder_label(
                        self.preview_queue.pending_for(photo.id),
                    )),
            );
        }

        let image_frame = image_frame.with_animation(
            ("viewer-box", box_state.epoch),
            Animation::new(box_state.duration)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
            move |el, t| {
                let (width, height) = box_state.size_at(t, viewport);
                el.w(px(width)).h(px(height))
            },
        );

        let opening_morph = self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing);
        let nav_driver = div().size(px(0.)).with_animation(
            ("viewer-nav-driver", nav_epoch),
            Animation::new(if nav_epoch == 0 {
                Duration::ZERO
            } else {
                MOTION_BASE
            })
            .with_easing(ease_motion)
            .with_max_fps(MOTION_FPS),
            |el, _| el,
        );
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
            .child(nav_driver)
            .child(image_frame);

        let message = self.message.clone();
        let mut details_panel = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if details_open {
            let mut body = v_flex()
                .id("viewer-details-body")
                .h_full()
                .p(px(16.))
                .gap(px(10.))
                .overflow_y_scrollbar();
            if let Some(message) = message {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child(message),
                );
            }
            if self.delete_armed == Some(photo.id) {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child("もう一度押すとOSゴミ箱へ移動します。"),
                );
            }
            let body = body.child(self.detail_body(&photo, cx)).with_animation(
                ("viewer-details-body", self.quick_look_details_epoch),
                Animation::new(if self.quick_look_details_epoch == 0 {
                    Duration::ZERO
                } else {
                    MOTION_FAST
                })
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
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
                    .border_color(rgba(0xffffff1b))
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(rgb(PANEL_CARD), 0.),
                        linear_color_stop(rgb(PANEL_DEEP), 1.),
                    ))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(18.), rgba(0x00000047).into())
                            .blur_radius(px(44.)),
                        BoxShadow::new(px(0.), px(1.), rgba(0xffffff10).into())
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
                .with_max_fps(MOTION_FPS),
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
            .bg(rgb(0x05070a))
            .key_context("quick_look")
            .occlude()
            .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
            .child(top_bar)
            .child(content)
            .with_animation(
                ("viewer", self.quick_look_epoch),
                Animation::new(Duration::from_millis(240)).with_easing(ease_motion),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    pub(crate) fn viewer_morph_overlay(&self) -> AnyElement {
        let Some(morph) = self.viewer_morph.as_ref() else {
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
        let (old_kind, new_kind, start_radius, end_radius) = morph_image_plan(
            closing,
            morph.source,
            morph.thumbnail.is_some(),
            morph.preview.is_some(),
        );
        let image_for = |kind: MorphImageKind| match kind {
            MorphImageKind::Thumbnail => morph.thumbnail.clone(),
            MorphImageKind::Preview => morph.preview.clone(),
        };
        let Some(old_image) = old_kind.and_then(image_for) else {
            // Keep the empty slot out of flow for the same reason as the idle
            // return above: a root flex gap would narrow the whole shell.
            return div().absolute().into_any_element();
        };
        let new_image = if fade_only || (!closing && morph.preview.is_none()) {
            // Never present the thumbnail as the full-size incoming image.
            // If the preview is missing, the viewer shows its explicit failure
            // state after the overlay clears.
            None
        } else {
            new_kind.and_then(image_for)
        };
        let old_fit = if closing {
            ObjectFit::Contain
        } else {
            ObjectFit::Cover
        };
        let new_fit = if closing {
            ObjectFit::Cover
        } else {
            ObjectFit::Contain
        };
        let layer_anim = || {
            Animation::new(MOTION_MORPH)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS)
        };
        let old_layer = gpui_kit::img(old_image)
            .object_fit(old_fit)
            .size_full()
            .with_animation(("viewer-morph-old", epoch), layer_anim(), move |el, t| {
                let radius = start_radius + (end_radius - start_radius) * t;
                let opacity = if fade_only { 1.0 } else { 1.0 - t };
                el.opacity(opacity).rounded(px(radius))
            });
        let new_layer = new_image.map(|image| {
            gpui_kit::img(image)
                .object_fit(new_fit)
                .size_full()
                .with_animation(("viewer-morph-new", epoch), layer_anim(), move |el, t| {
                    let radius = start_radius + (end_radius - start_radius) * t;
                    el.opacity(t).rounded(px(radius))
                })
        });
        let frame = div()
            .absolute()
            .overflow_hidden()
            .bg(rgb(color))
            .child(old_layer)
            .when(!fade_only, |this| {
                this.when_some(new_layer, |this, layer| this.child(layer))
            })
            .with_animation(
                ("viewer-morph-frame", epoch),
                Animation::new(MOTION_MORPH)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                move |el, t| {
                    let bounds = lerp_bounds(from, to, t);
                    let radius = start_radius + (end_radius - start_radius) * t;
                    el.opacity(if fade_only { 1.0 - t } else { 1.0 })
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
                .bg(rgb(0x05070a))
                .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
                .with_animation(
                    ("viewer-morph-backdrop", epoch),
                    Animation::new(MOTION_MORPH)
                        .with_easing(ease_motion)
                        .with_max_fps(MOTION_FPS),
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
