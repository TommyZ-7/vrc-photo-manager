//! Theme tokens, layout metrics, and the pure presentation helpers shared by
//! every screen.
//!
//! Keeping the values here means a screen never re-declares a colour, a width,
//! or a motion duration, and the layout maths stays testable without a window.

use std::time::{Duration, Instant};

use gpui_kit::{Bounds, Pixels, point, px, size};
use vrc_photo_core::model::{CompressionFormat, CompressionProfile};
use vrc_photo_core::settings::CardSize;

pub(crate) const BG: u32 = 0x07090d;
pub(crate) const PANEL: u32 = 0x121722;
pub(crate) const PANEL_2: u32 = 0x1a2130;
pub(crate) const SIDEBAR: u32 = 0x0d1119;
pub(crate) const BORDER: u32 = 0x232b38;
pub(crate) const BORDER_STRONG: u32 = 0x364256;
pub(crate) const TEXT: u32 = 0xeef3fb;
pub(crate) const MUTED: u32 = 0xa3adbd;
pub(crate) const MUTED_2: u32 = 0x768196;
pub(crate) const BLUE: u32 = 0x66b3ff;
pub(crate) const ACCENT_SOFT: u32 = 0x1a2f47;
pub(crate) const GREEN: u32 = 0x5eead4;
pub(crate) const AMBER: u32 = 0xfbbf24;
pub(crate) const RED: u32 = 0xfb7185;
pub(crate) const PURPLE: u32 = 0xb79cff;

pub(crate) const SIDEBAR_EXPANDED: f32 = 238.0;
pub(crate) const SIDEBAR_COLLAPSED: f32 = 78.0;
pub(crate) const MIN_CARD_WIDTH: f32 = 170.0;
pub(crate) const GRID_GAP: f32 = 16.0;
pub(crate) const INSPECTOR_WIDTH: f32 = 330.0;
pub(crate) const INSPECTOR_CARD_WIDTH: f32 = 314.0;
pub(crate) const INSPECTOR_MARGIN: f32 = 16.0;
pub(crate) const VIEWER_DETAILS_WIDTH: f32 = 380.0;
pub(crate) const VIEWER_TOP_BAR: f32 = 56.0;
pub(crate) const VIEWER_PAD: f32 = 24.0;
pub(crate) const WORKSPACE_PAD_X: f32 = 44.0;
pub(crate) const CARD_INTRO_SECS: f32 = 0.6;
pub(crate) const CARD_STAGGER_SECS: f32 = 0.04;
pub(crate) const CARD_STAGGER_MAX: usize = 8;
pub(crate) const RAIL_WIDTH: f32 = 20.0;
pub(crate) const MOTION_FAST: Duration = Duration::from_millis(160);
pub(crate) const MOTION_BASE: Duration = Duration::from_millis(220);
pub(crate) const MOTION_SLOW: Duration = Duration::from_millis(300);
pub(crate) const MOTION_MORPH: Duration = Duration::from_millis(340);
pub(crate) const MOTION_FPS: f32 = 60.0;
pub(crate) const SCREEN_SLIDE: f32 = 14.0;
pub(crate) const VIEWER_NAV_SLIDE: f32 = 28.0;

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let index = ((bytes as f64).log(1024.0).floor() as usize).min(UNITS.len() - 1);
    format!(
        "{:.1} {}",
        bytes as f64 / 1024f64.powi(index as i32),
        UNITS[index]
    )
}

pub(crate) fn format_count(value: i64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 10_000 {
        return format!("{},{:03}", value / 1_000, value % 1_000);
    }
    let tenths = ((value as f64 / 1_000.0) * 10.0).round() as i64;
    if tenths >= 1_000 {
        format!("{}k", tenths / 10)
    } else {
        format!("{}.{}k", tenths / 10, tenths % 10)
    }
}

pub(crate) fn average_bgra(bytes: &[u8]) -> u32 {
    let mut blue = 0u64;
    let mut green = 0u64;
    let mut red = 0u64;
    let mut count = 0u64;
    for pixel in bytes.as_chunks::<4>().0 {
        if pixel[3] < 8 {
            continue;
        }
        blue += u64::from(pixel[0]);
        green += u64::from(pixel[1]);
        red += u64::from(pixel[2]);
        count += 1;
    }
    if count == 0 {
        return 0;
    }
    let blue = (blue / count) as u32;
    let green = (green / count) as u32;
    let red = (red / count) as u32;
    (red << 16) | (green << 8) | blue
}

