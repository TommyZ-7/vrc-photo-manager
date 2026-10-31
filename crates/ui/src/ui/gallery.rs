//! Photo grid as a child entity.
//!
//! GPUI's scroll handler notifies the view that rendered the list. Rendering
//! the grid from `App` made every wheel event re-render the sidebar, chrome,
//! inspector, and the whole grid. This entity owns the scrollable list, the
//! gallery header, and the year rail, so a scroll only re-renders this view.
//!
//! The index, page cache, selection, and thumbnails stay shared with `App`:
//! `PhotoBrowser` is a `Rc<RefCell<_>>`, and page/thumbnail loads are deferred
//! back to `App` (a page boundary is rare and may re-render App; plain
//! scrolling never does).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::model::Photo;

use crate::app::{App, CardBoundsMap, Screen, retain_recent_bounds};
use crate::state::photo_browser::{
    PhotoBrowser, anchor_photo_id, first_visible_photo, list_scroll_top, row_for_photo,
    visible_month,
};
use crate::state::thumbnail::ThumbnailCache;
use crate::state::viewer::ViewerOpenSource;
use crate::state::year_rail::format_captured_month;
use crate::ui::components::glass_panel;
use crate::ui::theme::*;
use crate::ui::year_rail::{YearRail, YearRailLabels};

pub(crate) struct GalleryShared {
    pub(crate) app: WeakEntity<App>,
    pub(crate) browser: Rc<RefCell<PhotoBrowser>>,
    pub(crate) thumbnails: Rc<RefCell<ThumbnailCache>>,
    pub(crate) bounds: CardBoundsMap,
    /// Frame counter for card bounds. Updated here instead of App's render
    /// epoch because scrolling repaints cards without re-rendering App.
    pub(crate) bounds_epoch: Rc<Cell<u64>>,
    pub(crate) search: bool,
}

pub(crate) struct GalleryView {
    shared: Rc<GalleryShared>,
    _observe: Subscription,
}

impl GalleryView {
    pub(crate) fn new(
        app: Entity<App>,
        search: bool,
        browser: Rc<RefCell<PhotoBrowser>>,
        thumbnails: Rc<RefCell<ThumbnailCache>>,
        bounds: CardBoundsMap,
        bounds_epoch: Rc<Cell<u64>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let shared = Rc::new(GalleryShared {
            app: app.downgrade(),
            browser,
            thumbnails,
            bounds,
            bounds_epoch,
            search,
        });
        let observe = cx.observe(&app, |_, _, cx| cx.notify());
        Self {
            shared,
            _observe: observe,
        }
    }
}

impl Render for GalleryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app_entity) = self.shared.app.upgrade() else {
            return div().into_any_element();
        };
        let app = app_entity.read(cx);
        let frame = self.shared.bounds_epoch.get().wrapping_add(1);
        self.shared.bounds_epoch.set(frame);
        retain_recent_bounds(&mut self.shared.bounds.borrow_mut(), frame);

        if self.shared.search {
            render_grid(app, &self.shared, frame, window)
        } else {
            let header = render_header(app, &self.shared, window);
            let grid = render_grid(app, &self.shared, frame, window);
            render_panel(app, header, grid)
        }
    }
}

fn render_panel(app: &App, header: AnyElement, grid: AnyElement) -> AnyElement {
    let p = app.palette;
    v_flex()
        .relative()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .h_full()
        .overflow_hidden()
        .rounded(px(17.))
        .bg(rgb(p.panel_card))
        .border_1()
        .border_color(rgba(p.line_soft))
        .shadow(vec![
            BoxShadow::new(px(0.), px(18.), rgba(p.shadow).into()).blur_radius(px(44.)),
            BoxShadow::new(px(0.), px(1.), rgba(p.highlight).into())
                .inset()
                .blur_radius(px(0.)),
        ])
        .child(header)
        .child(grid)
        .into_any_element()
}

