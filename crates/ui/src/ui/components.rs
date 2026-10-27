//! Shared presentation components: buttons, chips, cards, and the small
//! labelled rows used by more than one screen.
//!
//! PR1 keeps every value and layout exactly as it was in `main.rs`; later
//! tasks consume these helpers from the per-screen modules.

use std::rc::Rc;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::settings::{AppSettings, CardSize};

use crate::app::App;
use crate::ui::theme::*;

pub(crate) type ViewerActivate = Rc<dyn Fn(&mut App, &mut Window, &mut Context<App>)>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonTone {
    Neutral,
    Primary,
    Danger,
}

#[derive(Clone, Copy)]
pub(crate) struct ButtonState {
    pub(crate) tone: ButtonTone,
    pub(crate) enabled: bool,
}

pub(crate) fn glass_panel() -> Div {
    div()
        .rounded(px(18.))
        .bg(rgba(0x141922c4))
        .border_1()
        .border_color(rgba(0xffffff14))
        .shadow(vec![
            BoxShadow::new(px(0.), px(24.), rgba(0x00000060).into()).blur_radius(px(60.)),
            BoxShadow::new(px(0.), px(1.), rgba(0xffffff0d).into())
                .inset()
                .blur_radius(px(0.)),
        ])
}

pub(crate) fn chip(label: &str, active: bool) -> Div {
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .text_size(px(10.))
        .font_medium()
        .bg(rgb(if active { ACCENT_SOFT } else { PANEL_2 }))
        .text_color(rgb(if active { BLUE } else { MUTED }))
        .child(label.to_string())
}

pub(crate) fn status_progress_bar(ratio: f32) -> Div {
    div()
        .h(px(2.))
        .w_full()
        .rounded_full()
        .bg(rgb(PANEL_2))
        .child(
            div()
                .h_full()
                .w(relative(ratio.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(rgb(BLUE)),
        )
}

pub(crate) fn manual_option_row(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    selected: bool,
    enabled: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let title: SharedString = title.into();
    let description: SharedString = description.into();
    h_flex()
        .id(id)
        .w_full()
        .items_start()
        .gap(px(10.))
        .p(px(11.))
        .rounded(px(10.))
        .bg(rgb(if selected { ACCENT_SOFT } else { PANEL_2 }))
        .border_1()
        .border_color(rgb(if selected { BLUE } else { BORDER }))
        .opacity(if enabled { 1.0 } else { 0.5 })
        .when(enabled, |this| this.cursor_pointer())
        .child(
            div()
                .mt(px(1.))
                .size(px(14.))
                .rounded_full()
                .border_2()
                .border_color(rgb(if selected { BLUE } else { BORDER_STRONG }))
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |this| {
                    this.child(div().size(px(6.)).rounded_full().bg(rgb(BLUE)))
                }),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_medium()
                        .text_color(rgb(if enabled { TEXT } else { MUTED_2 }))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(MUTED_2))
                        .child(description),
                ),
        )
}

pub(crate) fn compression_policy_warning(
    keep_original: bool,
    auto_compress: bool,
) -> Option<&'static str> {
    (auto_compress && !keep_original).then_some(
        "自動圧縮で圧縮に成功すると元画像をOSゴミ箱へ移動します。取り消すにはゴミ箱から復元してください。",
    )
}

pub(crate) fn release_confirmation(keep_original: bool, count: i64) -> Option<String> {
    (!keep_original).then(|| {
        format!(
            "待機中の自動圧縮 {count} 件を実行し、成功した元画像をOSゴミ箱へ移動します。取り消すにはゴミ箱から復元してください。"
        )
    })
}

