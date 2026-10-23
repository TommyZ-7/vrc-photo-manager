use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use smallvec::SmallVec;
use vrc_photo_core::db::Db;
use vrc_photo_core::importer::{self, ImportMode};
use vrc_photo_core::model::{
    CompressionFormat, CompressionJob, CompressionProfile, DaemonError, Derivative, LibraryStats,
    MAX_JOB_RETRIES, NewPhoto, Photo, PhotoContext, PhotoIndexEntry, PhotoPlayer, PhotoSort,
    PhotoTextFilter, QueueStatus,
};
use vrc_photo_core::settings::AppSettings;
use vrc_photo_core::thumbnail;
use vrc_photo_core::time;
use vrc_photo_core::{linker, scanner};

mod logging;

const BG: u32 = 0x07090d;
const PANEL: u32 = 0x121722;
const PANEL_2: u32 = 0x1a2130;
const SIDEBAR: u32 = 0x0d1119;
const BORDER: u32 = 0x232b38;
const BORDER_STRONG: u32 = 0x364256;
const TEXT: u32 = 0xeef3fb;
const MUTED: u32 = 0xa3adbd;
const MUTED_2: u32 = 0x768196;
const BLUE: u32 = 0x66b3ff;
const ACCENT_SOFT: u32 = 0x1a2f47;
const GREEN: u32 = 0x5eead4;
const AMBER: u32 = 0xfbbf24;
const RED: u32 = 0xfb7185;
const PURPLE: u32 = 0xb79cff;

const SIDEBAR_EXPANDED: f32 = 238.0;
const SIDEBAR_COLLAPSED: f32 = 78.0;
const MIN_CARD_WIDTH: f32 = 170.0;
const GRID_GAP: f32 = 16.0;
const INSPECTOR_WIDTH: f32 = 330.0;
const INSPECTOR_CARD_WIDTH: f32 = 314.0;
const INSPECTOR_MARGIN: f32 = 16.0;
const VIEWER_DETAILS_WIDTH: f32 = 380.0;
const VIEWER_TOP_BAR: f32 = 56.0;
const VIEWER_PAD: f32 = 24.0;
const WORKSPACE_PAD_X: f32 = 44.0;
const CARD_INTRO_SECS: f32 = 0.6;
const CARD_STAGGER_SECS: f32 = 0.04;
const CARD_STAGGER_MAX: usize = 8;
const THUMB_CACHE_ENTRIES: usize = 192;
const THUMB_IN_FLIGHT: usize = 4;
const PAGE_SIZE: usize = 256;
const MAX_PAGES: usize = 24;
const PHOTO_ROW_HEIGHT: f32 = 196.0;
const RAIL_WIDTH: f32 = 88.0;
const RAIL_TOP_PAD: f32 = 24.0;
const RAIL_TRACK_INSET: f32 = 12.0;
const RAIL_LABEL_GAP: f32 = 16.0;
const RAIL_YEAR_HIT_SLOP: f32 = 8.0;
const RAIL_PILL_HEIGHT: f32 = 18.0;

gpui_kit::actions!(
    ui,
    [
        ToggleSidebar,
        ToggleInspector,
        FocusSearch,
        OpenQuickLook,
        CloseQuickLook,
        ViewerPreviousPhoto,
        ViewerNextPhoto,
        QuickLookNextFocus,
        QuickLookPrevFocus,
    ]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonTone {
    Neutral,
    Primary,
    Danger,
}

#[derive(Clone, Copy)]
struct ButtonState {
    tone: ButtonTone,
    enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Photos,
    Compression,
    Search,
    Monitor,
    Settings,
}

impl Screen {
    fn title(self) -> &'static str {
        match self {
            Self::Photos => "写真",
            Self::Compression => "圧縮設定",
            Self::Search => "検索",
            Self::Monitor => "ログ監視",
            Self::Settings => "設定",
        }
    }
}

#[derive(Clone, Debug)]
struct LaunchOptions {
    background: bool,
    demo: bool,
    demo_reset: bool,
    window_size: (f32, f32),
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            background: false,
            demo: false,
            demo_reset: false,
            window_size: (1280.0, 820.0),
        }
    }
}

/// Timing probes stay silent unless `VRC_PHOTO_TIMING` is set.
fn timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VRC_PHOTO_TIMING").is_some())
}

fn log_timing(label: &str, start: Instant) {
    if timing_enabled() {
        eprintln!("[timing] {label}: {:?}", start.elapsed());
    }
}

/// Process start, captured once so the first-frame probe can report launch
/// latency. Only initialized when timing is enabled.
fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn parse_args(args: impl IntoIterator<Item = String>) -> LaunchOptions {
    let mut options = LaunchOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--background" => options.background = true,
            "--demo" => options.demo = true,
            "--demo-reset" => options.demo_reset = true,
            "--window-size" => {
                if let Some(value) = args.next()
                    && let Some(size) = parse_window_size(&value)
                {
                    options.window_size = size;
                }
            }
            _ => {}
        }
    }
    options
}

/// Bounded LRU cache for grid thumbnails.
///
/// Decoding runs on the background executor; the UI thread only turns the
/// decoded RGBA buffer into a `RenderImage`. `pending` and `failed` keep a
/// photo from being requested more than once while it is on screen.
#[derive(Default)]
struct ThumbnailCache {
    images: HashMap<i64, Arc<RenderImage>>,
    colors: HashMap<i64, u32>,
    lru: VecDeque<i64>,
    pending: HashSet<i64>,
    failed: HashSet<i64>,
    pinned: Option<i64>,
    in_flight: usize,
}

impl ThumbnailCache {
    fn get(&mut self, id: i64) -> Option<Arc<RenderImage>> {
        let image = self.images.get(&id)?.clone();
        self.touch(id);
        Some(image)
    }

    fn color(&self, id: i64) -> Option<u32> {
        self.colors.get(&id).copied()
    }

    fn begin(&mut self, id: i64) -> bool {
        if self.images.contains_key(&id)
            || self.pending.contains(&id)
            || self.failed.contains(&id)
            || self.in_flight >= THUMB_IN_FLIGHT
        {
            return false;
        }
        self.pending.insert(id);
        self.in_flight += 1;
        true
    }

