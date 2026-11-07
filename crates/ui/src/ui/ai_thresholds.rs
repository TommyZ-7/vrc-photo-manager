//! Tag-threshold editor modal.

use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::ai_thresholds::{
    AiThresholdRow, PRESENCE_DEFAULT, PRESENCE_SLIDER_KEY, ThresholdTab, threshold_group_label,
    threshold_marker_ratio, threshold_rows,
};
use crate::ui::components::dialog_panel;
use crate::ui::theme::*;

impl App {
    pub(crate) fn ai_thresholds_overlay(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.ai_thresholds.open {
            return div().into_any_element();
        }

        let p = self.palette;
        let language = vrc_photon_i18n::current_locale();
        let query = self.ai_thresholds.query.read(cx).value().to_string();
        let rows = threshold_rows(
            &self.ai_thresholds.definitions,
            &self.ai_thresholds.overrides,
            &self.ai_thresholds.draft,
            self.ai_thresholds.tab,
            &query,
            &language,
        );
        let selected_tab = self.ai_thresholds.tab;
        let presence_value = self
            .ai_thresholds
            .presence_draft
            .unwrap_or(PRESENCE_DEFAULT);
        let presence_overridden = self.ai_thresholds.presence_draft.is_some();

        let outdoor_tab = threshold_tab_button(
            "ai-thresholds-outdoor",
            tr!("settings.ai.thresholds.outdoor"),
            selected_tab == ThresholdTab::Outdoor,
        )
        .on_click(
            cx.listener(|this, _, _, cx| this.set_ai_threshold_tab(ThresholdTab::Outdoor, cx)),
        );
        let indoor_tab = threshold_tab_button(
            "ai-thresholds-indoor",
            tr!("settings.ai.thresholds.indoor"),
            selected_tab == ThresholdTab::Indoor,
        )
        .on_click(
            cx.listener(|this, _, _, cx| this.set_ai_threshold_tab(ThresholdTab::Indoor, cx)),
        );
        let atmosphere_tab = threshold_tab_button(
            "ai-thresholds-atmosphere",
            tr!("settings.ai.thresholds.atmosphere"),
            selected_tab == ThresholdTab::Atmosphere,
        )
        .on_click(
            cx.listener(|this, _, _, cx| this.set_ai_threshold_tab(ThresholdTab::Atmosphere, cx)),
        );

        let mut list = Vec::with_capacity(rows.len());
        let mut current_group = String::new();
        for row in rows {
            if row.group != current_group {
                if !row.group.is_empty() {
                    list.push(threshold_group_header(
                        &p,
                        threshold_group_label(&row.group),
                    ));
                }
                current_group = row.group.clone();
            }
            if let Some(slider) = self.ai_thresholds.sliders.get(&row.tag_key).cloned() {
                list.push(threshold_row(&p, row, slider, cx));
            }
        }
        let list = if list.is_empty() {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(130.))
                .text_size(px(11.))
                .text_color(rgb(p.muted_2))
                .child(tr!("settings.ai.thresholds.empty"))
                .into_any_element()
        } else {
            v_flex()
                .id("ai-thresholds-list")
                .max_h(px(360.))
                .overflow_y_scrollbar()
                .children(list)
                .into_any_element()
        };

        let presence_slider = self.ai_thresholds.sliders.get(PRESENCE_SLIDER_KEY).cloned();

        let panel = dialog_panel(&self.palette)
            .id("ai-thresholds-panel")
            .w(px(800.))
            .max_h(px(720.))
            .gap(px(12.))
            .p(px(18.))
            .track_focus(&self.ai_thresholds.focus)
            .focus_trap("ai-thresholds", &self.ai_thresholds.focus)
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.ai.thresholds.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.ai.thresholds.hint")),
                            ),
                    )
                    .child(
                        Button::new("ai-thresholds-close")
                            .icon(Icon::new(IconName::Close))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_ai_thresholds(window, cx)
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(rgb(p.accent_border))
                    .bg(rgb(p.accent_soft))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.))
                                    .font_medium()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.ai.thresholds.presence")),
                            )
                            .child(
                                div()
                                    .text_size(px(9.))
                                    .text_color(rgb(p.amber))
                                    .child(tr!("settings.ai.thresholds.next_run")),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.accent))
                                    .child(format!("{}%", (presence_value * 100.0).round() as u32)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("settings.ai.thresholds.presence_hint")),
                    )
                    .when_some(presence_slider, |this, slider| {
                        this.child(threshold_slider(
                            &p,
                            slider,
                            presence_value,
                            PRESENCE_DEFAULT,
                        ))
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(9.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.ai.thresholds.applies_now")),
                            )
                            .child(
                                Button::new("ai-thresholds-presence-reset")
                                    .label(tr!("settings.ai.thresholds.reset"))
                                    .small()
                                    .ghost()
                                    .disabled(!presence_overridden)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.ai_thresholds.presence_draft = None;
                                        if let Some(slider) = this
                                            .ai_thresholds
                                            .sliders
                                            .get(PRESENCE_SLIDER_KEY)
                                            .cloned()
                                        {
                                            slider.update(cx, |state, cx| {
                                                state.set_value(PRESENCE_DEFAULT, window, cx)
                                            });
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(180.))
                            .child(Input::new(&self.ai_thresholds.query)),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(outdoor_tab)
                            .child(indoor_tab)
                            .child(atmosphere_tab),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(5.))
                            .child(div().w(px(2.)).h(px(12.)).rounded_full().bg(rgb(p.amber)))
                            .child(
                                div()
                                    .text_size(px(9.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.ai.thresholds.default_legend")),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(5.))
                            .child(div().w(px(16.)).h(px(2.)).rounded_full().bg(rgb(p.accent)))
                            .child(
                                div()
                                    .text_size(px(9.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.ai.thresholds.current_legend")),
                            ),
                    ),
            )
            .child(
                div()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.panel_2))
                    .overflow_hidden()
                    .child(list),
            )
            .when_some(self.ai_thresholds.error.clone(), |this, error| {
                this.child(
                    div()
                        .p(px(10.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(rgb(p.red_border))
                        .bg(rgb(p.red_soft))
                        .text_size(px(10.))
                        .text_color(rgb(p.red))
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("settings.ai.thresholds.apply_note")),
                    )
                    .child(
                        Button::new("ai-thresholds-reset-all")
                            .label(tr!("settings.ai.thresholds.reset_all"))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_all_ai_thresholds(window, cx)
                            })),
                    )
                    .child(
                        Button::new("ai-thresholds-cancel")
                            .label(tr!("settings.ai.thresholds.cancel"))
                            .small()
                            .secondary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_ai_thresholds(window, cx)
                            })),
                    )
                    .child(
                        Button::new("ai-thresholds-save")
                            .label(tr!("settings.ai.thresholds.save"))
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_ai_thresholds(window, cx)
                            })),
                    ),
            );

        div()
            .id("ai-thresholds-scrim")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .p(px(DIALOG_VIEWPORT_MARGIN))
            .bg(rgba(p.scrim))
            .key_context("ai_thresholds")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_ai_thresholds(window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.close_ai_thresholds(window, cx)))
            .child(panel)
            .into_any_element()
    }
}

