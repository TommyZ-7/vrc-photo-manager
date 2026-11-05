//! AI-tag picker modal for the search filter.

use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_ai_protocol::TagBranch;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::ai_tag_picker::{AiTagPickerRow, ai_tag_picker_rows, branch_label};
use crate::ui::components::dialog_panel;
use crate::ui::theme::*;

impl App {
    /// Modal overlay for choosing the search filter's AI tags.
    pub(crate) fn ai_tag_picker_overlay(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.ai_tag_picker.open {
            return div().into_any_element();
        }

        let p = self.palette;
        let language = vrc_photon_i18n::current_locale();
        let query = self.ai_tag_picker.query.read(cx).value().to_string();
        let rows = ai_tag_picker_rows(
            &self.ai.tag_definitions,
            &self.ai_tag_picker.selected,
            self.ai_tag_picker.branch,
            &query,
            &language,
        );
        let selected_count = self.ai_tag_picker.selected.len();

        let all_tab = tab_button(
            "ai-tag-picker-all",
            tr!("ai.tags.picker.all"),
            self.ai_tag_picker.branch.is_none(),
        )
        .on_click(cx.listener(|this, _, _, cx| this.set_ai_tag_picker_branch(None, cx)));
        let outdoor_tab =
            tab_button(
                "ai-tag-picker-outdoor",
                tr!("ai.tags.picker.outdoor"),
                self.ai_tag_picker.branch == Some(TagBranch::Outdoor),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_ai_tag_picker_branch(Some(TagBranch::Outdoor), cx)
            }));
        let indoor_tab =
            tab_button(
                "ai-tag-picker-indoor",
                tr!("ai.tags.picker.indoor"),
                self.ai_tag_picker.branch == Some(TagBranch::Indoor),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_ai_tag_picker_branch(Some(TagBranch::Indoor), cx)
            }));

        let mut list = Vec::with_capacity(rows.len());
        for row in rows {
            list.push(picker_row(&p, row, cx));
        }
        let list = if list.is_empty() {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(130.))
                .text_size(px(11.))
                .text_color(rgb(p.muted_2))
                .child(tr!("ai.tags.picker.empty"))
                .into_any_element()
        } else {
            v_flex()
                .id("ai-tag-picker-list")
                .max_h(px(330.))
                .overflow_y_scrollbar()
                .children(list)
                .into_any_element()
        };

        let panel = dialog_panel(&self.palette)
            .id("ai-tag-picker-panel")
            .w(px(760.))
            .max_h(px(650.))
            .gap(px(12.))
            .p(px(18.))
            .track_focus(&self.ai_tag_picker.focus)
            .focus_trap("ai-tag-picker", &self.ai_tag_picker.focus)
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
                                    .child(tr!("ai.tags.picker.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("ai.tags.picker.hint")),
                            ),
                    )
                    .child(
                        Button::new("ai-tag-picker-close")
                            .icon(Icon::new(IconName::Close))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_ai_tag_picker(window, cx)
                            })),
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
                            .min_w_0()
                            .child(Input::new(&self.ai_tag_picker.query)),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(all_tab)
                            .child(outdoor_tab)
                            .child(indoor_tab),
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
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr_args!("ai.tags.picker.selected", count = selected_count)),
                    )
                    .child(
                        Button::new("ai-tag-picker-clear")
                            .label(tr!("ai.tags.picker.clear"))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.clear_ai_tag_picker(cx))),
                    )
                    .child(
                        Button::new("ai-tag-picker-cancel")
                            .label(tr!("ai.tags.picker.cancel"))
                            .small()
                            .secondary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_ai_tag_picker(window, cx)
                            })),
                    )
                    .child(
                        Button::new("ai-tag-picker-apply")
                            .label(tr!("ai.tags.picker.apply"))
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_ai_tag_picker(window, cx)
                            })),
                    ),
            );

        div()
            .id("ai-tag-picker-scrim")
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
            .key_context("ai_tag_picker")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_ai_tag_picker(window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.close_ai_tag_picker(window, cx)))
            .child(panel)
            .into_any_element()
    }
}

fn tab_button(id: &'static str, label: String, active: bool) -> Button {
    let button = Button::new(id).label(label).small();
    if active {
        button.primary()
    } else {
        button.secondary()
    }
}

fn picker_row(p: &Palette, row: AiTagPickerRow, cx: &mut Context<App>) -> AnyElement {
    let key = row.tag_key.clone();
    let selected = row.selected;
    let branch = row.branch;
    div()
        .id(SharedString::from(format!("ai-tag-picker-{}", row.tag_key)))
        .flex()
        .items_center()
        .gap(px(9.))
        .px(px(9.))
        .py(px(7.))
        .rounded(px(7.))
        .when(selected, |this| this.bg(rgb(p.accent_soft)))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(p.panel_inner)))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_ai_tag_picker_key(&key, cx)))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(15.))
                .rounded(px(4.))
                .border_1()
                .border_color(rgb(if selected { p.accent } else { p.border }))
                .bg(if selected {
                    rgb(p.accent)
                } else {
                    rgba(0x00000000)
                })
                .when(selected, |this| {
                    this.child(
                        Icon::new(IconName::Check)
                            .with_size(px(11.))
                            .text_color(rgb(p.on_accent)),
                    )
                }),
        )
        .child(
            v_flex()
                .flex_1()
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
                        .text_color(rgb(p.muted_2))
                        .child(row.tag_key),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(9.))
                .text_color(rgb(p.muted_2))
                .child(branch_label(branch)),
        )
        .into_any_element()
}
