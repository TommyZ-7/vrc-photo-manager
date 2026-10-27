//! Pure year-rail maths: year buckets, rail geometry, and hit testing.
//!
//! The GPUI `YearRail` element and its `YearRailLabels` paint cache live in
//! [`crate::ui::year_rail`]; this module owns only the values and helpers they
//! share.

use std::collections::HashMap;

use gpui_kit::{Bounds, Pixels, Point, TextRun, font, point, px, rgb, size};
use vrc_photo_core::model::PhotoIndexEntry;

pub(crate) const RAIL_TOP_PAD: f32 = 24.0;
pub(crate) const RAIL_TRACK_INSET: f32 = 6.0;
pub(crate) const RAIL_LABEL_GAP: f32 = 16.0;
pub(crate) const RAIL_PILL_HEIGHT: f32 = 18.0;
pub(crate) const RAIL_PILL_WIDTH: f32 = 80.0;
pub(crate) const RAIL_PILL_RIGHT_MARGIN: f32 = 4.0;
pub(crate) const RAIL_MARKER_SIZE: f32 = 10.0;
pub(crate) const RAIL_HIT_WIDTH: f32 = 12.0;

/// Contiguous run of photos that share a local capture year.
///
/// `display_row` is the bucket's cumulative position in the rail, which stays
/// proportional to the number of photos per year even when the underlying
/// entries interleave years. `start_row` keeps the first entry index so a
/// label click lands on a real photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct YearBucket {
    pub(crate) year: i32,
    pub(crate) start_row: usize,
    pub(crate) display_row: usize,
    pub(crate) count: usize,
}

pub(crate) fn build_year_buckets(entries: &[PhotoIndexEntry]) -> Vec<YearBucket> {
    let mut buckets: Vec<YearBucket> = Vec::new();
    let mut positions: HashMap<i32, usize> = HashMap::new();
    for (row, entry) in entries.iter().enumerate() {
        let year = entry.captured_month / 100;
        // Unknown capture times stay in the index, but a `0` bucket would
        // paint a bogus "0" label and dot at the top of the rail.
        if year <= 0 {
            continue;
        }
        match positions.get(&year) {
            Some(&index) => buckets[index].count += 1,
            None => {
                positions.insert(year, buckets.len());
                buckets.push(YearBucket {
                    year,
                    start_row: row,
                    display_row: 0,
                    count: 1,
                });
            }
        }
    }
    let mut display_row = 0;
    for bucket in &mut buckets {
        bucket.display_row = display_row;
        display_row += bucket.count;
    }
    buckets
}

/// Y offset of a photo row on the rail track, including the top pad.
pub(crate) fn rail_y(display_row: usize, total: usize, height: f32) -> f32 {
    let usable = (height - 2.0 * RAIL_TOP_PAD).max(0.0);
    if total == 0 {
        return RAIL_TOP_PAD;
    }
    let fraction = (display_row as f32 / total as f32).clamp(0.0, 1.0);
    (RAIL_TOP_PAD + fraction * usable).clamp(RAIL_TOP_PAD, RAIL_TOP_PAD + usable)
}

/// Inverse of [`rail_y`]: the row nearest to a y offset inside the rail.
pub(crate) fn rail_row_at(y: f32, total: usize, height: f32) -> usize {
    if total <= 1 {
        return 0;
    }
    let usable = (height - 2.0 * RAIL_TOP_PAD).max(0.0);
    let fraction = if usable <= f32::EPSILON {
        0.0
    } else {
        ((y - RAIL_TOP_PAD) / usable).clamp(0.0, 1.0)
    };
    ((fraction * (total - 1) as f32).round() as usize).min(total - 1)
}

/// Formats a local `YYYYMM` capture month for the rail pill.
pub(crate) fn format_captured_month(month: i32) -> String {
    if month <= 0 {
        return "不明".to_string();
    }
    format!("{}年{}月", month / 100, month % 100)
}

/// Visits the year labels that fit without overlapping, in bucket order.
///
/// Every bucket still gets a dot; only labels keep a 16 px gap so a long
/// library does not paint a solid block of years.
pub(crate) fn for_each_visible_year(
    buckets: &[YearBucket],
    height: f32,
    mut visit: impl FnMut(usize, &YearBucket, f32),
) {
    let total = buckets
        .last()
        .map(|bucket| bucket.display_row + bucket.count)
        .unwrap_or(0);
    let mut last_label_y: Option<f32> = None;
    for (index, bucket) in buckets.iter().enumerate() {
        let y = rail_y(bucket.display_row, total, height);
        if last_label_y.is_none_or(|last| y - last >= RAIL_LABEL_GAP) {
            last_label_y = Some(y);
            visit(index, bucket, y);
        }
    }
}