impl App {
    /// A text-only control that participates in the viewer's focus order.
    ///
    /// GPUI synthesizes a click on key-up for focused elements, so keyboard
    /// activation happens on key-down instead and the click handler ignores
    /// `ClickEvent::Keyboard`. Without that split every press would activate
    /// twice.
    pub(crate) fn focusable_text_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        focus: FocusHandle,
        state: ButtonState,
        cx: &mut Context<Self>,
        on_activate: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
    ) -> AnyElement {
        let ButtonState { tone, enabled } = state;
        let label: SharedString = label.into();
        let activate: ViewerActivate = Rc::new(on_activate);
        let click_activate = activate.clone();
        let key_activate = activate;
        let click_focus = focus.clone();
        let key_focus = focus.clone();

        let (border, background, fg) = if !enabled {
            (rgba(0xffffff08), rgba(0x00000000), rgba(0x76819680))
        } else {
            match tone {
                ButtonTone::Primary => (rgba(0x66b3ff40), rgba(0x66b3ff1a), rgb(0xdceeff)),
                ButtonTone::Neutral => (rgba(0xffffff10), rgba(0xffffff08), rgb(MUTED_2)),
                ButtonTone::Danger => (rgba(0xfb718566), rgba(0xfb718526), rgb(RED)),
            }
        };

        let mut button = self.base_text_button(id, border, background, fg);

        if enabled {
            let (hover_bg, hover_fg, focus_border, focus_bg, focus_fg) = match tone {
                ButtonTone::Primary => (
                    rgba(0xffffff14),
                    rgb(TEXT),
                    rgb(BLUE),
                    rgb(ACCENT_SOFT),
                    rgb(TEXT),
                ),
                ButtonTone::Neutral => (
                    rgba(0xffffff14),
                    rgb(TEXT),
                    rgb(BLUE),
                    rgb(ACCENT_SOFT),
                    rgb(TEXT),
                ),
                ButtonTone::Danger => (
                    rgba(0xfb71853d),
                    rgb(0xffd9df),
                    rgb(RED),
                    rgba(0xfb718533),
                    rgb(0xffd9df),
                ),
            };
            button = button
                .cursor_pointer()
                .track_focus(&focus)
                .tab_stop(true)
                .hover(move |this| this.bg(hover_bg).text_color(hover_fg))
                .focus(move |this| {
                    this.border_color(focus_border)
                        .bg(focus_bg)
                        .text_color(focus_fg)
                })
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // Enter/Space activation is handled on key down; GPUI also
                    // synthesizes a keyboard click on key up, so ignore that path
                    // to avoid firing the action twice.
                    if matches!(event, ClickEvent::Keyboard(_)) {
                        return;
                    }
                    window.focus(&click_focus, cx);
                    click_activate(this, window, cx);
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if !event.is_held && (key == "enter" || key == "space") {
                        window.prevent_default();
                        cx.stop_propagation();
                        window.focus(&key_focus, cx);
                        key_activate(this, window, cx);
                    }
                }));
        }

        button.child(label).into_any_element()
    }

    /// Shared chrome for the viewer's text-only controls.
    pub(crate) fn base_text_button(
        &self,
        id: &'static str,
        border: Rgba,
        background: Rgba,
        fg: Rgba,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(28.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .bg(background)
            .text_size(px(11.))
            .font_medium()
            .text_color(fg)
            .whitespace_nowrap()
    }

    /// A tag pill with a remove control for the full-screen viewer.
    pub(crate) fn tag_chip(
        &self,
        photo_id: i64,
        tag: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let remove = tag.clone();
        h_flex()
            .id(SharedString::from(format!("viewer-tag-{tag}")))
            .items_center()
            .gap(px(3.))
            .pl(px(7.))
            .pr(px(3.))
            .py(px(2.))
            .rounded_full()
            .bg(rgba(0xffffff0f))
            .border_1()
            .border_color(rgba(0xffffff12))
            .text_size(px(10.))
            .text_color(rgb(MUTED))
            .child(format!("#{tag}"))
            .child(
                div()
                    .id(SharedString::from(format!("viewer-tag-remove-{tag}")))
                    .cursor_pointer()
                    .size(px(14.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .text_color(rgb(MUTED_2))
                    .hover(|this| this.bg(rgba(0xfb71852e)).text_color(rgb(RED)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove_tag(photo_id, &remove, cx);
                    }))
                    .child("×"),
            )
            .into_any_element()
    }

    pub(crate) fn preset_button(
        &self,
        label: &str,
        days: Option<i64>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.search_days == days;
        chip(label, active)
            .id(SharedString::from(format!("preset-{label}")))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.search_days = days;
                this.run_search(cx);
            }))
    }

    pub(crate) fn filter_field(&self, label: &str, input: Input) -> impl IntoElement {
        v_flex()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(input.w_full())
    }

    pub(crate) fn setting_switch(
        &self,
        label: &str,
        description: &str,
        checked: bool,
        capture_layout: bool,
        cx: &mut Context<Self>,
        apply: fn(&mut AppSettings, bool),
    ) -> impl IntoElement {
        let label = label.to_string();
        h_flex()
            .items_center()
            .gap(px(12.))
            .py(px(2.))
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(description.to_string()),
                    ),
            )
            .child(div().flex_1())
            .child(
                Switch::new(SharedString::from(format!("setting-{label}")))
                    .checked(checked)
                    .on_click(cx.listener(move |this, value, _, cx| {
                        if capture_layout {
                            this.capture_display_layout_anchors();
                        }
                        apply(&mut this.settings, *value);
                        if let Err(error) = this.settings.save(&this.db) {
                            tracing::warn!(%error, setting = %label, "cannot save setting");
                        }
                        cx.notify();
                    })),
            )
    }

    pub(crate) fn card_size_button(&self, size: CardSize, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.settings.ui_card_size == size;
        let label = card_size_label(size);
        Button::new(SharedString::from(format!("card-size-{label}")))
            .label(label)
            .small()
            .selected(selected)
            .toggled(selected)
            .on_click(cx.listener(move |this, _, _, cx| this.set_card_size(size, cx)))
            .into_any_element()
    }

    pub(crate) fn stat_card(&self, label: &str, value: String) -> impl IntoElement {
        glass_panel()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .p(px(16.))
            .gap(px(6.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(value),
            )
    }

    pub(crate) fn meta_row(&self, label: &str, value: String) -> impl IntoElement {
        h_flex()
            .gap(px(8.))
            .text_size(px(10.))
            .child(
                div()
                    .w(px(86.))
                    .flex_shrink_0()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(value),
            )
    }

    pub(crate) fn section_label(&self, label: &str) -> impl IntoElement {
        div()
            .text_size(px(10.))
            .font_semibold()
            .text_color(rgb(MUTED_2))
            .child(label.to_string())
    }

    pub(crate) fn badge(&self, text: &str, fg: u32, bg: u32) -> impl IntoElement {
        div()
            .px(px(7.))
            .py(px(2.))
            .rounded(px(8.))
            .bg(rgb(bg))
            .text_size(px(10.))
            .font_medium()
            .text_color(rgb(fg))
            .child(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{compression_policy_warning, release_confirmation};

    #[test]
    fn compression_policy_warning_is_limited_to_automatic_compression() {
        assert!(compression_policy_warning(true, true).is_none());
        assert!(compression_policy_warning(true, false).is_none());
        assert!(compression_policy_warning(false, false).is_none());

        let automatic = compression_policy_warning(false, true).unwrap();
        assert!(automatic.contains("自動圧縮"));
        assert!(automatic.contains("OSゴミ箱"));
    }

    #[test]
    fn release_confirmation_only_appears_for_replace() {
        assert!(release_confirmation(true, 3).is_none());
        let replace = release_confirmation(false, 3).unwrap();
        assert!(replace.contains("3 件"));
        assert!(replace.contains("OSゴミ箱"));
    }
}
