//! Search screen: filter panel, result header, and result grid.
//!
//! The grid and selection bar come from `ui::library`; screen dispatch lives
//! in `ui::shell`.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::App;
use crate::ui::components::glass_panel;
use crate::ui::theme::*;

impl App {
    pub(crate) fn search_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filters = self.search_filter_panel(cx);
        let message = self.message.clone().or_else(|| self.library_error.clone());
        let total = self.search_view.index.total;
        let count = if self.searching {
            "検索中…".to_string()
        } else {
            format!("{total} 枚")
        };
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(filters)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .px(px(26.))
                    .pt(px(26.))
                    .pb(px(26.))
                    .gap(px(18.))
                    .child(
                        v_flex()
                            .flex_shrink_0()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_size(px(24.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child(count),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED_2))
                                            .child("検索結果"),
                                    ),
                            )
                            .when_some(message, |this, message| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(AMBER))
                                        .truncate()
                                        .child(message),
                                )
                            })
                            .child(self.selection_bar(cx)),
                    )
                    .child(self.photo_grid(true, window, cx)),
            )
            .into_any_element()
    }

    fn search_filter_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        glass_panel()
            .flex()
            .flex_col()
            .w(px(290.))
            .flex_shrink_0()
            .my(px(22.))
            .ml(px(26.))
            .p(px(16.))
            .gap(px(14.))
            .rounded(px(20.))
            .bg(rgb(PANEL))
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("絞り込み"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("カテゴリ間はAND、カテゴリ内はOR"),
            )
            .child(self.filter_field("ワールド", Input::new(&self.world_input)))
            .child(self.filter_field("プレイヤー", Input::new(&self.player_input)))
            .child(self.filter_field("タグ", Input::new(&self.tag_input)))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("日時"),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(self.preset_button("今日", Some(1), cx))
                            .child(self.preset_button("7日", Some(7), cx))
                            .child(self.preset_button("30日", Some(30), cx))
                            .child(self.preset_button("すべて", None, cx)),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("お気に入りのみ"),
                    )
                    .child(div().flex_1())
                    .child(
                        Switch::new("search-favorite")
                            .checked(self.search_favorite)
                            .on_click(cx.listener(|this, value, _, cx| {
                                this.search_favorite = *value;
                                this.run_search(cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("search-run")
                            .label(if self.searching {
                                "検索中…"
                            } else {
                                "検索"
                            })
                            .small()
                            .primary()
                            .disabled(self.searching)
                            .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
                    )
                    .child(
                        Button::new("search-clear")
                            .label("クリア")
                            .small()
                            .secondary()
                            .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
                    ),
            )
            .into_any_element()
    }
}
