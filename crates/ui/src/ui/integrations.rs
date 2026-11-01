//! Integrations & automation screen: automatic compression and clipboard
//! settings, with Discord destined for the same card stack (#142).

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_i18n::tr;

use crate::app::App;
use crate::state::motion::ToggleMotion;
use crate::ui::components::{compression_policy_warning, glass_panel, manual_option_row};
use crate::ui::theme::*;

/// Generous bounds for the collapsed-body height animation. The profile lists
/// scroll above these sizes, so the body never exceeds its bound.
const AUTO_BODY_MAX: f32 = 430.0;
const CLIPBOARD_BODY_MAX: f32 = 330.0;

impl App {
    pub(crate) fn integrations_screen(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(
                v_flex()
                    .id("integrations-sections")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.integrations_scroll)
                    .gap(px(CARD_GAP))
                    .child(self.auto_compression_card(window, cx))
                    .child(self.clipboard_card(window, cx)),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(8.))
                    .child(Scrollbar::vertical(&self.integrations_scroll)),
            )
    }

    fn auto_compression_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let enabled = self.settings.auto_compress;
        let motion = self.integrations_auto_motion;
        let profiles = self.profiles.clone();

        let body = v_flex()
            .pt(px(12.))
            .gap(px(12.))
            .child(self.setting_switch(
                tr!("settings.startup.immediate"),
                tr!("settings.startup.immediate_hint"),
                self.settings.auto_compress_immediate,
                false,
                cx,
                |settings, value| settings.auto_compress_immediate = value,
            ))
            .child(self.setting_switch(
                tr!("settings.startup.keep_original"),
                tr!("settings.startup.keep_original_hint"),
                self.settings.auto_compress_keep_original,
                false,
                cx,
                |settings, value| settings.auto_compress_keep_original = value,
            ))
            .when_some(
                compression_policy_warning(
                    self.settings.auto_compress_keep_original,
                    self.settings.auto_compress,
                ),
                |this, warning| {
                    this.child(
                        div()
                            .p(px(12.))
                            .rounded(px(8.))
                            .bg(rgb(p.red_soft))
                            .border_1()
                            .border_color(rgb(p.red_border))
                            .text_size(px(10.))
                            .text_color(rgb(p.red))
                            .child(warning),
                    )
                },
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.text))
                            .child(tr!("integrations.auto.profiles")),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.auto.profiles_hint")),
                    )
                    .child(if profiles.is_empty() {
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.auto.profiles_empty"))
                            .into_any_element()
                    } else {
                        v_flex()
                            .id("integrations-auto-profiles")
                            .max_h(px(160.))
                            .overflow_y_scrollbar()
                            .gap(px(4.))
                            .children(profiles.into_iter().map(|profile| {
                                let id = profile.id.clone();
                                Checkbox::new(SharedString::from(format!(
                                    "auto-profile-{}",
                                    profile.id
                                )))
                                .checked(profile.auto_enabled)
                                .label(profile.name.clone())
                                .small()
                                .on_click(cx.listener(move |this, checked, _, cx| {
                                    this.edit_profile_by_id(
                                        &id,
                                        |profile| profile.auto_enabled = *checked,
                                        cx,
                                    );
                                }))
                                .into_any_element()
                            }))
                            .into_any_element()
                    }),
            );

        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("integrations.auto.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.auto.hint")),
                            ),
                    )
                    .child(
                        Switch::new("integrations-auto-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.set_auto_compress_enabled(*checked, cx);
                            })),
                    ),
            )
            .child(self.collapsible_body(
                "integrations-auto-body",
                motion,
                AUTO_BODY_MAX,
                window,
                cx,
                body,
            ))
            .into_any_element()
    }

    fn clipboard_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let enabled = self.settings.clipboard_enabled;
        let motion = self.integrations_clipboard_motion;
        let profiles = self.profiles.clone();

        let body = v_flex()
            .pt(px(12.))
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.clipboard.profiles_hint")),
            )
            .child(if profiles.is_empty() {
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.clipboard.profiles_empty"))
                    .into_any_element()
            } else {
                v_flex()
                    .id("integrations-clipboard-profiles")
                    .max_h(px(180.))
                    .overflow_y_scrollbar()
                    .gap(px(4.))
                    .children(profiles.into_iter().map(|profile| {
                        let id = profile.id.clone();
                        let selected = profile.clipboard;
                        manual_option_row(
                            &p,
                            SharedString::from(format!("clipboard-profile-{}", profile.id)),
                            profile.name.clone(),
                            profile_summary(&profile),
                            selected,
                            true,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_clipboard_profile(&id, !selected, cx);
                        }))
                        .into_any_element()
                    }))
                    .into_any_element()
            });

        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("integrations.clipboard.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.clipboard.hint")),
                            ),
                    )
                    .child(
                        Switch::new("integrations-clipboard-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.set_clipboard_enabled(*checked, cx);
                            })),
                    ),
            )
            .child(self.collapsible_body(
                "integrations-clipboard-body",
                motion,
                CLIPBOARD_BODY_MAX,
                window,
                cx,
                body,
            ))
            .into_any_element()
    }

    fn collapsible_body(
        &self,
        id: &'static str,
        motion: ToggleMotion,
        max_height: f32,
        window: &Window,
        cx: &Context<Self>,
        body: impl IntoElement + 'static,
    ) -> AnyElement {
        div()
            .overflow_hidden()
            .with_animation(
                (id, motion.epoch()),
                Animation::new(motion.driver_duration())
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps(window, cx)),
                move |el, _| {
                    let collapsed = motion.value();
                    el.max_h(px((1.0 - collapsed) * max_height))
                        .opacity(1.0 - collapsed)
                },
            )
            .child(body)
            .into_any_element()
    }

    fn set_auto_compress_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.auto_compress == enabled {
            return;
        }
        self.settings.auto_compress = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save automatic compression setting");
        }
        let target = if enabled { 0.0 } else { 1.0 };
        if cx.reduce_motion() {
            self.integrations_auto_motion = ToggleMotion::settled(target);
        } else {
            self.integrations_auto_motion
                .animate_to(target, MOTION_FAST);
        }
        cx.notify();
    }

    fn set_clipboard_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.clipboard_enabled == enabled {
            return;
        }
        self.settings.clipboard_enabled = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save clipboard setting");
        }
        let target = if enabled { 0.0 } else { 1.0 };
        if cx.reduce_motion() {
            self.integrations_clipboard_motion = ToggleMotion::settled(target);
        } else {
            self.integrations_clipboard_motion
                .animate_to(target, MOTION_FAST);
        }
        cx.notify();
    }
}