pub(crate) fn visible_year_buckets(buckets: &[YearBucket], height: f32) -> Vec<usize> {
    let mut visible = Vec::new();
    for_each_visible_year(buckets, height, |index, _, _| visible.push(index));
    visible
}

/// The floating month pill already contains the year, so a year label it
/// covers would only show through as a duplicate at rest.
pub(crate) fn rail_label_hidden_by_pill(label_y: f32, pill_y: f32) -> bool {
    (label_y - pill_y).abs() < RAIL_PILL_HEIGHT / 2.0 + 6.0
}

/// Hit strip centered on the 2 px rail track.
pub(crate) fn rail_track_hit_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let track_x = bounds.right() - px(RAIL_TRACK_INSET);
    let usable = (f32::from(bounds.size.height) - 2.0 * RAIL_TOP_PAD).max(0.0);
    Bounds::new(
        point(
            track_x - px(RAIL_HIT_WIDTH / 2.0),
            bounds.top() + px(RAIL_TOP_PAD),
        ),
        size(px(RAIL_HIT_WIDTH), px(usable)),
    )
}

/// The expanded month pill, right-aligned inside the rail.
pub(crate) fn rail_pill_bounds(bounds: Bounds<Pixels>, pill_y: f32) -> Bounds<Pixels> {
    let center_y = bounds.top() + px(pill_y);
    Bounds::new(
        point(
            bounds.right() - px(RAIL_PILL_WIDTH + RAIL_PILL_RIGHT_MARGIN),
            center_y - px(RAIL_PILL_HEIGHT / 2.0),
        ),
        size(px(RAIL_PILL_WIDTH), px(RAIL_PILL_HEIGHT)),
    )
}

/// The compact black marker shown at rest, centered on the track.
pub(crate) fn rail_marker_bounds(bounds: Bounds<Pixels>, pill_y: f32) -> Bounds<Pixels> {
    let center = point(
        bounds.right() - px(RAIL_TRACK_INSET),
        bounds.top() + px(pill_y),
    );
    Bounds::new(
        point(
            center.x - px(RAIL_MARKER_SIZE / 2.0),
            center.y - px(RAIL_MARKER_SIZE / 2.0),
        ),
        size(px(RAIL_MARKER_SIZE), px(RAIL_MARKER_SIZE)),
    )
}

/// A press may start a rail drag only on the track or, while expanded, the pill.
pub(crate) fn rail_press_hits(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    pill_y: f32,
    expanded: bool,
) -> bool {
    rail_track_hit_bounds(bounds).contains(&position)
        || (expanded && rail_pill_bounds(bounds, pill_y).contains(&position))
}

/// A press must be on the narrow track/pill surfaces and one of their
/// hitboxes must be hovered. The hover gate keeps clicks from passing
/// through an occluding overlay (Quick Look) into the hidden grid.
pub(crate) fn rail_press_allowed(
    interactive_hovered: bool,
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    pill_y: f32,
    expanded: bool,
) -> bool {
    interactive_hovered && rail_press_hits(position, bounds, pill_y, expanded)
}

/// Hover follows the narrow gutter, and while expanded, the pill that
/// reaches outside it so the pill stays open while the user grabs it.
pub(crate) fn rail_hover_hits(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    pill_y: f32,
    expanded: bool,
) -> bool {
    bounds.contains(&position) || (expanded && rail_pill_bounds(bounds, pill_y).contains(&position))
}

/// Scroll offset that puts `photo_row` at the top of the grid, clamped to the
/// list's scrollable range.
///
/// `max_offset` is the non-negative extent reported by
/// `ScrollHandle::max_offset()`; the actual offset is zero or negative.
pub(crate) fn rail_scroll_offset(
    photo_row: usize,
    columns: usize,
    row_height: f32,
    max_offset: f32,
) -> f32 {
    let grid_row = photo_row / columns.max(1);
    (-(grid_row as f32 * row_height)).clamp(-max_offset.max(0.0), 0.0)
}

