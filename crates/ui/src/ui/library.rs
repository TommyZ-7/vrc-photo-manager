//! Library (Photos) screen: grid, selection bar, inspector, and detail body.
//!
//! The same grid and card widgets are reused by the Search screen; screen
//! dispatch lives in `ui::shell`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::model::Photo;

use crate::app::App;
use crate::app::Screen;
use crate::app::cli::log_timing;
use crate::state::photo_browser::PAGE_SIZE;
use crate::state::preview::preview_placeholder_label;
use crate::state::viewer::ViewerOpenSource;
use crate::state::year_rail::format_captured_month;
use crate::ui::components::{ButtonState, ButtonTone, chip, glass_panel};
use crate::ui::theme::*;
use crate::ui::year_rail::{YearRail, YearRailLabels};

impl App {
    pub(crate) fn photos_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selection = (self.selected_ids.len() > 1).then(|| self.selection_bar(cx));
        let import_panel = self
            .import_report
            .clone()
            .map(|report| self.import_report_panel(&report, cx));
        let library_error = self
            .library_error
            .clone()
            .map(|error| self.library_error_row(error, cx));
        let mut screen = v_flex().flex_1().min_h_0().gap(px(CARD_GAP));
        if let Some(selection) = selection {
            screen = screen.child(selection);
        }
        if let Some(import_panel) = import_panel {
            screen = screen.child(import_panel);
        }
        if let Some(library_error) = library_error {
            screen = screen.child(library_error);
        }
        let mut layout = h_flex().flex_1().min_h_0();
        if !self.inspector_collapsed {
            layout = layout.gap(px(CARD_GAP));
        }
        screen
            .child(
                layout
                    .child(self.gallery_card(window, cx))
                    .child(self.inspector_column(window, cx)),
            )
            .into_any_element()
    }

    fn gallery_card(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .rounded(px(17.))
            .bg(rgb(PANEL_CARD))
            .border_1()
            .border_color(rgba(0xffffff1b))
            .shadow(vec![
                BoxShadow::new(px(0.), px(18.), rgba(0x00000047).into()).blur_radius(px(44.)),
                BoxShadow::new(px(0.), px(1.), rgba(0xffffff14).into())
                    .inset()
                    .blur_radius(px(0.)),
            ])
            .child(self.gallery_header(cx));
        if self.gallery_filters_open {
            card = card.child(self.gallery_filter_row(cx));
        }
        card.child(self.photo_grid(false, window, cx))
            .into_any_element()
    }

    fn gallery_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let entries = &self.library.index.entries;
        let total = entries.len();
        let (month, month_count) = match entries.first() {
            Some(first) => {
                let count = entries
                    .iter()
                    .filter(|entry| entry.captured_month == first.captured_month)
                    .count();
                (format_captured_month(first.captured_month), count)
            }
            None => ("写真なし".to_string(), 0),
        };
        let filters_open = self.gallery_filters_open;
        let filter_button = Button::new("gallery-filter")
            .label("フィルター")
            .small()
            .on_click(cx.listener(|this, _, _, cx| {
                this.gallery_filters_open = !this.gallery_filters_open;
                cx.notify();
            }));
        let filter_button = if filters_open {
            filter_button.secondary()
        } else {
            filter_button.ghost()
        };

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
                    .min_w_0()
                    .items_baseline()
                    .gap(px(7.))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(month),
                    )
                    .child(
                        div()
                            .whitespace_nowrap()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!(
                                "{}枚 / {}",
                                format_count(month_count as i64),
                                format_count(total as i64)
                            )),
                    ),
            )
            .child(div().flex_1())
            .child(filter_button)
            .child(
                div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("並び: 新しい順"),
            )
            .into_any_element()
    }

    fn gallery_filter_row(&mut self, _cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_shrink_0()
            .h(px(34.))
            .px(px(GALLERY_PAD_X))
            .gap(px(6.))
            .items_center()
            .border_b_1()
            .border_color(rgba(0xffffff0f))
            .child(chip("ワールド", true))
            .child(chip("プレイヤー", false))
            .child(chip("日時", false))
            .child(chip("タグ", false))
            .into_any_element()
    }

    pub(crate) fn selection_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.selected_ids.len();
        if count <= 1 {
            return div().into_any_element();
        }
        let ids = self.selected_ids.clone();
        h_flex()
            .flex_shrink_0()
            .h(px(36.))
            .overflow_hidden()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .py(px(8.))
            .rounded(px(12.))
            .bg(rgb(ACCENT_SOFT))
            .border_1()
            .border_color(rgba(0xffffff14))
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(rgb(TEXT))
                    .child(format!("{count} 件選択中")),
            )
            .child(div().flex_1())
            .child(
                Button::new("selection-clear")
                    .label("選択解除")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
            )
            .child(
                Button::new("selection-compress")
                    .label(format!("{count} 件を圧縮"))
                    .small()
                    .primary()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_manual_compression(ids.clone(), cx)
                    })),
            )
            .with_animation(
                "selection-bar",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.h(px(36.0 * t)).opacity(t),
            )
            .into_any_element()
    }

    /// Slim inline failure row above the gallery. The message keeps its full
    /// text; detail is never truncated into the ribbon status slot.
    fn library_error_row(&mut self, error: String, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_shrink_0()
            .mx(px(GALLERY_PAD_X))
            .px(px(14.))
            .py(px(9.))
            .gap(px(10.))
            .items_center()
            .rounded(px(12.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(6.))
                    .h(px(6.))
                    .rounded_full()
                    .bg(rgb(RED)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(rgb(RED))
                    .child(error),
            )
            .child(
                Button::new("library-reload-banner")
                    .label("再読込")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_library(true, cx);
                    })),
            )
            .into_any_element()
    }

    /// Virtualized grid shared by the library and the search screen.
    ///
    /// The range callback only plans missing pages and renders the page cache;
    /// database work is deferred through [`Self::load_pages`], so neither
    /// render nor prepaint ever touches SQLite.
    pub(crate) fn photo_grid(
        &mut self,
        search: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let client_inset = window.client_inset().map(f32::from).unwrap_or(0.0);
        let width = window_content_extent(f32::from(window.viewport_size().width), client_inset);
        let pending = if search {
            self.pending_layout_anchor_search.take()
        } else {
            self.pending_layout_anchor_library.take()
        };
        let (next_columns, row_height, image_height) = if search {
            // Search keeps its pre-Task 17 fixed-image metrics.
            let metrics = self.card_metrics();
            (
                search_grid_columns(width, self.sidebar_collapsed, metrics.min_card_width),
                metrics.row_height,
                metrics.image_height,
            )
        } else {
            let metrics = library_grid_metrics(
                width,
                self.sidebar_collapsed,
                self.inspector_collapsed,
                self.settings.ui_card_size,
                self.settings.ui_card_show_world,
                self.settings.ui_card_show_date || self.settings.ui_card_show_size,
            );
            (metrics.columns, metrics.row_height, metrics.image_height)
        };
        let grid_gap = if search { 16.0 } else { GRID_GAP };
        let browser = self.browser_mut(search);
        browser.columns = next_columns;
        browser.row_height = row_height;
        if let Some(id) = pending {
            let row = browser
                .index
                .row_of_id(id)
                .map(|position| position / browser.columns.max(1))
                .unwrap_or(0);
            browser
                .scroll
                .scroll_to_item_strict(row, ScrollStrategy::Top);
        }
        let labels = if browser.labels.is_none() || browser.labels_generation != browser.generation
        {
            // Shaping is expensive, so do it once per index snapshot instead
            // of on every render or paint frame.
            let labels = Arc::new(YearRailLabels::build(&browser.index, window));
            browser.labels = Some(labels.clone());
            browser.labels_generation = browser.generation;
            labels
        } else {
            browser.labels.clone().expect("year rail labels")
        };
        let columns = browser.columns;
        let index = browser.index.clone();
        let scroll = browser.scroll.clone();
        let generation = browser.generation;
        let rail_drag = browser.rail_drag.clone();
        let rail_hover = browser.rail_hover.clone();
        let rows = index.total.div_ceil(columns);

        if rows == 0 {
            let empty = if search {
                if self.search_active {
                    glass_panel()
                        .p(px(20.))
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child("条件に一致する写真がありません")
                        .into_any_element()
                } else {
                    // Before a search runs the browser mirrors the library, so
                    // an empty library is not an empty result set.
                    div().into_any_element()
                }
            } else if let Some(error) = self.library_error.clone() {
                div()
                    .mt(px(18.))
                    .max_w(px(560.))
                    .p(px(20.))
                    .rounded(px(18.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(12.))
                    .text_color(rgb(RED))
                    .child(error)
                    .child(
                        div()
                            .pt(px(4.))
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("リボンの「再読込」で再試行できます。"),
                    )
                    .into_any_element()
            } else {
                div()
                    .mt(px(18.))
                    .max_w(px(560.))
                    .p(px(20.))
                    .rounded(px(18.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(
                        "写真がありません。VRChatの写真フォルダを設定するか、--demo でサンプルを表示できます。",
                    )
                    .into_any_element()
            };
            let mut container = v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .h_full()
                .items_stretch();
            if !search {
                container = container
                    .pl(px(GALLERY_PAD_X))
                    .pr(px(GALLERY_PAD_X))
                    .pt(px(GALLERY_PAD_TOP))
                    .pb(px(GALLERY_PAD_BOTTOM));
            }
            return container.child(empty).into_any_element();
        }

        let list_index = index.clone();
        let list = uniform_list(
            // Keep the list ID stable so uniform_list retains its scroll
            // offset when the column count changes; the row height is constant
            // for the current layout and only changes on resize.
            SharedString::from(if search {
                "search-grid-list"
            } else {
                "photo-grid-list"
            }),
            rows,
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                let photo_start = range.start * columns;
                let photo_end = (range.end * columns).min(list_index.total);
                let missing = this
                    .browser_mut(search)
                    .plan_pages(photo_start, photo_end, 2);
                if !missing.is_empty() {
                    cx.defer_in(window, move |this, _window, cx| {
                        this.load_pages(search, generation, missing, cx);
                    });
                }
                let mut rendered = Vec::with_capacity(range.len());
                for row in range {
                    let mut line = h_flex()
                        .h(px(row_height))
                        .w_full()
                        .min_w_0()
                        .items_start()
                        .gap(px(grid_gap))
                        // Keep cards out from under the year rail. The list
                        // itself stays full width so wheel events over the
                        // gutter still reach its scroll handle.
                        .pr(px(RAIL_WIDTH));
                    for column in 0..columns {
                        let photo_index = row * columns + column;
                        if photo_index >= list_index.total {
                            line = line.child(div().flex_1());
                            continue;
                        }
                        match this.browser_mut(search).photo_at(photo_index) {
                            Some(photo) => {
                                line = line.child(this.photo_card(
                                    photo.as_ref(),
                                    photo_index,
                                    image_height,
                                    cx,
                                ));
                            }
                            None => {
                                line = line.child(grid_placeholder(image_height));
                            }
                        }
                    }
                    rendered.push(line);
                }
                rendered
            }),
        )
        .track_scroll(&scroll)
        .flex_1()
        .w_full()
        .min_w_0()
        .min_h_0();
        // Search has no gallery padding above the list yet; leave room for
        // the first row's -4px hover lift. The library gallery supplies that
        // room through GALLERY_PAD_TOP.
        let list = if search { list.pt(px(4.)) } else { list };

        // The wrapper stays mounted (and relative) so the year rail can
        // overlay the list without remounting it.
        let mut wrapper = v_flex()
            .id(if search { "search-grid" } else { "photo-grid" })
            .relative()
            .flex_1()
            .w_full()
            .min_w_0()
            .min_h_0();
        if !search {
            wrapper = wrapper
                .track_focus(&self.photo_grid_focus)
                .key_context("photo_grid")
                .tab_stop(true);
        }
        let mut grid = wrapper.child(list);
        if index.total > 0 {
            // Added after the list so the rail owns the reserved gutter
            // instead of the cards underneath.
            grid = grid.child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(px(RAIL_WIDTH))
                    .child(YearRail::new(
                        if search {
                            "search-year-rail"
                        } else {
                            "year-rail"
                        },
                        labels,
                        index,
                        scroll,
                        columns,
                        row_height,
                        rail_drag,
                        rail_hover,
                    )),
            );
        }
        let mut container = v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .items_stretch();
        if !search {
            container = container
                .pl(px(GALLERY_PAD_X))
                .pr(px(GALLERY_PAD_X))
                .pt(px(GALLERY_PAD_TOP))
                .pb(px(GALLERY_PAD_BOTTOM));
        }
        container.child(grid).into_any_element()
    }

    /// Loads at most two pages for the grid, then inserts them unless the
    /// index was replaced while the query was running.
    fn load_pages(
        &mut self,
        search: bool,
        generation: u64,
        pages: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        let browser = self.browser_mut(search);
        if browser.generation != generation {
            return;
        }
        let index = browser.index.clone();
        let mut ids = Vec::new();
        let mut requests = Vec::new();
        for page in pages.into_iter().take(2) {
            let start = page * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(index.total);
            if start >= end {
                continue;
            }
            ids.extend(index.entries[start..end].iter().map(|entry| entry.id));
            requests.push((page, start, end));
        }
        if requests.is_empty() {
            return;
        }
        let page_start = Instant::now();
        let photos = match self.db.photos_by_ids(&ids) {
            Ok(photos) => photos,
            Err(error) => {
                tracing::error!(%error, "photos_by_ids failed");
                {
                    let browser = self.browser_mut(search);
                    for (page, _, _) in &requests {
                        browser.fail_page(*page, generation);
                    }
                }
                self.library_error = Some("一部の写真を読み込めませんでした".to_string());
                cx.notify();
                return;
            }
        };
        log_timing("photos_by_ids", page_start);
        let mut by_id: HashMap<i64, Photo> =
            photos.into_iter().map(|photo| (photo.id, photo)).collect();
        let mut short_page = false;
        {
            let browser = self.browser_mut(search);
            for (page, start, end) in requests {
                let page_photos: Vec<Photo> = index.entries[start..end]
                    .iter()
                    .filter_map(|entry| by_id.remove(&entry.id))
                    .collect();
                if page_photos.len() == end - start {
                    browser.insert_page(page, page_photos, generation);
                } else {
                    // A row deleted mid-flight would shift every later offset
                    // in the page; keep placeholders until the next reload
                    // rebuilds the snapshot. The retry action makes the
                    // failure recoverable instead of permanent.
                    tracing::warn!(
                        returned = page_photos.len(),
                        expected = end - start,
                        "photos_by_ids returned a short page"
                    );
                    short_page = true;
                    browser.fail_page(page, generation);
                }
            }
        }
        if short_page {
            self.library_error = Some("一部の写真を読み込めませんでした".to_string());
        }
        cx.notify();
    }

    fn photo_card(
        &mut self,
        photo: &Photo,
        grid_index: usize,
        image_height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = photo.id;
        let selected = self.selected_ids.contains(&id);
        let primary = self.selected == Some(id);
        let compressed = self.compressed_ids.contains(&id);
        let duplicate = self.duplicate_ids.contains(&id);
        let replaced = photo.is_replaced();
        let show_status = self.settings.ui_card_show_status;
        let show_world = self.settings.ui_card_show_world;
        let show_date = self.settings.ui_card_show_date;
        let show_size = self.settings.ui_card_show_size;
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        let thumbnail = self.thumbnails.get(id);
        if thumbnail.is_none() {
            self.request_thumbnail(photo, cx);
        }
        let color = self.color_for(photo);
        let hovered = self.hovered_photo == Some(id);
        let intro_total =
            CARD_INTRO_SECS + grid_index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS;
        let intro_started =
            (grid_index < 24).then(|| *self.intro_seen.entry(id).or_insert_with(Instant::now));
        let intro = intro_started.is_some_and(|started| {
            Instant::now().duration_since(started).as_secs_f32() < intro_total
        });
        let grid_epoch = self.grid_epoch;
        let card_radius = 13.0;
        let image_radius = inner_corner_radius(card_radius, 1.0);

        let mut thumb = div()
            .relative()
            .h(px(image_height))
            .w_full()
            .overflow_hidden()
            .bg(rgb(color))
            .rounded_tl(px(image_radius))
            .rounded_tr(px(image_radius));
        if let Some(image) = thumbnail {
            thumb = thumb.child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full()
                    // GPUI clips overflow to a rectangle, so the image has to
                    // carry the rounded silhouette itself.
                    .rounded_tl(px(image_radius))
                    .rounded_tr(px(image_radius)),
            );
        }
        let bounds_map = self.card_image_bounds.clone();
        let bounds_frame = self.render_epoch;
        thumb = thumb.child(
            canvas(
                move |bounds, _, _| {
                    bounds_map.borrow_mut().insert(id, (bounds_frame, bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        if show_status {
            let status_color = if replaced {
                BLUE
            } else if duplicate {
                MUTED
            } else if compressed {
                GREEN
            } else {
                AMBER
            };
            thumb = thumb
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(56.))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(rgba(0x07090d00), 0.),
                            linear_color_stop(rgba(0x07090dcc), 1.),
                        )),
                )
                .child(
                    h_flex()
                        .absolute()
                        .left(px(9.))
                        .bottom(px(8.))
                        .gap(px(5.))
                        .items_center()
                        .child(div().size(px(5.)).rounded_full().bg(rgb(status_color)))
                        .child(
                            div()
                                .text_size(px(9.))
                                .font_medium()
                                .text_color(rgb(status_color))
                                .child(if replaced {
                                    "圧縮版のみ"
                                } else if duplicate {
                                    "重複"
                                } else if compressed {
                                    "圧縮済"
                                } else {
                                    "未圧縮"
                                }),
                        ),
                );
        }
        thumb = thumb.when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .top(px(9.))
                    .left(px(9.))
                    .size(px(19.))
                    .rounded_full()
                    .bg(rgb(BLUE))
                    .border_2()
                    .border_color(rgb(0x07090d))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .font_semibold()
                    .text_color(rgb(0x071018))
                    .child("✓"),
            )
        });
        thumb = thumb.when(photo.is_favorite, |this| {
            this.child(
                div()
                    .absolute()
                    .top(px(9.))
                    .right(px(9.))
                    .size(px(7.))
                    .rounded_full()
                    .bg(rgb(AMBER))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(0.), rgba(0x00000080).into()).blur_radius(px(3.)),
                    ]),
            )
        });

        let meta_line = card_meta_line(
            show_date,
            show_size,
            &photo.captured_at_local,
            format_bytes(photo.effective_file_size()),
        );
        let has_info = show_world || meta_line.is_some();
        let mut info = v_flex()
            .min_w_0()
            .px(px(9.))
            .pt(px(CARD_BODY_GAP))
            .gap(px(CARD_TEXT_GAP));
        if show_world {
            info = info.child(
                div()
                    .min_w_0()
                    .text_size(px(12.))
                    .line_height(relative(1.5))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(world),
            );
        }
        if let Some(meta) = meta_line {
            info = info.child(
                div()
                    .min_w_0()
                    .text_size(px(10.))
                    .line_height(relative(1.5))
                    .text_color(rgb(MUTED_2))
                    .truncate()
                    .child(meta),
            );
        }

        let mut card = v_flex()
            .id(SharedString::from(format!("photo-{id}")))
            .relative()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .rounded(px(card_radius))
            .border_1()
            .border_color(if primary {
                rgba(0x66b3ff)
            } else if selected {
                rgba(0x9fcaff)
            } else {
                rgba(0xffffff16)
            })
            .bg(rgb(PANEL_DEEP))
            .cursor_pointer()
            .child(thumb);
        if selected {
            card = card.shadow(vec![
                BoxShadow::new(px(0.), px(8.), rgba(0x66b3ff4d).into()).blur_radius(px(24.)),
            ]);
        }
        if has_info {
            card = card.child(info);
        }
        let card = card
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Hover listeners fire in reverse paint order, so the
                // entering card must win over the leaving card's clear.
                if *hovered {
                    if this.hovered_photo != Some(id) {
                        this.hovered_photo = Some(id);
                        cx.notify();
                    }
                } else if this.hovered_photo == Some(id) {
                    this.hovered_photo = None;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.handle_photo_click(id, event, cx);
                if this.screen == Screen::Photos {
                    window.focus(&this.photo_grid_focus, cx);
                } else {
                    // Keep focus on a handle that is actually mounted. Search
                    // cards reuse this handler, but its grid is Photos-only.
                    let handle = this.world_input.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                }
                if let ClickEvent::Mouse(click) = event
                    && click.down.click_count >= 2
                    && !event.modifiers().modified()
                {
                    this.open_quick_look(window, cx, ViewerOpenSource::Card);
                }
            }))
            .with_animation(
                SharedString::from(format!("card-hover-{id}-{hovered}")),
                Animation::new(Duration::from_millis(180)).with_easing(ease_motion),
                move |el, t| {
                    let offset = if hovered { -4.0 * t } else { -4.0 * (1.0 - t) };
                    el.relative().top(px(offset))
                },
            );

        // The intro wrapper is the flex item; keep the hover animation inside
        // it so changing `hovered` does not remount the intro state. Keep the
        // wrapper mounted for the card's lifetime: removing it when the intro
        // ends would change the hover animation's ancestor path and remount it,
        // causing a 4 px settle without a hover.
        let cell = div().relative().flex_1().min_w_0().child(card);

        cell.with_animation(
            SharedString::from(format!("card-intro-{grid_epoch}-{id}")),
            Animation::new(if intro {
                Duration::from_secs_f32(intro_total)
            } else {
                Duration::ZERO
            })
            .with_max_fps(60.0),
            move |el, t| {
                if intro {
                    let value = stagger_value(t, grid_index);
                    el.opacity(value).relative().top(px(14.0 * (1.0 - value)))
                } else {
                    el
                }
            },
        )
        .into_any_element()
    }

    fn inspector_column(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.inspector_collapsed;
        let inset = window.client_inset().map(f32::from).unwrap_or(0.0);
        let content_width = window_content_extent(f32::from(window.viewport_size().width), inset);
        let column_width = inspector_column_width(content_width);
        let mut column = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if !collapsed {
            column = column.child(self.inspector(window, cx));
        }
        column
            .with_animation(
                ("inspector-column", self.inspector_epoch),
                Animation::new(if self.inspector_epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(300)
                })
                .with_easing(ease_motion)
                .with_max_fps(60.0),
                move |el, t| {
                    let from = if collapsed { column_width } else { 0.0 };
                    let to = if collapsed { 0.0 } else { column_width };
                    el.w(px(from + (to - from) * t))
                },
            )
            .into_any_element()
    }

    fn inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let viewport = window.viewport_size();
        let client_inset = window.client_inset().map(f32::from).unwrap_or(0.0);
        let content_width = window_content_extent(f32::from(viewport.width), client_inset);
        let content_height = window_content_extent(f32::from(viewport.height), client_inset);
        let column_width = inspector_column_width(content_width);
        let card_width = inspector_card_width(content_width);
        let preview_height = inspector_preview_height(content_height);
        let transitioning = self
            .selection_changed_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(260));
        if !transitioning {
            // The cross-fade snapshot is only valid for the selection window.
            // Keeping it would replay the old photo when the inspector remounts
            // after a collapse or a viewer session.
            self.previous_preview = None;
            self.previous_color = None;
        }
        let Some(photo) = self.selected_photo.as_ref() else {
            return v_flex()
                .w(px(column_width))
                .h_full()
                .flex_shrink_0()
                .pr(px(INSPECTOR_MARGIN))
                .child(
                    glass_panel()
                        .flex_1()
                        .w(px(card_width))
                        .rounded(px(17.))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(6.))
                        .px(px(20.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_medium()
                                .text_color(rgb(TEXT))
                                .child("写真を選択してください"),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("ライブラリから写真を選ぶと、ここに情報が表示されます。"),
                        ),
                )
                .into_any_element();
        };

        let id = photo.id;
        let current_color = self.selected_color();
        let previous_color = self.previous_color.unwrap_or(current_color);
        let selection_epoch = self.selection_epoch;
        let previous_preview = self.previous_preview.clone();
        let current_preview = self.preview.clone();
        let thumbnail = self.thumbnails.get(id);
        let compressed = self.compressed_ids.contains(&id);
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        let players = self
            .selected_players
            .iter()
            .map(|player| player.display_name.clone())
            .collect::<Vec<_>>();
        let tags = self.selected_tags.clone();
        let fade = || {
            Animation::new(Duration::from_millis(260))
                .with_easing(ease_motion)
                .with_max_fps(60.0)
        };
        let section = |label: &'static str| {
            div()
                .text_size(px(10.))
                .font_semibold()
                .text_color(rgb(MUTED_2))
                .child(label)
        };
        let pill = |text: String| {
            div()
                .px(px(7.))
                .py(px(2.))
                .rounded_full()
                .bg(rgba(0xffffff0f))
                .border_1()
                .border_color(rgba(0xffffff12))
                .text_size(px(10.))
                .text_color(rgb(MUTED))
                .child(text)
        };

        let mut card = v_flex()
            .relative()
            .flex_1()
            .w_full()
            .min_h_0()
            .overflow_hidden()
            .rounded(px(18.))
            .border_1()
            .border_color(rgba(0xffffff14))
            .bg(rgb(current_color))
            .shadow(vec![
                BoxShadow::new(px(0.), px(22.), rgba(0x00000073).into()).blur_radius(px(48.)),
            ]);

        // 1. Solid tint, lerped from the previous photo's average color only
        // while the selection is fresh; a remount renders the current tint
        // statically so the old color cannot flash back.
        let tint_layer = div()
            .absolute()
            .size_full()
            .rounded(px(inner_corner_radius(18.0, 1.0)))
            .bg(rgb(current_color));
        card = card.child(if transitioning {
            tint_layer
                .with_animation(("inspector-tint", selection_epoch), fade(), move |el, t| {
                    el.bg(rgb(lerp_color(previous_color, current_color, t)))
                })
                .into_any_element()
        } else {
            tint_layer.into_any_element()
        });

        // 2. The selected 256 px thumbnail, upscaled and softened by opacity.
        //    The layer matches the card bounds so its rounded corners can be
        //    clipped by the image itself (GPUI overflow clipping is rectangular).
        if let Some(thumbnail) = thumbnail {
            card = card.child(
                div().absolute().size_full().opacity(0.45).child(
                    gpui_kit::img(thumbnail)
                        .object_fit(ObjectFit::Cover)
                        .size_full()
                        .rounded(px(inner_corner_radius(18.0, 1.0))),
                ),
            );
        }

        // 3. Scrim so the metadata stays legible over bright photos.
        card = card.child(
            div()
                .absolute()
                .size_full()
                .rounded(px(inner_corner_radius(18.0, 1.0)))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(0x090d13cc), 0.),
                    linear_color_stop(rgba(0x090d13f7), 1.),
                )),
        );

        let image_layer = |image: Arc<RenderImage>| {
            div().absolute().size_full().child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full()
                    .rounded(px(inner_corner_radius(12.0, 1.0))),
            )
        };
        let mut preview_frame = div()
            .relative()
            .w_full()
            .flex_shrink_0()
            // The spec fixes the preview height (168 / 118 short); the image
            // is cropped to cover the 16:10 frame.
            .h(px(preview_height))
            .overflow_hidden()
            .rounded(px(12.))
            .border_1()
            .border_color(rgba(0xffffff1f))
            .bg(rgb(current_color))
            .shadow(vec![
                BoxShadow::new(px(0.), px(16.), rgba(0x000000a6).into()).blur_radius(px(32.)),
            ]);
        if transitioning {
            if let Some(previous) = previous_preview.as_ref() {
                preview_frame = preview_frame.child(image_layer(previous.clone()).with_animation(
                    ("inspector-preview-old", selection_epoch),
                    fade(),
                    move |el, t| el.opacity(1.0 - t),
                ));
            }
            if let Some(preview) = current_preview.as_ref() {
                preview_frame = preview_frame.child(image_layer(preview.clone()).with_animation(
                    ("inspector-preview-new", selection_epoch),
                    fade(),
                    move |el, t| el.opacity(t),
                ));
            }
        } else if let Some(preview) = current_preview.as_ref() {
            preview_frame = preview_frame.child(image_layer(preview.clone()));
        }
        if current_preview.is_none() {
            let label = preview_placeholder_label(self.preview_queue.pending_for(id));
            preview_frame = preview_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label),
            );
        }
        let preview_bounds = self.inspector_preview_bounds.clone();
        let preview_frame_epoch = self.render_epoch;
        preview_frame = preview_frame.child(
            canvas(
                move |bounds, _, _| {
                    *preview_bounds.borrow_mut() = Some((preview_frame_epoch, bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );

        let header = h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(10.))
                    .font_semibold()
                    .text_color(rgb(MUTED_2))
                    .child("選択中"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(MUTED_2))
                    .child("Space で開く"),
            );

        let title = v_flex()
            .min_w_0()
            .gap(px(3.))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(world.clone()),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(MUTED_2))
                    .truncate()
                    .child(photo.captured_at_local.clone()),
            );

        let mut badges = h_flex().flex_wrap().gap(px(5.));
        if photo.world_name.is_some() {
            badges = badges.child(self.badge("文脈あり", BLUE, 0x14283d));
        }
        if photo.is_favorite {
            badges = badges.child(self.badge("★ お気に入り", AMBER, 0x2a2111));
        }
        if compressed {
            badges = badges.child(self.badge("圧縮済", GREEN, 0x102a24));
        } else {
            badges = badges.child(self.badge("未圧縮", AMBER, 0x2a2111));
        }

        let meta = v_flex()
            .gap(px(4.))
            .child(self.meta_row("ワールド", world))
            .child(self.meta_row(
                "インスタンス",
                photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row(
                "解像度",
                format!(
                    "{} × {}",
                    photo.effective_dimensions().0,
                    photo.effective_dimensions().1
                ),
            ))
            .child(self.meta_row("サイズ", format_bytes(photo.effective_file_size())))
            .child(self.meta_row(
                "取得元",
                photo.link_method.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row(
                "撮影者",
                photo.author.clone().unwrap_or_else(|| "—".to_string()),
            ));

        let players_block = v_flex()
            .min_w_0()
            .gap(px(6.))
            .child(section("撮影時のプレイヤー"))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .when(players.is_empty(), |this| this.child(pill("—".to_string())))
                    .children(players.iter().map(|name| pill(name.clone()))),
            );

        let tags_block = v_flex().min_w_0().gap(px(6.)).child(section("タグ")).child(
            h_flex()
                .flex_wrap()
                .gap(px(4.))
                .when(tags.is_empty(), |this| this.child(pill("—".to_string())))
                .children(tags.iter().map(|tag| pill(format!("#{tag}")))),
        );

        let body = v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .gap(px(12.))
            .child(preview_frame)
            .child(title)
            .child(badges)
            .child(meta)
            .child(players_block)
            .child(tags_block);

        let actions = v_flex()
            .flex_shrink_0()
            .gap(px(8.))
            .child(
                Button::new("open-viewer")
                    .label("全画面表示")
                    .small()
                    .primary()
                    .w_full()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.load_selected_meta(id);
                        this.open_quick_look(window, cx, ViewerOpenSource::Inspector);
                    })),
            )
            .child({
                let compress_ids = if self.selected_ids.is_empty() {
                    vec![id]
                } else {
                    self.selected_ids.clone()
                };
                let label = if compress_ids.len() > 1 {
                    format!("{} 件を圧縮", compress_ids.len())
                } else {
                    "圧縮".to_string()
                };
                Button::new("compress-selected")
                    .label(label)
                    .small()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_manual_compression(compress_ids.clone(), cx)
                    }))
            });

        card = card.child(
            v_flex()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .p(px(12.))
                .gap(px(12.))
                .child(header)
                .child(body)
                .child(actions),
        );

        v_flex()
            .w(px(column_width))
            .h_full()
            .flex_shrink_0()
            .pr(px(INSPECTOR_MARGIN))
            .child(card.w(px(card_width)).rounded(px(17.)))
            .into_any_element()
    }

    /// Metadata and actions for one photo. Shared by the full-screen viewer.
    pub(crate) fn detail_body(
        &mut self,
        photo: &Photo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = photo.id;
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        let players = self
            .selected_players
            .iter()
            .map(|player| player.display_name.clone())
            .collect::<Vec<_>>();
        let tags = self.selected_tags.clone();
        let derivatives = self.selected_derivatives.clone();
        let favorite = photo.is_favorite;
        let note = photo.note.clone();
        let note_for_edit = note.clone();
        let armed = self.delete_armed == Some(id);
        let section = |label: &'static str| {
            div()
                .text_size(px(10.))
                .font_semibold()
                .text_color(rgb(MUTED_2))
                .child(label)
        };
        let pill = |text: String| {
            div()
                .px(px(7.))
                .py(px(2.))
                .rounded_full()
                .bg(rgba(0xffffff0f))
                .border_1()
                .border_color(rgba(0xffffff12))
                .text_size(px(10.))
                .text_color(rgb(MUTED))
                .child(text)
        };

        let favorite_button = self.focusable_text_button(
            "viewer-favorite",
            if favorite {
                "★ お気に入り"
            } else {
                "☆ お気に入り"
            },
            self.quick_look_favorite_focus.clone(),
            ButtonState {
                tone: if favorite {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: true,
            },
            cx,
            move |this, _, cx| {
                match this.db.set_favorite(id, !favorite) {
                    Ok(()) => this.load_selected_meta(id),
                    Err(error) => {
                        tracing::warn!(%error, photo_id = id, "cannot update favorite");
                        this.message = Some(format!("お気に入りを更新できません: {error}"));
                    }
                }
                cx.notify();
            },
        );
        let add_tag_button = self.focusable_text_button(
            "viewer-add-tag",
            "追加",
            self.quick_look_add_tag_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            move |this, _, cx| {
                let tag = this.tag_edit_input.read(cx).value().trim().to_string();
                if tag.is_empty() {
                    return;
                }
                this.add_tag(id, tag, cx);
            },
        );
        let compress_button = self.focusable_text_button(
            "viewer-compress",
            "圧縮",
            self.quick_look_compress_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            move |this, _, cx| this.open_manual_compression(vec![id], cx),
        );
        let trash_button = self.focusable_text_button(
            "viewer-trash",
            if armed {
                "本当にゴミ箱へ移動"
            } else {
                "ゴミ箱へ移動"
            },
            self.quick_look_trash_focus.clone(),
            ButtonState {
                tone: if armed {
                    ButtonTone::Danger
                } else {
                    ButtonTone::Neutral
                },
                enabled: true,
            },
            cx,
            move |this, window, cx| {
                this.trash_selected(cx);
                if this.selected.is_none() {
                    this.close_quick_look(window, cx);
                }
            },
        );
        let note_edit_button = self.focusable_text_button(
            "viewer-note-edit",
            "編集",
            self.quick_look_note_edit_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            move |this, window, cx| this.begin_note_edit(id, note_for_edit.clone(), window, cx),
        );
        let note_save_button = self.focusable_text_button(
            "viewer-note-save",
            "保存",
            self.quick_look_note_save_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            move |this, window, cx| this.save_note(id, window, cx),
        );
        let note_cancel_button = self.focusable_text_button(
            "viewer-note-cancel",
            "キャンセル",
            self.quick_look_note_cancel_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            |this, window, cx| this.cancel_note_edit(window, cx),
        );

        v_flex()
            .w_full()
            .flex_shrink_0()
            .gap(px(12.))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(24.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(world.clone()),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.))
                            .font_family("monospace")
                            .text_color(rgb(MUTED_2))
                            .truncate()
                            .child(photo.captured_at_local.clone()),
                    )
                    .child(favorite_button),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(self.meta_row("ワールド", world))
                    .child(self.meta_row(
                        "インスタンス",
                        photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(
                        "取得元",
                        photo.link_method.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(
                        "撮影者",
                        photo.author.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row("ファイル", photo.file_name.clone()))
                    .child(self.meta_row(
                        "解像度",
                        format!(
                            "{} × {}",
                            photo.effective_dimensions().0,
                            photo.effective_dimensions().1
                        ),
                    ))
                    .child(self.meta_row("サイズ", format_bytes(photo.effective_file_size()))),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(6.))
                    .child(section("撮影時のプレイヤー"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(4.))
                            .when(players.is_empty(), |this| this.child(pill("—".to_string())))
                            .children(players.iter().map(|name| pill(name.clone()))),
                    ),
            )
            .child(
                v_flex().min_w_0().gap(px(6.)).child(section("タグ")).child(
                    h_flex()
                        .flex_wrap()
                        .gap(px(4.))
                        .when(tags.is_empty(), |this| this.child(pill("—".to_string())))
                        .children(
                            tags.iter()
                                .map(|tag| self.tag_chip(id, tag.clone(), cx))
                                .collect::<Vec<_>>(),
                        ),
                ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(Input::new(&self.tag_edit_input).flex_1())
                    .child(add_tag_button),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section("圧縮履歴"))
                    .when(derivatives.is_empty(), |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("まだ圧縮されていません"),
                        )
                    })
                    .children(derivatives.iter().map(|derivative| {
                        h_flex()
                            .gap(px(8.))
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(derivative.format.to_uppercase())
                            .child(format_bytes(derivative.file_size))
                            .child(div().min_w_0().truncate().child(derivative.path.clone()))
                    })),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(section("メモ"))
                            .when(self.note_edit_id != Some(id), |this| {
                                this.child(note_edit_button)
                            }),
                    )
                    .when(self.note_edit_id == Some(id), |this| {
                        this.child(Textarea::new(&self.note_edit_input).w_full())
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(note_save_button)
                                    .child(note_cancel_button)
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED_2))
                                            .child("Esc でキャンセル"),
                                    ),
                            )
                    })
                    .when(self.note_edit_id != Some(id), |this| {
                        this.child(
                            div()
                                .p(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(PANEL))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(
                                    note.clone()
                                        .unwrap_or_else(|| "メモはまだありません".to_string()),
                                ),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(compress_button)
                    .child(trash_button),
            )
    }
}
/// Stands in for a photo whose page has not loaded yet. Matches the card's
/// current image height so the uniform row never changes height.
fn grid_placeholder(image_height: f32) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h(px(image_height))
        .rounded(px(13.))
        .bg(rgb(PANEL_DEEP))
        .border_1()
        .border_color(rgba(0xffffff16))
        .into_any_element()
}