fn render_header(app: &App, shared: &Rc<GalleryShared>, window: &Window) -> AnyElement {
    let p = app.palette;
    let browser = shared.browser.borrow();
    let entries = &browser.index.entries;
    let total = entries.len();
    let client_inset = window.client_inset().map(f32::from).unwrap_or(0.0);
    let width = window_content_extent(f32::from(window.viewport_size().width), client_inset);
    let metrics = library_grid_metrics(
        width,
        app.sidebar_collapsed,
        app.inspector_collapsed,
        app.settings.ui_card_size,
        app.settings.ui_card_show_world,
        app.settings.ui_card_show_date || app.settings.ui_card_show_size,
    );
    let first_visible = first_visible_photo(
        list_scroll_top(&browser.scroll),
        metrics.columns,
        metrics.row_height,
        total,
    );
    let (month, month_count) = if total == 0 {
        ("写真なし".to_string(), 0)
    } else {
        let (month, count) = visible_month(&browser.index, first_visible);
        (format_captured_month(month), count)
    };

    h_flex()
        .flex_shrink_0()
        .h(px(GALLERY_HEADER_HEIGHT))
        .px(px(GALLERY_PAD_X))
        .gap(px(8.))
        .items_center()
        .border_b_1()
        .border_color(rgba(p.highlight))
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
                        .text_color(rgb(p.text))
                        .child(month),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .font_family("monospace")
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(format!(
                            "{}枚 / {}",
                            format_count(month_count as i64),
                            format_count(total as i64)
                        )),
                ),
        )
        .child(div().flex_1())
        .into_any_element()
}

