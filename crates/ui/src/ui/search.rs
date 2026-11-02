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

use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::ui::components::{card_header, glass_panel};
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
        let p = self.palette;
        let total = self.search_view.borrow().index.total;
        let count = tr_args!("search.count", total = total);
        let selection = (self.selected_ids.len() > 1).then(|| self.selection_bar(window, cx));
        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .rounded(px(17.))
            .child(
                card_header(&p)
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
                                    .text_color(rgb(p.text))
                                    .child(count),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("search.results")),
                            ),
                    )
                    .when(self.searching, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.))
                                .text_color(rgb(p.amber))
                                .child(tr!("search.searching")),
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
        card.child(self.search_gallery.clone()).into_any_element()
    }

    fn search_filter_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        glass_panel(&self.palette)
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
                    .text_color(rgb(p.text))
                    .child(tr!("search.filters")),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("search.filter_logic")),
            )
            .child(self.filter_field(tr!("search.world"), Input::new(&self.world_input)))
            .child(self.filter_field(tr!("search.player"), Input::new(&self.player_input)))
            .child(self.filter_field(tr!("search.tag"), Input::new(&self.tag_input)))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("search.date")),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(self.preset_button(tr!("search.today"), Some(1), cx))
                            .child(self.preset_button(tr!("search.7days"), Some(7), cx))
                            .child(self.preset_button(tr!("search.30days"), Some(30), cx))
                            .child(self.preset_button(tr!("search.all"), None, cx)),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(tr!("search.favorites_only")),
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
                                tr!("search.searching")
                            } else {
                                tr!("search.action")
                            })
                            .small()
                            .primary()
                            .disabled(self.searching)
                            .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
                    )
                    .child(
                        Button::new("search-clear")
                            .label(tr!("search.clear"))
                            .small()
                            .secondary()
                            .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
                    ),
            )
            .into_any_element()
    }
}