fn threshold_tab_button(id: &'static str, label: String, active: bool) -> Button {
    let button = Button::new(id).label(label).small();
    if active {
        button.primary()
    } else {
        button.secondary()
    }
}

fn threshold_group_header(p: &Palette, label: String) -> AnyElement {
    h_flex()
        .items_center()
        .gap(px(7.))
        .px(px(10.))
        .py(px(6.))
        .border_b_1()
        .border_color(rgb(p.line_soft))
        .bg(rgb(p.panel_2))
        .child(div().w(px(2.)).h(px(10.)).rounded_full().bg(rgb(p.green)))
        .child(
            div()
                .text_size(px(9.))
                .font_medium()
                .text_color(rgb(p.muted))
                .child(label),
        )
        .into_any_element()
}

fn threshold_slider(
    p: &Palette,
    slider: Entity<SliderState>,
    value: f32,
    default: f32,
) -> AnyElement {
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .child(Slider::new(&slider).w_full())
        .child(
            div()
                .absolute()
                .top(px(1.))
                .bottom(px(1.))
                .left(relative(threshold_marker_ratio(default)))
                .w(px(2.))
                .rounded_full()
                .bg(rgb(p.amber)),
        )
        .child(
            div()
                .absolute()
                .top(px(1.))
                .bottom(px(1.))
                .left(relative(threshold_marker_ratio(value)))
                .w(px(2.))
                .rounded_full()
                .bg(rgb(p.accent)),
        )
        .into_any_element()
}

fn threshold_row(
    p: &Palette,
    row: AiThresholdRow,
    slider: Entity<SliderState>,
    cx: &mut Context<App>,
) -> AnyElement {
    let key = row.tag_key.clone();
    let default_percent = (row.default_threshold * 100.0).round() as u32;
    let threshold_percent = (row.threshold * 100.0).round() as u32;
    let meta = if row.overridden {
        format!(
            "{} · {} · {}",
            row.tag_key,
            tr_args!(
                "settings.ai.thresholds.default",
                value = format!("{default_percent}%")
            ),
            tr!("settings.ai.thresholds.overridden")
        )
    } else {
        format!(
            "{} · {}",
            row.tag_key,
            tr_args!(
                "settings.ai.thresholds.default",
                value = format!("{default_percent}%")
            )
        )
    };
    h_flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .py(px(8.))
        .border_b_1()
        .border_color(rgb(p.line_soft))
        .when(row.overridden, |this| this.bg(rgb(p.accent_soft)))
        .child(
            v_flex()
                .w(px(190.))
                .flex_shrink_0()
                .min_w_0()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(p.text))
                        .child(row.name),
                )
                .child(
                    div()
                        .text_size(px(8.5))
                        .text_color(rgb(if row.overridden { p.amber } else { p.muted_2 }))
                        .child(meta),
                ),
        )
        .child(threshold_slider(
            p,
            slider,
            row.threshold,
            row.default_threshold,
        ))
        .child(
            div().w(px(42.)).flex_shrink_0().flex().justify_end().child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.text))
                    .child(format!("{threshold_percent}%")),
            ),
        )
        .child(
            Button::new(SharedString::from(format!("ai-threshold-reset-{key}")))
                .label(tr!("settings.ai.thresholds.reset"))
                .small()
                .ghost()
                .disabled(!row.overridden)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.reset_ai_threshold(&key, window, cx)
                })),
        )
        .into_any_element()
}