fn render_grid(
    app: &App,
    shared: &Rc<GalleryShared>,
    frame: u64,
    window: &mut Window,
) -> AnyElement {
    let p = app.palette;
    let search = shared.search;
    let client_inset = window.client_inset().map(f32::from).unwrap_or(0.0);
    let width = window_content_extent(f32::from(window.viewport_size().width), client_inset);
    let metrics = if search {
        search_grid_metrics(
            width,
            app.sidebar_collapsed,
            app.settings.ui_card_size,
            app.settings.ui_card_show_world,
            app.settings.ui_card_show_date || app.settings.ui_card_show_size,
        )
    } else {
        library_grid_metrics(
            width,
            app.sidebar_collapsed,
            app.inspector_collapsed,
            app.settings.ui_card_size,
            app.settings.ui_card_show_world,
            app.settings.ui_card_show_date || app.settings.ui_card_show_size,
        )
    };
    let image_height = metrics.image_height;
    let card_width = Some(metrics.card_width);
    let grid_gap = GRID_GAP;

    let (index, scroll, columns, row_height, generation, labels, rail_drag, rail_hover) = {
        let mut browser = shared.browser.borrow_mut();
        let pending = browser.pending_anchor.take();
        let layout_changed = metrics.columns != browser.columns
            || (metrics.row_height - browser.row_height).abs() > 0.5;
        // A resize or pane toggle changes the metrics mid-scroll. Capture the
        // old top photo before replacing them so `uniform_list` does not keep
        // a pixel offset that now points at a different row.
        let anchor = pending.or_else(|| {
            layout_changed
                .then(|| {
                    anchor_photo_id(
                        &browser.index,
                        list_scroll_top(&browser.scroll),
                        browser.columns,
                        browser.row_height,
                    )
                })
                .flatten()
        });
        browser.columns = metrics.columns;
        browser.row_height = metrics.row_height;
        if let Some(id) = anchor {
            let row = row_for_photo(&browser.index, id, browser.columns);
            browser
                .scroll
                .scroll_to_item_strict(row, ScrollStrategy::Top);
        }
        let labels = if browser.labels.is_none() || browser.labels_generation != browser.generation
        {
            // Shaping is expensive, so do it once per index snapshot instead
            // of on every render or paint frame.
            let labels = Arc::new(YearRailLabels::build(&browser.index, window, &p));
            browser.labels = Some(labels.clone());
            browser.labels_generation = browser.generation;
            labels
        } else {
            browser.labels.clone().expect("year rail labels")
        };
        (
            browser.index.clone(),
            browser.scroll.clone(),
            browser.columns,
            browser.row_height,
            browser.generation,
            labels,
            browser.rail_drag.clone(),
            browser.rail_hover.clone(),
        )
    };
    let rows = index.total.div_ceil(columns);

    if rows == 0 {
        let empty = if search {
            if app.search_active {
                glass_panel(&p)
                    .p(px(20.))
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child("条件に一致する写真がありません")
                    .into_any_element()
            } else {
                // Before a search runs the browser mirrors the library, so an
                // empty library is not an empty result set.
                div().into_any_element()
            }
        } else if let Some(error) = app.library_error.clone() {
            div()
                .mt(px(18.))
                .max_w(px(560.))
                .p(px(20.))
                .rounded(px(18.))
                .bg(rgb(p.panel))
                .border_1()
                .border_color(rgb(p.border))
                .text_size(px(12.))
                .text_color(rgb(p.red))
                .child(error)
                .child(
                    div()
                        .pt(px(4.))
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child("リボンの「再読込」で再試行できます。"),
                )
                .into_any_element()
        } else {
            div()
                .mt(px(18.))
                .max_w(px(560.))
                .p(px(20.))
                .rounded(px(18.))
                .bg(rgb(p.panel))
                .border_1()
                .border_color(rgb(p.border))
                .text_size(px(12.))
                .text_color(rgb(p.muted))
                .child(
                    "写真がありません。VRChatの写真フォルダを設定するか、--demo でサンプルを表示できます。",
                )
                .into_any_element()
        };
        let container = v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .items_stretch()
            .pl(px(GALLERY_PAD_X))
            .pr(px(GALLERY_PAD_X))
            .pt(px(GALLERY_PAD_TOP))
            .pb(px(GALLERY_PAD_BOTTOM));
        return container.child(empty).into_any_element();
    }

    let list_index = index.clone();
    let list_shared = shared.clone();
    let list = uniform_list(
        // Keep the list ID stable so uniform_list retains its scroll offset
        // when the column count changes; the row height is constant for the
        // current layout and only changes on resize.
        SharedString::from(if search {
            "search-grid-list"
        } else {
            "photo-grid-list"
        }),
        rows,
        move |range: std::ops::Range<usize>, _window, cx| {
            let Some(app_entity) = list_shared.app.upgrade() else {
                return Vec::new();
            };
            let (rendered, missing, missing_thumbnails) = {
                let app = app_entity.read(cx);
                let mut browser = list_shared.browser.borrow_mut();
                let photo_start = range.start * columns;
                let photo_end = (range.end * columns).min(list_index.total);
                let missing = browser.plan_pages(photo_start, photo_end, 2);
                let mut missing_thumbnails = Vec::new();
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
                        match browser.photo_at(photo_index) {
                            Some(photo) => {
                                line = line.child(render_card(
                                    app,
                                    list_shared.as_ref(),
                                    frame,
                                    &photo,
                                    image_height,
                                    card_width,
                                    &mut missing_thumbnails,
                                ));
                            }
                            None => {
                                line = line.child(grid_placeholder(&p, image_height, card_width));
                            }
                        }
                    }
                    rendered.push(line);
                }
                (rendered, missing, missing_thumbnails)
            };

            if !missing.is_empty() {
                let app = list_shared.app.clone();
                cx.defer(move |cx| {
                    if let Some(app_entity) = app.upgrade() {
                        app_entity.update(cx, |app, cx| {
                            app.load_pages(search, generation, missing, cx);
                        });
                    }
                });
            }
            if !missing_thumbnails.is_empty() {
                let app = list_shared.app.clone();
                cx.defer(move |cx| {
                    if let Some(app_entity) = app.upgrade() {
                        app_entity.update(cx, |app, cx| {
                            for photo in missing_thumbnails {
                                app.request_thumbnail(photo.as_ref(), cx);
                            }
                        });
                    }
                });
            }
            // Cards read the memory cache during the render above; hand the
            // ids that crossed the throttle window to one background batch.
            let access_refreshes = list_shared.thumbnails.borrow_mut().take_access_refreshes();
            if !access_refreshes.is_empty() {
                let app = list_shared.app.clone();
                cx.defer(move |cx| {
                    if let Some(app_entity) = app.upgrade() {
                        app_entity.update(cx, |app, cx| {
                            app.refresh_thumbnail_access(access_refreshes, cx);
                        });
                    }
                });
            }
            rendered
        },
    )
    .track_scroll(&scroll)
    .flex_1()
    .w_full()
    .min_w_0()
    .min_h_0();

    // The wrapper stays mounted (and relative) so the year rail can overlay
    // the list without remounting it.
    let mut wrapper = v_flex()
        .id(if search { "search-grid" } else { "photo-grid" })
        .relative()
        .flex_1()
        .w_full()
        .min_w_0()
        .min_h_0();
    if !search {
        wrapper = wrapper
            .track_focus(&app.photo_grid_focus)
            .key_context("photo_grid")
            .tab_stop(true);
    }
    let mut grid = wrapper.child(list);
    if index.total > 0 {
        // Added after the list so the rail owns the reserved gutter instead
        // of the cards underneath.
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
                    p,
                )),
        );
    }
    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .h_full()
        .items_stretch()
        .pl(px(GALLERY_PAD_X))
        .pr(px(GALLERY_PAD_X))
        .pt(px(GALLERY_PAD_TOP))
        .pb(px(GALLERY_PAD_BOTTOM))
        .child(grid)
        .into_any_element()
}

