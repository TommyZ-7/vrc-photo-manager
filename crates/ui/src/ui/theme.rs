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
/// Legacy sidebar fill, superseded by the opaque `PANEL_SIDEBAR` card. Kept as
/// a named token for callers that still reference the original palette.
#[allow(dead_code)]
pub(crate) const SIDEBAR: u32 = 0x0d1119;

// Window glass and the ambient backdrop. The low byte is the alpha channel, so
// these are used with `rgba()`; the values follow the design spec §9.1 and the
// fractional alphas are rounded to the nearest 8-bit channel.
pub(crate) const GLASS_WINDOW: u32 = 0x0a101c61; // rgba(10,16,28,0.38)
pub(crate) const BG_AMBIENT_A: u32 = 0x366bff47; // rgba(54,107,255,0.28)
pub(crate) const BG_AMBIENT_B: u32 = 0xff56b51f; // rgba(255,86,181,0.12)
pub(crate) const BG_AMBIENT_C: u32 = 0x1ecab21a; // rgba(30,202,178,0.10)

// Opaque panel surfaces. Every card, sidebar, header, and inner row keeps the
// ambient glass behind it instead of letting it show through. `PANEL_CARD` is
// live now; the rest ship with the palette they belong to and are consumed by
// the Task 14-17 shell refit.
#[allow(dead_code)]
pub(crate) const PANEL_SIDEBAR: u32 = 0x0b111b;
#[allow(dead_code)]
pub(crate) const PANEL_SIDEBAR_DEEP: u32 = 0x080d15;
#[allow(dead_code)]
pub(crate) const PANEL_HEADER: u32 = 0x101725;
pub(crate) const PANEL_CARD: u32 = 0x0d1420;
#[allow(dead_code)]
pub(crate) const PANEL_INNER: u32 = 0x111927;
#[allow(dead_code)]
pub(crate) const PANEL_INNER_DEEP: u32 = 0x0c1320;
#[allow(dead_code)]
pub(crate) const PANEL_DEEP: u32 = 0x0a1019;
#[allow(dead_code)]
pub(crate) const PANEL_RAISED: u32 = 0x172132;
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
/// Default (medium) minimum photo-card width. Larger card presets scale this
/// up so the density setting stays meaningful.
pub(crate) const MIN_CARD_WIDTH: f32 = 190.0;
pub(crate) const CARD_BODY_GAP: f32 = 8.0;
pub(crate) const CARD_TEXT_GAP: f32 = 3.0;
pub(crate) const CARD_WORLD_LINE_HEIGHT: f32 = 18.0;
pub(crate) const CARD_META_LINE_HEIGHT: f32 = 15.0;
/// Legacy row slack retained for the Search grid until Task 17 moves it to the
/// 3:2 library metrics.
pub(crate) const CARD_ROW_SLACK: f32 = 24.0;
/// Vertical room reserved inside each library row for the card's -4px hover
/// lift. The actual gap between rows is this slack; the horizontal gap is
/// [`GRID_GAP`].
pub(crate) const CARD_HOVER_SLACK: f32 = 4.0;
pub(crate) const GRID_GAP: f32 = 9.0;
pub(crate) const INSPECTOR_MARGIN: f32 = 16.0;
pub(crate) const INSPECTOR_PREVIEW_HEIGHT: f32 = 168.0;
pub(crate) const INSPECTOR_SHORT_PREVIEW_HEIGHT: f32 = 118.0;
/// Shell padding: 14px around the window and between the sidebar, header, and
/// content column.
pub(crate) const WORKSPACE_PAD: f32 = 14.0;
pub(crate) const WORKSPACE_GAP: f32 = 14.0;
/// Inner-screen gap between the gallery and inspector cards.
pub(crate) const CARD_GAP: f32 = 12.0;
pub(crate) const GALLERY_HEADER_HEIGHT: f32 = 42.0;
pub(crate) const GALLERY_PAD_X: f32 = 12.0;
pub(crate) const GALLERY_PAD_TOP: f32 = 10.0;
pub(crate) const GALLERY_PAD_BOTTOM: f32 = 12.0;
pub(crate) const VIEWER_DETAILS_WIDTH: f32 = 380.0;
pub(crate) const VIEWER_TOP_BAR: f32 = 56.0;
pub(crate) const VIEWER_PAD: f32 = 24.0;
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