pub(crate) fn lerp_color(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let from = ((from >> shift) & 0xff) as f32;
        let to = ((to >> shift) & 0xff) as f32;
        (((from + (to - from) * t).round() as u32) & 0xff) << shift
    };
    channel(16) | channel(8) | channel(0)
}

pub(crate) fn cubic_bezier(t: f64, p1: f64, p2: f64) -> f64 {
    let mt = 1.0 - t;
    3.0 * mt * mt * t * p1 + 3.0 * mt * t * t * p2 + t * t * t
}

pub(crate) fn cubic_bezier_slope(t: f64, p1: f64, p2: f64) -> f64 {
    let mt = 1.0 - t;
    3.0 * mt * mt * p1 + 6.0 * mt * t * (p2 - p1) + 3.0 * t * t * (1.0 - p2)
}

pub(crate) fn ease_motion(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let x = f64::from(x);
    let mut guess = x;
    for _ in 0..8 {
        let curve = cubic_bezier(guess, 0.22, 0.24);
        let slope = cubic_bezier_slope(guess, 0.22, 0.24);
        if slope.abs() < 1e-9 {
            break;
        }
        guess = (guess - (curve - x) / slope).clamp(0.0, 1.0);
    }
    cubic_bezier(guess, 0.8, 1.0).clamp(0.0, 1.0) as f32
}

pub(crate) fn lerp_bounds(from: Bounds<Pixels>, to: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    let t = t.clamp(0.0, 1.0);
    let x = f32::from(from.origin.x) + (f32::from(to.origin.x) - f32::from(from.origin.x)) * t;
    let y = f32::from(from.origin.y) + (f32::from(to.origin.y) - f32::from(from.origin.y)) * t;
    let width =
        f32::from(from.size.width) + (f32::from(to.size.width) - f32::from(from.size.width)) * t;
    let height =
        f32::from(from.size.height) + (f32::from(to.size.height) - f32::from(from.size.height)) * t;
    Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
}

pub(crate) fn eased_progress(started_at: Instant, duration: Duration, now: Instant) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    let elapsed = now.saturating_duration_since(started_at).as_secs_f32();
    ease_motion((elapsed / duration.as_secs_f32()).clamp(0.0, 1.0))
}

pub(crate) fn viewer_nav_progress(started_at: Instant, now: Instant, reduce_motion: bool) -> f32 {
    if reduce_motion {
        1.0
    } else {
        eased_progress(started_at, MOTION_BASE, now)
    }
}

pub(crate) fn stagger_value(elapsed: f32, index: usize) -> f32 {
    let delay = index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS;
    let total = CARD_INTRO_SECS + delay;
    let local = ((elapsed * total - delay) / CARD_INTRO_SECS).clamp(0.0, 1.0);
    ease_motion(local)
}

pub(crate) fn next_focus_index(current: usize, len: usize, reverse: bool) -> usize {
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    if reverse {
        if current == 0 { len - 1 } else { current - 1 }
    } else {
        (current + 1) % len
    }
}

pub(crate) fn step_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let base = current.min(len - 1) as isize;
    (base.saturating_add(delta)).clamp(0, len as isize - 1) as usize
}

/// IDs between `anchor` and `target` in index order, inclusive. The anchor
/// and target must both still exist in the current snapshot.
pub(crate) fn selection_range(ids: &[i64], anchor: i64, target: i64) -> Option<Vec<i64>> {
    let anchor = ids.iter().position(|id| *id == anchor)?;
    let target = ids.iter().position(|id| *id == target)?;
    let (start, end) = if anchor <= target {
        (anchor, target)
    } else {
        (target, anchor)
    };
    Some(ids[start..=end].to_vec())
}

/// Adds `id` to the selection or removes it, returning whether it is selected
/// afterwards.
pub(crate) fn toggle_selected_id(ids: &mut Vec<i64>, id: i64) -> bool {
    if let Some(index) = ids.iter().position(|selected| *selected == id) {
        ids.remove(index);
        false
    } else {
        ids.push(id);
        true
    }
}

