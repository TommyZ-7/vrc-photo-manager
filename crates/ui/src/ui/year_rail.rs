//! GPUI year-rail element painted over the right edge of a photo grid.
//!
//! The pure geometry and bucket maths live in [`crate::state::year_rail`];
//! this module owns the element, its label cache, and pointer handling.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::*;

use crate::state::photo_browser::{PhotoIndex, first_visible_photo, list_scroll_top};
use crate::state::year_rail::{
    RAIL_PILL_WIDTH, RAIL_TOP_PAD, RAIL_TRACK_INSET, for_each_visible_year, format_captured_month,
    rail_hover_hits, rail_label_hidden_by_pill, rail_marker_bounds, rail_pill_bounds,
    rail_press_allowed, rail_row_at, rail_scroll_offset, rail_text_run, rail_track_hit_bounds,
    rail_y, visible_year_buckets,
};
use crate::ui::theme::{MUTED_2, PANEL_2, TEXT};

/// Snap distance around a year label for click-to-jump on the rail.
const RAIL_YEAR_HIT_SLOP: f32 = 8.0;

/// Shaped year/month text for the rail.
///
/// Building shapes text, so it is cached per index generation on
/// [`PhotoBrowser`]; paint only reuses these lines.
pub(crate) struct YearRailLabels {
    years: Vec<ShapedLine>,
    current_years: Vec<ShapedLine>,
    months: HashMap<i32, ShapedLine>,
}

impl YearRailLabels {
    pub(crate) fn build(index: &PhotoIndex, window: &mut Window) -> Self {
        let text_system = window.text_system();
        let mut years = Vec::with_capacity(index.years.len());
        let mut current_years = Vec::with_capacity(index.years.len());
        for bucket in &index.years {
            let text = SharedString::from(bucket.year.to_string());
            years.push(text_system.shape_line(
                text.clone(),
                px(10.),
                &[rail_text_run(text.len(), MUTED_2)],
                None,
            ));
            current_years.push(text_system.shape_line(
                text.clone(),
                px(10.),
                &[rail_text_run(text.len(), TEXT)],
                None,
            ));
        }

        let mut months: HashMap<i32, ShapedLine> = HashMap::new();
        for entry in &index.entries {
            if months.contains_key(&entry.captured_month) {
                continue;
            }
            let text = SharedString::from(format_captured_month(entry.captured_month));
            months.insert(
                entry.captured_month,
                text_system.shape_line(
                    text.clone(),
                    px(10.),
                    &[rail_text_run(text.len(), TEXT)],
                    None,
                ),
            );
        }

        Self {
            years,
            current_years,
            months,
        }
    }
}

/// Google Photos-style year rail painted over the right edge of a grid.
///
/// The list offset is read from the tracked [`UniformListScrollHandle`], and
/// clicks/drags write the same handle, so the rail never touches the database.
#[derive(Clone)]
pub(crate) struct YearRail {
    id: &'static str,
    labels: Arc<YearRailLabels>,
    index: Arc<PhotoIndex>,
    scroll: UniformListScrollHandle,
    columns: usize,
    row_height: f32,
    drag_active: Rc<Cell<bool>>,
    hovered: Rc<Cell<bool>>,
}

impl YearRail {
    // The rail is assembled from per-grid state and shared cells; splitting
    // this private constructor would only move the same values into a struct.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: &'static str,
        labels: Arc<YearRailLabels>,
        index: Arc<PhotoIndex>,
        scroll: UniformListScrollHandle,
        columns: usize,
        row_height: f32,
        drag_active: Rc<Cell<bool>>,
        hovered: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            id,
            labels,
            index,
            scroll,
            columns,
            row_height,
            drag_active,
            hovered,
        }
    }

    /// Scrolls the grid to the row under `y`, snapping to a year label when
    /// the pointer is within its 8 px hit area.
    fn jump_to(&self, y: Pixels, bounds_top: Pixels, height: f32, window: &mut Window) {
        let total = self.index.total;
        if total == 0 {
            return;
        }
        let local_y: f32 = (y - bounds_top).into();
        let mut nearest: Option<(f32, usize)> = None;
        for slot in visible_year_buckets(&self.index.years, height) {
            let bucket = self.index.years[slot];
            let label_y = rail_y(bucket.display_row, total, height);
            let distance = (local_y - label_y).abs();
            if distance <= RAIL_YEAR_HIT_SLOP && nearest.is_none_or(|(best, _)| distance < best) {
                nearest = Some((distance, bucket.start_row));
            }
        }

        let photo_row = nearest
            .map(|(_, row)| row)
            .unwrap_or_else(|| rail_row_at(local_y, total, height))
            .min(total - 1);
        let max_offset = f32::from(self.scroll.0.borrow().base_handle.max_offset().y);
        let offset = rail_scroll_offset(photo_row, self.columns, self.row_height, max_offset);
        self.scroll
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(offset)));
        window.refresh();
    }
}