/// Usable window extent after the platform's client-side decoration inset.
///
/// X11 reports the outer window (including the shadow/resize inset) from
/// `viewport_size`, so layout maths must use the inner extent. Wayland and
/// X11 without a client inset report `0`.
pub(crate) fn window_content_extent(viewport: f32, client_inset: f32) -> f32 {
    (viewport - client_inset * 2.0).max(0.0)
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

/// Responsive inspector column width from design §8.2. The card itself is
/// [`inspector_card_width`]; the difference is the right-hand margin.
pub(crate) fn inspector_column_width(window_width: f32) -> f32 {
    if window_width <= 1_100.0 {
        254.0
    } else if window_width <= 1_320.0 {
        276.0
    } else {
        298.0
    }
}

pub(crate) fn inspector_card_width(window_width: f32) -> f32 {
    (inspector_column_width(window_width) - INSPECTOR_MARGIN).max(0.0)
}

/// The inspector preview is a fixed 16:10 frame; short windows use the mock's
/// compact height.
pub(crate) fn inspector_preview_height(window_height: f32) -> f32 {
    if window_height <= 760.0 {
        INSPECTOR_SHORT_PREVIEW_HEIGHT
    } else {
        INSPECTOR_PREVIEW_HEIGHT
    }
}

/// Width available to cards in the library gallery: the window minus the shell
/// padding/gap, sidebar, inspector column and card gap, then minus the
/// gallery's own padding and the year-rail gutter.
pub(crate) fn grid_available_width(
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
) -> f32 {
    let inspector = if inspector_collapsed {
        0.0
    } else {
        inspector_column_width(window_width)
    };
    let card_gap = if inspector_collapsed { 0.0 } else { CARD_GAP };
    let gallery = window_width
        - WORKSPACE_PAD * 2.0
        - sidebar_width(sidebar_collapsed)
        - WORKSPACE_GAP
        - inspector
        - card_gap;
    (gallery - GALLERY_PAD_X * 2.0 - RAIL_WIDTH).max(0.0)
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

/// Card layout for one uniform-list row of the library gallery.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridMetrics {
    pub(crate) columns: usize,
    pub(crate) card_width: f32,
    pub(crate) image_height: f32,
    pub(crate) row_height: f32,
}

/// Minimum card width for each density preset. Medium keeps the spec's 190 px
/// floor; larger presets scale the cards and therefore reduce the column
/// count.
pub(crate) fn library_min_card_width(size: CardSize) -> f32 {
    match size {
        CardSize::Small => 140.0,
        CardSize::Medium => MIN_CARD_WIDTH,
        CardSize::Large => 220.0,
        CardSize::ExtraLarge => 280.0,
    }
}

/// Card width when `available_width` is split into `columns` equal cells with
/// one [`GRID_GAP`] between neighbours.
pub(crate) fn photo_card_width(available_width: f32, columns: usize) -> f32 {
    let columns = columns.max(1) as f32;
    ((available_width - GRID_GAP * (columns - 1.0)) / columns).max(0.0)
}

/// Uniform-list row height for a 3:2 card plus the visible metadata lines and
/// hover slack. Hiding text through the display settings tightens the row.
pub(crate) fn photo_row_height(card_width: f32, show_world: bool, show_meta: bool) -> f32 {
    let image_height = (card_width.max(0.0) * 2.0 / 3.0).max(0.0);
    let mut height = image_height + CARD_HOVER_SLACK;
    if show_world || show_meta {
        height += CARD_BODY_GAP;
    }
    if show_world {
        height += CARD_WORLD_LINE_HEIGHT;
    }
    if show_world && show_meta {
        height += CARD_TEXT_GAP;
    }
    if show_meta {
        height += CARD_META_LINE_HEIGHT;
    }
    height
}

/// The default medium grid keeps three columns once the gallery is wide
/// enough; pure 190 px packing would produce a fourth column at 1440. Narrow
/// windows keep two, and collapsing the inspector may reveal more room.
fn library_column_cap(window_width: f32, inspector_collapsed: bool, card_size: CardSize) -> usize {
    if card_size != CardSize::Medium {
        return usize::MAX;
    }
    if window_width <= 1_100.0 {
        2
    } else if inspector_collapsed {
        usize::MAX
    } else {
        3
    }
}

/// Complete library grid layout for the current window, sidebar/inspector
/// state, density preset, and visible card text.
pub(crate) fn library_grid_metrics(
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
    card_size: CardSize,
    show_world: bool,
    show_meta: bool,
) -> GridMetrics {
    let available_width =
        grid_available_width(window_width, sidebar_collapsed, inspector_collapsed);
    let raw_columns = grid_columns(available_width, library_min_card_width(card_size));
    let columns = raw_columns.min(library_column_cap(
        window_width,
        inspector_collapsed,
        card_size,
    ));
    let card_width = photo_card_width(available_width, columns);
    let image_height = card_width * 2.0 / 3.0;
    let row_height = photo_row_height(card_width, show_world, show_meta);
    GridMetrics {
        columns,
        card_width,
        image_height,
        row_height,
    }
}

/// Column count for the Search grid while it keeps its pre-Task 17 metrics.
pub(crate) fn search_grid_columns(
    window_width: f32,
    sidebar_collapsed: bool,
    min_card_width: f32,
) -> usize {
    grid_columns(
        search_available_width(window_width, sidebar_collapsed),
        min_card_width,
    )
}

pub(crate) fn viewer_image_rect(
    viewport: gpui_kit::Size<Pixels>,
    details_open: bool,
    aspect: f32,
) -> Bounds<Pixels> {
    let details_width = if details_open {
        VIEWER_DETAILS_WIDTH
    } else {
        0.0
    };
    let (width, height) = viewer_image_box_with_details_width(
        viewport.width.into(),
        viewport.height.into(),
        details_width,
        aspect,
    );
    centered_viewer_rect(viewport, details_width, width, height)
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
    fn window_content_extent_removes_the_client_inset() {
        assert_eq!(window_content_extent(1320.0, 20.0), 1280.0);
        assert_eq!(window_content_extent(1480.0, 20.0), 1440.0);
        assert_eq!(window_content_extent(1280.0, 0.0), 1280.0);
        assert_eq!(window_content_extent(10.0, 20.0), 0.0);
    }

    #[test]
    fn sidebar_widths_match_the_rail() {
        assert_eq!(sidebar_width(false), 238.0);
        assert_eq!(sidebar_width(true), 78.0);
        assert_eq!(sidebar_reveal_width(false, 0.0), 0.0);
        assert_eq!(sidebar_reveal_width(false, 1.0), 130.0);
        assert_eq!(sidebar_reveal_width(true, 0.0), 130.0);
        assert_eq!(sidebar_reveal_width(true, 1.0), 0.0);
    }

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.01, "{actual} != {expected}");
    }

    #[test]
    fn inspector_widths_follow_the_responsive_table() {
        assert_eq!(inspector_column_width(1080.0), 254.0);
        assert_eq!(inspector_column_width(1280.0), 276.0);
        assert_eq!(inspector_column_width(1440.0), 298.0);
        assert_eq!(inspector_card_width(1080.0), 238.0);
        assert_eq!(inspector_card_width(1280.0), 260.0);
        assert_eq!(inspector_card_width(1440.0), 282.0);
        assert_eq!(inspector_card_width(1280.0) + INSPECTOR_MARGIN, 276.0);
        assert_eq!(inspector_preview_height(760.0), 118.0);
        assert_eq!(inspector_preview_height(761.0), 168.0);
    }

    #[test]
    fn library_uses_three_columns_at_1280_and_1440_and_two_at_1080() {
        for (width, expected_columns) in [(1080.0, 2), (1280.0, 3), (1440.0, 3)] {
            let metrics = library_grid_metrics(width, false, false, CardSize::Medium, true, true);
            assert_eq!(metrics.columns, expected_columns, "width {width}");
        }

        // Without the medium-width cap, 1440 would pack four 190px cards.
        assert_eq!(
            grid_columns(
                grid_available_width(1440.0, false, false),
                library_min_card_width(CardSize::Medium)
            ),
            4
        );
        assert_eq!(
            library_grid_metrics(1440.0, false, false, CardSize::Medium, true, true).columns,
            3
        );
    }

    #[test]
    fn photo_card_width_splits_the_gallery_with_the_grid_gap() {
        close(photo_card_width(668.0, 3), 216.666_67);
        close(photo_card_width(806.0, 3), 262.666_67);
        close(photo_card_width(490.0, 2), 240.5);
        close(photo_card_width(100.0, 1), 100.0);
        close(photo_card_width(100.0, 0), 100.0);
        close(photo_card_width(0.0, 3), 0.0);
    }

    #[test]
    fn photo_row_height_adds_only_the_visible_text_lines() {
        close(photo_row_height(300.0, false, false), 204.0);
        close(photo_row_height(300.0, true, false), 230.0);
        close(photo_row_height(300.0, false, true), 227.0);
        close(photo_row_height(300.0, true, true), 248.0);

        let metrics = library_grid_metrics(1280.0, false, false, CardSize::Medium, true, true);
        close(metrics.image_height, metrics.card_width * 2.0 / 3.0);
        close(metrics.row_height, metrics.image_height + 48.0);
    }

    #[test]
    fn library_card_size_presets_scale_card_density() {
        let small = library_grid_metrics(1280.0, false, false, CardSize::Small, true, true);
        let medium = library_grid_metrics(1280.0, false, false, CardSize::Medium, true, true);
        let large = library_grid_metrics(1280.0, false, false, CardSize::Large, true, true);
        let extra = library_grid_metrics(1280.0, false, false, CardSize::ExtraLarge, true, true);
        assert!(small.columns > medium.columns);
        assert!(medium.columns > large.columns);
        assert!(large.columns >= extra.columns);
        assert_eq!(large.columns, 2);
        assert_eq!(extra.columns, 2);
    }

    #[test]
    fn narrow_windows_keep_two_columns_even_with_the_sidebar_collapsed() {
        assert_eq!(
            library_grid_metrics(1080.0, true, false, CardSize::Medium, true, true).columns,
            2
        );
    }

    #[test]
    fn grid_available_width_matches_the_shell_padding() {
        assert_eq!(grid_available_width(1440.0, false, false), 806.0);
        assert_eq!(grid_available_width(1280.0, false, false), 668.0);
        assert_eq!(grid_available_width(1080.0, false, false), 490.0);
        assert_eq!(grid_available_width(1440.0, true, false), 966.0);
        assert_eq!(search_available_width(1280.0, false), 654.0);
        assert_eq!(grid_columns(0.0, MIN_CARD_WIDTH), 1);
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
        assert_eq!(search_grid_columns(1280.0, false, MIN_CARD_WIDTH), 3);
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
        assert_eq!(search_grid_columns(1280.0, false, MIN_CARD_WIDTH), 3);
        assert_eq!(search_grid_columns(1440.0, false, MIN_CARD_WIDTH), 4);
        assert_eq!(search_grid_columns(1080.0, false, MIN_CARD_WIDTH), 2);
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

    #[test]
    fn viewer_image_rect_centers_the_box_and_respects_details() {
        let viewport = size(px(1200.), px(800.));
        let closed = viewer_image_rect(viewport, false, 16.0 / 10.0);
        let open = viewer_image_rect(viewport, true, 16.0 / 10.0);
        assert!((f32::from(closed.center().x) - 600.0).abs() < 0.01);
        assert!((f32::from(closed.center().y) - 428.0).abs() < 0.01);
        assert!((f32::from(open.center().x) - 410.0).abs() < 0.01);
        assert!((f32::from(open.center().y) - 428.0).abs() < 0.01);
        assert!(closed.size.width > open.size.width);
    }

    #[test]
    fn glass_and_panel_tokens_follow_the_spec() {
        // Translucent tokens pack the spec's alpha into the low byte, so the
        // spec's fractional alphas survive within one 8-bit step.
        let alpha = |token: u32| (token & 0xff) as f32 / 255.0;
        let close = |value: f32, expected: f32| assert!((value - expected).abs() <= 1.0 / 255.0);

        close(alpha(GLASS_WINDOW), 0.38);
        assert_eq!(GLASS_WINDOW >> 8, 0x0a101c);
        close(alpha(BG_AMBIENT_A), 0.28);
        assert_eq!(BG_AMBIENT_A >> 8, 0x366bff);
        close(alpha(BG_AMBIENT_B), 0.12);
        assert_eq!(BG_AMBIENT_B >> 8, 0xff56b5);
        close(alpha(BG_AMBIENT_C), 0.10);
        assert_eq!(BG_AMBIENT_C >> 8, 0x1ecab2);

        // Panels are opaque: they are used with `rgb()` and never carry a
        // window-glass alpha.
        assert_eq!(PANEL_SIDEBAR, 0x0b111b);
        assert_eq!(PANEL_SIDEBAR_DEEP, 0x080d15);
        assert_eq!(PANEL_HEADER, 0x101725);
        assert_eq!(PANEL_CARD, 0x0d1420);
        assert_eq!(PANEL_INNER, 0x111927);
        assert_eq!(PANEL_INNER_DEEP, 0x0c1320);
        assert_eq!(PANEL_DEEP, 0x0a1019);
        assert_eq!(PANEL_RAISED, 0x172132);
    }
}