    fn insert(&mut self, id: i64, image: Arc<RenderImage>, color: u32) {
        if self.pending.remove(&id) {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.images.insert(id, image);
        self.colors.insert(id, color);
        self.touch(id);
        self.evict();
    }

    fn fail(&mut self, id: i64) {
        if self.pending.remove(&id) {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.failed.insert(id);
    }

    fn pin(&mut self, id: i64) {
        self.pinned = Some(id);
    }

    /// Drops the pin when the selected photo is deleted or deselected.
    fn unpin(&mut self, id: i64) {
        if self.pinned == Some(id) {
            self.pinned = None;
        }
    }

    fn touch(&mut self, id: i64) {
        if let Some(index) = self.lru.iter().position(|entry| *entry == id) {
            self.lru.remove(index);
        }
        self.lru.push_back(id);
    }

    fn evict(&mut self) {
        while self.images.len() > THUMB_CACHE_ENTRIES {
            let Some(id) = self.lru.pop_front() else {
                break;
            };
            // Rotate the pinned entry instead of dropping it. All cached
            // images own an LRU slot, so this always terminates.
            if Some(id) == self.pinned {
                self.lru.push_back(id);
                continue;
            }
            self.images.remove(&id);
            self.colors.remove(&id);
        }
    }
}

/// Contiguous run of photos that share a local capture year.
///
/// `display_row` is the bucket's cumulative position in the rail, which stays
/// proportional to the number of photos per year even when the underlying
/// entries interleave years. `start_row` keeps the first entry index so a
/// label click lands on a real photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YearBucket {
    year: i32,
    start_row: usize,
    display_row: usize,
    count: usize,
}

fn build_year_buckets(entries: &[PhotoIndexEntry]) -> Vec<YearBucket> {
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
fn rail_y(display_row: usize, total: usize, height: f32) -> f32 {
    let usable = (height - 2.0 * RAIL_TOP_PAD).max(0.0);
    if total == 0 {
        return RAIL_TOP_PAD;
    }
    let fraction = (display_row as f32 / total as f32).clamp(0.0, 1.0);
    (RAIL_TOP_PAD + fraction * usable).clamp(RAIL_TOP_PAD, RAIL_TOP_PAD + usable)
}

/// Inverse of [`rail_y`]: the row nearest to a y offset inside the rail.
fn rail_row_at(y: f32, total: usize, height: f32) -> usize {
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
fn format_captured_month(month: i32) -> String {
    if month <= 0 {
        return "不明".to_string();
    }
    format!("{}年{}月", month / 100, month % 100)
}

/// Visits the year labels that fit without overlapping, in bucket order.
///
/// Every bucket still gets a dot; only labels keep a 16 px gap so a long
/// library does not paint a solid block of years.
fn for_each_visible_year(
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

fn visible_year_buckets(buckets: &[YearBucket], height: f32) -> Vec<usize> {
    let mut visible = Vec::new();
    for_each_visible_year(buckets, height, |index, _, _| visible.push(index));
    visible
}

/// The floating month pill already contains the year, so a year label it
/// covers would only show through as a duplicate at rest.
fn rail_label_hidden_by_pill(label_y: f32, pill_y: f32) -> bool {
    (label_y - pill_y).abs() < RAIL_PILL_HEIGHT / 2.0 + 6.0
}

/// Scroll distance from the top of a uniform list, in pixels.
fn list_scroll_top(handle: &UniformListScrollHandle) -> f32 {
    let offset = handle.0.borrow().base_handle.offset().y;
    (-f32::from(offset)).max(0.0)
}

/// Scroll offset that puts `photo_row` at the top of the grid, clamped to the
/// list's scrollable range.
///
/// `max_offset` is the non-negative extent reported by
/// `ScrollHandle::max_offset()`; the actual offset is zero or negative.
fn rail_scroll_offset(photo_row: usize, columns: usize, max_offset: f32) -> f32 {
    let grid_row = photo_row / columns.max(1);
    (-(grid_row as f32 * PHOTO_ROW_HEIGHT)).clamp(-max_offset.max(0.0), 0.0)
}

/// Ordered snapshot of the library: only IDs and capture timestamps, so the
/// full 100k-entry list stays cheap while photo rows load in pages.
struct PhotoIndex {
    entries: Vec<PhotoIndexEntry>,
    total: usize,
    years: Vec<YearBucket>,
}

impl PhotoIndex {
    fn new(entries: Vec<PhotoIndexEntry>) -> Self {
        let total = entries.len();
        let years = build_year_buckets(&entries);
        Self {
            entries,
            total,
            years,
        }
    }

    /// Position of `id` in the ordered snapshot, if it is still present.
    fn row_of_id(&self, id: i64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }
}

/// First photo in the top visible row.
///
/// `scroll_top` is the non-negative scroll distance from the top of the list.
/// The row is floored before multiplying by the column count: halfway into
/// row 2 the top row is still row 2, not the first card of row 3.
fn first_visible_photo(scroll_top: f32, columns: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let columns = columns.max(1);
    let row = (scroll_top.max(0.0) / PHOTO_ROW_HEIGHT).floor() as usize;
    row.saturating_mul(columns).min(total - 1)
}

/// Photo at the top-left of the first visible row.
///
/// `offset_y` is the non-negative scroll distance from the top of the list;
/// callers convert the scroll handle's negative offset before calling.
fn anchor_photo_id(index: &PhotoIndex, offset_y: f32, columns: usize) -> Option<i64> {
    let total = index.total;
    if total == 0 {
        return None;
    }
    let first_photo = first_visible_photo(offset_y, columns, total);
    index.entries.get(first_photo).map(|entry| entry.id)
}

/// The photo the next search result should be anchored to. Keep the requested
/// anchor when it survived the query; otherwise fall back to the first photo
/// (the top of the grid) or `None` for an empty result.
fn search_anchor_after_result(requested: Option<i64>, index: &PhotoIndex) -> Option<i64> {
    requested
        .filter(|id| index.row_of_id(*id).is_some())
        .or_else(|| index.entries.first().map(|entry| entry.id))
}

/// Bounded access-order LRU cache for grid photo pages.
///
/// `get_page` moves the page to the back of the order; `insert_page` evicts
/// from the front, so a page the user scrolled back to is not displaced by a
/// backlog of page loads that finish later.
#[derive(Default)]
struct PhotoPageCache {
    pages: HashMap<usize, Arc<Vec<Arc<Photo>>>>,
    order: VecDeque<usize>,
    pending: HashSet<usize>,
    failed: HashSet<usize>,
}

impl PhotoPageCache {
    fn new() -> Self {
        Self::default()
    }

    fn clear(&mut self) {
        self.pages.clear();
        self.order.clear();
        self.pending.clear();
        self.failed.clear();
    }

    fn get_page(&mut self, page: usize) -> Option<Arc<Vec<Arc<Photo>>>> {
        let page_photos = self.pages.get(&page).cloned()?;
        self.touch(page);
        Some(page_photos)
    }

    fn insert_page(&mut self, page: usize, photos: Vec<Photo>) {
        self.pending.remove(&page);
        self.failed.remove(&page);
        let page_photos = photos.into_iter().map(Arc::new).collect::<Vec<_>>();
        self.pages.insert(page, Arc::new(page_photos));
        self.touch(page);
        while self.pages.len() > MAX_PAGES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.pages.remove(&oldest);
        }
    }

    fn touch(&mut self, page: usize) {
        if let Some(index) = self.order.iter().position(|entry| *entry == page) {
            self.order.remove(index);
        }
        self.order.push_back(page);
    }

    fn plan_pages(&mut self, start: usize, end: usize, limit: usize) -> Vec<usize> {
        if end <= start || limit == 0 {
            return Vec::new();
        }
        let first = start / PAGE_SIZE;
        let last = (end - 1) / PAGE_SIZE;
        let mut planned = Vec::new();
        for page in first..=last {
            if planned.len() >= limit {
                break;
            }
            if self.pages.contains_key(&page)
                || self.pending.contains(&page)
                || self.failed.contains(&page)
            {
                continue;
            }
            self.pending.insert(page);
            planned.push(page);
        }
        planned
    }

    fn fail_page(&mut self, page: usize) {
        self.pending.remove(&page);
        self.failed.insert(page);
    }
}

/// Snapshot plus page cache for one grid (library or search).
struct PhotoBrowser {
    index: Arc<PhotoIndex>,
    pages: PhotoPageCache,
    scroll: UniformListScrollHandle,
    columns: usize,
    generation: u64,
    labels_generation: u64,
    labels: Option<Arc<YearRailLabels>>,
    /// Shared with the mounted rail so a re-render cannot drop an in-flight
    /// drag or let it outlive the browser.
    rail_drag: Rc<Cell<bool>>,
}

impl PhotoBrowser {
    fn new(index: Arc<PhotoIndex>) -> Self {
        Self {
            index,
            pages: PhotoPageCache::new(),
            scroll: UniformListScrollHandle::new(),
            columns: 1,
            generation: 0,
            labels_generation: 0,
            labels: None,
            rail_drag: Rc::new(Cell::new(false)),
        }
    }

    fn replace_index(&mut self, index: Arc<PhotoIndex>) {
        self.index = index;
        self.generation = self.generation.wrapping_add(1);
        self.pages.clear();
        self.labels = None;
        self.rail_drag.set(false);
    }

    /// Drops a photo from this snapshot without notifying the caller.
    ///
    /// The page cache and scroll position refer to the old row layout, so the
    /// cache is cleared and a scroll that now points past the last row is
    /// clamped to it.
    fn remove_photo(&mut self, id: i64) {
        let Some(position) = self.index.row_of_id(id) else {
            return;
        };
        let mut entries = self.index.entries.clone();
        entries.remove(position);
        self.replace_index(Arc::new(PhotoIndex::new(entries)));

        let columns = self.columns.max(1);
        let rows = self.index.total.div_ceil(columns);
        if rows == 0 {
            self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
            return;
        }
        let current_row = (list_scroll_top(&self.scroll) / PHOTO_ROW_HEIGHT).floor() as usize;
        if current_row >= rows {
            self.scroll
                .scroll_to_item_strict(rows - 1, ScrollStrategy::Nearest);
        }
    }

    fn photo_at(&mut self, photo_index: usize) -> Option<Arc<Photo>> {
        let page = photo_index / PAGE_SIZE;
        let offset = photo_index % PAGE_SIZE;
        self.pages.get_page(page)?.get(offset).cloned()
    }

    fn plan_pages(&mut self, start: usize, end: usize, limit: usize) -> Vec<usize> {
        let end = end.min(self.index.total);
        self.pages.plan_pages(start, end, limit)
    }

    fn insert_page(&mut self, page: usize, photos: Vec<Photo>, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.pages.insert_page(page, photos);
    }

    fn fail_page(&mut self, page: usize, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.pages.fail_page(page);
    }
}

/// Keeps the no-query search mirror on the current library snapshot.
///
/// Returns `true` when the mirror was replaced (and its page cache cleared).
fn sync_search_mirror(search: &mut PhotoBrowser, library: &PhotoBrowser) -> bool {
    if Arc::ptr_eq(&search.index, &library.index) {
        return false;
    }
    search.replace_index(library.index.clone());
    search.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
    true
}

/// Replaces the library snapshot with a fresh index result.
///
/// On failure the previously mounted snapshot, page cache, and scroll offset
/// stay untouched so a transient locked/corrupt/I-O error cannot blank the
/// visible library. Returns the user-facing error in that case.
fn apply_library_index(
    browser: &mut PhotoBrowser,
    result: vrc_photo_core::error::Result<Vec<PhotoIndexEntry>>,
) -> Option<String> {
    match result {
        Ok(entries) => {
            browser.replace_index(Arc::new(PhotoIndex::new(entries)));
            None
        }
        Err(error) => {
            tracing::error!(%error, "photo_index failed");
            Some("写真一覧を読み込めませんでした".to_string())
        }
    }
}

/// Quiet-period tracker for the live photo count.
///
/// The three-second refresh tick observes a changing count while the daemon
/// imports a burst of files. Rebuilding the 100k index on every tick would
/// waste work, so a rebuild waits until the count has stayed at the same
/// value for one full quiet period. Any further change restarts the period.
#[derive(Default)]
struct CountDebounce {
    pending: Option<i64>,
    since: Option<Instant>,
}

impl CountDebounce {
    fn observe(&mut self, count: i64, now: Instant, quiet: Duration) -> bool {
        match self.pending {
            Some(pending) if pending == count => {
                if self
                    .since
                    .is_some_and(|since| now.duration_since(since) >= quiet)
                {
                    self.reset();
                    true
                } else {
                    false
                }
            }
            _ => {
                self.pending = Some(count);
                self.since = Some(now);
                false
            }
        }
    }

    fn reset(&mut self) {
        self.pending = None;
        self.since = None;
    }
}

fn rail_text_run(len: usize, color: u32) -> TextRun {
    TextRun {
        len,
        font: font("Noto Sans JP"),
        color: rgb(color).into(),
        ..TextRun::default()
    }
}

/// Shaped year/month text for the rail.
///
/// Building shapes text, so it is cached per index generation on
/// [`PhotoBrowser`]; paint only reuses these lines.
struct YearRailLabels {
    years: Vec<ShapedLine>,
    current_years: Vec<ShapedLine>,
    months: HashMap<i32, ShapedLine>,
}

impl YearRailLabels {
    fn build(index: &PhotoIndex, window: &mut Window) -> Self {
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
struct YearRail {
    id: &'static str,
    labels: Arc<YearRailLabels>,
    index: Arc<PhotoIndex>,
    scroll: UniformListScrollHandle,
    columns: usize,
    drag_active: Rc<Cell<bool>>,
}

impl YearRail {
    fn new(
        id: &'static str,
        labels: Arc<YearRailLabels>,
        index: Arc<PhotoIndex>,
        scroll: UniformListScrollHandle,
        columns: usize,
        drag_active: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            id,
            labels,
            index,
            scroll,
            columns,
            drag_active,
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
        let offset = rail_scroll_offset(photo_row, self.columns, max_offset);
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

impl Element for YearRail {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

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
        // Keep cards under the rail from hovering or receiving clicks while
        // still letting the wheel scroll the grid through the rail.
        window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
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
        let hitbox = hitbox.clone();

        window.set_cursor_style(CursorStyle::PointingHand, &hitbox);

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
        let first_visible = first_visible_photo(scroll_top, columns, total);
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
        let pill_bounds = Bounds::new(
            point(
                bounds.right() - px(RAIL_WIDTH) + px(4.),
                pill_center - px(RAIL_PILL_HEIGHT / 2.0),
            ),
            size(px(RAIL_WIDTH - 8.), px(RAIL_PILL_HEIGHT)),
        );
        window.paint_quad(fill(pill_bounds, rgba((PANEL_2 << 8) | 0xf0)).corner_radii(px(9.)));
        if let Some(line) = labels.months.get(&month) {
            line.paint(
                point(bounds.right() - px(RAIL_WIDTH - 10.), pill_center - px(6.)),
                px(12.),
                TextAlign::Right,
                Some(px(RAIL_WIDTH - 20.)),
                window,
                cx,
            )
            .ok();
        }

        let rail = self.clone();
        window.on_mouse_event({
            let rail = rail.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                    || !bounds.contains(&event.position)
                {
                    return;
                }
                // The rail is painted above the cards; keep the press from
                // reaching the photo underneath it.
                cx.stop_propagation();
                rail.drag_active.set(true);
                rail.jump_to(event.position.y, bounds.top(), height, window);
            }
        });

        window.on_mouse_event({
            let rail = rail.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !rail.drag_active.get() {
                    return;
                }
                if !event.dragging() {
                    // Mouse-up can be swallowed by an overlay; stop dragging
                    // as soon as the left button is no longer held.
                    rail.drag_active.set(false);
                    return;
                }
                if !phase.bubble() {
                    return;
                }
                cx.stop_propagation();
                rail.jump_to(event.position.y, bounds.top(), height, window);
            }
        });

        window.on_mouse_event({
            let rail = rail;
            move |_: &MouseUpEvent, phase, _, _| {
                if phase.bubble() {
                    rail.drag_active.set(false);
                }
            }
        });
    }
}

pub struct App {
    db: Db,
    screen: Screen,
    sidebar_collapsed: bool,
    sidebar_motion_epoch: u64,
    inspector_collapsed: bool,
    inspector_epoch: u64,
    hovered_nav: Option<Screen>,
    daemon_age_secs: Option<i64>,
    library: PhotoBrowser,
    search_view: PhotoBrowser,
    /// The library index changed while another screen was mounted; the next
    /// visit to Photos rebuilds it.
    library_dirty: bool,
    /// A failed index/page query kept the previous snapshot mounted; retry from
    /// the Photos header.
    library_error: Option<String>,
    count_debounce: CountDebounce,
    selected: Option<i64>,
    selection_epoch: u64,
    selection_changed_at: Option<Instant>,
    selected_photo: Option<Photo>,
    selected_players: Vec<PhotoPlayer>,
    selected_tags: Vec<String>,
    selected_derivatives: Vec<Derivative>,
    compressed_ids: HashSet<i64>,
    thumbnails: ThumbnailCache,
    grid_epoch: u64,
    hovered_photo: Option<i64>,
    intro_seen: HashMap<i64, Instant>,
    photo_grid_focus: FocusHandle,
    quick_look_open: bool,
    quick_look_epoch: u64,
    quick_look_return_focus: Option<FocusHandle>,
    quick_look_focus_index: usize,
    quick_look_details_open: bool,
    quick_look_details_epoch: u64,
    quick_look_photo_ids: Vec<i64>,
    quick_look_index: usize,
    quick_look_close_focus: FocusHandle,
    quick_look_prev_focus: FocusHandle,
    quick_look_next_focus: FocusHandle,
    quick_look_details_focus: FocusHandle,
    quick_look_favorite_focus: FocusHandle,
    quick_look_compress_focus: FocusHandle,
    quick_look_trash_focus: FocusHandle,
    quick_look_add_tag_focus: FocusHandle,
    quick_look_note_edit_focus: FocusHandle,
    quick_look_note_save_focus: FocusHandle,
    quick_look_note_cancel_focus: FocusHandle,
    preview: Option<Arc<RenderImage>>,
    previous_preview: Option<Arc<RenderImage>>,
    previous_color: Option<u32>,
    profiles: Vec<CompressionProfile>,
    profile_index: usize,
    stats: LibraryStats,
    settings: AppSettings,
    jobs: Vec<CompressionJob>,
    daemon_errors: Vec<DaemonError>,
    failed_job_count: i64,
    failed_jobs: Vec<CompressionJob>,
    failed_job_names: HashMap<i64, String>,
    monitor_world: Option<String>,
    monitor_instance: Option<String>,
    monitor_players: Vec<String>,
    daemon_active: bool,
    world_input: Entity<InputState>,
    player_input: Entity<InputState>,
    tag_input: Entity<InputState>,
    tag_edit_input: Entity<InputState>,
    note_edit_input: Entity<TextareaState>,
    note_edit_id: Option<i64>,
    photo_root_input: Entity<InputState>,
    log_dir_input: Entity<InputState>,
    import_dir_input: Entity<InputState>,
    search_active: bool,
    searching: bool,
    search_request: u64,
    applied_search: Option<PhotoTextFilter>,
    pending_search_anchor: Option<i64>,
    search_refresh_pending: bool,
    search_favorite: bool,
    search_days: Option<i64>,
    compress_policy: String,
    delete_armed: Option<i64>,
    message: Option<String>,
    demo: bool,
    demo_assets: Option<DemoAssets>,
}

type ViewerActivate = Rc<dyn Fn(&mut App, &mut Window, &mut Context<App>)>;

impl App {
    fn new(window: &mut Window, cx: &mut Context<Self>, options: &LaunchOptions) -> Self {
        let (db, demo, mut message, demo_assets) = open_database(options);
        let initial_settings = AppSettings::load(&db).unwrap_or_default();
        cx.set_reduce_motion(initial_settings.ui_reduce_motion);
        let world_input = cx.new(|cx| InputState::new(window, cx).placeholder("ワールド名 / ID"));
        let player_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("プレイヤー名 / ID"));
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder("タグ"));
        let tag_edit_input = cx.new(|cx| InputState::new(window, cx).placeholder("タグを追加"));
        let note_edit_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("メモを入力")
                .auto_grow(2, 6)
        });
        let photo_root_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:/Users/you/Pictures/VRChat")
                .default_value(
                    initial_settings
                        .photo_roots
                        .first()
                        .cloned()
                        .unwrap_or_default(),
                )
        });
        let log_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:/Users/you/AppData/LocalLow/VRChat/VRChat")
                .default_value(initial_settings.log_dir.clone().unwrap_or_default())
        });
        let import_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("取り込み先フォルダ")
                .default_value(initial_settings.import_dir().display().to_string())
        });
        let photo_grid_focus = cx.focus_handle();
        let quick_look_close_focus = cx.focus_handle();
        let quick_look_prev_focus = cx.focus_handle();
        let quick_look_next_focus = cx.focus_handle();
        let quick_look_details_focus = cx.focus_handle();
        let quick_look_favorite_focus = cx.focus_handle();
        let quick_look_compress_focus = cx.focus_handle();
        let quick_look_trash_focus = cx.focus_handle();
        let quick_look_add_tag_focus = cx.focus_handle();
        let quick_look_note_edit_focus = cx.focus_handle();
        let quick_look_note_save_focus = cx.focus_handle();
        let quick_look_note_cancel_focus = cx.focus_handle();
        let empty_index = Arc::new(PhotoIndex::new(Vec::new()));
        let mut app = Self {
            db,
            screen: Screen::Photos,
            sidebar_collapsed: initial_settings.ui_sidebar_collapsed,
            sidebar_motion_epoch: 0,
            inspector_collapsed: initial_settings.ui_inspector_collapsed,
            inspector_epoch: 0,
            hovered_nav: None,
            daemon_age_secs: None,
            library: PhotoBrowser::new(empty_index.clone()),
            search_view: PhotoBrowser::new(empty_index),
            library_dirty: false,
            library_error: None,
            count_debounce: CountDebounce::default(),
            selected: None,
            selection_epoch: 0,
            selection_changed_at: None,
            selected_photo: None,
            selected_players: Vec::new(),
            selected_tags: Vec::new(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            thumbnails: ThumbnailCache::default(),
            grid_epoch: 0,
            hovered_photo: None,
            intro_seen: HashMap::new(),
            photo_grid_focus,
            quick_look_open: false,
            quick_look_epoch: 0,
            quick_look_return_focus: None,
            quick_look_focus_index: 0,
            quick_look_details_open: false,
            quick_look_details_epoch: 0,
            quick_look_photo_ids: Vec::new(),
            quick_look_index: 0,
            quick_look_close_focus,
            quick_look_prev_focus,
            quick_look_next_focus,
            quick_look_details_focus,
            quick_look_favorite_focus,
            quick_look_compress_focus,
            quick_look_trash_focus,
            quick_look_add_tag_focus,
            quick_look_note_edit_focus,
            quick_look_note_save_focus,
            quick_look_note_cancel_focus,
            preview: None,
            previous_preview: None,
            previous_color: None,
            profiles: Vec::new(),
            profile_index: 0,
            stats: LibraryStats::default(),
            settings: AppSettings::default(),
            jobs: Vec::new(),
            daemon_errors: Vec::new(),
            failed_job_count: 0,
            failed_jobs: Vec::new(),
            failed_job_names: HashMap::new(),
            monitor_world: None,
            monitor_instance: None,
            monitor_players: Vec::new(),
            daemon_active: false,
            world_input,
            player_input,
            tag_input,
            tag_edit_input,
            note_edit_input,
            note_edit_id: None,
            photo_root_input,
            log_dir_input,
            import_dir_input,
            search_active: false,
            searching: false,
            search_request: 0,
            applied_search: None,
            pending_search_anchor: None,
            search_refresh_pending: false,
            search_favorite: false,
            search_days: Some(7),
            compress_policy: "keep".to_string(),
            delete_armed: None,
            message: None,
            demo,
            demo_assets,
        };
        app.ensure_profiles();
        app.reload_all(true, cx);
        if app.library_error.is_none() && app.library.index.total == 0 {
            message.get_or_insert_with(|| {
                "写真がありません。VRChatの写真フォルダを設定するか、--demo で起動してください。"
                    .to_string()
            });
        }
        app.message = message;
        if let Some(id) = app.library.index.entries.first().map(|entry| entry.id) {
            app.select_photo(id, cx);
            // The grid is not mounted until the first frame, so focus it once
            // the initial render has attached it to the focus tree.
            let photo_grid_focus = app.photo_grid_focus.clone();
            window.defer(cx, move |window, cx| {
                window.focus(&photo_grid_focus, cx);
            });
        }
        if !app.demo
            && !app.daemon_active
            && let Err(error) = spawn_daemon_process()
        {
            app.message = Some(format!("デーモンを起動できません: {error}"));
        }
        app.start_auto_refresh(cx);
        if timing_enabled() {
            let started = process_start();
            cx.on_next_frame(window, move |_, _, _| {
                log_timing("first frame (from process start)", started)
            });
        }
        app
    }

    fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                if this.update(cx, |this, cx| this.refresh_live(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let count = self.db.count_photos();
        if let Ok(stats) = self.db.stats() {
            self.stats = stats;
        }
        self.reload_job_state();
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
        }
        // Only compare counts when the query succeeds; treating a transient
        // error as zero used to blank the whole library.
        if let Ok(count) = count {
            if count != self.library.index.total as i64 {
                // A burst of files arriving from the daemon must not rebuild
                // the 100k-entry index on every three-second tick. Wait until
                // the observed count has been stable for a quiet period.
                self.mark_library_dirty();
                if self.screen == Screen::Photos
                    && self
                        .count_debounce
                        .observe(count, Instant::now(), Duration::from_secs(2))
                    && self.reload_library(true, cx)
                {
                    self.count_debounce.reset();
                }
            } else {
                self.count_debounce.reset();
            }
        }
        cx.notify();
    }

    fn reload_job_state(&mut self) {
        self.jobs = self.db.list_jobs(20).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load recent compression jobs");
            Vec::new()
        });
        self.failed_job_count = self.db.count_jobs(QueueStatus::Failed).unwrap_or(0);
        self.failed_jobs = self
            .db
            .list_jobs_by_status(QueueStatus::Failed, 100)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot load failed compression jobs");
                Vec::new()
            });
        self.failed_job_names.clear();
        for job in &self.failed_jobs {
            if self.failed_job_names.contains_key(&job.photo_id) {
                continue;
            }
            if let Ok(Some(photo)) = self.db.photo_by_id(job.photo_id) {
                self.failed_job_names.insert(job.photo_id, photo.file_name);
            }
        }
        self.daemon_errors = self.db.daemon_errors().unwrap_or_default();
    }

    fn retry_job(&mut self, id: String, cx: &mut Context<Self>) {
        match self.db.retry_job(&id) {
            Ok(true) => {
                self.message = Some("再試行をキューに戻しました".to_string());
                self.reload_job_state();
            }
            Ok(false) => {
                self.message = Some(format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました"));
            }
            Err(error) => {
                tracing::warn!(%error, %id, "cannot retry compression job");
                self.message = Some(format!("再試行できません: {error}"));
            }
        }
        cx.notify();
    }

    fn retry_all_failed_jobs(&mut self, cx: &mut Context<Self>) {
        match self.db.retry_failed_jobs() {
            Ok(count) => {
                self.message = Some(format!("{count} 件を再試行に戻しました（上限到達は除外）"));
                self.reload_job_state();
            }
            Err(error) => {
                tracing::warn!(%error, "cannot retry failed compression jobs");
                self.message = Some(format!("再試行できません: {error}"));
            }
        }
        cx.notify();
    }

    fn ensure_profiles(&mut self) {
        self.profiles = self.db.ensure_default_profiles().unwrap_or_default();
    }

    /// Refreshes side panels without touching the library snapshot or pages.
    fn reload_aux(&mut self, cx: &mut Context<Self>) {
        self.profiles = self.db.list_profiles().unwrap_or_default();
        if self.profile_index >= self.profiles.len() {
            self.profile_index = 0;
        }
        self.stats = self.db.stats().unwrap_or_default();
        self.compressed_ids = self
            .db
            .compressed_photo_ids()
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.reload_job_state();
        self.settings = AppSettings::load(&self.db).unwrap_or_default();
        cx.set_reduce_motion(self.settings.ui_reduce_motion);
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected(id);
        }
    }

    /// Rebuilds the ordered library snapshot and its page cache.
    ///
    /// With `preserve_anchor`, the photo at the top-left of the first visible
    /// row stays at the top after the rebuild. A root change or an explicit
    /// reset passes `false` to return the grid to the first row.
    ///
    /// On a query error the previous snapshot, pages, and scroll offset stay
    /// mounted and `library_error` exposes the retry action. Returns `true`
    /// when the snapshot was replaced.
    fn reload_library(&mut self, preserve_anchor: bool, cx: &mut Context<Self>) -> bool {
        let anchor = preserve_anchor.then(|| {
            anchor_photo_id(
                &self.library.index,
                list_scroll_top(&self.library.scroll),
                self.library.columns,
            )
        });
        let anchor = anchor.flatten();

        let index_start = Instant::now();
        let result = self.db.photo_index(PhotoSort::Newest);
        log_timing("photo_index", index_start);
        let error = apply_library_index(&mut self.library, result);
        if let Some(error) = error {
            self.library_error = Some(error);
            // Leave the mounted view and side panels untouched; the next
            // successful reload repopulates them.
            return false;
        }

        self.library_error = None;
        self.library_dirty = false;
        let columns = self.library.columns.max(1);
        if let Some(row) = anchor.and_then(|id| self.library.index.row_of_id(id)) {
            self.library
                .scroll
                .scroll_to_item_strict(row / columns, ScrollStrategy::Top);
        } else {
            self.library
                .scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }

        // An active result set stays mounted until the user clears it or runs
        // a new search; the inactive mirror only needs the fresh snapshot.
        if !self.search_active {
            self.search_view.replace_index(self.library.index.clone());
        }
        self.reload_aux(cx);
        true
    }

    /// Full rebuild used at startup: resets the grid animation state too.
    fn reload_all(&mut self, reset_scroll: bool, cx: &mut Context<Self>) {
        self.grid_epoch = self.grid_epoch.wrapping_add(1);
        self.intro_seen.clear();
        self.hovered_photo = None;
        self.reload_library(!reset_scroll, cx);
    }

    fn mark_library_dirty(&mut self) {
        self.library_dirty = true;
    }

    /// Changes screens without rebuilding the library unless Photos was marked
    /// dirty while another screen was mounted.
    fn switch_screen(&mut self, screen: Screen, cx: &mut Context<Self>) {
        self.screen = screen;
        if screen == Screen::Photos && self.library_dirty {
            self.reload_library(true, cx);
        } else if screen == Screen::Search && !self.search_active {
            // The no-query mirror only needs a refresh when the library
            // snapshot changed while another screen was mounted. A dirty
            // library is rebuilt first so the mirror also sees new photos.
            if self.library_dirty {
                self.reload_library(true, cx);
            } else {
                sync_search_mirror(&mut self.search_view, &self.library);
            }
        }
        cx.notify();
    }

    fn refresh_monitor(&mut self) {
        let now = Utc::now().timestamp();
        self.daemon_age_secs = self
            .db
            .get_setting("daemon_heartbeat")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|timestamp| (now - timestamp).max(0));
        self.daemon_active = self.daemon_age_secs.is_some_and(|age| age < 20);
        self.monitor_world = None;
        self.monitor_instance = None;
        self.monitor_players.clear();
        if let Ok(Some(session)) = self.db.session_at(now) {
            if let Ok(Some(world)) = self.db.world_by_id(session.world_id) {
                self.monitor_world = world.alias.or(world.name);
            }
            self.monitor_instance = session.instance_id;
            if let Ok(players) = self.db.players_at(session.id, now) {
                self.monitor_players = players
                    .into_iter()
                    .map(|player| player.display_name)
                    .collect();
            }
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.sidebar_motion_epoch = self.sidebar_motion_epoch.wrapping_add(1);
        self.hovered_nav = None;
        self.settings.ui_sidebar_collapsed = self.sidebar_collapsed;
        let _ = self.settings.save(&self.db);
        cx.notify();
    }

    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        self.inspector_collapsed = !self.inspector_collapsed;
        self.inspector_epoch = self.inspector_epoch.wrapping_add(1);
        self.settings.ui_inspector_collapsed = self.inspector_collapsed;
        let _ = self.settings.save(&self.db);
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        self.switch_screen(Screen::Search, cx);
        let focus_handle = self.world_input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        // The search screen may not be mounted yet when the action fires, so
        // retry after the deferred update has attached the input to the tree.
        window.defer(cx, move |window, cx| {
            window.focus(&focus_handle, cx);
        });
        cx.notify();
    }

    fn color_for(&self, photo: &Photo) -> u32 {
        self.thumbnails
            .color(photo.id)
            .unwrap_or_else(|| scene_color(photo.world_name.as_deref().unwrap_or("")))
    }

    fn selected_color(&self) -> u32 {
        self.selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(BG)
    }

    /// Reloads the metadata for a photo without decoding its 1200 px preview.
    /// Favorite and tag edits use this so the preview does not flash.
    fn load_selected_meta(&mut self, id: i64) {
        self.selected = Some(id);
        self.selected_photo = self.db.photo_by_id(id).ok().flatten();
        self.selected_players = self.db.photo_players(id).unwrap_or_default();
        self.selected_tags = self.db.photo_tags(id).unwrap_or_default();
        self.selected_derivatives = self.db.derivatives_for_photo(id).unwrap_or_default();
        if self.note_edit_id.is_some_and(|edit_id| edit_id != id) {
            self.note_edit_id = None;
        }
    }

    fn load_selected(&mut self, id: i64) {
        self.load_selected_meta(id);
        self.preview = self
            .selected_photo
            .as_ref()
            .and_then(|photo| load_preview(Path::new(&photo.abs_path)));
    }

    fn begin_note_edit(
        &mut self,
        id: i64,
        note: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_edit_id = Some(id);
        let text = note.unwrap_or_default();
        self.note_edit_input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        let focus = self.note_edit_input.read(cx).focus_handle(cx);
        // The textarea mounts on the next frame, after the editor state flips.
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id == Some(id)
            {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    fn save_note(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.note_edit_input.read(cx).value();
        let note = normalize_note(&value);
        if let Err(error) = self.db.set_note(id, note) {
            self.message = Some(format!("メモを保存できません: {error}"));
            cx.notify();
            return;
        }
        self.note_edit_id = None;
        self.load_selected_meta(id);
        let focus = self.quick_look_note_edit_focus.clone();
        // The edit action mounts after the textarea disappears.
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id.is_none() {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    fn cancel_note_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_edit_id.take().is_none() {
            return;
        }
        let focus = self.quick_look_note_edit_focus.clone();
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id.is_none() {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    fn handle_viewer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_edit_id.is_some() {
            self.cancel_note_edit(window, cx);
        } else {
            self.close_quick_look(window, cx);
        }
    }

    /// Starts a background decode for a grid thumbnail when the cache has a
    /// free slot. Cached, pending, and previously failed photos are skipped.
    fn request_thumbnail(&mut self, photo: &Photo, cx: &mut Context<Self>) {
        if !self.thumbnails.begin(photo.id) {
            return;
        }
        let id = photo.id;
        let source = PathBuf::from(&photo.abs_path);
        let cache_path = match &self.demo_assets {
            Some(assets) => demo_thumb_path(assets, id),
            None => thumbnail::thumbnail_path(id).unwrap_or_default(),
        };
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    let decode_start = Instant::now();
                    let decoded = decode_thumbnail(id, &cache_path, &source);
                    log_timing("thumbnail decode", decode_start);
                    decoded
                })
                .await;
            this.update(cx, |this, cx| {
                match decoded {
                    Some((color, rgba)) => {
                        this.thumbnails.insert(id, make_render_image(rgba), color);
                    }
                    None => this.thumbnails.fail(id),
                }
                // The selected photo may have been refused earlier because
                // every in-flight slot was busy; a freed slot is its retry.
                this.retry_pinned_thumbnail(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-requests the pinned (selected) thumbnail if it is still missing.
    /// `begin` ignores cached, pending, failed, and over-cap requests, so this
    /// is safe to call after every decode completion.
    fn retry_pinned_thumbnail(&mut self, cx: &mut Context<Self>) {
        let Some(pinned) = self.thumbnails.pinned else {
            return;
        };
        if self.thumbnails.get(pinned).is_some() {
            return;
        }
        let Some(photo) = self
            .selected_photo
            .clone()
            .filter(|photo| photo.id == pinned)
        else {
            return;
        };
        self.request_thumbnail(&photo, cx);
    }

    fn select_photo(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.delete_armed != Some(id) {
            self.delete_armed = None;
        }
        if self.selected != Some(id) {
            let previous_color = self
                .selected_photo
                .as_ref()
                .map(|photo| self.color_for(photo));
            self.previous_preview = self.preview.take();
            self.previous_color = previous_color;
            self.selection_epoch = self.selection_epoch.wrapping_add(1);
            self.selection_changed_at = Some(Instant::now());
        }
        self.thumbnails.pin(id);
        self.load_selected(id);
        if self.thumbnails.get(id).is_none()
            && let Some(photo) = self.selected_photo.clone()
        {
            self.request_thumbnail(&photo, cx);
        }
        cx.notify();
    }

    fn open_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_look_open || self.selected_photo.is_none() {
            return;
        }
        self.message = None;
        self.delete_armed = None;
        self.note_edit_id = None;
        self.quick_look_return_focus = window.focused(cx);
        self.quick_look_photo_ids = self.viewer_source_ids();
        self.quick_look_index = self
            .quick_look_photo_ids
            .iter()
            .position(|id| Some(*id) == self.selected)
            .unwrap_or(0);
        self.quick_look_open = true;
        self.quick_look_epoch = self.quick_look_epoch.wrapping_add(1);
        self.quick_look_focus_index = 0;
        // Remember the last details state for the session, but render it
        // statically: only an explicit toggle animates the panel.
        self.quick_look_details_epoch = 0;
        let close_focus = self.quick_look_close_focus.clone();
        window.focus(&close_focus, cx);
        // The close button mounts with the overlay on this frame, so make the
        // focus stick once the focus tree contains it. Guard the retry so a
        // close (or close+reopen) cannot steal focus back to a stale overlay.
        let epoch = self.quick_look_epoch;
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_epoch == epoch {
                window.focus(&close_focus, cx);
            }
        });
        cx.notify();
    }

    fn close_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open {
            return;
        }
        self.quick_look_open = false;
        self.delete_armed = None;
        self.note_edit_id = None;
        let focus = self
            .quick_look_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes a viewer whose backing photo vanished (for example, an external
    /// delete picked up by the three-second refresh) and restores focus.
    fn guard_viewer_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open || self.selected_photo.is_some() {
            return;
        }
        self.quick_look_open = false;
        self.delete_armed = None;
        self.note_edit_id = None;
        if let Some(id) = self.selected.take() {
            self.thumbnails.unpin(id);
        }
        self.quick_look_photo_ids.clear();
        self.quick_look_index = 0;
        self.preview = None;
        self.previous_preview = None;
        self.previous_color = None;
        let focus = self
            .quick_look_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        cx.defer_in(window, move |this, window, cx| {
            if !this.quick_look_open {
                window.focus(&focus, cx);
            }
        });
    }

    fn viewer_source_ids(&self) -> Vec<i64> {
        let browser = if self.screen == Screen::Search && self.search_active {
            &self.search_view
        } else {
            &self.library
        };
        let source: Vec<i64> = browser.index.entries.iter().map(|entry| entry.id).collect();
        match self.selected {
            Some(id) if source.contains(&id) => source,
            Some(id) => vec![id],
            None => Vec::new(),
        }
    }

    fn step_viewer_photo(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open || self.quick_look_photo_ids.is_empty() {
            return;
        }
        let next = step_index(
            self.quick_look_index,
            self.quick_look_photo_ids.len(),
            delta,
        );
        if next == self.quick_look_index {
            return;
        }
        self.quick_look_index = next;
        let focused = window.focused(cx);
        let prev_disabled = next == 0 && focused.as_ref() == Some(&self.quick_look_prev_focus);
        let next_disabled = next + 1 == self.quick_look_photo_ids.len()
            && focused.as_ref() == Some(&self.quick_look_next_focus);
        if prev_disabled || next_disabled {
            self.quick_look_focus_index = 0;
            window.focus(&self.quick_look_close_focus, cx);
        }
        // Keep the top-bar navigation focused; only the library selection and
        // the displayed photo change behind the overlay.
        self.select_photo(self.quick_look_photo_ids[next], cx);
        cx.notify();
    }

    fn toggle_quick_look_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.quick_look_details_open = !self.quick_look_details_open;
        if !self.quick_look_details_open {
            self.note_edit_id = None;
        }
        self.quick_look_details_epoch = self.quick_look_details_epoch.wrapping_add(1);
        let index = self
            .quick_look_focus_order(cx)
            .iter()
            .position(|handle| *handle == self.quick_look_details_focus)
            .unwrap_or(0);
        self.quick_look_focus_index = index;
        window.focus(&self.quick_look_details_focus, cx);
        cx.notify();
    }

    fn quick_look_focus_order(&self, cx: &gpui_kit::App) -> Vec<FocusHandle> {
        let index = self
            .quick_look_index
            .min(self.quick_look_photo_ids.len().saturating_sub(1));
        let mut order = vec![self.quick_look_close_focus.clone()];
        if index > 0 {
            order.push(self.quick_look_prev_focus.clone());
        }
        if index + 1 < self.quick_look_photo_ids.len() {
            order.push(self.quick_look_next_focus.clone());
        }
        order.push(self.quick_look_details_focus.clone());
        if self.quick_look_details_open {
            order.push(self.quick_look_favorite_focus.clone());
            order.push(self.tag_edit_input.read(cx).focus_handle(cx));
            order.push(self.quick_look_add_tag_focus.clone());
            if self.note_edit_id.is_some() {
                order.push(self.note_edit_input.read(cx).focus_handle(cx));
                order.push(self.quick_look_note_save_focus.clone());
                order.push(self.quick_look_note_cancel_focus.clone());
            } else {
                order.push(self.quick_look_note_edit_focus.clone());
            }
            order.push(self.quick_look_compress_focus.clone());
            order.push(self.quick_look_trash_focus.clone());
        }
        order
    }

    fn cycle_quick_look_focus(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.quick_look_focus_order(cx);
        if order.is_empty() {
            return;
        }
        // Clicks move focus without touching `quick_look_focus_index`, so
        // derive the position from the actual focus before stepping.
        let current = window
            .focused(cx)
            .and_then(|focused| order.iter().position(|handle| *handle == focused))
            .unwrap_or(self.quick_look_focus_index);
        let next = next_focus_index(current, order.len(), reverse);
        self.quick_look_focus_index = next;
        window.focus(&order[next], cx);
    }

    fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = &self.selected_photo else {
            return;
        };
        let id = photo.id;
        let path = photo.abs_path.clone();
        if self.delete_armed != Some(id) {
            self.delete_armed = Some(id);
            cx.notify();
            return;
        }
        match trash::delete(&path) {
            Ok(_) => {
                let _ = self.db.mark_missing(id);
                self.library.remove_photo(id);
                self.search_view.remove_photo(id);
                self.thumbnails.unpin(id);
                self.selected = None;
                self.selected_photo = None;
                self.selected_players.clear();
                self.selected_tags.clear();
                self.selected_derivatives.clear();
                self.preview = None;
                self.delete_armed = None;
                self.message = Some("写真をゴミ箱へ移動しました".to_string());
                self.reload_aux(cx);
            }
            Err(error) => {
                self.delete_armed = None;
                self.message = Some(format!("ゴミ箱へ移動できません: {error}"));
            }
        }
        cx.notify();
    }

    fn enqueue_selected(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = &self.selected_photo else {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        };
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.message = Some("圧縮プロファイルがありません".to_string());
            cx.notify();
            return;
        };
        let policy = self.compress_policy.clone();
        match self.db.enqueue_job(photo.id, &profile.id, &policy) {
            Ok(_) => {
                self.message = Some(if policy == "replace" {
                    "圧縮ジョブを追加しました（成功後に元画像をゴミ箱へ移動）".to_string()
                } else {
                    "圧縮ジョブをキューに追加しました（元画像を保持）".to_string()
                });
                self.reload_aux(cx);
            }
            Err(error) => self.message = Some(format!("キュー追加に失敗: {error}")),
        }
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        // One 100k text query at a time; the Search button is disabled while a
        // request is in flight, but the favorite switch and presets also call
        // this method.
        if self.searching {
            return;
        }
        let world = self.world_input.read(cx).value().to_string();
        let player = self.player_input.read(cx).value().to_string();
        let tag = self.tag_input.read(cx).value().to_string();
        let from_utc = self
            .search_days
            .map(|days| Utc::now().timestamp() - days * 86_400);
        let filter = PhotoTextFilter {
            world: Some(world),
            player: Some(player),
            tag: Some(tag),
            favorite: self.search_favorite,
            from_utc,
            to_utc: None,
        };
        self.applied_search = Some(filter.clone());
        self.pending_search_anchor = None;
        self.search_refresh_pending = false;
        self.start_search(filter, cx);
    }

    fn start_search(&mut self, filter: PhotoTextFilter, cx: &mut Context<Self>) {
        self.search_request = self.search_request.wrapping_add(1);
        let generation = self.search_request;

        let Some(path) = self.db.path().map(Path::to_path_buf) else {
            // In-memory demo/fallback databases have no second handle to open.
            let result = self.db.photo_index_text(&filter);
            self.apply_search_result(generation, result, cx);
            return;
        };

        self.searching = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let background_filter = filter.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let search_start = Instant::now();
                    let result = Db::open_read_only(path)
                        .and_then(|db| db.photo_index_text(&background_filter));
                    log_timing("search photo_index_text", search_start);
                    result
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.search_request {
                    return;
                }
                match result {
                    Ok(entries) => this.apply_search_result(generation, Ok(entries), cx),
                    // A read-only handle can fail on a locked or WAL-only
                    // database. Query the live connection on the UI thread
                    // rather than leaving the spinner up.
                    Err(_) => {
                        let fallback = this.db.photo_index_text(&filter);
                        this.apply_search_result(generation, fallback, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Applies a completed search to the virtual grid. A stale generation (the
    /// user searched again or cleared the view) is discarded.
    fn apply_search_result(
        &mut self,
        generation: u64,
        result: vrc_photo_core::error::Result<Vec<PhotoIndexEntry>>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.search_request {
            return;
        }
        match result {
            Ok(entries) => {
                self.search_view
                    .replace_index(Arc::new(PhotoIndex::new(entries)));
                let requested_anchor = self.pending_search_anchor.take();
                let anchor = search_anchor_after_result(requested_anchor, &self.search_view.index);
                let columns = self.search_view.columns.max(1);
                let row = anchor.and_then(|id| self.search_view.index.row_of_id(id));
                match row {
                    Some(row) => self
                        .search_view
                        .scroll
                        .scroll_to_item_strict(row / columns, ScrollStrategy::Top),
                    None => self
                        .search_view
                        .scroll
                        .scroll_to_item_strict(0, ScrollStrategy::Top),
                }
                self.search_active = true;
                self.searching = false;
                // A previous failure must not linger above a fresh result.
                self.message = None;
                if self.search_refresh_pending {
                    self.search_refresh_pending = false;
                    self.start_search_refresh(anchor, cx);
                }
            }
            Err(error) => {
                self.pending_search_anchor = None;
                self.search_refresh_pending = false;
                self.searching = false;
                self.message = Some(format!("検索に失敗しました: {error}"));
            }
        }
        cx.notify();
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search_active = false;
        self.searching = false;
        self.applied_search = None;
        self.pending_search_anchor = None;
        self.search_refresh_pending = false;
        self.message = None;
        // Invalidate an in-flight request so it cannot revive the cleared view.
        self.search_request = self.search_request.wrapping_add(1);
        let index = self.library.index.clone();
        self.search_view.replace_index(index);
        self.search_view
            .scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn refresh_search_after_tag_edit(&mut self, cx: &mut Context<Self>) {
        if !self.search_active {
            return;
        }
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !has_tag_filter(&filter) {
            return;
        }
        if self.searching {
            // The in-flight query may have started before the tag write.
            // Apply its result first, then refresh from the new state.
            self.search_refresh_pending = true;
            return;
        }
        let anchor = anchor_photo_id(
            &self.search_view.index,
            list_scroll_top(&self.search_view.scroll),
            self.search_view.columns,
        );
        self.start_search_refresh(anchor, cx);
    }

    fn start_search_refresh(&mut self, anchor: Option<i64>, cx: &mut Context<Self>) {
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !has_tag_filter(&filter) {
            return;
        }
        self.pending_search_anchor = anchor;
        self.start_search(filter, cx);
    }

    fn commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>) {
        if let Err(error) = self.db.set_photo_tags(photo_id, &tags) {
            self.message = Some(format!("タグを保存できません: {error}"));
            cx.notify();
            return;
        }
        self.load_selected_meta(photo_id);
        // A successful write is authoritative even if the metadata read failed.
        if self.selected_tags.is_empty() && !tags.is_empty() {
            self.selected_tags = tags;
        }
        self.refresh_search_after_tag_edit(cx);
        cx.notify();
    }

    fn add_tag(&mut self, photo_id: i64, tag: String, cx: &mut Context<Self>) {
        let tags = match self.db.photo_tags(photo_id) {
            Ok(tags) => tags,
            Err(error) => {
                self.message = Some(format!("タグを読み込めません: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(tags) = with_tag(&tags, &tag) else {
            return;
        };
        self.commit_tags(photo_id, tags, cx);
    }

    fn remove_tag(&mut self, photo_id: i64, tag: &str, cx: &mut Context<Self>) {
        let tags = match self.db.photo_tags(photo_id) {
            Ok(tags) => tags,
            Err(error) => {
                self.message = Some(format!("タグを読み込めません: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(tags) = without_tag(&tags, tag) else {
            return;
        };
        self.commit_tags(photo_id, tags, cx);
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let photo_count = format_count(self.stats.photo_count);
        let pending_jobs = self
            .jobs
            .iter()
            .filter(|job| matches!(job.status, QueueStatus::Pending | QueueStatus::Processing))
            .count();
        let compression_count = (pending_jobs > 0).then(|| format_count(pending_jobs as i64));

        let brand_text = v_flex()
            .min_w_0()
            .gap(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("VRC Photo"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(format!("Manager · {photo_count}枚")),
            )
            .with_animation(
                SharedString::from(format!("sidebar-brand-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        let body = v_flex()
            .relative()
            .flex_shrink_0()
            .h_full()
            .py(px(16.))
            .px(px(12.))
            .gap(px(3.))
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgba(0xffffff14))
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(2.))
                    .gap(px(10.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center().gap(px(0.)))
                    .child(
                        div()
                            .size(px(34.))
                            .rounded(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(linear_gradient(
                                135.,
                                linear_color_stop(rgb(BLUE), 0.),
                                linear_color_stop(rgb(PURPLE), 1.),
                            ))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(0xffffff))
                            .child("VP"),
                    )
                    .child(brand_text),
            )
            .child(div().h(px(12.)))
            .child(self.nav_item(
                "写真",
                Screen::Photos,
                IconName::GalleryVerticalEnd,
                Some(photo_count.clone()),
                cx,
            ))
            .child(self.nav_item("検索", Screen::Search, IconName::Search, None, cx))
            .child(self.nav_item(
                "圧縮",
                Screen::Compression,
                IconName::Minimize,
                compression_count,
                cx,
            ))
            .child(self.nav_item("監視", Screen::Monitor, IconName::Eye, None, cx))
            .child(self.nav_item("設定", Screen::Settings, IconName::Settings, None, cx))
            .child(div().flex_1())
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .p(px(11.))
                        .gap(px(3.))
                        .rounded(px(13.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgba(0xffffff0f))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("ライブラリ"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(rgb(TEXT))
                                .child(format!("{photo_count} 枚")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(format_bytes(self.stats.total_bytes)),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(26.))
                    .gap(px(7.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center())
                    .when(!collapsed, |this| this.px(px(9.)))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(if self.daemon_active { GREEN } else { MUTED_2 })),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(if self.daemon_active { MUTED } else { MUTED_2 }))
                                .child(if self.daemon_active {
                                    "監視中"
                                } else {
                                    "デーモン停止"
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(if collapsed { 9. } else { 10. }))
                    .text_color(rgb(MUTED_2))
                    .child(if self.demo_assets.is_some() {
                        "DEMO DATA".to_string()
                    } else {
                        format!("v{}", env!("CARGO_PKG_VERSION"))
                    }),
            );

        body.with_animation(
            ("sidebar", epoch),
            Animation::new(if epoch == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(440)
            })
            .with_easing(ease_motion)
            .with_max_fps(60.0),
            move |el, t| {
                let from = sidebar_width(!collapsed);
                let to = sidebar_width(collapsed);
                el.w(px(from + (to - from) * t))
            },
        )
        .into_any_element()
    }

    fn nav_item(
        &mut self,
        label: &'static str,
        screen: Screen,
        icon: IconName,
        count: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.screen == screen;
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let icon_color = if active { BLUE } else { MUTED };
        let icon_element = if active {
            Icon::new(icon)
                .with_size(px(17.))
                .text_color(rgb(icon_color))
        } else {
            Icon::new(icon).with_size(px(17.))
        };

        let label_block = h_flex()
            .min_w_0()
            .items_center()
            .justify_between()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(div().text_size(px(12.)).child(label))
            .when_some(count, |this, count| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .font_family("monospace")
                        .text_color(rgb(if active { BLUE } else { MUTED_2 }))
                        .child(count),
                )
            })
            .with_animation(
                SharedString::from(format!("sidebar-label-{label}-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        h_flex()
            .id(label)
            .relative()
            .h(px(40.))
            .w_full()
            .rounded(px(11.))
            .cursor_pointer()
            .items_center()
            .when(collapsed, |this| this.justify_center())
            .when(!collapsed, |this| this.px(px(11.)).gap(px(11.)))
            .when(active, |this| {
                this.bg(rgb(ACCENT_SOFT)).text_color(rgb(0xeaf4ff))
            })
            .when(!active, |this| {
                this.text_color(rgb(MUTED))
                    .hover(|this| this.bg(rgb(0x161d28)).text_color(rgb(TEXT)))
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if !this.sidebar_collapsed {
                    if this.hovered_nav.is_some() {
                        this.hovered_nav = None;
                        cx.notify();
                    }
                    return;
                }
                let next = hovered.then_some(screen);
                if this.hovered_nav != next {
                    this.hovered_nav = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_screen(screen, cx);
            }))
            .when(active, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(11.))
                        .left(px(if collapsed { 5. } else { -1. }))
                        .w(px(2.))
                        .h(px(18.))
                        .rounded_full()
                        .bg(rgb(BLUE)),
                )
            })
            .child(icon_element)
            .child(label_block)
            .into_any_element()
    }

    fn nav_tooltip(&self) -> AnyElement {
        let Some(screen) = self.hovered_nav.filter(|_| self.sidebar_collapsed) else {
            return div().into_any_element();
        };
        let index = match screen {
            Screen::Photos => 0,
            Screen::Search => 1,
            Screen::Compression => 2,
            Screen::Monitor => 3,
            Screen::Settings => 4,
        };

        div()
            .absolute()
            .left(px(86.))
            .top(px(62. + index as f32 * 43. + 8.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(7.))
            .bg(rgb(0x161c26))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .text_size(px(10.))
            .text_color(rgb(TEXT))
            .whitespace_nowrap()
            .child(screen.title())
            .into_any_element()
    }

    fn rail_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronLeft
        };
        let tooltip = if collapsed {
            "サイドバーを展開 (Ctrl+B)"
        } else {
            "サイドバーを最小化 (Ctrl+B)"
        };

        div()
            .absolute()
            .top(px(18.))
            .left(px(sidebar_width(collapsed) - 12.))
            .child(
                Button::new("rail-toggle")
                    .ghost()
                    .icon(icon)
                    .tooltip(tooltip)
                    .size(px(24.))
                    .rounded_full()
                    .bg(rgb(0x131923))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
                    .text_color(rgb(MUTED))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .with_animation(
                ("rail-toggle", epoch),
                Animation::new(if epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(440)
                })
                .with_easing(ease_motion)
                .with_max_fps(60.0),
                move |el, t| {
                    let from = sidebar_width(!collapsed);
                    let to = sidebar_width(collapsed);
                    el.left(px(from + (to - from) * t - 12.))
                },
            )
            .into_any_element()
    }

    fn screen_header(
        &self,
        title: &'static str,
        subtitle: String,
        message: Option<String>,
        actions: Vec<AnyElement>,
    ) -> AnyElement {
        h_flex()
            .h(px(72.))
            .flex_shrink_0()
            .px(px(26.))
            .gap(px(14.))
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when_some(message, |this, message| {
                        this.text_size(px(10.))
                            .text_color(rgb(AMBER))
                            .truncate()
                            .child(message)
                    }),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(8.))
                    .children(actions),
            )
            .into_any_element()
    }

    /// One compact row per failure domain. Detail text stays on the target
    /// screen so the bar never truncates the actual reason.
    fn global_alerts(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        alert_summaries(self.failed_job_count, self.daemon_errors.len(), 0)
            .into_iter()
            .map(|(target, message)| {
                let (id, action) = match target {
                    Screen::Photos => ("alert-photos", "写真を開く"),
                    Screen::Compression => ("alert-compression", "圧縮を開く"),
                    Screen::Search => ("alert-search", "検索を開く"),
                    Screen::Monitor => ("alert-monitor", "ログ監視を開く"),
                    Screen::Settings => ("alert-settings", "設定を開く"),
                };
                h_flex()
                    .flex_shrink_0()
                    .w_full()
                    .items_center()
                    .gap(px(10.))
                    .px(px(26.))
                    .py(px(8.))
                    .bg(rgb(PANEL))
                    .border_b_1()
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
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(RED))
                            .child(message),
                    )
                    .child(
                        Button::new(id)
                            .label(action)
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.switch_screen(target, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let screen = match self.screen {
            Screen::Photos => self.photos_screen(window, cx).into_any_element(),
            Screen::Compression => self.compression_screen(cx).into_any_element(),
            Screen::Search => self.search_screen(window, cx).into_any_element(),
            Screen::Monitor => self.monitor_screen(cx).into_any_element(),
            Screen::Settings => self.settings_screen(cx).into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .children(self.global_alerts(cx))
            .child(screen)
            .into_any_element()
    }

    fn photos_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.photos_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.photo_grid(false, window, cx))
                    .child(self.inspector_column(cx)),
            )
    }

    fn photos_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let sync = if self.demo_assets.is_some() {
            "デモデータ".to_string()
        } else if let Some(age) = self.daemon_age_secs
            && age < 20
        {
            format!("最終同期 {age}秒前")
        } else {
            "デーモン停止".to_string()
        };
        let subtitle = format!(
            "{} 枚 · {} · {}",
            format_count(self.stats.photo_count),
            format_bytes(self.stats.total_bytes),
            sync
        );
        let message = self.message.clone();
        let inspector_collapsed = self.inspector_collapsed;
        let library_error = self.library_error.clone();

        let actions = vec![
            Button::new("library-reload")
                .label("再読込")
                .small()
                .ghost()
                .tooltip("写真一覧を再読み込み")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.reload_library(true, cx);
                }))
                .into_any_element(),
            Button::new("inspector-toggle")
                .label(if inspector_collapsed {
                    "詳細"
                } else {
                    "詳細を隠す"
                })
                .small()
                .ghost()
                .tooltip("詳細パネル (Ctrl+I)")
                .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx)))
                .into_any_element(),
            Button::new("import-copy")
                .label("コピー取込")
                .small()
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.import_dialog(ImportMode::Copy, cx);
                }))
                .into_any_element(),
            Button::new("import-move")
                .label("移動取込")
                .small()
                .primary()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.import_dialog(ImportMode::Move, cx);
                }))
                .into_any_element(),
        ];

        let mut header = v_flex()
            .flex_shrink_0()
            .child(self.screen_header("ライブラリ", subtitle, message, actions))
            .child(
                h_flex()
                    .px(px(26.))
                    .pt(px(12.))
                    .pb(px(14.))
                    .gap(px(8.))
                    .items_center()
                    .child(chip("ワールド", true))
                    .child(chip("プレイヤー", false))
                    .child(chip("日時", false))
                    .child(chip("タグ", false))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("並び: 新しい順"),
                    ),
            );
        if let Some(error) = library_error {
            header = header.child(
                h_flex()
                    .px(px(26.))
                    .pb(px(12.))
                    .gap(px(10.))
                    .items_center()
                    .child(div().text_size(px(11.)).text_color(rgb(RED)).child(error))
                    .child(
                        Button::new("library-reload-banner")
                            .label("再読込")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_library(true, cx);
                            })),
                    ),
            );
        }
        header.into_any_element()
    }

    fn browser_mut(&mut self, search: bool) -> &mut PhotoBrowser {
        if search {
            &mut self.search_view
        } else {
            &mut self.library
        }
    }

    /// Virtualized grid shared by the library and the search screen.
    ///
    /// The range callback only plans missing pages and renders the page cache;
    /// database work is deferred through [`Self::load_pages`], so neither
    /// render nor prepaint ever touches SQLite.
    fn photo_grid(
        &mut self,
        search: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let width: f32 = window.viewport_size().width.into();
        let next_columns = grid_columns_for(
            search,
            width,
            self.sidebar_collapsed,
            self.inspector_collapsed,
        );
        let browser = self.browser_mut(search);
        browser.columns = next_columns;
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
                            .child("右上の「再読込」で再試行できます。"),
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
                container = container.pl(px(26.)).pr(px(18.)).pb(px(18.));
            }
            return container.child(empty).into_any_element();
        }

        let list_index = index.clone();
        let list = uniform_list(
            // Keep the list ID stable so uniform_list retains its scroll
            // offset when the column count changes; row height is fixed.
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
                        .h(px(PHOTO_ROW_HEIGHT))
                        .w_full()
                        .min_w_0()
                        .items_start()
                        .gap(px(16.));
                    for column in 0..columns {
                        let photo_index = row * columns + column;
                        if photo_index >= list_index.total {
                            line = line.child(div().flex_1());
                            continue;
                        }
                        match this.browser_mut(search).photo_at(photo_index) {
                            Some(photo) => {
                                line = line.child(this.photo_card(photo.as_ref(), photo_index, cx));
                            }
                            None => {
                                line = line.child(grid_placeholder());
                            }
                        }
                    }
                    rendered.push(line);
                }
                rendered
            }),
        )
        .track_scroll(&scroll)
        // Room for the first row's -4 px hover lift.
        .pt(px(4.))
        .flex_1()
        .w_full()
        .min_w_0()
        .min_h_0();

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
            // Added after the list so the rail owns input inside its 88 px
            // column instead of the cards underneath.
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
                        rail_drag,
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
            container = container.pl(px(26.)).pr(px(18.)).pb(px(18.));
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = photo.id;
        let selected = self.selected == Some(id);
        let compressed = self.compressed_ids.contains(&id);
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

        let mut thumb = div()
            .relative()
            .h(px(128.))
            .w_full()
            .rounded(px(14.))
            .overflow_hidden()
            .bg(rgb(color))
            .border_2()
            .border_color(if selected { rgb(BLUE) } else { rgb(BORDER) });
        if selected {
            thumb = thumb.shadow(vec![
                BoxShadow::new(px(0.), px(8.), rgba(0x66b3ff4d).into()).blur_radius(px(24.)),
            ]);
        }
        if let Some(image) = thumbnail {
            thumb = thumb.child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full(),
            );
        }
        let status_color = if compressed { GREEN } else { AMBER };
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
                            .child(if compressed { "圧縮済" } else { "未圧縮" }),
                    ),
            )
            .when(photo.is_favorite, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(9.))
                        .right(px(9.))
                        .size(px(7.))
                        .rounded_full()
                        .bg(rgb(AMBER))
                        .shadow(vec![
                            BoxShadow::new(px(0.), px(0.), rgba(0x00000080).into())
                                .blur_radius(px(3.)),
                        ]),
                )
            });

        let body = v_flex().min_w_0().gap(px(8.)).child(thumb).child(
            v_flex()
                .min_w_0()
                .gap(px(3.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_semibold()
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(world),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(MUTED_2))
                        .truncate()
                        .child(format!(
                            "{} · {}",
                            short_date(&photo.captured_at_local),
                            format_bytes(photo.file_size)
                        )),
                ),
        );

        let card = v_flex()
            .id(SharedString::from(format!("photo-{id}")))
            .relative()
            .w_full()
            .cursor_pointer()
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
                this.select_photo(id, cx);
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
                {
                    this.open_quick_look(window, cx);
                }
            }))
            .child(body)
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

    fn inspector_column(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.inspector_collapsed;
        let mut column = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if !collapsed {
            column = column.child(self.inspector(cx));
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
                    let from = if collapsed { INSPECTOR_WIDTH } else { 0.0 };
                    let to = if collapsed { 0.0 } else { INSPECTOR_WIDTH };
                    el.w(px(from + (to - from) * t))
                },
            )
            .into_any_element()
    }

    fn inspector(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
                .w(px(INSPECTOR_CARD_WIDTH))
                .h_full()
                .flex_shrink_0()
                .mr(px(INSPECTOR_MARGIN))
                .py(px(INSPECTOR_MARGIN))
                .child(
                    glass_panel()
                        .flex_1()
                        .w_full()
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
        let tint_layer = div().absolute().size_full().bg(rgb(current_color));
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
        if let Some(thumbnail) = thumbnail {
            card = card.child(
                div()
                    .absolute()
                    .top(px(-24.))
                    .left(px(-24.))
                    .right(px(-24.))
                    .bottom(px(-24.))
                    .opacity(0.45)
                    .child(
                        gpui_kit::img(thumbnail)
                            .object_fit(ObjectFit::Cover)
                            .size_full(),
                    ),
            );
        }

        // 3. Scrim so the metadata stays legible over bright photos.
        card = card.child(div().absolute().size_full().bg(linear_gradient(
            180.,
            linear_color_stop(rgba(0x090d13cc), 0.),
            linear_color_stop(rgba(0x090d13f7), 1.),
        )));

        let image_layer = |image: Arc<RenderImage>| {
            div().absolute().size_full().child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full(),
            )
        };
        let mut preview_frame = div()
            .relative()
            .w_full()
            .flex_shrink_0()
            .aspect_ratio(16.0 / 10.0)
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
            preview_frame = preview_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("プレビューを読み込めません"),
            );
        }

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
            .child(self.meta_row("解像度", format!("{} × {}", photo.width, photo.height)))
            .child(self.meta_row("サイズ", format_bytes(photo.file_size)))
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
                        this.open_quick_look(window, cx);
                    })),
            )
            .child(
                Button::new("compress-selected")
                    .label("圧縮")
                    .small()
                    .w_full()
                    .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx))),
            );

        card = card.child(
            v_flex()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .p(px(18.))
                .gap(px(12.))
                .child(header)
                .child(body)
                .child(actions),
        );

        v_flex()
            .w(px(INSPECTOR_CARD_WIDTH))
            .h_full()
            .flex_shrink_0()
            .mr(px(INSPECTOR_MARGIN))
            .py(px(INSPECTOR_MARGIN))
            .child(card)
            .into_any_element()
    }

    /// A text-only control that participates in the viewer's focus order.
    ///
    /// GPUI synthesizes a click on key-up for focused elements, so keyboard
    /// activation happens on key-down instead and the click handler ignores
    /// `ClickEvent::Keyboard`. Without that split every press would activate
    /// twice.
    fn focusable_text_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        focus: FocusHandle,
        state: ButtonState,
        cx: &mut Context<Self>,
        on_activate: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
    ) -> AnyElement {
        let ButtonState { tone, enabled } = state;
        let label: SharedString = label.into();
        let activate: ViewerActivate = Rc::new(on_activate);
        let click_activate = activate.clone();
        let key_activate = activate;
        let click_focus = focus.clone();
        let key_focus = focus.clone();

        let (border, background, fg) = if !enabled {
            (rgba(0xffffff08), rgba(0x00000000), rgba(0x76819680))
        } else {
            match tone {
                ButtonTone::Primary => (rgba(0x66b3ff40), rgba(0x66b3ff1a), rgb(0xdceeff)),
                ButtonTone::Neutral => (rgba(0xffffff10), rgba(0xffffff08), rgb(MUTED_2)),
                ButtonTone::Danger => (rgba(0xfb718566), rgba(0xfb718526), rgb(RED)),
            }
        };

        let mut button = self.base_text_button(id, border, background, fg);

        if enabled {
            let (hover_bg, hover_fg, focus_border, focus_bg, focus_fg) = match tone {
                ButtonTone::Primary => (
                    rgba(0xffffff14),
                    rgb(TEXT),
                    rgb(BLUE),
                    rgb(ACCENT_SOFT),
                    rgb(TEXT),
                ),
                ButtonTone::Neutral => (
                    rgba(0xffffff14),
                    rgb(TEXT),
                    rgb(BLUE),
                    rgb(ACCENT_SOFT),
                    rgb(TEXT),
                ),
                ButtonTone::Danger => (
                    rgba(0xfb71853d),
                    rgb(0xffd9df),
                    rgb(RED),
                    rgba(0xfb718533),
                    rgb(0xffd9df),
                ),
            };
            button = button
                .cursor_pointer()
                .track_focus(&focus)
                .tab_stop(true)
                .hover(move |this| this.bg(hover_bg).text_color(hover_fg))
                .focus(move |this| {
                    this.border_color(focus_border)
                        .bg(focus_bg)
                        .text_color(focus_fg)
                })
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // Enter/Space activation is handled on key down; GPUI also
                    // synthesizes a keyboard click on key up, so ignore that path
                    // to avoid firing the action twice.
                    if matches!(event, ClickEvent::Keyboard(_)) {
                        return;
                    }
                    window.focus(&click_focus, cx);
                    click_activate(this, window, cx);
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if !event.is_held && (key == "enter" || key == "space") {
                        window.prevent_default();
                        cx.stop_propagation();
                        window.focus(&key_focus, cx);
                        key_activate(this, window, cx);
                    }
                }));
        }

        button.child(label).into_any_element()
    }

    /// Shared chrome for the viewer's text-only controls.
    fn base_text_button(
        &self,
        id: &'static str,
        border: Rgba,
        background: Rgba,
        fg: Rgba,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(28.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .bg(background)
            .text_size(px(11.))
            .font_medium()
            .text_color(fg)
            .whitespace_nowrap()
    }

    /// A tag pill with a remove control for the full-screen viewer.
    fn tag_chip(&self, photo_id: i64, tag: String, cx: &mut Context<Self>) -> AnyElement {
        let remove = tag.clone();
        h_flex()
            .id(SharedString::from(format!("viewer-tag-{tag}")))
            .items_center()
            .gap(px(3.))
            .pl(px(7.))
            .pr(px(3.))
            .py(px(2.))
            .rounded_full()
            .bg(rgba(0xffffff0f))
            .border_1()
            .border_color(rgba(0xffffff12))
            .text_size(px(10.))
            .text_color(rgb(MUTED))
            .child(format!("#{tag}"))
            .child(
                div()
                    .id(SharedString::from(format!("viewer-tag-remove-{tag}")))
                    .cursor_pointer()
                    .size(px(14.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .text_color(rgb(MUTED_2))
                    .hover(|this| this.bg(rgba(0xfb71852e)).text_color(rgb(RED)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove_tag(photo_id, &remove, cx);
                    }))
                    .child("×"),
            )
            .into_any_element()
    }

    /// Metadata and actions for one photo. Shared by the full-screen viewer.
    fn detail_body(&mut self, photo: &Photo, cx: &mut Context<Self>) -> impl IntoElement {
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
                let _ = this.db.set_favorite(id, !favorite);
                this.load_selected_meta(id);
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
            |this, _, cx| this.enqueue_selected(cx),
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
                    .child(self.meta_row("解像度", format!("{} × {}", photo.width, photo.height)))
                    .child(self.meta_row("サイズ", format_bytes(photo.file_size))),
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

    fn quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(photo) = self.selected_photo.clone() else {
            return div().into_any_element();
        };
        let color = self.color_for(&photo);
        let details_open = self.quick_look_details_open;
        let photo_count = self.quick_look_photo_ids.len();
        let index = self.quick_look_index.min(photo_count.saturating_sub(1));
        let prev_enabled = index > 0;
        let next_enabled = index + 1 < photo_count;

        let close_button = self.focusable_text_button(
            "viewer-close",
            "閉じる Esc",
            self.quick_look_close_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            |this, window, cx| this.close_quick_look(window, cx),
        );
        let prev_button = self.focusable_text_button(
            "viewer-prev",
            "前へ",
            self.quick_look_prev_focus.clone(),
            ButtonState {
                tone: if prev_enabled {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: prev_enabled,
            },
            cx,
            |this, window, cx| this.step_viewer_photo(-1, window, cx),
        );
        let next_button = self.focusable_text_button(
            "viewer-next",
            "次へ",
            self.quick_look_next_focus.clone(),
            ButtonState {
                tone: if next_enabled {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: next_enabled,
            },
            cx,
            |this, window, cx| this.step_viewer_photo(1, window, cx),
        );
        let details_button = self.focusable_text_button(
            "viewer-details",
            if details_open {
                "詳細を閉じる"
            } else {
                "詳細"
            },
            self.quick_look_details_focus.clone(),
            ButtonState {
                tone: if details_open {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: true,
            },
            cx,
            |this, window, cx| this.toggle_quick_look_details(window, cx),
        );

        let top_bar = h_flex()
            .h(px(VIEWER_TOP_BAR))
            .flex_shrink_0()
            .px(px(20.))
            .gap(px(10.))
            .items_center()
            .bg(rgba(0x05070a99))
            .border_b_1()
            .border_color(rgba(0xffffff12))
            .key_context("viewer_nav")
            .child(close_button)
            .child(prev_button)
            .child(next_button)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(photo.file_name.clone()),
            )
            .child(details_button);

        let aspect = if photo.height > 0 {
            photo.width as f32 / photo.height as f32
        } else {
            16.0 / 9.0
        };
        let viewport = window.viewport_size();
        let (image_w, image_h) = viewer_image_box(
            viewport.width.into(),
            viewport.height.into(),
            details_open,
            aspect,
        );
        let image = if let Some(preview) = self.preview.clone() {
            div()
                .id("viewer-image")
                .relative()
                .w(px(image_w))
                .h(px(image_h))
                .overflow_hidden()
                .rounded(px(10.))
                .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .child(
                    gpui_kit::img(preview)
                        .object_fit(ObjectFit::Contain)
                        .size_full(),
                )
        } else {
            div()
                .id("viewer-image")
                .relative()
                .w(px(image_w))
                .h(px(image_h))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.))
                .bg(rgba((color << 8) | 0x30))
                .border_1()
                .border_color(rgba(0xffffff14))
                .text_size(px(11.))
                .text_color(rgb(MUTED))
                .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .child("プレビューを読み込めません")
        };
        let image = image.with_animation(
            SharedString::from(format!(
                "viewer-photo-{}-{}",
                photo.id, self.quick_look_epoch
            )),
            Animation::new(Duration::from_millis(220)).with_easing(ease_motion),
            |el, t| el.opacity(t),
        );

        let image_area = div()
            .id("viewer-image-area")
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .p(px(VIEWER_PAD))
            .on_click(cx.listener(|this, _, window, cx| this.close_quick_look(window, cx)))
            .child(image);

        let message = self.message.clone();
        let mut details_panel = v_flex()
            .h_full()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgba(0x0e131bdc))
            .when(details_open, |this| {
                this.border_l_1().border_color(rgba(0xffffff12))
            });
        if details_open {
            let mut body = v_flex()
                .id("viewer-details-body")
                .h_full()
                .p(px(16.))
                .gap(px(10.))
                .overflow_y_scrollbar();
            if let Some(message) = message {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child(message),
                );
            }
            if self.delete_armed == Some(photo.id) {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child("もう一度押すとOSゴミ箱へ移動します。"),
                );
            }
            details_panel = details_panel.child(body.child(self.detail_body(&photo, cx)));
        }
        let details_panel = details_panel.with_animation(
            ("viewer-details", self.quick_look_details_epoch),
            Animation::new(if self.quick_look_details_epoch == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(300)
            })
            .with_easing(ease_motion),
            move |el, t| {
                el.w(px(viewer_details_width(details_open, t)))
                    .overflow_hidden()
            },
        );

        let content = h_flex()
            .flex_1()
            .min_h_0()
            .child(image_area)
            .child(details_panel);

        v_flex()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .bg(rgb(0x05070a))
            .key_context("quick_look")
            .occlude()
            .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
            .child(top_bar)
            .child(content)
            .with_animation(
                ("viewer", self.quick_look_epoch),
                Animation::new(Duration::from_millis(240)).with_easing(ease_motion),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    fn compression_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = self.profiles.get(self.profile_index).cloned();
        let profile_count = self.profiles.len();
        let message = self.message.clone();
        let header = self.screen_header(
            "圧縮設定",
            "自動圧縮は元画像を保持します。置換は手動実行時のみです。".to_string(),
            message,
            vec![
                h_flex()
                    .gap_1()
                    .child(self.policy_button("元画像を保持", "keep", cx))
                    .child(self.policy_button("置換（ゴミ箱）", "replace", cx))
                    .into_any_element(),
                Button::new("add-profile")
                    .label("＋ プロファイル")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let profile = CompressionProfile {
                            name: format!("新規プロファイル {}", this.profiles.len() + 1),
                            auto_enabled: false,
                            ..CompressionProfile::default()
                        };
                        let _ = this.db.save_profile(&profile);
                        this.profiles = this.db.list_profiles().unwrap_or_default();
                        this.profile_index = this
                            .profiles
                            .iter()
                            .position(|item| item.id == profile.id)
                            .unwrap_or(0);
                        cx.notify();
                    }))
                    .into_any_element(),
                Button::new("manual-compress")
                    .label("手動圧縮")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx)))
                    .into_any_element(),
            ],
        );

        v_flex().flex_1().min_h_0().child(header).child(
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .px(px(26.))
                .pt(px(22.))
                .pb(px(26.))
                .gap(px(24.))
                .child(
                    h_flex()
                        .gap(px(24.))
                        .items_start()
                        .child(v_flex().w(px(280.)).gap(px(8.)).children(
                            self.profiles.iter().enumerate().map(|(index, profile)| {
                                let active = index == self.profile_index;
                                let auto = profile.auto_enabled;
                                let title = profile.name.clone();
                                let meta = format!(
                                    "{} · Q{} · {}",
                                    format_label(&profile.format),
                                    profile.quality,
                                    if profile.lossless {
                                        "Lossless"
                                    } else {
                                        "Lossy"
                                    }
                                );
                                glass_panel()
                                    .id(SharedString::from(format!("profile-{index}")))
                                    .flex()
                                    .flex_col()
                                    .p(px(14.))
                                    .gap(px(6.))
                                    .bg(rgb(if active { ACCENT_SOFT } else { PANEL }))
                                    .border_color(rgb(if active { BLUE } else { BORDER }))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.profile_index = index;
                                        cx.notify();
                                    }))
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .font_semibold()
                                                    .text_color(rgb(if active {
                                                        BLUE
                                                    } else {
                                                        MUTED
                                                    }))
                                                    .child(title),
                                            )
                                            .child(self.badge(
                                                if auto { "自動" } else { "手動" },
                                                if auto { GREEN } else { MUTED },
                                                if auto { 0x112a22 } else { PANEL_2 },
                                            ))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "profile-up-{index}"
                                                    )))
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(6.))
                                                    .text_size(px(11.))
                                                    .text_color(rgb(if index == 0 {
                                                        MUTED_2
                                                    } else {
                                                        MUTED
                                                    }))
                                                    .when(index > 0, |this| {
                                                        this.cursor_pointer()
                                                            .hover(|this| {
                                                                this.bg(rgb(PANEL_2))
                                                                    .text_color(rgb(TEXT))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.move_profile(
                                                                        index, -1, cx,
                                                                    );
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                    })
                                                    .child("↑"),
                                            )
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "profile-down-{index}"
                                                    )))
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(6.))
                                                    .text_size(px(11.))
                                                    .text_color(rgb(
                                                        if index + 1 == profile_count {
                                                            MUTED_2
                                                        } else {
                                                            MUTED
                                                        },
                                                    ))
                                                    .when(index + 1 < profile_count, |this| {
                                                        this.cursor_pointer()
                                                            .hover(|this| {
                                                                this.bg(rgb(PANEL_2))
                                                                    .text_color(rgb(TEXT))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.move_profile(index, 1, cx);
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                    })
                                                    .child("↓"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(if active { MUTED } else { MUTED_2 }))
                                            .child(meta),
                                    )
                            }),
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(24.))
                                .when_some(profile, |this, profile| {
                                    this.child(self.profile_editor(&profile, cx))
                                })
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .child(self.stat_card(
                                            "総圧縮数",
                                            self.stats.compressed_count.to_string(),
                                        ))
                                        .child(
                                            self.stat_card(
                                                "写真",
                                                self.stats.photo_count.to_string(),
                                            ),
                                        )
                                        .child(self.stat_card(
                                            "圧縮後合計",
                                            format_bytes(self.db.derivative_bytes().unwrap_or(0)),
                                        )),
                                )
                                .child(self.queue_panel(cx)),
                        ),
                ),
        )
    }

    fn edit_profile(&mut self, edit: impl FnOnce(&mut CompressionProfile), cx: &mut Context<Self>) {
        if let Some(profile) = self.profiles.get_mut(self.profile_index) {
            edit(profile);
            let profile = profile.clone();
            let _ = self.db.save_profile(&profile);
        }
        cx.notify();
    }

    fn move_profile(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
        let Some(order) = moved_profile_order(&ids, index, delta) else {
            return;
        };
        let target = step_index(index, ids.len(), delta);
        match self.db.reorder_profiles(&order) {
            Ok(()) => {
                self.profiles.swap(index, target);
                if self.profile_index == index {
                    self.profile_index = target;
                } else if self.profile_index == target {
                    self.profile_index = index;
                }
            }
            Err(error) => self.message = Some(format!("並び替えを保存できません: {error}")),
        }
        cx.notify();
    }

    fn duplicate_profile(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(self.profile_index) else {
            return;
        };
        let id = profile.id.clone();
        match self.db.duplicate_profile(&id) {
            Ok(copy) => {
                self.profiles = self.db.list_profiles().unwrap_or_default();
                self.profile_index = self
                    .profiles
                    .iter()
                    .position(|profile| profile.id == copy.id)
                    .unwrap_or(0);
                self.message = Some(format!("「{}」を複製しました（自動適用はOFF）", copy.name));
            }
            Err(error) => self.message = Some(format!("複製に失敗: {error}")),
        }
        cx.notify();
    }

    fn request_delete_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .cloned()
        else {
            return;
        };
        if self.profiles.len() <= 1 {
            self.message = Some("最後のプロファイルは削除できません".to_string());
            cx.notify();
            return;
        }
        match self.db.active_job_count_for_profile(id) {
            Ok(0) => {
                let entity = cx.entity();
                let id = id.to_string();
                let name = profile.name.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let entity = entity.clone();
                    let id = id.clone();
                    alert
                        .title("プロファイルを削除")
                        .description(format!(
                            "「{name}」を削除します。この操作は元に戻せません。"
                        ))
                        .confirm()
                        .ok_text("削除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("キャンセル")
                        .on_ok(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.delete_profile(&id, cx));
                            true
                        })
                });
            }
            Ok(count) => {
                self.message = Some(format!(
                    "「{}」は未完了のジョブが{}件あるため削除できません",
                    profile.name, count
                ));
                cx.notify();
            }
            Err(error) => {
                self.message = Some(format!("削除前の確認に失敗: {error}"));
                cx.notify();
            }
        }
    }

    fn delete_profile(&mut self, id: &str, cx: &mut Context<Self>) {
        let removed_index = self
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .unwrap_or(self.profile_index);
        let name = self
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| id.to_string());
        match self.db.delete_profile(id) {
            Ok(()) => {
                self.profiles = self.db.list_profiles().unwrap_or_default();
                self.profile_index = removed_index.min(self.profiles.len().saturating_sub(1));
                self.message = Some("プロファイルを削除しました".to_string());
            }
            Err(error) => {
                let message = match self.db.active_job_count_for_profile(id) {
                    Ok(count) if count > 0 => {
                        format!("「{name}」は未完了のジョブが{count}件あるため削除できません")
                    }
                    _ if self.profiles.len() <= 1 => {
                        "最後のプロファイルは削除できません".to_string()
                    }
                    _ => format!("プロファイルを削除できません: {error}"),
                };
                self.message = Some(message);
            }
        }
        cx.notify();
    }

    fn profile_editor(
        &self,
        profile: &CompressionProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let index = self.profile_index;
        let profile_id = profile.id.clone();
        glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(14.))
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(profile.name.clone()),
                    )
                    .child(self.badge(&format_label(&profile.format), BLUE, 0x14283d))
                    .child(div().flex_1())
                    .child(
                        Button::new("duplicate-profile")
                            .label("複製")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.duplicate_profile(cx))),
                    )
                    .child(
                        Button::new("delete-profile")
                            .label("削除")
                            .small()
                            .danger()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_delete_profile(&profile_id, window, cx);
                            })),
                    )
                    .child(
                        Switch::new("profile-auto")
                            .checked(profile.auto_enabled)
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.auto_enabled = *checked;
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex().gap(px(4.)).children(
                    [
                        CompressionFormat::Jpeg,
                        CompressionFormat::Webp,
                        CompressionFormat::Avif,
                        CompressionFormat::JpegXl,
                    ]
                    .into_iter()
                    .map(|format| {
                        let active = profile.format == format;
                        let label = format_label(&format);
                        let selected_format = format.clone();
                        div()
                            .id(SharedString::from(format!("format-{}", format.as_str())))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(8.))
                            .bg(if active {
                                rgb(ACCENT_SOFT)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.))
                            .font_medium()
                            .text_color(if active { rgb(BLUE) } else { rgb(MUTED) })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.format = selected_format.clone();
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            }))
                            .child(label)
                    }),
                ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(80.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("品質"),
                    )
                    .child(
                        Button::new("quality-minus")
                            .label("−")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.quality = profile.quality.saturating_sub(1).max(1);
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .w(px(36.))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(profile.quality.to_string()),
                    )
                    .child(
                        Button::new("quality-plus")
                            .label("＋")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.quality = profile.quality.saturating_add(1).min(100);
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().text_size(px(10.)).text_color(rgb(MUTED_2)).child(
                        if profile.lossless {
                            "ロスレス"
                        } else {
                            "品質を調整"
                        },
                    )),
            )
            .child(
                self.meta_row(
                    "出力先",
                    profile
                        .output_dir
                        .clone()
                        .unwrap_or_else(|| "compressed/".to_string()),
                ),
            )
            .child(self.meta_row("サフィックス", profile.suffix.clone()))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("リサイズ"),
                    )
                    .child(
                        Switch::new("profile-resize")
                            .checked(profile.resize_enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.edit_profile(|profile| profile.resize_enabled = *checked, cx);
                            })),
                    )
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .children(
                                    [
                                        ("HD", 1280u32, 720u32),
                                        ("FHD", 1920, 1080),
                                        ("2K", 2560, 1440),
                                        ("4K", 3840, 2160),
                                    ]
                                    .into_iter()
                                    .map(
                                        |(label, width, height)| {
                                            let active =
                                                profile.width == width && profile.height == height;
                                            div()
                                                .id(SharedString::from(format!("resize-{label}")))
                                                .px(px(8.))
                                                .py(px(4.))
                                                .rounded(px(8.))
                                                .bg(if active {
                                                    rgb(ACCENT_SOFT)
                                                } else {
                                                    rgba(0x00000000)
                                                })
                                                .text_size(px(10.))
                                                .font_medium()
                                                .text_color(if active {
                                                    rgb(BLUE)
                                                } else {
                                                    rgb(MUTED)
                                                })
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.edit_profile(
                                                        |profile| {
                                                            profile.resize_enabled = true;
                                                            profile.width = width;
                                                            profile.height = height;
                                                        },
                                                        cx,
                                                    );
                                                }))
                                                .child(label)
                                        },
                                    ),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(format!("{} × {}", profile.width, profile.height)),
                                ),
                        )
                    })
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(MUTED_2))
                                        .child("アスペクト比維持"),
                                )
                                .child(
                                    Switch::new("profile-aspect")
                                        .checked(profile.maintain_aspect)
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.edit_profile(
                                                |profile| profile.maintain_aspect = *checked,
                                                cx,
                                            );
                                        })),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(0x102a24))
                    .border_1()
                    .border_color(rgba(0x5eead433))
                    .text_size(px(10.))
                    .text_color(rgb(GREEN))
                    .child("自動圧縮では元画像を必ず保持します。"),
            )
    }

    fn queue_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let retryable_jobs = self.failed_jobs.iter().any(job_retry_available);
        glass_panel()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .px(px(16.))
                    .py(px(14.))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("圧縮キュー"),
                    )
                    .child(self.badge(&format!("{} 件", self.jobs.len()), BLUE, 0x14283d))
                    .when(self.failed_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("失敗 {}", self.failed_job_count),
                            RED,
                            0x2a1414,
                        ))
                    })
                    .child(div().flex_1())
                    .when(retryable_jobs, |this| {
                        this.child(
                            Button::new("retry-all-failed-jobs")
                                .label("失敗をすべて再試行")
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.retry_all_failed_jobs(cx)),
                                ),
                        )
                    }),
            )
            .when(self.failed_job_count > 100, |this| {
                this.child(
                    div()
                        .px(px(16.))
                        .py(px(8.))
                        .text_size(px(10.))
                        .text_color(rgb(MUTED_2))
                        .child("失敗は最新の100件のみ表示しています（再試行は上限内のすべてが対象です）"),
                )
            })
            .children(self.failed_jobs.iter().map(|job| {
                let name = self
                    .failed_job_names
                    .get(&job.photo_id)
                    .cloned()
                    .unwrap_or_else(|| format!("photo #{}", job.photo_id));
                let profile_name = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == job.profile_id)
                    .map(|profile| profile.name.clone());
                let error = job.error.clone().unwrap_or_default();
                let retryable = job_retry_available(job);
                let id = job.id.clone();
                v_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(self.badge("失敗", RED, 0x2a1414))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(name),
                            )
                            .when_some(profile_name, |this, name| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(name),
                                )
                            }),
                    )
                    .when(!error.is_empty(), |this| {
                        this.child(
                            div()
                                .w_full()
                                .text_size(px(10.))
                                .text_color(rgb(RED))
                                .child(error),
                        )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(job_retry_label(job)),
                            )
                            .when(retryable, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!(
                                        "retry-job-{}",
                                        job.id
                                    )))
                                    .label("再試行")
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_job(id.clone(), cx)
                                    })),
                                )
                            }),
                    )
            }))
            .children(
                self.jobs
                    .iter()
                    .filter(|job| job.status != QueueStatus::Failed)
                    .take(6)
                    .map(|job| {
                        let status = match job.status {
                            QueueStatus::Pending => ("待機", AMBER, 0x2a2412),
                            QueueStatus::Processing => ("処理中", BLUE, 0x14283d),
                            QueueStatus::Completed => ("完了", GREEN, 0x112a22),
                            QueueStatus::Failed => ("失敗", RED, 0x2a1414),
                            QueueStatus::Cancelled => ("取消", MUTED, PANEL_2),
                        };
                        h_flex()
                            .px(px(16.))
                            .py(px(10.))
                            .gap(px(8.))
                            .items_center()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(self.badge(status.0, status.1, status.2))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(format!("photo #{}", job.photo_id)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!("{:.0}%", job.progress * 100.0)),
                            )
                    }),
            )
            .when(self.jobs.is_empty(), |this| {
                this.child(
                    div()
                        .p(px(16.))
                        .text_size(px(12.))
                        .text_color(rgb(MUTED_2))
                        .child("キューは空です"),
                )
            })
    }

    fn search_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
                            }),
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

    fn import_dialog(&mut self, mode: ImportMode, cx: &mut Context<Self>) {
        let Some(files) = rfd::FileDialog::new()
            .add_filter("画像", &["png", "jpg", "jpeg"])
            .pick_files()
        else {
            return;
        };
        let dest = self.settings.import_dir();
        let result = match importer::import_files(&files, &dest, mode) {
            Ok(result) => result,
            Err(error) => {
                self.message = Some(format!("取り込みに失敗: {error}"));
                cx.notify();
                return;
            }
        };
        let dest_string = dest.to_string_lossy().to_string();
        if !self
            .settings
            .photo_roots
            .iter()
            .any(|root| root == &dest_string)
        {
            self.settings.photo_roots.push(dest_string.clone());
            let _ = self.settings.save(&self.db);
        }
        let root_id = match self.db.add_root(&dest_string, "managed") {
            Ok(id) => id,
            Err(error) => {
                self.message = Some(format!("取り込み先を登録できません: {error}"));
                cx.notify();
                return;
            }
        };
        let report = scanner::scan_and_index(&self.db, root_id, &dest).unwrap_or_default();
        let _ = linker::relink_unlinked(&self.db, 1000);
        for id in &report.new_ids {
            if let Ok(Some(photo)) = self.db.photo_by_id(*id) {
                let _ = thumbnail::ensure_thumbnail(*id, Path::new(&photo.abs_path));
            }
        }
        self.reload_library(true, cx);
        self.message = Some(format!(
            "{} 枚を取り込みました（{} 枚スキップ）",
            result.imported.len(),
            result.skipped
        ));
        cx.notify();
    }

    fn policy_button(&self, label: &str, policy: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.compress_policy == policy;
        let policy = policy.to_string();
        div()
            .id(SharedString::from(format!("policy-{label}")))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(8.))
            .bg(if active {
                rgb(ACCENT_SOFT)
            } else {
                rgba(0x00000000)
            })
            .text_size(px(12.))
            .font_medium()
            .text_color(if active { rgb(BLUE) } else { rgb(MUTED) })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.compress_policy = policy.clone();
                cx.notify();
            }))
            .child(label.to_string())
    }

    fn preset_button(
        &self,
        label: &str,
        days: Option<i64>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.search_days == days;
        chip(label, active)
            .id(SharedString::from(format!("preset-{label}")))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.search_days = days;
                this.run_search(cx);
            }))
    }

    fn filter_field(&self, label: &str, input: Input) -> impl IntoElement {
        v_flex()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(input.w_full())
    }

    fn monitor_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.screen_header(
                "ログ監視",
                "VRChatの出力ログからワールドとプレイヤーを記録します。".to_string(),
                self.message.clone(),
                vec![
                    Button::new("monitor-refresh")
                        .label("再読込")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reload_aux(cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                ],
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px(px(26.))
                    .pt(px(22.))
                    .pb(px(26.))
                    .gap(px(24.))
                    .child(
                        h_flex()
                            .gap(px(16.))
                            .child(self.stat_card(
                                "監視",
                                if self.daemon_active {
                                    "稼働中".to_string()
                                } else if self.settings.log_dir.is_some() {
                                    "停止中".to_string()
                                } else {
                                    "未設定".to_string()
                                },
                            ))
                            .child(self.stat_card(
                                "現在のワールド",
                                self.monitor_world.clone().unwrap_or_else(|| "待機".to_string()),
                            ))
                            .child(self.stat_card(
                                "同接",
                                format!("{} 人", self.monitor_players.len()),
                            ))
                            .child(self.stat_card(
                                "写真",
                                format!("{} 枚", self.stats.photo_count),
                            )),
                    )
                    .child(
                        h_flex()
                            .gap(px(24.))
                            .items_start()
                            .child(
                                glass_panel()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .p(px(16.))
                                    .gap(px(10.))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child("現在のセッション"),
                                    )
                                    .child(self.meta_row(
                                        "インスタンス",
                                        self.monitor_instance
                                            .clone()
                                            .unwrap_or_else(|| "—".to_string()),
                                    ))
                                    .child(self.section_label(if self.monitor_players.is_empty() {
                                        "プレイヤー情報はまだありません"
                                    } else {
                                        "入室中のプレイヤー"
                                    }))
                                    .children(self.monitor_players.iter().map(|player| {
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(MUTED))
                                            .child(player.clone())
                                    })),
                            )
                            .child(
                                glass_panel()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .p(px(16.))
                                    .gap(px(10.))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child("監視パス"),
                                    )
                                    .children(self.settings.photo_roots.iter().map(|path| {
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .font_family("monospace")
                                            .text_color(rgb(MUTED))
                                            .child(path.clone())
                                    }))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .font_family("monospace")
                                            .text_color(rgb(MUTED))
                                            .child(
                                                self.settings
                                                    .log_dir
                                                    .clone()
                                                    .unwrap_or_else(|| "ログフォルダ未設定".to_string()),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .p(px(14.))
                            .rounded(px(14.))
                            .bg(rgb(0x2a2412))
                            .border_1()
                            .border_color(rgba(0xfbbf2440))
                            .text_size(px(12.))
                            .text_color(rgb(AMBER))
                            .child("VRChat本体は出力ログを24時間で削除します。アプリ側には解析済みイベントだけを保存します。"),
                    ),
            )
    }

    fn set_reduce_motion(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.ui_reduce_motion = enabled;
        let _ = self.settings.save(&self.db);
        cx.set_reduce_motion(enabled);
        cx.notify();
    }

    fn settings_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.screen_header(
                "設定",
                "ライブラリと起動オプション".to_string(),
                self.message.clone(),
                Vec::new(),
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px(px(26.))
                    .pt(px(22.))
                    .pb(px(26.))
                    .gap(px(24.))
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(24.))
                            .gap(px(16.))
                            .bg(rgb(PANEL))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("ライブラリ"),
                            )
                            .child(
                                self.filter_field(
                                    "写真フォルダ",
                                    Input::new(&self.photo_root_input),
                                ),
                            )
                            .child(
                                self.filter_field("ログフォルダ", Input::new(&self.log_dir_input)),
                            )
                            .child(self.filter_field(
                                "取り込み先",
                                Input::new(&self.import_dir_input),
                            ))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        Button::new("save-paths")
                                            .label("パスを保存")
                                            .small()
                                            .primary()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let root = this
                                                    .photo_root_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let log = this
                                                    .log_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let imported = this
                                                    .import_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let previous_roots =
                                                    this.settings.photo_roots.clone();
                                                this.settings.photo_roots =
                                                    if root.trim().is_empty() {
                                                        Vec::new()
                                                    } else {
                                                        vec![root.trim().to_string()]
                                                    };
                                                this.settings.log_dir = if log.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(log.trim().to_string())
                                                };
                                                this.settings.managed_dir =
                                                    if imported.trim().is_empty() {
                                                        None
                                                    } else {
                                                        Some(imported.trim().to_string())
                                                    };
                                                let root_changed =
                                                    this.settings.photo_roots != previous_roots;
                                                let _ = this.settings.save(&this.db);
                                                if root_changed {
                                                    this.reload_library(false, cx);
                                                }
                                                this.message = Some(
                                                    "保存しました。デーモン再起動で監視に反映されます。"
                                                        .to_string(),
                                                );
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED_2))
                                            .child("デーモン実行中は再起動後に反映"),
                                    ),
                            ),
                    )
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(24.))
                            .gap(px(16.))
                            .bg(rgb(PANEL))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("起動と圧縮"),
                            )
                            .child(self.setting_switch(
                                "自動起動",
                                "OSログイン時にデーモンを起動",
                                self.settings.autostart,
                                cx,
                                |settings, value| settings.autostart = value,
                            ))
                            .child(self.setting_switch(
                                "トレイ常駐",
                                "UIを閉じても監視を継続",
                                self.settings.tray,
                                cx,
                                |settings, value| settings.tray = value,
                            ))
                            .child(self.setting_switch(
                                "自動圧縮",
                                "新規写真に自動で圧縮を適用",
                                self.settings.auto_compress,
                                cx,
                                |settings, value| settings.auto_compress = value,
                            ))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .py(px(2.))
                                    .child(
                                        v_flex()
                                            .gap(px(2.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(TEXT))
                                                    .child("アニメーションを減らす"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(MUTED_2))
                                                    .child(
                                                        "画面遷移とカードの動きを抑制します",
                                                    ),
                                            ),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Switch::new("reduce-motion")
                                            .checked(self.settings.ui_reduce_motion)
                                            .on_click(cx.listener(|this, value, _, cx| {
                                                this.set_reduce_motion(*value, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-reload")
                                    .label("設定を再読込")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_aux(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!(
                                        "言語: {} · サムネイルキャッシュ: {} MB",
                                        self.settings.language, self.settings.thumbnail_cache_mb
                                    )),
                            ),
                    ),
            )
    }

    fn setting_switch(
        &self,
        label: &str,
        description: &str,
        checked: bool,
        cx: &mut Context<Self>,
        apply: fn(&mut AppSettings, bool),
    ) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap(px(12.))
            .py(px(2.))
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(description.to_string()),
                    ),
            )
            .child(div().flex_1())
            .child(
                Switch::new(SharedString::from(format!("setting-{label}")))
                    .checked(checked)
                    .on_click(cx.listener(move |this, value, _, cx| {
                        apply(&mut this.settings, *value);
                        let _ = this.settings.save(&this.db);
                        cx.notify();
                    })),
            )
    }

    fn stat_card(&self, label: &str, value: String) -> impl IntoElement {
        glass_panel()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .p(px(16.))
            .gap(px(6.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(value),
            )
    }

    fn meta_row(&self, label: &str, value: String) -> impl IntoElement {
        h_flex()
            .gap(px(8.))
            .text_size(px(10.))
            .child(
                div()
                    .w(px(86.))
                    .flex_shrink_0()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(value),
            )
    }

    fn section_label(&self, label: &str) -> impl IntoElement {
        div()
            .text_size(px(10.))
            .font_semibold()
            .text_color(rgb(MUTED_2))
            .child(label.to_string())
    }

    fn badge(&self, text: &str, fg: u32, bg: u32) -> impl IntoElement {
        div()
            .px(px(7.))
            .py(px(2.))
            .rounded(px(8.))
            .bg(rgb(bg))
            .text_size(px(10.))
            .font_medium()
            .text_color(rgb(fg))
            .child(text.to_string())
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.guard_viewer_selection(window, cx);
        let workspace = v_flex().flex_1().min_w_0().h_full();
        let root = h_flex()
            .relative()
            .size_full()
            .bg(rgb(BG))
            .font_family("Noto Sans JP")
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)))
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenQuickLook, window, cx| this.open_quick_look(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseQuickLook, window, cx| {
                this.handle_viewer_escape(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewerPreviousPhoto, window, cx| {
                this.step_viewer_photo(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewerNextPhoto, window, cx| {
                this.step_viewer_photo(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickLookNextFocus, window, cx| {
                this.cycle_quick_look_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickLookPrevFocus, window, cx| {
                this.cycle_quick_look_focus(true, window, cx)
            }))
            .child(self.sidebar(cx))
            .child(workspace.child(self.content(window, cx)))
            .child(self.nav_tooltip())
            .child(self.rail_toggle(cx));
        if self.quick_look_open {
            root.child(self.quick_look(window, cx))
        } else {
            root
        }
    }
}

struct DemoAssets {
    #[allow(dead_code)] // Fixture root, kept for inspection and future previews.
    photo_dir: PathBuf,
    thumb_dir: PathBuf,
}

fn prepare_demo(reset: bool) -> Result<(Db, DemoAssets), String> {
    let root = std::env::temp_dir().join("vrc-photo-manager-demo");
    if reset {
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "demo ディレクトリを削除できません ({}): {error}",
                    root.display()
                ));
            }
        }
    }
    let photo_dir = root.join("photos");
    let thumb_dir = root.join("thumbnails");
    std::fs::create_dir_all(&photo_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let db = Db::open(root.join("library.db")).map_err(|error| error.to_string())?;
    if db.count_photos().map_err(|error| error.to_string())? == 0 {
        seed_demo(&db, &photo_dir)?;
    }
    Ok((
        db,
        DemoAssets {
            photo_dir,
            thumb_dir,
        },
    ))
}

fn demo_thumb_path(assets: &DemoAssets, photo_id: i64) -> PathBuf {
    assets.thumb_dir.join(format!("{photo_id}.png"))
}

fn empty_database() -> Db {
    Db::open_in_memory().expect("fallback in-memory database")
}

fn open_database(options: &LaunchOptions) -> (Db, bool, Option<String>, Option<DemoAssets>) {
    if options.demo {
        return match prepare_demo(options.demo_reset) {
            Ok((db, assets)) => (db, true, None, Some(assets)),
            Err(error) => (
                empty_database(),
                true,
                Some(format!(
                    "デモデータを準備できませんでした（{error}）。空のライブラリで起動します。"
                )),
                None,
            ),
        };
    }
    let path = std::env::var("VRC_PHOTO_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(vrc_photo_core::default_db_path)
        .unwrap_or_else(|| PathBuf::from("vrc-photo-manager.db"));
    match Db::open(&path) {
        Ok(db) => (db, false, None, None),
        Err(error) => {
            let message = format!("DBを開けませんでした（{error}）。デモデータで起動します。");
            match prepare_demo(true) {
                Ok((db, assets)) => (db, true, Some(message), Some(assets)),
                Err(demo_error) => (
                    empty_database(),
                    true,
                    Some(format!(
                        "DBを開けませんでした（{error}）。デモデータも準備できませんでした（{demo_error}）。空のライブラリで起動します。"
                    )),
                    None,
                ),
            }
        }
    }
}

const DEMO_PALETTE: [u32; 8] = [
    0x1b1b3a, 0x2d1b4e, 0x5a2a5e, 0x8e3b5e, 0xc75a4a, 0xe08a4c, 0x4f6d8c, 0x22314f,
];

fn demo_image(index: usize) -> image::RgbaImage {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 800;
    let top = DEMO_PALETTE[index % DEMO_PALETTE.len()];
    let bottom = DEMO_PALETTE[(index + 1) % DEMO_PALETTE.len()];
    let mut image = image::RgbaImage::from_fn(WIDTH, HEIGHT, |_, y| {
        let t = y as f32 / (HEIGHT - 1) as f32;
        let color = lerp_color(top, bottom, t);
        image::Rgba([
            ((color >> 16) & 0xff) as u8,
            ((color >> 8) & 0xff) as u8,
            (color & 0xff) as u8,
            255,
        ])
    });
    let sun_x = WIDTH as i64 * 7 / 10;
    let sun_y = HEIGHT as i64 * 3 / 10;
    let radius = 90i64;
    for y in (sun_y - radius).max(0)..=(sun_y + radius).min(HEIGHT as i64 - 1) {
        for x in (sun_x - radius).max(0)..=(sun_x + radius).min(WIDTH as i64 - 1) {
            let dx = x - sun_x;
            let dy = y - sun_y;
            if dx * dx + dy * dy <= radius * radius {
                image.put_pixel(x as u32, y as u32, image::Rgba([255, 210, 138, 255]));
            }
        }
    }
    let silhouette = image::Rgba([10, 14, 22, 255]);
    for x in 0..WIDTH {
        let wave = ((x as f32 * 0.013) + index as f32 * 1.7).sin() * 0.5 + 0.5;
        let ridge = ((x as f32 * 0.031) + index as f32 * 0.9).cos() * 0.5 + 0.5;
        let ridge_height = 36.0 + 96.0 * wave * ridge;
        let top = HEIGHT.saturating_sub(ridge_height as u32);
        for y in top..HEIGHT {
            image.put_pixel(x, y, silhouette);
        }
    }
    image
}

fn seed_demo(db: &Db, photo_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(photo_dir).map_err(|error| error.to_string())?;
    let thumb_dir = photo_dir
        .parent()
        .map(|root| root.join("thumbnails"))
        .unwrap_or_else(|| photo_dir.join("thumbnails"));
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let root = db
        .add_root(&photo_dir.display().to_string(), "reference")
        .map_err(|error| error.to_string())?;
    let worlds = [
        "Midnight Rooftop",
        "The Black Cat",
        "Furality Aqua",
        "Sunset Bar",
        "Neon District",
        "Cozy Cabin",
        "Snowfield",
        "Deep Space",
    ];
    let names = ["Mio", "Kenta", "Lua", "Nox", "Sora", "Yui", "Aki", "Ren"];
    for index in 0..18 {
        let stamp = time::from_system_time(
            SystemTime::now() - Duration::from_secs(index as u64 * 3600 + 600),
        );
        let world = worlds[index % worlds.len()];
        let file_name = format!("VRChat_demo_{index}.png");
        let file_path = photo_dir.join(&file_name);
        demo_image(index)
            .save(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?;
        let file_size = std::fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .map_err(|error| error.to_string())?;
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: file_name.clone(),
                abs_path: file_path.display().to_string(),
                file_name,
                captured_at_utc: stamp.utc,
                captured_at_local: stamp.local,
                tz_offset_sec: stamp.offset_sec,
                width: 1280,
                height: 800,
                file_size,
                mtime: stamp.utc,
                blake3: None,
            })
            .map_err(|error| error.to_string())?;
        let thumb_path = thumb_dir.join(format!("{id}.png"));
        image::open(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?
            .thumbnail(256, 256)
            .save_with_format(&thumb_path, image::ImageFormat::Png)
            .map_err(|error| format!("{}: {error}", thumb_path.display()))?;
        let world_id = db
            .upsert_world(
                &format!("wrld_demo_{}", index % worlds.len()),
                Some(world),
                Some("demo"),
            )
            .map_err(|error| error.to_string())?;
        db.set_photo_context(&PhotoContext {
            photo_id: id,
            session_id: None,
            world_id: Some(world_id),
            world_name: Some(world.to_string()),
            instance_id: Some(format!("{}~region(jp)", 12000 + index)),
            link_method: "demo".to_string(),
            confidence: 1.0,
            author: Some("TommyZ".to_string()),
            linked_at: Utc::now().to_rfc3339(),
        })
        .map_err(|error| error.to_string())?;
        let players = (0..(3 + index % 5))
            .map(|offset| PhotoPlayer {
                user_id: format!("usr_{}", names[(index + offset) % names.len()]),
                display_name: names[(index + offset) % names.len()].to_string(),
            })
            .collect::<Vec<_>>();
        db.set_photo_players(id, &players)
            .map_err(|error| error.to_string())?;
        db.set_photo_tags(
            id,
            &[
                "夜".to_string(),
                world.split(' ').next().unwrap_or("VRChat").to_string(),
            ],
        )
        .map_err(|error| error.to_string())?;
        if index % 4 == 0 {
            db.set_favorite(id, true)
                .map_err(|error| error.to_string())?;
        }
        if index % 3 == 0 {
            db.add_derivative(
                id,
                &format!("/demo/compressed/{index}.webp"),
                "webp",
                1280,
                800,
                1_800_000,
                "demo",
                "keep",
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Decodes a thumbnail on a background thread. Returns the average color in
/// RGB and the RGBA buffer with channels already swapped to BGRA.
fn decode_thumbnail(id: i64, cache_path: &Path, source: &Path) -> Option<(u32, image::RgbaImage)> {
    let cached = cache_path.is_file()
        && std::fs::metadata(cache_path)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false);
    let path = if cached {
        cache_path.to_path_buf()
    } else {
        thumbnail::ensure_thumbnail(id, source).ok().flatten()?
    };
    let mut buffer = image::open(path).ok()?.to_rgba8();
    for pixel in buffer.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let color = average_bgra(buffer.as_raw());
    Some((color, buffer))
}

/// Builds a GPUI image from a decoded buffer. Must run on the UI thread.
fn make_render_image(rgba: image::RgbaImage) -> Arc<RenderImage> {
    let frame = image::Frame::new(rgba);
    Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1)))
}

fn load_preview(path: &Path) -> Option<Arc<RenderImage>> {
    if !path.is_file() {
        return None;
    }
    let image = image::open(path).ok()?.thumbnail(1200, 1200).to_rgba8();
    let mut buffer = image;
    for pixel in buffer.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let frame = image::Frame::new(buffer);
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1))))
}