pub(crate) fn moved_profile_order(
    ids: &[String],
    index: usize,
    delta: isize,
) -> Option<Vec<String>> {
    if index >= ids.len() {
        return None;
    }
    let target = step_index(index, ids.len(), delta);
    if target == index {
        return None;
    }
    let mut order = ids.to_vec();
    order.swap(index, target);
    Some(order)
}

pub(crate) fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed {
        SIDEBAR_COLLAPSED
    } else {
        SIDEBAR_EXPANDED
    }
}

pub(crate) fn sidebar_reveal_width(collapsed: bool, t: f32) -> f32 {
    let (from, to) = if collapsed {
        (130.0, 0.0)
    } else {
        (0.0, 130.0)
    };
    from + (to - from) * t
}

pub(crate) fn grid_available_width(
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
) -> f32 {
    let inspector = if inspector_collapsed {
        0.0
    } else {
        INSPECTOR_WIDTH
    };
    (window_width - sidebar_width(sidebar_collapsed) - inspector - WORKSPACE_PAD_X - RAIL_WIDTH)
        .max(0.0)
}

/// Width left for the search grid after the filter panel (290 px + 26 px
/// margin), the right column's 26 px padding on both sides, and the rail
/// gutter. The Search screen has no inspector column, so none is subtracted.
pub(crate) fn search_available_width(window_width: f32, sidebar_collapsed: bool) -> f32 {
    (window_width - sidebar_width(sidebar_collapsed) - 290.0 - 26.0 - 52.0 - RAIL_WIDTH).max(0.0)
}

pub(crate) fn viewer_image_box_with_details_width(
    viewport_w: f32,
    viewport_h: f32,
    details_width: f32,
    aspect: f32,
) -> (f32, f32) {
    let avail_w = (viewport_w - details_width - VIEWER_PAD * 2.0).max(1.0);
    let avail_h = (viewport_h - VIEWER_TOP_BAR - VIEWER_PAD * 2.0).max(1.0);
    let ratio = aspect.max(0.05);
    let width = avail_w.min(avail_h * ratio);
    (width, width / ratio)
}

#[cfg(test)]
pub(crate) fn viewer_image_box(
    viewport_w: f32,
    viewport_h: f32,
    details_open: bool,
    aspect: f32,
) -> (f32, f32) {
    let details_width = if details_open {
        VIEWER_DETAILS_WIDTH
    } else {
        0.0
    };
    viewer_image_box_with_details_width(viewport_w, viewport_h, details_width, aspect)
}

pub(crate) fn centered_viewer_rect(
    viewport: gpui_kit::Size<Pixels>,
    details_width: f32,
    width: f32,
    height: f32,
) -> Bounds<Pixels> {
    let center_x = (f32::from(viewport.width) - details_width) / 2.0;
    let center_y = VIEWER_TOP_BAR + (f32::from(viewport.height) - VIEWER_TOP_BAR) / 2.0;
    Bounds::new(
        point(px(center_x - width / 2.0), px(center_y - height / 2.0)),
        size(px(width), px(height)),
    )
}

pub(crate) fn grid_columns(available_width: f32, min_card_width: f32) -> usize {
    (((available_width + GRID_GAP) / (min_card_width + GRID_GAP)).floor() as usize).max(1)
}

/// Column count for a grid screen. Search uses its own available width because
/// it has a filter panel and no inspector; the library keeps its inspector
/// aware width.
pub(crate) fn grid_columns_for(
    search: bool,
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
    min_card_width: f32,
) -> usize {
    let available = if search {
        search_available_width(window_width, sidebar_collapsed)
    } else {
        grid_available_width(window_width, sidebar_collapsed, inspector_collapsed)
    };
    grid_columns(available, min_card_width)
}

/// Radius for an image or full-bleed layer inside a rounded, bordered
/// container. GPUI's `overflow_hidden` only clips to a rectangle, so the
/// child has to paint the rounded silhouette itself. The child's bounds are
/// inset by the border, so its radius is the container radius minus the
/// border width.
pub(crate) fn inner_corner_radius(radius: f32, border_width: f32) -> f32 {
    (radius - border_width).max(0.0)
}

pub(crate) fn short_date(value: &str) -> String {
    value
        .split('.')
        .next()
        .and_then(|value| value.get(5..))
        .unwrap_or(value)
        .to_string()
}

