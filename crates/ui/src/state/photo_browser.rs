//! Photo browser index snapshot, page cache, and query helpers.
//!
//! The `uniform_list` page loading and rendering stay in the UI layer; this
//! module owns only the index snapshot, the page cache, and the pure helpers
//! that describe scroll and anchor state.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{ScrollStrategy, UniformListScrollHandle};
use vrc_photo_core::model::{Photo, PhotoIndexEntry};

use crate::state::year_rail::{YearBucket, build_year_buckets};
use crate::ui::theme::{MIN_CARD_WIDTH, photo_row_height};
use crate::ui::year_rail::YearRailLabels;

pub(crate) const PAGE_SIZE: usize = 256;
pub(crate) const MAX_PAGES: usize = 24;

/// Scroll distance from the top of a uniform list, in pixels.
pub(crate) fn list_scroll_top(handle: &UniformListScrollHandle) -> f32 {
    let offset = handle.0.borrow().base_handle.offset().y;
    (-f32::from(offset)).max(0.0)
}

/// Ordered snapshot of the library: only IDs and capture timestamps, so the
/// full 100k-entry list stays cheap while photo rows load in pages.
pub(crate) struct PhotoIndex {
    pub(crate) entries: Vec<PhotoIndexEntry>,
    pub(crate) total: usize,
    pub(crate) years: Vec<YearBucket>,
    /// Photo count per local capture month. The gallery header needs the
    /// count for the month at the top of the viewport on every scroll-driven
    /// render; scanning the 100k-entry index there was measurable work.
    months: HashMap<i32, usize>,
}

impl PhotoIndex {
    pub(crate) fn new(entries: Vec<PhotoIndexEntry>) -> Self {
        let total = entries.len();
        let years = build_year_buckets(&entries);
        let mut months: HashMap<i32, usize> = HashMap::new();
        for entry in &entries {
            *months.entry(entry.captured_month).or_insert(0) += 1;
        }
        Self {
            entries,
            total,
            years,
            months,
        }
    }

    pub(crate) fn month_count(&self, month: i32) -> usize {
        self.months.get(&month).copied().unwrap_or(0)
    }

    /// Entries between `anchor` and `target` in index order, inclusive.
    /// Returns `None` when either id is no longer in the snapshot.
    pub(crate) fn range_between(&self, anchor: i64, target: i64) -> Option<&[PhotoIndexEntry]> {
        let anchor = self.row_of_id(anchor)?;
        let target = self.row_of_id(target)?;
        let (start, end) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        Some(&self.entries[start..=end])
    }

    /// Position of `id` in the ordered snapshot, if it is still present.
    pub(crate) fn row_of_id(&self, id: i64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }
}

/// First photo in the top visible row.
///
/// `scroll_top` is the non-negative scroll distance from the top of the list.
/// The row is floored before multiplying by the column count: halfway into
/// row 2 the top row is still row 2, not the first card of row 3.
pub(crate) fn first_visible_photo(
    scroll_top: f32,
    columns: usize,
    row_height: f32,
    total: usize,
) -> usize {
    if total == 0 {
        return 0;
    }
    let columns = columns.max(1);
    let row = (scroll_top.max(0.0) / row_height).floor() as usize;
    row.saturating_mul(columns).min(total - 1)
}

/// Month and count for the top visible row.
///
/// The gallery header follows the scroll position, not the first entry in the
/// index, so the month label always describes the cards at the top of the
/// viewport.
pub(crate) fn visible_month(index: &PhotoIndex, first_visible: usize) -> (i32, usize) {
    let Some(entry) = index.entries.get(first_visible) else {
        return (0, 0);
    };
    let month = entry.captured_month;
    (month, index.month_count(month))
}

/// Photo at the top-left of the first visible row.
///
/// `offset_y` is the non-negative scroll distance from the top of the list;
/// callers convert the scroll handle's negative offset before calling.
pub(crate) fn anchor_photo_id(
    index: &PhotoIndex,
    offset_y: f32,
    columns: usize,
    row_height: f32,
) -> Option<i64> {
    let total = index.total;
    if total == 0 {
        return None;
    }
    let first_photo = first_visible_photo(offset_y, columns, row_height, total);
    index.entries.get(first_photo).map(|entry| entry.id)
}

/// List row that keeps `id` as the first photo of the top visible row under
/// `columns`. Missing ids anchor at the top.
pub(crate) fn row_for_photo(index: &PhotoIndex, id: i64, columns: usize) -> usize {
    index
        .row_of_id(id)
        .map(|position| position / columns.max(1))
        .unwrap_or(0)
}