fn glass_panel() -> Div {
    div()
        .rounded(px(18.))
        .bg(rgba(0x141922c4))
        .border_1()
        .border_color(rgba(0xffffff14))
        .shadow(vec![
            BoxShadow::new(px(0.), px(24.), rgba(0x00000060).into()).blur_radius(px(60.)),
            BoxShadow::new(px(0.), px(1.), rgba(0xffffff0d).into())
                .inset()
                .blur_radius(px(0.)),
        ])
}

fn chip(label: &str, active: bool) -> Div {
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .text_size(px(10.))
        .font_medium()
        .bg(rgb(if active { ACCENT_SOFT } else { PANEL_2 }))
        .text_color(rgb(if active { BLUE } else { MUTED }))
        .child(label.to_string())
}

/// Trims the outer whitespace of a note and treats an empty result as "no note".
fn normalize_note(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Returns the tags left after removing `tag`, or `None` when it is absent.
fn without_tag(tags: &[String], tag: &str) -> Option<Vec<String>> {
    if !tags.iter().any(|existing| existing == tag) {
        return None;
    }
    Some(
        tags.iter()
            .filter(|existing| existing.as_str() != tag)
            .cloned()
            .collect(),
    )
}

/// Returns the tags with `tag` appended, or `None` when it is already present.
fn with_tag(tags: &[String], tag: &str) -> Option<Vec<String>> {
    if tags.iter().any(|existing| existing == tag) {
        return None;
    }
    let mut tags = tags.to_vec();
    tags.push(tag.to_string());
    Some(tags)
}

/// Whether an executed search would be affected by a tag edit.
fn has_tag_filter(filter: &PhotoTextFilter) -> bool {
    filter
        .tag
        .as_deref()
        .map(str::trim)
        .is_some_and(|tag| !tag.is_empty())
}

fn format_bytes(bytes: u64) -> String {
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

fn format_count(value: i64) -> String {
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

/// Whether a failed job can return to the queue without exceeding the cap.
fn job_retry_available(job: &CompressionJob) -> bool {
    job.status == QueueStatus::Failed && job.retry_count < MAX_JOB_RETRIES
}

/// Retry usage for a failed job, or the cap explanation once exhausted.
fn job_retry_label(job: &CompressionJob) -> String {
    if job_retry_available(job) {
        format!("再試行 {}/{MAX_JOB_RETRIES}", job.retry_count)
    } else {
        format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました")
    }
}

/// Compact, ordered summaries of every failure that deserves a global alert.
/// The target screen owns the detail, so the bar only carries the count.
fn alert_summaries(
    failed_jobs: i64,
    daemon_errors: usize,
    import_failed: usize,
) -> Vec<(Screen, String)> {
    let mut rows = Vec::new();
    if failed_jobs > 0 {
        rows.push((
            Screen::Compression,
            format!("圧縮に {failed_jobs} 件の失敗があります"),
        ));
    }
    if daemon_errors > 0 {
        rows.push((
            Screen::Monitor,
            format!("デーモンで {daemon_errors} 件の処理エラーがあります"),
        ));
    }
    if import_failed > 0 {
        rows.push((
            Screen::Photos,
            format!("取り込みで {import_failed} 件失敗しました"),
        ));
    }
    rows
}

fn average_bgra(bytes: &[u8]) -> u32 {
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

fn lerp_color(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let from = ((from >> shift) & 0xff) as f32;
        let to = ((to >> shift) & 0xff) as f32;
        (((from + (to - from) * t).round() as u32) & 0xff) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn cubic_bezier(t: f64, p1: f64, p2: f64) -> f64 {
    let mt = 1.0 - t;
    3.0 * mt * mt * t * p1 + 3.0 * mt * t * t * p2 + t * t * t
}

fn cubic_bezier_slope(t: f64, p1: f64, p2: f64) -> f64 {
    let mt = 1.0 - t;
    3.0 * mt * mt * p1 + 6.0 * mt * t * (p2 - p1) + 3.0 * t * t * (1.0 - p2)
}

fn ease_motion(t: f32) -> f32 {
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

fn stagger_value(elapsed: f32, index: usize) -> f32 {
    let delay = index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS;
    let total = CARD_INTRO_SECS + delay;
    let local = ((elapsed * total - delay) / CARD_INTRO_SECS).clamp(0.0, 1.0);
    ease_motion(local)
}

fn next_focus_index(current: usize, len: usize, reverse: bool) -> usize {
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

fn step_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let base = current.min(len - 1) as isize;
    (base.saturating_add(delta)).clamp(0, len as isize - 1) as usize
}

fn moved_profile_order(ids: &[String], index: usize, delta: isize) -> Option<Vec<String>> {
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

fn parse_window_size(value: &str) -> Option<(f32, f32)> {
    let (width, height) = value.split_once(['x', 'X'])?;
    let width: f32 = width.trim().parse().ok()?;
    let height: f32 = height.trim().parse().ok()?;
    if !width.is_finite() || !height.is_finite() || width < 320.0 || height < 240.0 {
        return None;
    }
    Some((width, height))
}

fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed {
        SIDEBAR_COLLAPSED
    } else {
        SIDEBAR_EXPANDED
    }
}

fn sidebar_reveal_width(collapsed: bool, t: f32) -> f32 {
    let (from, to) = if collapsed {
        (130.0, 0.0)
    } else {
        (0.0, 130.0)
    };
    from + (to - from) * t
}

fn grid_available_width(
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
) -> f32 {
    let inspector = if inspector_collapsed {
        0.0
    } else {
        INSPECTOR_WIDTH
    };
    (window_width - sidebar_width(sidebar_collapsed) - inspector - WORKSPACE_PAD_X).max(0.0)
}

/// Width left for the search grid after the filter panel (290 px + 26 px
/// margin) and the right column's 26 px padding on both sides. The Search
/// screen has no inspector column, so none is subtracted.
fn search_available_width(window_width: f32, sidebar_collapsed: bool) -> f32 {
    (window_width - sidebar_width(sidebar_collapsed) - 290.0 - 26.0 - 52.0).max(0.0)
}

fn viewer_details_width(open: bool, t: f32) -> f32 {
    let (from, to) = if open {
        (0.0, VIEWER_DETAILS_WIDTH)
    } else {
        (VIEWER_DETAILS_WIDTH, 0.0)
    };
    from + (to - from) * t
}

fn viewer_image_box(
    viewport_w: f32,
    viewport_h: f32,
    details_open: bool,
    aspect: f32,
) -> (f32, f32) {
    let details = if details_open {
        VIEWER_DETAILS_WIDTH
    } else {
        0.0
    };
    let avail_w = (viewport_w - details - VIEWER_PAD * 2.0).max(1.0);
    let avail_h = (viewport_h - VIEWER_TOP_BAR - VIEWER_PAD * 2.0).max(1.0);
    let ratio = aspect.max(0.05);
    let width = avail_w.min(avail_h * ratio);
    (width, width / ratio)
}

fn grid_columns(available_width: f32) -> usize {
    (((available_width + GRID_GAP) / (MIN_CARD_WIDTH + GRID_GAP)).floor() as usize).max(1)
}

/// Column count for a grid screen. Search uses its own available width because
/// it has a filter panel and no inspector; the library keeps its inspector
/// aware width.
fn grid_columns_for(
    search: bool,
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
) -> usize {
    let available = if search {
        search_available_width(window_width, sidebar_collapsed)
    } else {
        grid_available_width(window_width, sidebar_collapsed, inspector_collapsed)
    };
    grid_columns(available)
}

/// Stands in for a photo whose page has not loaded yet. Matches the card's
/// 128 px image area so the fixed 196 px row height never changes.
fn grid_placeholder() -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h(px(128.))
        .rounded(px(14.))
        .bg(rgb(PANEL_2))
        .border_2()
        .border_color(rgb(BORDER))
        .into_any_element()
}

fn short_date(value: &str) -> String {
    value
        .split('.')
        .next()
        .and_then(|value| value.get(5..))
        .unwrap_or(value)
        .to_string()
}

fn scene_color(world: &str) -> u32 {
    let mut hash = 0u32;
    for byte in world.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*byte as u32);
    }
    const PALETTE: [u32; 8] = [
        0x1d2a4a, 0x3a1d2b, 0x123b3b, 0x3b2a14, 0x2a1b3d, 0x1d3320, 0x243447, 0x151a33,
    ];
    PALETTE[hash as usize % PALETTE.len()]
}

fn format_label(format: &CompressionFormat) -> String {
    match format {
        CompressionFormat::Jpeg => "JPEG",
        CompressionFormat::Webp => "WebP",
        CompressionFormat::Avif => "AVIF",
        CompressionFormat::JpegXl => "JPEG XL",
    }
    .to_string()
}

fn daemon_binary_path() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "vrc-photo-daemon.exe"
    } else {
        "vrc-photo-daemon"
    };
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join(name))
}