pub(crate) fn card_meta_line(
    show_date: bool,
    show_size: bool,
    date: &str,
    size: String,
) -> Option<String> {
    match (show_date, show_size) {
        (true, true) => Some(format!("{} · {}", short_date(date), size)),
        (true, false) => Some(short_date(date)),
        (false, true) => Some(size),
        (false, false) => None,
    }
}

pub(crate) fn card_size_options() -> [(CardSize, &'static str); 4] {
    [
        (CardSize::Small, "小"),
        (CardSize::Medium, "中"),
        (CardSize::Large, "大"),
        (CardSize::ExtraLarge, "特大"),
    ]
}

pub(crate) fn card_size_label(size: CardSize) -> &'static str {
    card_size_options()
        .into_iter()
        .find_map(|(option, label)| (option == size).then_some(label))
        .unwrap_or("中")
}

pub(crate) fn scene_color(world: &str) -> u32 {
    let mut hash = 0u32;
    for byte in world.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*byte as u32);
    }
    const PALETTE: [u32; 8] = [
        0x1d2a4a, 0x3a1d2b, 0x123b3b, 0x3b2a14, 0x2a1b3d, 0x1d3320, 0x243447, 0x151a33,
    ];
    PALETTE[hash as usize % PALETTE.len()]
}

pub(crate) fn format_label(format: &CompressionFormat) -> String {
    match format {
        CompressionFormat::Jpeg => "JPEG",
        CompressionFormat::Webp => "WebP",
        CompressionFormat::Avif => "AVIF",
        CompressionFormat::JpegXl => "JPEG XL",
    }
    .to_string()
}

pub(crate) fn compression_policy(keep_original: bool) -> &'static str {
    if keep_original { "keep" } else { "replace" }
}

pub(crate) fn lossless_supported(format: &CompressionFormat) -> bool {
    !matches!(format, CompressionFormat::Jpeg)
}

pub(crate) fn lossless_hint(format: &CompressionFormat) -> &'static str {
    match format {
        CompressionFormat::Jpeg => "JPEGはロスレス非対応",
        CompressionFormat::Avif => "AVIFは近似ロスレス（品質100）",
        CompressionFormat::Webp => "WebPの可逆圧縮",
        CompressionFormat::JpegXl => "JPEG XLの可逆圧縮",
    }
}

pub(crate) fn profile_summary(profile: &CompressionProfile) -> String {
    if profile.lossless {
        format!("{} · Lossless", format_label(&profile.format))
    } else {
        format!(
            "{} · Q{} · Lossy",
            format_label(&profile.format),
            profile.quality
        )
    }
}