fn render_card(
    app: &App,
    shared: &GalleryShared,
    bounds_frame: u64,
    photo: &Arc<Photo>,
    image_height: f32,
    card_width: Option<f32>,
    missing_thumbnails: &mut Vec<Arc<Photo>>,
) -> AnyElement {
    let p = app.palette;
    let id = photo.id;
    let selected = app.selected_ids.contains(id);
    let primary = app.selected == Some(id);
    let compressed = app.compressed_ids.contains(&id);
    let duplicate = app.duplicate_ids.contains(&id);
    let replaced = photo.is_replaced();
    let show_status = app.settings.ui_card_show_status;
    let show_world = app.settings.ui_card_show_world;
    let show_date = app.settings.ui_card_show_date;
    let show_size = app.settings.ui_card_show_size;
    let world = photo
        .world_name
        .clone()
        .unwrap_or_else(|| "未紐付け".to_string());
    let thumbnail = shared.thumbnails.borrow_mut().get(id);
    if thumbnail.is_none() {
        missing_thumbnails.push(photo.clone());
    }
    let color = shared
        .thumbnails
        .borrow()
        .color(id)
        .unwrap_or_else(|| p.scene_color(photo.world_name.as_deref().unwrap_or("")));
    let card_radius = 13.0;
    let image_radius = inner_corner_radius(card_radius, 1.0);
    let meta_line = card_meta_line(
        show_date,
        show_size,
        &photo.captured_at_local,
        format_bytes(photo.effective_file_size()),
    );
    let has_info = show_world || meta_line.is_some();

    let mut thumb = div()
        .relative()
        .h(px(image_height))
        .w_full()
        .overflow_hidden()
        .bg(rgb(color))
        .rounded_tl(px(image_radius))
        .rounded_tr(px(image_radius))
        .when(!has_info, |this| {
            this.rounded_bl(px(image_radius))
                .rounded_br(px(image_radius))
        });
    if let Some(image) = thumbnail {
        let image = gpui_kit::img(image)
            .object_fit(ObjectFit::Cover)
            .size_full()
            // GPUI clips overflow to a rectangle, so the image has to carry
            // the rounded silhouette itself.
            .rounded_tl(px(image_radius))
            .rounded_tr(px(image_radius))
            .when(!has_info, |this| {
                this.rounded_bl(px(image_radius))
                    .rounded_br(px(image_radius))
            });
        thumb = thumb.child(image);
    }
    let bounds_map = shared.bounds.clone();
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
            p.accent
        } else if duplicate {
            p.muted
        } else if compressed {
            p.green
        } else {
            p.amber
        };
        thumb = thumb.child(
            h_flex()
                .absolute()
                .left(px(9.))
                .bottom(px(8.))
                .px(px(6.))
                .py(px(3.))
                .rounded(px(6.))
                .bg(rgb(p.panel_deep))
                .border_1()
                .border_color(rgba(p.line_soft))
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
                .bg(rgb(p.accent_fill))
                .border_2()
                .border_color(rgb(p.bg))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .font_semibold()
                .text_color(rgb(p.on_accent))
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
                .bg(rgb(p.amber))
                .shadow(vec![
                    BoxShadow::new(px(0.), px(0.), rgba(p.shadow).into()).blur_radius(px(3.)),
                ]),
        )
    });

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
                .text_color(rgb(p.text))
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
                .text_color(rgb(p.muted_2))
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
            rgb(p.accent)
        } else if selected {
            rgb(p.accent_border)
        } else {
            rgba(p.line_soft)
        })
        .bg(rgb(p.panel_deep))
        .cursor_pointer()
        .when(!primary && !selected, |this| {
            this.hover(|this| this.border_color(rgba(with_alpha(p.text, 0x30))))
        })
        .child(thumb);
    if selected {
        card = card.shadow(vec![
            BoxShadow::new(px(0.), px(8.), rgba(with_alpha(p.accent, 0x4d)).into())
                .blur_radius(px(24.)),
        ]);
    }
    if has_info {
        card = card.child(info);
    }

    let app_down = shared.app.clone();
    let app_click = shared.app.clone();
    let card = card
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, window, cx| {
                if let Some(app_entity) = app_down.upgrade() {
                    app_entity.update(cx, |app, cx| {
                        app.handle_photo_press(id, event.modifiers, cx);
                        focus_grid_or_search(app, window, cx);
                    });
                }
            },
        )
        .on_click(move |event: &ClickEvent, window, cx| {
            if let Some(app_entity) = app_click.upgrade() {
                app_entity.update(cx, |app, cx| {
                    focus_grid_or_search(app, window, cx);
                    if let ClickEvent::Mouse(click) = event
                        && click.down.click_count >= 2
                        && !event.modifiers().modified()
                    {
                        app.open_quick_look(window, cx, ViewerOpenSource::Card);
                    }
                });
            }
        });

    // Library and Search cells use the computed fixed width so a preset never
    // stretches a card beyond the width its image was sized for.
    let cell = div().relative().min_w_0().child(card);
    let cell = match card_width {
        Some(width) => cell.w(px(width)).flex_shrink_0(),
        None => cell.flex_1(),
    };

    cell.into_any_element()
}

fn focus_grid_or_search(app: &App, window: &mut Window, cx: &mut Context<App>) {
    if app.screen == Screen::Photos {
        window.focus(&app.photo_grid_focus, cx);
    } else {
        // Search cards reuse the library card widget, but its grid is
        // Photos-only. Keep focus on a handle that is actually mounted.
        let handle = app.world_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }
}

/// Stands in for a photo whose page has not loaded yet. Matches the card's
/// current image height so the uniform row never changes height.
fn grid_placeholder(palette: &Palette, image_height: f32, card_width: Option<f32>) -> AnyElement {
    let p = palette;
    let mut placeholder = div()
        .min_w_0()
        .h(px(image_height))
        .rounded(px(13.))
        .bg(rgb(p.panel_deep))
        .border_1()
        .border_color(rgba(p.line_soft));
    placeholder = match card_width {
        Some(width) => placeholder.w(px(width)).flex_shrink_0(),
        None => placeholder.flex_1(),
    };
    placeholder.into_any_element()
}