fn spawn_daemon_process() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    Command::new(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn run_background_supervisor() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    let mut child = Command::new(&path)
        .spawn()
        .map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())?;
    Ok(())
}

fn register_global_actions(cx: &mut gpui_kit::App, app: Entity<App>) {
    let sidebar = app.downgrade();
    cx.on_action(move |_: &ToggleSidebar, cx| {
        sidebar.update(cx, |app, cx| app.toggle_sidebar(cx)).ok();
    });

    let inspector = app.downgrade();
    cx.on_action(move |_: &ToggleInspector, cx| {
        inspector
            .update(cx, |app, cx| app.toggle_inspector(cx))
            .ok();
    });

    let focus_search = app.downgrade();
    cx.on_action(move |_: &FocusSearch, cx| {
        let Some(window) = cx.active_window() else {
            return;
        };
        let focus_search = focus_search.clone();
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    focus_search
                        .update(cx, |app, cx| app.focus_search(window, cx))
                        .ok();
                })
                .ok();
        });
    });
}

fn main() {
    if logging::init("vrc-photo-manager", "info,vrc_photo_manager=debug").is_none() {
        tracing::warn!("file logging is unavailable; using stderr only");
    }
    if timing_enabled() {
        let _ = process_start();
    }
    let options = parse_args(std::env::args());
    if options.background {
        if let Err(error) = run_background_supervisor() {
            tracing::error!(%error, "background supervisor failed");
        }
        return;
    }
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            cx.text_system()
                .add_fonts(vec![Cow::Borrowed(
                    include_bytes!("../assets/NotoSansJP.ttf").as_slice(),
                )])
                .expect("load Noto Sans JP");
            gpui_kit::init(cx);
            // gpui_component::init forces the light theme; the app is dark-only.
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("ctrl-i", ToggleInspector, None),
                KeyBinding::new("cmd-i", ToggleInspector, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                KeyBinding::new("cmd-k", FocusSearch, None),
                KeyBinding::new("space", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("enter", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("escape", CloseQuickLook, Some("quick_look")),
                KeyBinding::new("left", ViewerPreviousPhoto, Some("viewer_nav")),
                KeyBinding::new("right", ViewerNextPhoto, Some("viewer_nav")),
                KeyBinding::new("tab", QuickLookNextFocus, Some("quick_look")),
                KeyBinding::new("shift-tab", QuickLookPrevFocus, Some("quick_look")),
            ]);
            let (width, height) = options.window_size;
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
                window_min_size: Some(size(px(1080.), px(700.))),
                ..Default::default()
            };
            let (_, app) = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_window_title("VRC Photo Manager");
                cx.new(|cx| App::new(window, cx, &options))
            })
            .expect("open window");
            register_global_actions(cx, app);
        });
}