/// The photo the next search result should be anchored to. Keep the requested
/// anchor when it survived the query; otherwise fall back to the first photo
/// (the top of the grid) or `None` for an empty result.
pub(crate) fn search_anchor_after_result(
    requested: Option<i64>,
    index: &PhotoIndex,
) -> Option<i64> {
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
pub(crate) struct PhotoPageCache {
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

    pub(crate) fn get_page(&mut self, page: usize) -> Option<Arc<Vec<Arc<Photo>>>> {
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
pub(crate) struct PhotoBrowser {
    pub(crate) index: Arc<PhotoIndex>,
    pub(crate) pages: PhotoPageCache,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) columns: usize,
    /// Row height of the current card layout, used when clamping after a
    /// removal. The gallery refreshes it from the display settings.
    pub(crate) row_height: f32,
    /// Top photo the gallery should keep anchored when the layout changes.
    pub(crate) pending_anchor: Option<i64>,
    pub(crate) generation: u64,
    pub(crate) labels_generation: u64,
    pub(crate) labels: Option<Arc<YearRailLabels>>,
    /// Shared with the mounted rail so a re-render cannot drop an in-flight
    /// drag or let it outlive the browser.
    pub(crate) rail_drag: Rc<Cell<bool>>,
    /// Whether the pointer is currently inside the rail gutter.
    pub(crate) rail_hover: Rc<Cell<bool>>,
}

impl PhotoBrowser {
    pub(crate) fn new(index: Arc<PhotoIndex>) -> Self {
        Self {
            index,
            pages: PhotoPageCache::new(),
            scroll: UniformListScrollHandle::new(),
            columns: 1,
            // The first grid render replaces this with the current layout.
            row_height: photo_row_height(MIN_CARD_WIDTH, true, true),
            pending_anchor: None,
            generation: 0,
            labels_generation: 0,
            labels: None,
            rail_drag: Rc::new(Cell::new(false)),
            rail_hover: Rc::new(Cell::new(false)),
        }
    }

    pub(crate) fn replace_index(&mut self, index: Arc<PhotoIndex>) {
        self.index = index;
        self.generation = self.generation.wrapping_add(1);
        self.pages.clear();
        self.labels = None;
        self.pending_anchor = None;
        self.rail_drag.set(false);
        self.rail_hover.set(false);
    }

    /// Invalidates the decoded page cache without replacing the index. Used
    /// when photo rows changed (compression replacement, rebinds) but the set
    /// and order of ids did not.
    pub(crate) fn invalidate_pages(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pages.clear();
    }

    /// Removes a batch of photos with a single index rebuild and page-cache
    /// clear. Bulk trash used to rebuild the whole snapshot once per photo.
    pub(crate) fn remove_photos(&mut self, ids: &HashSet<i64>) {
        if ids.is_empty() {
            return;
        }
        let entries: Vec<PhotoIndexEntry> = self
            .index
            .entries
            .iter()
            .filter(|entry| !ids.contains(&entry.id))
            .copied()
            .collect();
        if entries.len() == self.index.total {
            return;
        }
        self.replace_index(Arc::new(PhotoIndex::new(entries)));
        self.clamp_scroll_after_removal();
    }

    fn clamp_scroll_after_removal(&mut self) {
        let columns = self.columns.max(1);
        let rows = self.index.total.div_ceil(columns);
        if rows == 0 {
            self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
            return;
        }
        let current_row = (list_scroll_top(&self.scroll) / self.row_height).floor() as usize;
        if current_row >= rows {
            self.scroll
                .scroll_to_item_strict(rows - 1, ScrollStrategy::Nearest);
        }
    }

    pub(crate) fn photo_at(&mut self, photo_index: usize) -> Option<Arc<Photo>> {
        let page = photo_index / PAGE_SIZE;
        let offset = photo_index % PAGE_SIZE;
        self.pages.get_page(page)?.get(offset).cloned()
    }

    pub(crate) fn plan_pages(&mut self, start: usize, end: usize, limit: usize) -> Vec<usize> {
        let end = end.min(self.index.total);
        self.pages.plan_pages(start, end, limit)
    }

    pub(crate) fn insert_page(&mut self, page: usize, photos: Vec<Photo>, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.pages.insert_page(page, photos);
    }

    pub(crate) fn fail_page(&mut self, page: usize, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.pages.fail_page(page);
    }
}

/// Keeps the no-query search mirror on the current library snapshot.
///
/// Returns `true` when the mirror was replaced (and its page cache cleared).
pub(crate) fn sync_search_mirror(search: &mut PhotoBrowser, library: &PhotoBrowser) -> bool {
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
pub(crate) fn apply_library_index(
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
pub(crate) struct CountDebounce {
    pending: Option<i64>,
    since: Option<Instant>,
}

impl CountDebounce {
    pub(crate) fn observe(&mut self, count: i64, now: Instant, quiet: Duration) -> bool {
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

    pub(crate) fn reset(&mut self) {
        self.pending = None;
        self.since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{index_entry, test_photo};
    use crate::ui::theme::library_grid_metrics;
    use gpui_kit::{ScrollStrategy, UniformListScrollHandle, point, px};
    use vrc_photo_core::error::CoreError;
    use vrc_photo_core::settings::CardSize;

    /// Arbitrary non-196 row height so these tests prove the maths reads the
    /// supplied layout instead of a fixed constant.
    const TEST_ROW_HEIGHT: f32 = 180.0;

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
    fn revision_debounce_coalesces_a_burst() {
        let mut debounce = CountDebounce::default();
        let quiet = Duration::from_secs(2);
        let start = Instant::now();
        assert!(!debounce.observe(10, start, quiet));
        assert!(!debounce.observe(11, start + Duration::from_secs(3), quiet));
        assert!(!debounce.observe(12, start + Duration::from_secs(6), quiet));
        assert!(debounce.observe(12, start + Duration::from_secs(9), quiet));
        assert!(!debounce.observe(13, start + Duration::from_secs(12), quiet));
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
        assert_eq!(anchor_photo_id(&index, 0.0, 3, TEST_ROW_HEIGHT), Some(1));
        assert_eq!(
            anchor_photo_id(&index, TEST_ROW_HEIGHT + 1.0, 1, TEST_ROW_HEIGHT),
            Some(2)
        );
        assert_eq!(
            anchor_photo_id(&index, TEST_ROW_HEIGHT * 2.0 + 1.0, 1, TEST_ROW_HEIGHT),
            Some(3)
        );
    }

    #[test]
    fn row_for_photo_keeps_an_old_layout_anchor_on_the_new_grid() {
        let index = PhotoIndex::new((1..=12).map(|id| index_entry(id, 202601)).collect());
        // Under the old two-column layout the top visible row starts at id 7.
        let old_anchor = anchor_photo_id(&index, 3.0 * 100.0, 2, 100.0);
        assert_eq!(old_anchor, Some(7));

        // After the metrics change (three columns) the same photo must still
        // be the first card of the anchored row.
        let row = row_for_photo(&index, old_anchor.expect("anchor"), 3);
        assert_eq!(row, 2);
        assert_eq!(index.entries[row * 3].id, 7);

        // A removed photo falls back to the top instead of a stale row.
        assert_eq!(row_for_photo(&index, 999, 3), 0);
    }

    #[test]
    fn anchor_photo_id_handles_empty_and_overscrolled_indexes() {
        assert_eq!(
            anchor_photo_id(&PhotoIndex::new(Vec::new()), 0.0, 3, TEST_ROW_HEIGHT),
            None
        );
        let index = PhotoIndex::new(vec![index_entry(1, 202601), index_entry(2, 202512)]);
        assert_eq!(
            anchor_photo_id(&index, TEST_ROW_HEIGHT * 50.0, 4, TEST_ROW_HEIGHT),
            Some(2)
        );
    }

    #[test]
    fn first_visible_photo_uses_the_3_2_library_row_height() {
        let metrics = library_grid_metrics(1280.0, false, false, CardSize::Medium, true, true);
        assert_eq!(metrics.columns, 3);
        assert_eq!(
            first_visible_photo(
                metrics.row_height * 2.5,
                metrics.columns,
                metrics.row_height,
                100,
            ),
            6
        );
        assert_eq!(
            first_visible_photo(0.0, metrics.columns, metrics.row_height, 100),
            0
        );
    }

    #[test]
    fn first_visible_photo_floors_the_row_before_applying_columns() {
        // Halfway into row 2 with 3 columns: the top row starts at entry 6.
        // Truncating after multiplying (`2.5 * 3 = 7.5`) would pick entry 7.
        assert_eq!(
            first_visible_photo(TEST_ROW_HEIGHT * 2.5, 3, TEST_ROW_HEIGHT, 100),
            6
        );
        assert_eq!(first_visible_photo(0.0, 3, TEST_ROW_HEIGHT, 100), 0);
        assert_eq!(first_visible_photo(-20.0, 3, TEST_ROW_HEIGHT, 100), 0);
        assert_eq!(
            first_visible_photo(TEST_ROW_HEIGHT * 50.0, 3, TEST_ROW_HEIGHT, 10),
            9
        );
        assert_eq!(first_visible_photo(0.0, 3, TEST_ROW_HEIGHT, 0), 0);
    }

    #[test]
    fn visible_month_follows_the_first_visible_row() {
        let index = PhotoIndex::new(vec![
            index_entry(1, 202610),
            index_entry(2, 202610),
            index_entry(3, 202609),
            index_entry(4, 202609),
        ]);
        assert_eq!(index.month_count(202610), 2);
        assert_eq!(index.month_count(202609), 2);
        assert_eq!(index.month_count(202501), 0);
        assert_eq!(visible_month(&index, 0), (202610, 2));
        assert_eq!(visible_month(&index, 2), (202609, 2));
        assert_eq!(visible_month(&index, 3), (202609, 2));
        assert_eq!(visible_month(&index, 99), (0, 0));
    }

    #[test]
    fn range_between_returns_entries_in_index_order() {
        let index = PhotoIndex::new((1..=5).map(|id| index_entry(id, 202601)).collect());
        let range = index.range_between(4, 2).expect("range");
        assert_eq!(
            range.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(index.range_between(4, 99).is_none());
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
            anchor_photo_id(&updated, TEST_ROW_HEIGHT * 2.0 + 1.0, 1, TEST_ROW_HEIGHT),
            Some(11)
        );
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
        browser.remove_photos(&HashSet::from([2]));
        assert_eq!(browser.index.total, 2);
        assert_eq!(browser.index.years[0].count, 1);
        assert_eq!(browser.index.years[1].count, 1);
        assert!(browser.pages.get_page(0).is_none());
        assert_eq!(browser.index.row_of_id(1), Some(0));
        assert_eq!(browser.index.row_of_id(2), None);
        // Removing an id that is already gone is a no-op.
        browser.remove_photos(&HashSet::from([2]));
        assert_eq!(browser.index.total, 2);
    }

    #[test]
    fn invalidating_pages_keeps_the_index_and_rejects_stale_loads() {
        let mut browser =
            PhotoBrowser::new(Arc::new(PhotoIndex::new(vec![index_entry(1, 202601)])));
        let generation = browser.generation;
        browser.insert_page(0, vec![test_photo(1)], generation);
        assert!(browser.pages.get_page(0).is_some());

        browser.invalidate_pages();
        assert_eq!(browser.index.total, 1);
        assert!(browser.pages.get_page(0).is_none());
        assert_ne!(browser.generation, generation);

        // A page load that started before the invalidation must not resurrect.
        browser.insert_page(0, vec![test_photo(1)], generation);
        assert!(browser.pages.get_page(0).is_none());
    }

    #[test]
    fn removing_several_photos_rebuilds_the_index_once() {
        let entries = vec![
            index_entry(1, 202601),
            index_entry(2, 202601),
            index_entry(3, 202512),
            index_entry(4, 202512),
        ];
        let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(entries)));
        let generation = browser.generation;
        browser.insert_page(0, vec![test_photo(1)], generation);

        let removed: HashSet<i64> = [1, 3].into_iter().collect();
        browser.remove_photos(&removed);

        assert_eq!(browser.index.total, 2);
        assert_eq!(browser.index.row_of_id(2), Some(0));
        assert_eq!(browser.index.row_of_id(4), Some(1));
        assert_eq!(browser.index.month_count(202601), 1);
        assert_eq!(browser.index.month_count(202512), 1);
        assert!(browser.pages.get_page(0).is_none());

        // Removing ids that are already gone is a no-op.
        browser.remove_photos(&removed);
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
            .set_offset(point(px(0.), px(-TEST_ROW_HEIGHT * 10.0)));
        browser.remove_photos(&HashSet::from([1]));

        let state = browser.scroll.0.borrow();
        let deferred = state.deferred_scroll_to_item.expect("scroll clamp");
        // 9 photos over 2 columns is 5 rows; the last row index is 4.
        assert_eq!(deferred.item_index, 4);
        assert_eq!(deferred.strategy, ScrollStrategy::Nearest);
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
}
