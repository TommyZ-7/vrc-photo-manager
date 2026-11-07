//! Tag editor modal for a single photo or a multi-selection.

use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::*;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::ui::components::dialog_panel;
use crate::ui::theme::*;

impl App {
    pub(crate) fn tag_edit_overlay(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.tag_edit.open {
            return div().into_any_element();
        }

        let p = self.palette;
        let subtitle = if self.tag_edit.is_multi {
            tr_args!(
                "photos.tag_edit.subtitle_multi",
                count = self.tag_edit.photo_ids.len()
            )
        } else {
            tr!("photos.tag_edit.subtitle_single")
        };
        let note = if self.tag_edit.is_multi {
            tr!("photos.tag_edit.common_note")
        } else {
            tr!("photos.tag_edit.single_note")
        };
        let empty = if self.tag_edit.is_multi {
            tr!("photos.tag_edit.empty_multi")
        } else {
            tr!("photos.tag_edit.empty_single")
        };

        let tags = if self.tag_edit.tags.is_empty() {
            div()
                .flex()
                .items_center()
                .h(px(34.))
                .text_size(px(10.))
                .text_color(rgb(p.muted_2))
                .child(empty)
                .into_any_element()
        } else {
            h_flex()
                .flex_wrap()
                .gap(px(5.))
                .children(
                    self.tag_edit
                        .tags
                        .iter()
                        .map(|tag| tag_chip(&p, tag, cx))
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        };

        let body = v_flex()
            .gap(px(10.))
            .max_h(px(430.))
            .overflow_y_scrollbar()
            .child(
                h_flex()
                    .items_start()
                    .gap(px(8.))
                    .p(px(9.))
                    .rounded(px(9.))
                    .border_1()
                    .border_color(rgba(with_alpha(p.accent, 0x2e)))
                    .bg(rgba(with_alpha(p.accent, 0x0f)))
                    .child(Icon::new(IconName::Info).xsmall().text_color(rgb(p.accent)))
                    .child(
                        div()
                            .text_size(px(10.))
                            .line_height(relative(1.5))
                            .text_color(rgb(p.muted))
                            .child(note),
                    ),
            )
            .child(tags)
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(Input::new(&self.tag_edit.input).flex_1())
                    .child(
                        Button::new("tag-edit-add")
                            .label(tr!("photos.action.add"))
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_tag_from_dialog(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("photos.tag_edit.hint")),
            );

        let panel = dialog_panel(&p)
            .id("tag-edit-panel")
            .w(px(520.))
            .max_h(px(600.))
            .gap(px(12.))
            .p(px(18.))
            .track_focus(&self.tag_edit.focus)
            .focus_trap("tag-edit", &self.tag_edit.focus)
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                h_flex()
                    .items_start()
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
                                    .child(tr!("photos.tag_edit.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        Button::new("tag-edit-close")
                            .icon(Icon::new(IconName::Close))
                            .small()
                            .ghost()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_tag_edit(window, cx)),
                            ),
                    ),
            )
            .child(body)
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1())
                    .child(
                        Button::new("tag-edit-cancel")
                            .label(tr!("common.cancel"))
                            .small()
                            .secondary()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_tag_edit(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("tag-edit-save")
                            .label(tr!("photos.action.save"))
                            .small()
                            .primary()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_tag_edit(window, cx)),
                            ),
                    ),
            );

        div()
            .id("tag-edit-scrim")
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
            .key_context("tag_edit")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_tag_edit(window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.close_tag_edit(window, cx)))
            .child(panel)
            .into_any_element()
    }
}

fn tag_chip(p: &Palette, tag: &str, cx: &mut Context<App>) -> AnyElement {
    let remove = tag.to_string();
    h_flex()
        .id(SharedString::from(format!("tag-edit-{tag}")))
        .items_center()
        .gap(px(3.))
        .pl(px(7.))
        .pr(px(3.))
        .py(px(2.))
        .rounded_full()
        .bg(rgba(with_alpha(p.text, 0x0f)))
        .border_1()
        .border_color(rgba(with_alpha(p.text, 0x12)))
        .text_size(px(10.))
        .text_color(rgb(p.muted))
        .child(format!("#{tag}"))
        .child(
            div()
                .id(SharedString::from(format!("tag-edit-remove-{tag}")))
                .cursor_pointer()
                .size(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_color(rgb(p.muted_2))
                .hover(|this| {
                    this.bg(rgba(with_alpha(p.red, 0x2e)))
                        .text_color(rgb(p.red))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.remove_tag_from_dialog(&remove, cx);
                }))
                .child("×"),
        )
        .into_any_element()
}
