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

// `PHOTO_ROW_HEIGHT` still lives at the crate root (Task 16 moves it), so this
// module borrows it temporarily instead of duplicating its value.
use crate::PHOTO_ROW_HEIGHT;
use crate::state::year_rail::{YearBucket, build_year_buckets};
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
}

impl PhotoIndex {
    pub(crate) fn new(entries: Vec<PhotoIndexEntry>) -> Self {
        let total = entries.len();
        let years = build_year_buckets(&entries);
        Self {
            entries,
            total,
            years,
        }
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
    /// removal. `photo_grid` refreshes it from the display settings.
    pub(crate) row_height: f32,
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
            row_height: PHOTO_ROW_HEIGHT,
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
        self.rail_drag.set(false);
        self.rail_hover.set(false);
    }

    /// Drops a photo from this snapshot without notifying the caller.
    ///
    /// The page cache and scroll position refer to the old row layout, so the
    /// cache is cleared and a scroll that now points past the last row is
    /// clamped to it.
    pub(crate) fn remove_photo(&mut self, id: i64) {
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
    use vrc_photo_core::error::CoreError;

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
        assert_eq!(anchor_photo_id(&index, 0.0, 3, PHOTO_ROW_HEIGHT), Some(1));
        assert_eq!(
            anchor_photo_id(&index, PHOTO_ROW_HEIGHT + 1.0, 1, PHOTO_ROW_HEIGHT),
            Some(2)
        );
        assert_eq!(
            anchor_photo_id(&index, PHOTO_ROW_HEIGHT * 2.0 + 1.0, 1, PHOTO_ROW_HEIGHT),
            Some(3)
        );
    }
}