#[cfg(test)]
mod tests {
    // `use super::*` would also import GPUI's `test` attribute macro through
    // `gpui_kit::*`, shadowing the built-in `#[test]` and recursing forever.
    use super::{
        CountDebounce, DemoAssets, INSPECTOR_CARD_WIDTH, INSPECTOR_MARGIN, INSPECTOR_WIDTH,
        MAX_PAGES, PAGE_SIZE, PHOTO_ROW_HEIGHT, PhotoBrowser, PhotoIndex, PhotoPageCache, Screen,
        THUMB_CACHE_ENTRIES, THUMB_IN_FLIGHT, ThumbnailCache, YearBucket, alert_summaries,
        anchor_photo_id, apply_library_index, average_bgra, build_year_buckets, decode_thumbnail,
        demo_thumb_path, ease_motion, first_visible_photo, format_captured_month, format_count,
        grid_available_width, grid_columns, grid_columns_for, has_tag_filter, job_retry_available,
        job_retry_label, lerp_color, list_scroll_top, moved_profile_order, next_focus_index,
        normalize_note, parse_args, parse_window_size, rail_label_hidden_by_pill, rail_row_at,
        rail_scroll_offset, rail_y, search_anchor_after_result, search_available_width,
        sidebar_reveal_width, sidebar_width, stagger_value, step_index, sync_search_mirror,
        viewer_details_width, viewer_image_box, visible_year_buckets, with_tag, without_tag,
    };
    use gpui_kit::{RenderImage, ScrollStrategy, UniformListScrollHandle, point, px};
    use smallvec::SmallVec;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use vrc_photo_core::error::CoreError;
    use vrc_photo_core::model::{
        CompressionJob, Photo, PhotoIndexEntry, PhotoStatus, PhotoTextFilter, QueueStatus,
    };

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
        assert_eq!(grid_columns(grid_available_width(1280.0, false, false)), 3);
        assert_eq!(grid_columns(grid_available_width(1440.0, false, false)), 4);
        assert_eq!(grid_columns(grid_available_width(1280.0, true, false)), 4);
        assert_eq!(grid_columns(grid_available_width(1440.0, true, false)), 5);
        assert_eq!(grid_columns(grid_available_width(1280.0, false, true)), 5);
        assert_eq!(grid_columns(grid_available_width(1440.0, false, true)), 6);
        assert_eq!(grid_columns(grid_available_width(1080.0, false, false)), 2);
        assert_eq!(grid_columns(grid_available_width(1080.0, false, true)), 4);
        assert_eq!(grid_columns(0.0), 1);
    }

    #[test]
    fn viewer_details_width_opens_and_closes() {
        assert_eq!(viewer_details_width(false, 0.0), 380.0);
        assert_eq!(viewer_details_width(false, 1.0), 0.0);
        assert_eq!(viewer_details_width(true, 0.0), 0.0);
        assert_eq!(viewer_details_width(true, 1.0), 380.0);
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
    fn alert_summaries_follow_priority_order() {
        let rows = alert_summaries(2, 1, 3);
        assert_eq!(rows[0].0, Screen::Compression);
        assert!(rows[0].1.contains("2"));
        assert_eq!(rows[1].0, Screen::Monitor);
        assert_eq!(rows[2].0, Screen::Photos);
        assert!(alert_summaries(0, 0, 0).is_empty());
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
    fn window_size_parser_accepts_wxh() {
        assert_eq!(parse_window_size("1440x900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("1440X900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("0x900"), None);
        assert_eq!(parse_window_size("1440,900"), None);
        assert_eq!(parse_window_size("1440x"), None);
    }

    #[test]
    fn launch_options_parse_demo_and_size() {
        let options = parse_args([
            "app".to_string(),
            "--demo".to_string(),
            "--window-size".to_string(),
            "1440x900".to_string(),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1440.0, 900.0));
    }

    #[test]
    fn launch_options_ignore_unknown_flags() {
        let options = parse_args([
            "app".to_string(),
            "--demo".to_string(),
            "--unexpected".to_string(),
            "--window-size".to_string(),
            "1280x820".to_string(),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1280.0, 820.0));
    }

    #[test]
    fn demo_thumb_path_uses_png_in_demo_dir() {
        let assets = DemoAssets {
            photo_dir: std::path::PathBuf::from("/tmp/vrc-demo/photos"),
            thumb_dir: std::path::PathBuf::from("/tmp/vrc-demo/thumbnails"),
        };
        let path = demo_thumb_path(&assets, 42);
        assert!(path.ends_with("42.png"));
        assert_eq!(path.parent(), Some(assets.thumb_dir.as_path()));
    }

    fn test_render_image() -> Arc<RenderImage> {
        let frame = image::Frame::new(image::RgbaImage::new(1, 1));
        Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1)))
    }

    #[test]
    fn thumbnail_cache_limits_in_flight_requests() {
        let mut cache = ThumbnailCache::default();
        for id in 0..THUMB_IN_FLIGHT as i64 {
            assert!(cache.begin(id));
        }
        assert!(!cache.begin(999));
    }

    #[test]
    fn thumbnail_cache_evicts_least_recently_used_but_keeps_pinned() {
        let mut cache = ThumbnailCache::default();
        cache.pin(1);
        for id in 1..=THUMB_CACHE_ENTRIES as i64 {
            assert!(cache.begin(id));
            cache.insert(id, test_render_image(), 0x112233);
        }
        // Do not touch id 1: it is the least recently used entry, so only the
        // pin can keep it alive when the next insert overflows the cache.
        assert!(cache.begin(999));
        cache.insert(999, test_render_image(), 0x445566);
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn thumbnail_cache_unpins_a_deleted_selection() {
        let mut cache = ThumbnailCache::default();
        cache.pin(7);
        cache.unpin(7);
        assert_eq!(cache.pinned, None);

        cache.pin(8);
        cache.unpin(7);
        assert_eq!(cache.pinned, Some(8));
    }

    #[test]
    fn thumbnail_cache_frees_a_slot_for_a_pinned_retry() {
        let mut cache = ThumbnailCache::default();
        for id in 0..THUMB_IN_FLIGHT as i64 {
            assert!(cache.begin(id));
        }
        cache.pin(99);
        // The selected thumbnail is refused while every slot is busy.
        assert!(!cache.begin(99));
        // A completed decode frees a slot; the pinned retry can now start.
        cache.insert(0, test_render_image(), 0);
        assert!(cache.begin(99));
    }

    #[test]
    fn thumbnail_cache_does_not_retry_failed_or_pending_ids() {
        let mut cache = ThumbnailCache::default();
        assert!(cache.begin(1));
        assert!(!cache.begin(1));
        cache.fail(1);
        assert!(!cache.begin(1));
    }

    #[test]
    fn decode_thumbnail_uses_the_cached_file_and_swaps_channels() {
        let dir = std::env::temp_dir().join(format!("vrc-photo-ui-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache_path = dir.join("thumb.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .save(&cache_path)
            .unwrap();
        let (color, rgba) =
            decode_thumbnail(7, &cache_path, &dir.join("missing-source.png")).unwrap();
        assert_eq!(color, 0x0a141e);
        assert_eq!(&rgba.as_raw()[..4], &[30, 20, 10, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_photo(id: i64) -> Photo {
        Photo {
            id,
            root_id: 1,
            rel_path: format!("p/{id}.png"),
            abs_path: format!("/p/{id}.png"),
            file_name: format!("{id}.png"),
            captured_at_utc: id,
            captured_at_local: "2026-10-21 21:14:03".to_string(),
            tz_offset_sec: 0,
            width: 1,
            height: 1,
            file_size: 1,
            mtime: id,
            blake3: None,
            status: PhotoStatus::Present,
            is_favorite: false,
            note: None,
            world_name: None,
            instance_id: None,
            link_method: None,
            confidence: None,
            author: None,
        }
    }

    #[test]
    fn page_cache_evicts_the_oldest_page() {
        let mut cache = PhotoPageCache::new();
        for page in 0..MAX_PAGES {
            cache.insert_page(page, vec![test_photo(page as i64)]);
        }
        cache.insert_page(MAX_PAGES, vec![test_photo(999)]);
        assert!(cache.get_page(0).is_none());
        assert!(cache.get_page(MAX_PAGES).is_some());
    }

    #[test]
    fn page_cache_keeps_recently_touched_pages() {
        let mut cache = PhotoPageCache::new();
        for page in 0..MAX_PAGES {
            cache.insert_page(page, vec![test_photo(page as i64)]);
        }
        // Touch page 0 so it is no longer the least recently used entry.
        assert!(cache.get_page(0).is_some());
        cache.insert_page(MAX_PAGES, vec![test_photo(999)]);
        assert!(cache.get_page(0).is_some());
        assert!(cache.get_page(1).is_none());
        assert!(cache.get_page(MAX_PAGES).is_some());
    }

    #[test]
    fn page_cache_plans_only_missing_pages_and_marks_them_pending() {
        let mut cache = PhotoPageCache::new();
        let first = cache.plan_pages(0, PAGE_SIZE * 3, 2);
        assert_eq!(first, vec![0, 1]);
        let second = cache.plan_pages(0, PAGE_SIZE * 3, 2);
        assert_eq!(second, vec![2]);
        cache.insert_page(0, vec![test_photo(0)]);
        assert!(cache.get_page(0).is_some());
        let third = cache.plan_pages(0, PAGE_SIZE, 2);
        assert!(third.is_empty());
    }

    #[test]
    fn count_debounce_waits_for_a_quiet_period() {
        let mut debounce = CountDebounce::default();
        let quiet = Duration::from_secs(2);
        let start = Instant::now();
        assert!(!debounce.observe(100, start, quiet));
        // The three-second tick re-observes the same count after the quiet
        // period and releases the rebuild.
        assert!(debounce.observe(100, start + Duration::from_secs(3), quiet));
        // A successful reload starts a fresh period.
        assert!(!debounce.observe(101, start + Duration::from_secs(6), quiet));
    }

    #[test]
    fn count_debounce_restarts_while_imports_keep_arriving() {
        let mut debounce = CountDebounce::default();
        let quiet = Duration::from_secs(2);
        let start = Instant::now();
        assert!(!debounce.observe(100, start, quiet));
        assert!(!debounce.observe(101, start + Duration::from_secs(3), quiet));
        assert!(!debounce.observe(102, start + Duration::from_secs(6), quiet));
        // Only after the count stays at 102 for a full tick does it reload.
        assert!(debounce.observe(102, start + Duration::from_secs(9), quiet));
    }

    #[test]
    fn stale_generation_cannot_insert_or_fail_pages() {
        let mut browser =
            PhotoBrowser::new(Arc::new(PhotoIndex::new(vec![index_entry(1, 202601)])));
        let stale = browser.generation;
        browser.replace_index(Arc::new(PhotoIndex::new(vec![index_entry(2, 202601)])));

        browser.insert_page(0, vec![test_photo(1)], stale);
        assert!(browser.pages.get_page(0).is_none());
        browser.fail_page(0, stale);
        assert!(!browser.pages.failed.contains(&0));

        let current = browser.generation;
        browser.fail_page(0, current);
        assert!(browser.pages.failed.contains(&0));
        browser.insert_page(0, vec![test_photo(2)], current);
        assert!(browser.pages.get_page(0).is_some());
        assert!(!browser.pages.failed.contains(&0));
    }

    #[test]
    fn failed_library_index_keeps_the_mounted_snapshot_and_pages() {
        let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(vec![
            index_entry(1, 202601),
            index_entry(2, 202512),
        ])));
        let generation = browser.generation;
        browser.insert_page(0, vec![test_photo(1)], generation);
        let mounted = browser.index.clone();

        let error = apply_library_index(
            &mut browser,
            Err(CoreError::Invalid("database is locked".to_string())),
        );

        assert_eq!(error.as_deref(), Some("写真一覧を読み込めませんでした"));
        assert!(Arc::ptr_eq(&browser.index, &mounted));
        assert_eq!(browser.index.total, 2);
        assert!(browser.pages.get_page(0).is_some());
    }

    #[test]
    fn fresh_library_index_replaces_the_snapshot() {
        let mut browser =
            PhotoBrowser::new(Arc::new(PhotoIndex::new(vec![index_entry(1, 202601)])));
        let generation = browser.generation;
        browser.insert_page(0, vec![test_photo(1)], generation);

        let error = apply_library_index(
            &mut browser,
            Ok(vec![index_entry(2, 202512), index_entry(3, 202511)]),
        );

        assert!(error.is_none());
        assert_eq!(browser.index.total, 2);
        assert_eq!(browser.index.row_of_id(2), Some(0));
        assert!(browser.pages.get_page(0).is_none());
    }

    #[test]
    fn search_mirror_refreshes_only_when_the_library_snapshot_changed() {
        let library = PhotoBrowser::new(Arc::new(PhotoIndex::new(vec![index_entry(1, 202601)])));
        let mut search = PhotoBrowser::new(library.index.clone());
        let generation = search.generation;
        search.insert_page(0, vec![test_photo(1)], generation);

        assert!(!sync_search_mirror(&mut search, &library));
        assert!(search.pages.get_page(0).is_some());

        let updated = PhotoBrowser::new(Arc::new(PhotoIndex::new(vec![
            index_entry(2, 202601),
            index_entry(1, 202601),
        ])));
        assert!(sync_search_mirror(&mut search, &updated));
        assert!(Arc::ptr_eq(&search.index, &updated.index));
        assert_eq!(search.index.total, 2);
        assert!(search.pages.get_page(0).is_none());
    }

    #[test]
    fn anchor_photo_id_uses_the_first_visible_row() {
        let entries = vec![
            index_entry(1, 202601),
            index_entry(2, 202512),
            index_entry(3, 202411),
        ];
        let index = PhotoIndex::new(entries);
        assert_eq!(anchor_photo_id(&index, 0.0, 3), Some(1));
        assert_eq!(anchor_photo_id(&index, PHOTO_ROW_HEIGHT + 1.0, 1), Some(2));
        assert_eq!(
            anchor_photo_id(&index, PHOTO_ROW_HEIGHT * 2.0 + 1.0, 1),
            Some(3)
        );
    }

    #[test]
    fn anchor_photo_id_handles_empty_and_overscrolled_indexes() {
        assert_eq!(anchor_photo_id(&PhotoIndex::new(Vec::new()), 0.0, 3), None);
        let index = PhotoIndex::new(vec![index_entry(1, 202601), index_entry(2, 202512)]);
        assert_eq!(anchor_photo_id(&index, PHOTO_ROW_HEIGHT * 50.0, 4), Some(2));
    }

    #[test]
    fn first_visible_photo_floors_the_row_before_applying_columns() {
        // Halfway into row 2 with 3 columns: the top row starts at entry 6.
        // Truncating after multiplying (`2.5 * 3 = 7.5`) would pick entry 7.
        assert_eq!(first_visible_photo(PHOTO_ROW_HEIGHT * 2.5, 3, 100), 6);
        assert_eq!(first_visible_photo(0.0, 3, 100), 0);
        assert_eq!(first_visible_photo(-20.0, 3, 100), 0);
        assert_eq!(first_visible_photo(PHOTO_ROW_HEIGHT * 50.0, 3, 10), 9);
        assert_eq!(first_visible_photo(0.0, 3, 0), 0);
    }

    #[test]
    fn anchor_row_remaps_after_an_insertion_before_it() {
        let original = PhotoIndex::new(vec![index_entry(10, 202601), index_entry(11, 202512)]);
        assert_eq!(original.row_of_id(11), Some(1));
        let updated = PhotoIndex::new(vec![
            index_entry(12, 202601),
            index_entry(10, 202601),
            index_entry(11, 202512),
        ]);
        assert_eq!(updated.row_of_id(11), Some(2));
        assert_eq!(
            anchor_photo_id(&updated, PHOTO_ROW_HEIGHT * 2.0 + 1.0, 1),
            Some(11)
        );
    }

    #[test]
    fn grid_row_height_matches_the_card_height() {
        assert_eq!(PHOTO_ROW_HEIGHT, 196.0);
    }

    #[test]
    fn removing_a_photo_clears_pages_and_rebuilds_year_counts() {
        let entries = vec![
            index_entry(1, 202601),
            index_entry(2, 202601),
            index_entry(3, 202412),
        ];
        let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(entries)));
        let generation = browser.generation;
        browser.insert_page(0, vec![test_photo(1)], generation);
        browser.remove_photo(2);
        assert_eq!(browser.index.total, 2);
        assert_eq!(browser.index.years[0].count, 1);
        assert_eq!(browser.index.years[1].count, 1);
        assert!(browser.pages.get_page(0).is_none());
        assert_eq!(browser.index.row_of_id(1), Some(0));
        assert_eq!(browser.index.row_of_id(2), None);
        // Removing an id that is already gone is a no-op.
        browser.remove_photo(2);
        assert_eq!(browser.index.total, 2);
    }

    #[test]
    fn removing_a_photo_clamps_a_scroll_past_the_last_row() {
        let entries = (1..=10).map(|id| index_entry(id, 202601)).collect();
        let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(entries)));
        browser.columns = 2;
        browser
            .scroll
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(-PHOTO_ROW_HEIGHT * 10.0)));
        browser.remove_photo(1);

        let state = browser.scroll.0.borrow();
        let deferred = state.deferred_scroll_to_item.expect("scroll clamp");
        // 9 photos over 2 columns is 5 rows; the last row index is 4.
        assert_eq!(deferred.item_index, 4);
        assert_eq!(deferred.strategy, ScrollStrategy::Nearest);
    }

    #[test]
    fn search_available_width_drops_to_the_filter_panel_width() {
        assert_eq!(grid_columns(search_available_width(1280.0, false)), 3);
        assert_eq!(grid_columns(search_available_width(1440.0, false)), 4);
        assert_eq!(grid_columns(search_available_width(1080.0, false)), 2);
        assert_eq!(grid_columns(search_available_width(1280.0, true)), 4);
    }

    #[test]
    fn search_grid_columns_ignore_the_inspector_state() {
        // The Search screen has no inspector column, so the default expanded
        // inspector must not shrink its grid.
        assert_eq!(grid_columns_for(true, 1280.0, false, false), 3);
        assert_eq!(grid_columns_for(true, 1440.0, false, false), 4);
        assert_eq!(grid_columns_for(true, 1080.0, false, false), 2);
        assert_eq!(grid_columns_for(false, 1280.0, false, false), 3);
    }

    fn index_entry(id: i64, month: i32) -> PhotoIndexEntry {
        PhotoIndexEntry {
            id,
            captured_at_utc: id,
            captured_month: month,
        }
    }

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
    fn list_scroll_top_reads_the_uniform_list_offset() {
        let handle = UniformListScrollHandle::new();
        handle
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(-392.)));
        assert_eq!(list_scroll_top(&handle), 392.0);
        handle
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(12.)));
        assert_eq!(list_scroll_top(&handle), 0.0);
    }

    #[test]
    fn notes_normalize_empty_to_none_and_trim_text() {
        assert_eq!(normalize_note(""), None);
        assert_eq!(normalize_note("  \n\t "), None);
        assert_eq!(normalize_note(" hello \n"), Some("hello"));
        assert_eq!(normalize_note("line1\nline2"), Some("line1\nline2"));
    }

    #[test]
    fn tag_removal_filters_only_the_named_tag() {
        let tags = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(without_tag(&tags, "alpha"), Some(vec!["beta".to_string()]));
        assert_eq!(without_tag(&tags, "missing"), None);
    }

    #[test]
    fn tag_addition_rejects_duplicates() {
        let tags = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(
            with_tag(&tags, "gamma"),
            Some(vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string()
            ])
        );
        assert_eq!(with_tag(&tags, "alpha"), None);
    }

    #[test]
    fn search_anchor_after_result_keeps_existing_or_falls_back_to_first() {
        let index = PhotoIndex::new(vec![index_entry(1, 202601), index_entry(2, 202512)]);
        assert_eq!(search_anchor_after_result(Some(2), &index), Some(2));
        assert_eq!(search_anchor_after_result(Some(9), &index), Some(1));
        assert_eq!(search_anchor_after_result(None, &index), Some(1));
        assert_eq!(
            search_anchor_after_result(Some(1), &PhotoIndex::new(Vec::new())),
            None
        );
    }

    #[test]
    fn search_refresh_requires_a_non_empty_tag_filter() {
        assert!(!has_tag_filter(&PhotoTextFilter::default()));
        assert!(!has_tag_filter(&PhotoTextFilter {
            tag: Some("  ".to_string()),
            ..Default::default()
        }));
        assert!(has_tag_filter(&PhotoTextFilter {
            tag: Some("vrc".to_string()),
            ..Default::default()
        }));
    }

    #[test]
    fn rail_jump_offset_uses_grid_rows_and_clamps_to_the_list() {
        // `ScrollHandle::max_offset()` is a non-negative scrollable extent.
        assert_eq!(rail_scroll_offset(0, 3, 10_000.0), 0.0);
        assert_eq!(rail_scroll_offset(2, 3, 10_000.0), 0.0);
        assert_eq!(rail_scroll_offset(3, 3, 10_000.0), -196.0);
        assert_eq!(rail_scroll_offset(5, 3, 10_000.0), -196.0);
        // The target (-2156) is clamped to the 1000 px extent.
        assert_eq!(rail_scroll_offset(11, 1, 1_000.0), -1_000.0);
        assert_eq!(rail_scroll_offset(600, 3, 10_000.0), -10_000.0);
        assert_eq!(rail_scroll_offset(600, 0, 10_000.0), -10_000.0);
        // No scrollable extent means no movement, even for a negative extent
        // rather than the real non-negative contract.
        assert_eq!(rail_scroll_offset(600, 3, 0.0), 0.0);
        assert_eq!(rail_scroll_offset(600, 3, -5.0), 0.0);
    }

    fn test_job(status: QueueStatus, retry_count: i64) -> CompressionJob {
        CompressionJob {
            id: "job".to_string(),
            photo_id: 1,
            profile_id: "p".to_string(),
            status,
            progress: 0.0,
            error: Some("boom".to_string()),
            output_path: None,
            policy: "keep".to_string(),
            created_at: "now".to_string(),
            started_at: None,
            finished_at: None,
            retry_count,
        }
    }

    #[test]
    fn job_retry_availability_stops_at_the_cap() {
        assert!(job_retry_available(&test_job(QueueStatus::Failed, 0)));
        assert!(job_retry_available(&test_job(QueueStatus::Failed, 2)));
        assert!(!job_retry_available(&test_job(QueueStatus::Failed, 3)));
        assert!(!job_retry_available(&test_job(QueueStatus::Completed, 0)));
        assert!(job_retry_label(&test_job(QueueStatus::Failed, 3)).contains("上限"));
    }
}