pub(crate) fn apply_profile_format(profile: &mut CompressionProfile, format: CompressionFormat) {
    if !lossless_supported(&format) {
        profile.lossless = false;
    }
    profile.format = format;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_widths_match_the_rail() {
        assert_eq!(sidebar_width(false), 238.0);
        assert_eq!(sidebar_width(true), 78.0);
        assert_eq!(sidebar_reveal_width(false, 0.0), 0.0);
        assert_eq!(sidebar_reveal_width(false, 1.0), 130.0);
        assert_eq!(sidebar_reveal_width(true, 0.0), 130.0);
        assert_eq!(sidebar_reveal_width(true, 1.0), 0.0);
    }

    #[test]
    fn inspector_card_and_margin_fill_the_column() {
        assert_eq!(INSPECTOR_CARD_WIDTH, 314.0);
        assert_eq!(INSPECTOR_MARGIN, 16.0);
        assert_eq!(INSPECTOR_CARD_WIDTH + INSPECTOR_MARGIN, INSPECTOR_WIDTH);
    }

    #[test]
    fn grid_columns_adapt_to_window_rail_and_inspector() {
        assert_eq!(
            grid_columns(grid_available_width(1280.0, false, false), MIN_CARD_WIDTH),
            3
        );
        assert_eq!(
            grid_columns(grid_available_width(1440.0, false, false), MIN_CARD_WIDTH),
            4
        );
        assert_eq!(
            grid_columns(grid_available_width(1280.0, true, false), MIN_CARD_WIDTH),
            4
        );
        assert_eq!(
            grid_columns(grid_available_width(1440.0, true, false), MIN_CARD_WIDTH),
            5
        );
        assert_eq!(
            grid_columns(grid_available_width(1280.0, false, true), MIN_CARD_WIDTH),
            5
        );
        assert_eq!(
            grid_columns(grid_available_width(1440.0, false, true), MIN_CARD_WIDTH),
            6
        );
        assert_eq!(
            grid_columns(grid_available_width(1080.0, false, false), MIN_CARD_WIDTH),
            2
        );
        assert_eq!(
            grid_columns(grid_available_width(1080.0, false, true), MIN_CARD_WIDTH),
            4
        );
        assert_eq!(grid_columns(0.0, MIN_CARD_WIDTH), 1);
        assert_eq!(grid_available_width(1440.0, true, false), 968.0);
        assert_eq!(search_available_width(1280.0, false), 654.0);
    }

    #[test]
    fn viewer_image_box_fits_inside_padding_and_details() {
        let close = |a: f32, b: f32| assert!((a - b).abs() < 0.01, "{a} != {b}");

        let (w, h) = viewer_image_box(1280.0, 820.0, false, 16.0 / 9.0);
        close(w, 1232.0);
        close(h, 693.0);

        let (w, h) = viewer_image_box(1280.0, 820.0, true, 16.0 / 9.0);
        close(w, 852.0);
        close(h, 479.25);

        let (w, h) = viewer_image_box(1280.0, 820.0, false, 2.0 / 3.0);
        close(w, 477.333);
        close(h, 716.0);

        let (w, h) = viewer_image_box(400.0, 400.0, false, 1.0);
        close(w, 296.0);
        close(h, 296.0);
    }

    #[test]
    fn compact_count_uses_k_for_large_libraries() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_234), "1,234");
        assert_eq!(format_count(12_842), "12.8k");
        assert_eq!(format_count(100_000), "100k");
    }

    #[test]
    fn average_bgra_ignores_transparent_pixels() {
        let bytes = [0, 0, 255, 255, 255, 0, 0, 255, 255, 255, 255, 0];
        assert_eq!(average_bgra(&bytes), 0x7f007f);
        assert_eq!(average_bgra(&[0, 0, 0, 0]), 0);
    }

    #[test]
    fn lerp_color_hits_both_ends_and_midpoint() {
        assert_eq!(lerp_color(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(lerp_color(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(lerp_color(0x000000, 0xffffff, 0.5), 0x808080);
    }

    #[test]
    fn ease_motion_is_monotonic_and_clamped() {
        assert_eq!(ease_motion(0.0), 0.0);
        assert!((ease_motion(1.0) - 1.0).abs() < 1e-4);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = ease_motion(step as f32 / 100.0);
            assert!(value >= previous - 1e-5);
            previous = value;
        }
        assert!(ease_motion(0.5) > 0.5);
    }

    #[test]
    fn lerp_bounds_hits_endpoints_and_midpoint() {
        let from = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(50.)));
        let to = Bounds::new(point(px(30.), px(60.)), size(px(200.), px(150.)));
        assert_eq!(lerp_bounds(from, to, 0.0), from);
        assert_eq!(lerp_bounds(from, to, 1.0), to);
        let middle = lerp_bounds(from, to, 0.5);
        assert_eq!(f32::from(middle.origin.x), 20.0);
        assert_eq!(f32::from(middle.origin.y), 40.0);
        assert_eq!(f32::from(middle.size.width), 150.0);
        assert_eq!(f32::from(middle.size.height), 100.0);
    }

    #[test]
    fn eased_progress_is_clamped_and_eased() {
        let start = Instant::now();
        assert_eq!(eased_progress(start, MOTION_BASE, start), 0.0);
        assert_eq!(eased_progress(start, MOTION_BASE, start + MOTION_BASE), 1.0);
        let half = eased_progress(start, MOTION_BASE, start + MOTION_BASE / 2);
        assert!(half > 0.5 && half < 1.0);
        assert_eq!(eased_progress(start, Duration::ZERO, start), 1.0);
    }

    #[test]
    fn viewer_nav_progress_respects_reduce_motion() {
        let start = Instant::now();
        assert_eq!(viewer_nav_progress(start, start, true), 1.0);
        assert_eq!(viewer_nav_progress(start, start, false), 0.0);
        assert_eq!(viewer_nav_progress(start, start + MOTION_BASE, false), 1.0);
    }

    #[test]
    fn centered_viewer_rect_uses_the_current_details_width() {
        let viewport = size(px(1200.), px(800.));
        let closed = centered_viewer_rect(viewport, 0.0, 1000.0, 600.0);
        let half = centered_viewer_rect(viewport, 190.0, 1000.0, 600.0);
        let open = centered_viewer_rect(viewport, 380.0, 1000.0, 600.0);
        assert!((f32::from(closed.center().x) - 600.0).abs() < 0.01);
        assert!((f32::from(half.center().x) - 505.0).abs() < 0.01);
        assert!((f32::from(open.center().x) - 410.0).abs() < 0.01);
    }

    #[test]
    fn motion_durations_are_ordered() {
        assert!(MOTION_FAST < MOTION_BASE);
        assert!(MOTION_BASE < MOTION_SLOW);
        assert!(MOTION_SLOW < MOTION_MORPH);
    }

    #[test]
    fn stagger_value_waits_for_the_card_delay() {
        assert_eq!(stagger_value(0.0, 0), 0.0);
        assert!((stagger_value(1.0, 0) - 1.0).abs() < 1e-4);
        let total = 0.6 + 0.32;
        assert!(stagger_value(0.32 / total, 8) < 1e-4);
        assert!((stagger_value(1.0, 8) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn focus_cycle_wraps_in_both_directions() {
        assert_eq!(next_focus_index(0, 5, false), 1);
        assert_eq!(next_focus_index(4, 5, false), 0);
        assert_eq!(next_focus_index(0, 5, true), 4);
        assert_eq!(next_focus_index(3, 5, true), 2);
        assert_eq!(next_focus_index(0, 0, false), 0);
    }

    #[test]
    fn viewer_step_clamps_at_both_ends() {
        assert_eq!(step_index(1, 5, -1), 0);
        assert_eq!(step_index(0, 5, -1), 0);
        assert_eq!(step_index(2, 5, 1), 3);
        assert_eq!(step_index(4, 5, 1), 4);
        assert_eq!(step_index(0, 0, 1), 0);
    }

    #[test]
    fn profile_move_order_swaps_neighbors_and_stops_at_edges() {
        let ids: Vec<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        assert_eq!(
            moved_profile_order(&ids, 1, -1),
            Some(vec!["b".to_string(), "a".to_string(), "c".to_string()])
        );
        assert_eq!(
            moved_profile_order(&ids, 1, 1),
            Some(vec!["a".to_string(), "c".to_string(), "b".to_string()])
        );
        assert_eq!(moved_profile_order(&ids, 0, -1), None);
        assert_eq!(moved_profile_order(&ids, 2, 1), None);
        assert_eq!(moved_profile_order(&[], 0, 1), None);
    }

    #[test]
    fn image_corner_radius_matches_the_inset_border_curve() {
        assert_eq!(inner_corner_radius(14.0, 2.0), 12.0);
        assert_eq!(inner_corner_radius(18.0, 1.0), 17.0);
        assert_eq!(inner_corner_radius(12.0, 1.0), 11.0);
        assert_eq!(inner_corner_radius(2.0, 4.0), 0.0);
    }

    #[test]
    fn grid_columns_respect_the_card_size() {
        assert!(grid_columns(1280.0, 140.0) >= grid_columns(1280.0, 280.0));
        assert_eq!(grid_columns(300.0, 280.0), 1);
        assert_eq!(grid_columns_for(true, 1280.0, false, false, 170.0), 3);
    }

    #[test]
    fn card_meta_line_joins_only_visible_parts() {
        let date = "2026-10-25 12:34:56.000";
        let size = "12.3 MB".to_string();
        assert_eq!(
            card_meta_line(true, true, date, size.clone()),
            Some("10-25 12:34:56 · 12.3 MB".to_string())
        );
        assert_eq!(
            card_meta_line(true, false, date, size.clone()),
            Some("10-25 12:34:56".to_string())
        );
        assert_eq!(
            card_meta_line(false, true, date, size.clone()),
            Some("12.3 MB".to_string())
        );
        assert_eq!(card_meta_line(false, false, date, size), None);
    }

    #[test]
    fn card_size_options_cover_every_preset_in_order() {
        let options = card_size_options();
        assert_eq!(
            options.map(|(size, _)| size),
            [
                CardSize::Small,
                CardSize::Medium,
                CardSize::Large,
                CardSize::ExtraLarge,
            ]
        );
        assert_eq!(card_size_label(CardSize::ExtraLarge), "特大");
    }

    #[test]
    fn search_available_width_drops_to_the_filter_panel_width() {
        assert_eq!(
            grid_columns(search_available_width(1280.0, false), MIN_CARD_WIDTH),
            3
        );
        assert_eq!(
            grid_columns(search_available_width(1440.0, false), MIN_CARD_WIDTH),
            4
        );
        assert_eq!(
            grid_columns(search_available_width(1080.0, false), MIN_CARD_WIDTH),
            2
        );
        assert_eq!(
            grid_columns(search_available_width(1280.0, true), MIN_CARD_WIDTH),
            4
        );
    }

    #[test]
    fn search_grid_columns_ignore_the_inspector_state() {
        // The Search screen has no inspector column, so the default expanded
        // inspector must not shrink its grid.
        assert_eq!(
            grid_columns_for(true, 1280.0, false, false, MIN_CARD_WIDTH),
            3
        );
        assert_eq!(
            grid_columns_for(true, 1440.0, false, false, MIN_CARD_WIDTH),
            4
        );
        assert_eq!(
            grid_columns_for(true, 1080.0, false, false, MIN_CARD_WIDTH),
            2
        );
        assert_eq!(
            grid_columns_for(false, 1280.0, false, false, MIN_CARD_WIDTH),
            3
        );
    }

    #[test]
    fn selection_range_includes_both_ends_in_index_order() {
        let ids = [10, 20, 30, 40];
        assert_eq!(selection_range(&ids, 20, 40), Some(vec![20, 30, 40]));
        assert_eq!(selection_range(&ids, 40, 20), Some(vec![20, 30, 40]));
        assert_eq!(selection_range(&ids, 30, 30), Some(vec![30]));
        assert_eq!(selection_range(&ids, 99, 20), None);
        assert_eq!(selection_range(&ids, 20, 99), None);
    }

    #[test]
    fn toggle_selected_id_adds_once_and_removes() {
        let mut ids = Vec::new();
        assert!(toggle_selected_id(&mut ids, 7));
        assert_eq!(ids, vec![7]);
        assert!(!toggle_selected_id(&mut ids, 7));
        assert!(ids.is_empty());
    }

    #[test]
    fn compression_policy_maps_to_stored_values() {
        assert_eq!(compression_policy(true), "keep");
        assert_eq!(compression_policy(false), "replace");
    }

    #[test]
    fn lossless_is_unsupported_for_jpeg_and_resets_on_format_change() {
        assert!(!lossless_supported(&CompressionFormat::Jpeg));
        assert!(lossless_supported(&CompressionFormat::Webp));
        assert!(lossless_supported(&CompressionFormat::Avif));
        assert!(lossless_supported(&CompressionFormat::JpegXl));

        let mut profile = CompressionProfile {
            format: CompressionFormat::Webp,
            lossless: true,
            ..Default::default()
        };
        apply_profile_format(&mut profile, CompressionFormat::Jpeg);
        assert_eq!(profile.format, CompressionFormat::Jpeg);
        assert!(!profile.lossless);

        profile.lossless = true;
        apply_profile_format(&mut profile, CompressionFormat::Webp);
        assert!(profile.lossless);
    }

    #[test]
    fn lossless_hints_cover_every_format() {
        assert!(lossless_hint(&CompressionFormat::Jpeg).contains("非対応"));
        assert!(lossless_hint(&CompressionFormat::Avif).contains("近似"));
        assert!(lossless_hint(&CompressionFormat::Webp).contains("可逆"));
        assert!(lossless_hint(&CompressionFormat::JpegXl).contains("可逆"));
    }

    #[test]
    fn profile_summary_hides_quality_when_lossless() {
        let lossless = CompressionProfile {
            format: CompressionFormat::JpegXl,
            lossless: true,
            ..Default::default()
        };
        assert_eq!(profile_summary(&lossless), "JPEG XL · Lossless");

        let lossy = CompressionProfile {
            format: CompressionFormat::Webp,
            quality: 82,
            lossless: false,
            ..Default::default()
        };
        assert_eq!(profile_summary(&lossy), "WebP · Q82 · Lossy");
    }
}
