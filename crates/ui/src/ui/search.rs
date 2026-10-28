//! Search screen: filter card, results card, and the shared 3:2 result grid.
//!
//! The grid and selection bar come from `ui::library`; screen dispatch lives
//! in `ui::shell`.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
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
        let results = self.search_results_card(window, cx);
        h_flex()
            .flex_1()
            .min_h_0()
            .gap(px(CARD_GAP))
            .items_stretch()
            .child(filters)
            .child(results)
            .into_any_element()
    }

    fn search_results_card(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let total = self.search_view.index.total;
        let count = format!("{total} 枚");
        let selection = (self.selected_ids.len() > 1).then(|| self.selection_bar(cx));
        let mut card = glass_panel()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .rounded(px(17.))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(px(GALLERY_HEADER_HEIGHT))
                    .px(px(GALLERY_PAD_X))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgba(0xffffff14))
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_baseline()
                            .gap(px(7.))
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(12.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child(count),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child("検索結果"),
                            ),
                    )
                    .when(self.searching, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.))
                                .text_color(rgb(AMBER))
                                .child("検索中…"),
                        )
                    })
                    .child(div().flex_1()),
            );
        if let Some(selection) = selection {
            card = card.child(
                div()
                    .flex_shrink_0()
                    .px(px(GALLERY_PAD_X))
                    .pt(px(8.))
                    .child(selection),
            );
        }
        card.child(self.photo_grid(true, window, cx))
            .into_any_element()
    }

    fn search_filter_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        glass_panel()
            .flex()
            .flex_col()
            .w(px(SEARCH_FILTER_WIDTH))
            .h_full()
            .min_h_0()
            .flex_shrink_0()
            .overflow_y_scrollbar()
            .p(px(16.))
            .gap(px(14.))
            .rounded(px(17.))
            .child(
                div()
                    .text_size(px(13.))
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