pub(crate) fn rail_text_run(len: usize, color: u32) -> TextRun {
    TextRun {
        len,
        font: font("Noto Sans JP"),
        color: rgb(color).into(),
        ..TextRun::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::photo_browser::{PhotoIndex, anchor_photo_id};
    use crate::state::test_support::index_entry;
    use crate::ui::theme::{PHOTO_ROW_HEIGHT, RAIL_WIDTH};
    use gpui_kit::{Bounds, point, px, size};

    #[test]
    fn year_buckets_merge_interleaved_local_years() {
        let entries = vec![
            index_entry(1, 202601),
            index_entry(2, 202512),
            index_entry(3, 202601),
            index_entry(4, 202511),
        ];
        let years = build_year_buckets(&entries);
        assert_eq!(
            years
                .iter()
                .map(|bucket| (
                    bucket.year,
                    bucket.start_row,
                    bucket.display_row,
                    bucket.count
                ))
                .collect::<Vec<_>>(),
            vec![(2026, 0, 0, 2), (2025, 1, 2, 2)]
        );
    }

    #[test]
    fn year_buckets_skip_unknown_capture_months() {
        let entries = vec![
            index_entry(1, 0),
            index_entry(2, 202601),
            index_entry(3, 0),
            index_entry(4, 202512),
        ];
        let index = PhotoIndex::new(entries);
        // Unknown capture times stay in the ordered index for the grid...
        assert_eq!(index.total, 4);
        // ...but they must not paint a "0" year label or dot.
        assert_eq!(
            index
                .years
                .iter()
                .map(|bucket| (bucket.year, bucket.start_row, bucket.count))
                .collect::<Vec<_>>(),
            vec![(2026, 1, 1), (2025, 3, 1)]
        );
    }

    #[test]
    fn rail_position_round_trips_within_one_pixel() {
        let height = 600.0;
        for row in [0, 1, 99, 50_000, 99_999] {
            let y = rail_y(row, 100_000, height);
            let back = rail_row_at(y, 100_000, height);
            let y2 = rail_y(back, 100_000, height);
            assert!((y - y2).abs() <= 1.0, "{y} != {y2}");
        }
        assert_eq!(rail_row_at(-100.0, 100, height), 0);
        assert_eq!(rail_row_at(1000.0, 100, height), 99);
    }

    #[test]
    fn rail_month_format_is_japanese() {
        assert_eq!(format_captured_month(202504), "2025年4月");
        assert_eq!(format_captured_month(202612), "2026年12月");
        assert_eq!(format_captured_month(0), "不明");
        assert_eq!(format_captured_month(-202601), "不明");
    }

    #[test]
    fn month_pill_hides_the_label_it_covers() {
        // The pill is 18 px tall; labels inside its vertical span would be
        // hidden behind it, so they are skipped instead.
        assert!(rail_label_hidden_by_pill(24.0, 24.0));
        assert!(rail_label_hidden_by_pill(32.0, 24.0));
        assert!(!rail_label_hidden_by_pill(48.0, 24.0));
    }

    #[test]
    fn rail_hides_labels_that_would_overlap() {
        let buckets: Vec<YearBucket> = (0..30)
            .map(|index| YearBucket {
                year: 2026 - index,
                start_row: index as usize * 100,
                display_row: index as usize * 100,
                count: 100,
            })
            .collect();
        let visible = visible_year_buckets(&buckets, 120.0);
        assert!(visible.len() < buckets.len());
        assert!(visible.windows(2).all(|window| {
            let a = rail_y(buckets[window[0]].display_row, 3000, 120.0);
            let b = rail_y(buckets[window[1]].display_row, 3000, 120.0);
            b - a >= 16.0
        }));
    }

    #[test]
    fn rail_jump_offset_uses_grid_rows_and_clamps_to_the_list() {
        // `ScrollHandle::max_offset()` is a non-negative scrollable extent.
        assert_eq!(rail_scroll_offset(0, 3, PHOTO_ROW_HEIGHT, 10_000.0), 0.0);
        assert_eq!(rail_scroll_offset(2, 3, PHOTO_ROW_HEIGHT, 10_000.0), 0.0);
        assert_eq!(rail_scroll_offset(3, 3, PHOTO_ROW_HEIGHT, 10_000.0), -196.0);
        assert_eq!(rail_scroll_offset(5, 3, PHOTO_ROW_HEIGHT, 10_000.0), -196.0);
        // The target (-2156) is clamped to the 1000 px extent.
        assert_eq!(
            rail_scroll_offset(11, 1, PHOTO_ROW_HEIGHT, 1_000.0),
            -1_000.0
        );
        assert_eq!(
            rail_scroll_offset(600, 3, PHOTO_ROW_HEIGHT, 10_000.0),
            -10_000.0
        );
        assert_eq!(
            rail_scroll_offset(600, 0, PHOTO_ROW_HEIGHT, 10_000.0),
            -10_000.0
        );
        // No scrollable extent means no movement, even for a negative extent
        // rather than the real non-negative contract.
        assert_eq!(rail_scroll_offset(600, 3, PHOTO_ROW_HEIGHT, 0.0), 0.0);
        assert_eq!(rail_scroll_offset(600, 3, PHOTO_ROW_HEIGHT, -5.0), 0.0);
    }

    #[test]
    fn rail_geometry_limits_presses_to_track_and_expanded_pill() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
        let pill_y = 300.0;

        // Track strip: x 18..30, y 44..596 for this 20 px gutter.
        assert!(rail_press_hits(
            point(px(24.), px(100.)),
            bounds,
            pill_y,
            false
        ));
        // Expanded pill: x -54..26, y 311..329. It deliberately reaches
        // outside the narrow gutter and over the cards.
        assert!(rail_press_hits(
            point(px(0.), px(320.)),
            bounds,
            pill_y,
            true
        ));
        // The same point is inert while the pill is collapsed.
        assert!(!rail_press_hits(
            point(px(0.), px(320.)),
            bounds,
            pill_y,
            false
        ));
        // Left of the track stays inert while the pill is collapsed.
        assert!(!rail_press_hits(
            point(px(15.), px(320.)),
            bounds,
            pill_y,
            false
        ));
        // Top and bottom padding of the track are outside the interactive strip.
        assert!(!rail_press_hits(
            point(px(24.), px(30.)),
            bounds,
            pill_y,
            true
        ));
        assert!(!rail_press_hits(
            point(px(24.), px(610.)),
            bounds,
            pill_y,
            true
        ));
    }

    #[test]
    fn rail_press_requires_a_hovered_gutter() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
        let pill_y = 300.0;
        let track = point(px(24.), px(100.));
        let pill = point(px(0.), px(320.));

        assert!(!rail_press_allowed(false, track, bounds, pill_y, false));
        assert!(rail_press_allowed(true, track, bounds, pill_y, false));
        assert!(!rail_press_allowed(false, pill, bounds, pill_y, true));
        assert!(rail_press_allowed(true, pill, bounds, pill_y, true));
        assert!(!rail_press_allowed(
            true,
            point(px(15.), px(320.)),
            bounds,
            pill_y,
            false
        ));
    }

    #[test]
    fn rail_hover_stays_on_the_expanded_pill() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
        let pill_y = 300.0;

        assert!(rail_hover_hits(
            point(px(24.), px(100.)),
            bounds,
            pill_y,
            false
        ));
        assert!(!rail_hover_hits(
            point(px(0.), px(320.)),
            bounds,
            pill_y,
            false
        ));
        assert!(rail_hover_hits(
            point(px(0.), px(320.)),
            bounds,
            pill_y,
            true
        ));
        assert!(!rail_hover_hits(
            point(px(0.), px(100.)),
            bounds,
            pill_y,
            true
        ));
    }

    #[test]
    fn rail_track_sits_right_of_the_gutter_center() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
        let track = rail_track_hit_bounds(bounds);
        assert!(track.center().x > bounds.center().x);
        assert_eq!(
            f32::from(bounds.right() - track.center().x),
            RAIL_TRACK_INSET
        );
    }

    #[test]
    fn rail_marker_sits_on_the_track_at_the_pill_center() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
        let pill_y = 300.0;
        let marker = rail_marker_bounds(bounds, pill_y);
        let pill = rail_pill_bounds(bounds, pill_y);

        assert_eq!(f32::from(marker.size.width), RAIL_MARKER_SIZE);
        assert_eq!(f32::from(marker.size.height), RAIL_MARKER_SIZE);
        assert_eq!(f32::from(pill.size.width), RAIL_PILL_WIDTH);
        assert_eq!(marker.center().y, pill.center().y);
        assert!(rail_track_hit_bounds(bounds).contains(&marker.center()));
    }

    #[test]
    fn anchor_and_rail_helpers_accept_a_custom_row_height() {
        let index = PhotoIndex::new(vec![
            index_entry(1, 202601),
            index_entry(2, 202601),
            index_entry(3, 202601),
        ]);
        assert_eq!(anchor_photo_id(&index, 153.0, 2, 152.0), Some(3));
        assert_eq!(rail_scroll_offset(3, 3, 152.0, 10_000.0), -152.0);
    }
}