impl IntoElement for YearRail {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct YearRailPrepaint {
    gutter: Hitbox,
    track: Hitbox,
    pill: Option<Hitbox>,
}

impl Element for YearRail {
    type RequestLayoutState = ();
    type PrepaintState = YearRailPrepaint;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name(SharedString::from(self.id)))
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut gpui_kit::App,
    ) -> Self::PrepaintState {
        let total = self.index.total;
        let height: f32 = bounds.size.height.into();
        let first_visible = first_visible_photo(
            list_scroll_top(&self.scroll),
            self.columns.max(1),
            self.row_height,
            total,
        );
        let pill_y = rail_y(first_visible, total, height);
        let expanded = self.hovered.get() || self.drag_active.get();

        // The full gutter keeps cards from hovering underneath and lets the
        // wheel reach the list behind it. The narrow track/pill hitboxes sit
        // in front so only those surfaces look and behave interactive.
        let gutter = window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll);
        let track = window.insert_hitbox(rail_track_hit_bounds(bounds), HitboxBehavior::Normal);
        let pill = expanded.then(|| {
            window.insert_hitbox(rail_pill_bounds(bounds, pill_y), HitboxBehavior::Normal)
        });
        YearRailPrepaint {
            gutter,
            track,
            pill,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitboxes: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) {
        let total = self.index.total;
        if total == 0 {
            return;
        }
        let height: f32 = bounds.size.height.into();
        let track_x = bounds.right() - px(RAIL_TRACK_INSET);
        let usable = (height - 2.0 * RAIL_TOP_PAD).max(0.0);
        let expanded = self.hovered.get() || self.drag_active.get();

        window.set_cursor_style(CursorStyle::PointingHand, &hitboxes.track);
        if let Some(pill) = hitboxes.pill.as_ref() {
            window.set_cursor_style(CursorStyle::PointingHand, pill);
        }

        if usable > 0.0 {
            window.paint_quad(
                fill(
                    Bounds::new(
                        point(track_x - px(1.), bounds.top() + px(RAIL_TOP_PAD)),
                        size(px(2.), px(usable)),
                    ),
                    rgba(0xffffff14),
                )
                .corner_radii(px(1.)),
            );
        }

        let scroll_top = list_scroll_top(&self.scroll);
        let columns = self.columns.max(1);
        let first_visible = first_visible_photo(scroll_top, columns, self.row_height, total);
        let month = self
            .index
            .entries
            .get(first_visible)
            .map(|entry| entry.captured_month)
            .unwrap_or(0);
        let current_year = month / 100;
        let pill_y = rail_y(first_visible, total, height);

        for bucket in &self.index.years {
            let y = bounds.top() + px(rail_y(bucket.display_row, total, height));
            let color = if bucket.year == current_year {
                rgb(TEXT)
            } else {
                rgba(0xffffff30)
            };
            window.paint_quad(
                fill(
                    Bounds::new(point(track_x - px(1.5), y - px(1.5)), size(px(3.), px(3.))),
                    color,
                )
                .corner_radii(px(1.5)),
            );
        }

        let labels = self.labels.clone();
        let buckets = self.index.years.as_slice();
        for_each_visible_year(buckets, height, |slot, bucket, y| {
            // The pill sits on top of the topmost label at rest and repeats
            // the year, so skip the label instead of letting it peek out.
            if rail_label_hidden_by_pill(y, pill_y) {
                return;
            }
            let line = if bucket.year == current_year {
                labels.current_years.get(slot)
            } else {
                labels.years.get(slot)
            };
            if let Some(line) = line {
                line.paint(
                    point(
                        bounds.right() - px(RAIL_TRACK_INSET + 8.0 + 56.0),
                        bounds.top() + px(y) - px(6.),
                    ),
                    px(12.),
                    TextAlign::Right,
                    Some(px(56.)),
                    window,
                    cx,
                )
                .ok();
            }
        });

        let pill_center = bounds.top() + px(pill_y);
        if expanded {
            let pill_bounds = rail_pill_bounds(bounds, pill_y);
            window.paint_quad(fill(pill_bounds, rgba((PANEL_2 << 8) | 0xf0)).corner_radii(px(9.)));
            if let Some(line) = labels.months.get(&month) {
                line.paint(
                    point(
                        bounds.right() - px(RAIL_PILL_WIDTH - 10.),
                        pill_center - px(6.),
                    ),
                    px(12.),
                    TextAlign::Right,
                    Some(px(RAIL_PILL_WIDTH - 20.)),
                    window,
                    cx,
                )
                .ok();
            }
        } else {
            let marker = rail_marker_bounds(bounds, pill_y);
            let ring = Bounds::new(
                point(marker.origin.x - px(1.), marker.origin.y - px(1.)),
                size(marker.size.width + px(2.), marker.size.height + px(2.)),
            );
            window.paint_quad(fill(ring, rgba(0xffffff2e)).corner_radii(px(4.)));
            window.paint_quad(fill(marker, rgba(0x05070af2)).corner_radii(px(3.)));
        }

        let gutter_hitbox = hitboxes.gutter.clone();
        let pill_hitbox = hitboxes.pill.clone();
        let rail = self.clone();
        window.on_mouse_event({
            let rail = rail.clone();
            let gutter_hitbox = gutter_hitbox.clone();
            let pill_hitbox = pill_hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                let first_visible = first_visible_photo(
                    list_scroll_top(&rail.scroll),
                    rail.columns.max(1),
                    rail.row_height,
                    rail.index.total,
                );
                let pill_y = rail_y(first_visible, rail.index.total, height);
                let expanded = rail.hovered.get() || rail.drag_active.get();
                let interactive_hovered = gutter_hitbox.is_hovered(window)
                    || pill_hitbox
                        .as_ref()
                        .is_some_and(|pill| pill.is_hovered(window));
                if !rail_press_allowed(
                    interactive_hovered,
                    event.position,
                    bounds,
                    pill_y,
                    expanded,
                ) {
                    return;
                }
                // The rail is painted above the cards; keep the press from
                // reaching the photo underneath it.
                cx.stop_propagation();
                rail.hovered.set(true);
                rail.drag_active.set(true);
                rail.jump_to(event.position.y, bounds.top(), height, window);
            }
        });

        window.on_mouse_event({
            let rail = rail.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if rail.drag_active.get() {
                    if !event.dragging() {
                        // Mouse-up can be swallowed by an overlay; stop
                        // dragging as soon as the left button is no longer
                        // held.
                        let first_visible = first_visible_photo(
                            list_scroll_top(&rail.scroll),
                            rail.columns.max(1),
                            rail.row_height,
                            rail.index.total,
                        );
                        let pill_y = rail_y(first_visible, rail.index.total, height);
                        let expanded = rail.hovered.get() || rail.drag_active.get();
                        rail.drag_active.set(false);
                        rail.hovered
                            .set(rail_hover_hits(event.position, bounds, pill_y, expanded));
                        window.refresh();
                        return;
                    }
                    if !phase.bubble() {
                        return;
                    }
                    cx.stop_propagation();
                    rail.jump_to(event.position.y, bounds.top(), height, window);
                    return;
                }
                if !phase.bubble() {
                    return;
                }
                let first_visible = first_visible_photo(
                    list_scroll_top(&rail.scroll),
                    rail.columns.max(1),
                    rail.row_height,
                    rail.index.total,
                );
                let pill_y = rail_y(first_visible, rail.index.total, height);
                let expanded = rail.hovered.get() || rail.drag_active.get();
                let hovered = rail_hover_hits(event.position, bounds, pill_y, expanded);
                if hovered != rail.hovered.get() {
                    rail.hovered.set(hovered);
                    window.refresh();
                }
            }
        });

        window.on_mouse_event({
            let rail = rail;
            move |event: &MouseUpEvent, phase, window, _| {
                if !phase.bubble() {
                    return;
                }
                let first_visible = first_visible_photo(
                    list_scroll_top(&rail.scroll),
                    rail.columns.max(1),
                    rail.row_height,
                    rail.index.total,
                );
                let pill_y = rail_y(first_visible, rail.index.total, height);
                let expanded = rail.hovered.get() || rail.drag_active.get();
                let was_dragging = rail.drag_active.replace(false);
                let hovered = rail_hover_hits(event.position, bounds, pill_y, expanded);
                let hover_changed = rail.hovered.replace(hovered) != hovered;
                if was_dragging || hover_changed {
                    window.refresh();
                }
            }
        });
    }
}
