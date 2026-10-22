# 100k Photo Library Infinite Scroll Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the 300-photo limit from the library and search, fetch only the visible photo pages, and add a Google Photos-style year/month rail that jumps through a 100k-photo library without decoding images or querying SQLite in the UI thread.

**Architecture:** Load one ordered `(id, captured_at_utc, captured_month)` index per result set (about 2.4 MB at 100k). Render grid rows through `uniform_list`; fetch 256-photo pages by primary key into a small LRU cache as rows become visible. The rail paints year labels and the current month from the index and the `UniformListScrollHandle` offset, so it needs no database access and no per-scroll app state.

**Tech Stack:** Rust 2024, `gpui-kit = "=0.8.0"` / `gpui-pre 0.3.9`, `uniform_list`, SQLite via `rusqlite 0.40`, existing `image`/`webp` thumbnail pipeline.

**Spec:** `docs/superpowers/specs/2026-10-23-photo-library-100k-design.md`

## Global Constraints

- Do not bump `gpui-kit` or add a direct `gpui` dependency.
- Keep `crates/ui/src/main.rs` as the single UI file; do not split the UI into modules.
- No new Cargo dependencies.
- Page size is 256 photos; page cache is at most 24 pages; thumbnail cache is 192 entries; grid row height is 196 px.
- Index order for the UI is always `captured_at_utc DESC, id DESC`.
- The UI thread must never decode an image or run a SQLite query from `Render`/`prepaint`/`paint`.
- Work in a git worktree and a feature branch; never push to `main`.
- Code commits use `feat:` / `fix:` prefixes. Keep documentation in a separate docs-only PR per `AGENTS.md` §4.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Do not create tags or run the Release workflow.

## Review Focus

These are the conditions most likely to break even when the happy-path tests pass. Each one is pinned by a test in the task that owns the code.

1. **A 100k library at first paint** must not load 100k `Photo` rows or decode 300 thumbnails synchronously. The ignored 100k timing test and the page-cache tests cover this.
2. **The search viewport is much narrower than the library** because of the 290 px filter panel. Search must compute its own column count; the `search_available_width` tests cover this.
3. **Local years can interleave in a UTC-ordered index** around New Year/timezone changes. The bucket test uses interleaved `(2026, 2025, 2026)` entries and requires one bucket per year.
4. **Import or refresh while scrolled** must not throw the user back to the top, while Settings/Monitor reload must not rebuild the index at all. The anchor and reload-contract tests cover this.
5. **Trash from an active search snapshot** must remove the card immediately; otherwise the deleted photo remains selectable and quick look fails. `PhotoBrowser::remove_photo` covers this.
6. **The selected photo's thumbnail** can scroll out of view and be evicted; the inspector background then turns flat. The pinned-thumbnail test covers this.

---

### Task 1: Core index snapshot, page fetch, and migration

**Files:**
- Modify: `crates/core/src/model.rs` (near `PhotoSort` / `PhotoFilter`, around lines 300–320)
- Modify: `crates/core/src/db.rs` (schema constants around 173, `Db` around 184, queries around 323–437, helpers around 1052, tests around 1149)

**Interfaces:**
- Produces: `model::PhotoIndexEntry { id: i64, captured_at_utc: i64, captured_month: i32 }`
- Produces: `model::PhotoTextFilter { world: Option<String>, player: Option<String>, tag: Option<String>, favorite: bool, from_utc: Option<i64>, to_utc: Option<i64> }`
- Produces: `Db::photo_index(&self, sort: PhotoSort) -> Result<Vec<PhotoIndexEntry>>`
- Produces: `Db::photo_index_text(&self, filter: &PhotoTextFilter) -> Result<Vec<PhotoIndexEntry>>`
- Produces: `Db::photos_by_ids(&self, ids: &[i64]) -> Result<Vec<Photo>>`
- Produces: `Db::path(&self) -> Option<&Path>` and `Db::open_read_only(path: impl AsRef<Path>) -> Result<Self>`
- Consumes: existing `PHOTO_SELECT`, `PHOTO_FROM`, `sample_photo`, `PhotoSort`, `PhotoFilter`.

- [ ] **Step 1: Write the failing migration test**

Add to `crates/core/src/db.rs` tests:

```rust
#[test]
fn migration_adds_captured_id_index() {
    let db = Db::open_in_memory().unwrap();
    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_photos_captured_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-core migration_adds_captured_id_index`
Expected: FAIL because the index does not exist.

- [ ] **Step 3: Add `SCHEMA_V3` and migration step**

Add next to `SCHEMA_V2`:

```sql
CREATE INDEX IF NOT EXISTS idx_photos_captured_id
    ON photos(captured_at_utc DESC, id DESC)
    WHERE status = 'present';
```

Add `if version < 3 { ... }` to `Db::migrate`, inserting the migration row and setting `user_version = 3` exactly like the V2 block.

- [ ] **Step 4: Run the migration test to verify it passes**

Run: `cargo test -p vrc-photo-core migration_adds_captured_id_index`
Expected: PASS.

- [ ] **Step 5: Write the failing index/page tests**

Add a helper next to `sample_photo`:

```rust
fn photo_at(root_id: i64, path: &str, utc: i64, local: &str) -> NewPhoto {
    let mut photo = sample_photo(root_id, path, utc);
    photo.captured_at_local = local.to_string();
    photo
}
```

Add these tests:

```rust
#[test]
fn photo_index_orders_newest_first_and_uses_local_month() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let older = db
        .upsert_photo(&photo_at(root, "/photos/older.png", 1_700_000_000, "2025-04-30 09:00:00"))
        .unwrap();
    let newer = db
        .upsert_photo(&photo_at(root, "/photos/newer.png", 1_700_100_000, "2025-05-01 08:00:00"))
        .unwrap();
    let missing = db
        .upsert_photo(&photo_at(root, "/photos/missing.png", 1_700_200_000, "2025-06-01 08:00:00"))
        .unwrap();
    db.mark_missing(missing).unwrap();

    let entries = db.photo_index(PhotoSort::Newest).unwrap();
    assert_eq!(
        entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![newer, older]
    );
    assert_eq!(entries[0].captured_month, 202505);
    assert_eq!(entries[1].captured_month, 202504);
}

#[test]
fn photo_index_falls_back_to_utc_month_for_bad_local_time() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    // 2025-05-01 00:00:00 UTC
    db.upsert_photo(&photo_at(root, "/photos/bad-local.png", 1_746_057_600, ""))
        .unwrap();
    let entries = db.photo_index(PhotoSort::Newest).unwrap();
    assert_eq!(entries[0].captured_month, 202505);
}

#[test]
fn photo_index_text_returns_more_than_300_matches() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    for index in 0..305 {
        db.upsert_photo(&photo_at(
            root,
            &format!("/photos/{index}.png"),
            1_700_000_000 + index,
            "2025-04-30 09:00:00",
        ))
        .unwrap();
    }
    let entries = db
        .photo_index_text(&PhotoTextFilter::default())
        .unwrap();
    assert_eq!(entries.len(), 305);
}

#[test]
fn photo_index_text_applies_favorite_filter() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let favorite = db
        .upsert_photo(&photo_at(root, "/photos/favorite.png", 1_700_000_100, "2025-04-30 09:00:00"))
        .unwrap();
    db.upsert_photo(&photo_at(root, "/photos/plain.png", 1_700_000_000, "2025-04-30 08:00:00"))
        .unwrap();
    db.set_favorite(favorite, true).unwrap();

    let entries = db
        .photo_index_text(&PhotoTextFilter {
            favorite: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(entries.iter().map(|entry| entry.id).collect::<Vec<_>>(), vec![favorite]);
}

#[test]
fn photos_by_ids_preserves_order_and_skips_missing_ids() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let mut ids = Vec::new();
    for index in 0..501 {
        ids.push(
            db.upsert_photo(&photo_at(
                root,
                &format!("/photos/{index}.png"),
                1_700_000_000 + index,
                "2025-04-30 09:00:00",
            ))
            .unwrap(),
        );
    }

    let photos = db.photos_by_ids(&ids).unwrap();
    assert_eq!(photos.len(), 501);
    assert_eq!(photos.first().map(|photo| photo.id), Some(ids[0]));
    assert_eq!(photos.last().map(|photo| photo.id), Some(ids[500]));
    let missing = db.photos_by_ids(&[ids[500], ids[0], -1]).unwrap();
    assert_eq!(
        missing.iter().map(|photo| photo.id).collect::<Vec<_>>(),
        vec![ids[500], ids[0]]
    );
}

#[test]
fn database_path_is_exposed_for_read_only_workers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let db = Db::open(&path).unwrap();
    assert_eq!(db.path(), Some(path.as_path()));
    assert_eq!(Db::open_in_memory().unwrap().path(), None);
}
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core photo_index`
Expected: FAIL because `PhotoIndexEntry`, `PhotoTextFilter`, and the methods do not exist.

- [ ] **Step 7: Add the model types**

Add near `PhotoSort` in `model.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoIndexEntry {
    pub id: i64,
    pub captured_at_utc: i64,
    /// Local capture month encoded as YYYYMM, e.g. 202504.
    pub captured_month: i32,
}

#[derive(Debug, Clone, Default)]
pub struct PhotoTextFilter {
    pub world: Option<String>,
    pub player: Option<String>,
    pub tag: Option<String>,
    pub favorite: bool,
    pub from_utc: Option<i64>,
    pub to_utc: Option<i64>,
}
```

- [ ] **Step 8: Implement the DB methods**

In `db.rs`:

- Extract `fn photo_order(sort: PhotoSort) -> &'static str` and use it in `list_photos`, `search_photos`, and the new index methods.
- Extract `fn build_text_filter(world, player, tag, favorite, from_utc, to_utc) -> (String, Vec<Value>)` from `search_photos_text`; make `search_photos_text` call it.
- Implement `photo_index` with:

```sql
SELECT p.id, p.captured_at_utc,
       COALESCE(
         CAST(strftime('%Y%m', p.captured_at_local) AS INTEGER),
         CAST(strftime('%Y%m', p.captured_at_utc, 'unixepoch') AS INTEGER),
         0
       )
FROM photos p
WHERE p.status = 'present'
ORDER BY <photo_order>
```

- Implement `photo_index_text` with `PHOTO_FROM`, the extracted `build_text_filter` clauses, the same month expression, and `ORDER BY p.captured_at_utc DESC, p.id DESC`.
- Implement `photos_by_ids`: chunk `ids` into groups of 500, run `SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE p.id IN (...)`, insert into a `HashMap<i64, Photo>`, then return `ids.iter().filter_map(|id| by_id.remove(id)).collect()`.
- Add `path: Option<PathBuf>` to `Db`; set it in `open`, leave it `None` in `open_in_memory`.
- Add `open_read_only` using `OpenFlags::SQLITE_OPEN_READ_ONLY`; call `configure` but not `migrate`. Return an error if the file cannot be opened; callers fall back to a synchronous query.
- Add `path()` accessor.

- [ ] **Step 9: Run the core tests to verify they pass**

Run: `cargo test -p vrc-photo-core`
Expected: PASS, including the existing filter tests.

- [ ] **Step 10: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat: add photo index and page queries"
```

---

### Task 2: Asynchronous thumbnail cache

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields ~155–223, `App::new` ~267, `reload` ~394, `load_thumbnails` ~488, `color_for` ~510, `select_photo` ~542, `photo_card` ~1430, quick look ~1707, `load_render` ~4081, tests ~4490)

**Interfaces:**
- Produces:

```rust
const THUMB_CACHE_ENTRIES: usize = 192;
const THUMB_IN_FLIGHT: usize = 4;

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
    fn get(&mut self, id: i64) -> Option<Arc<RenderImage>>;
    fn color(&self, id: i64) -> Option<u32>;
    fn begin(&mut self, id: i64) -> bool;
    fn insert(&mut self, id: i64, image: Arc<RenderImage>, color: u32);
    fn fail(&mut self, id: i64);
    fn pin(&mut self, id: i64);
}

fn decode_thumbnail(id: i64, cache_path: &Path, source: &Path)
    -> Option<(u32, image::RgbaImage)>;
fn make_render_image(rgba: image::RgbaImage) -> Arc<RenderImage>;
```

- Consumes: `average_bgra`, `demo_thumb_path`, `thumbnail::ensure_thumbnail`, `thumbnail::thumbnail_path`, existing `App.demo_assets`.

- [ ] **Step 1: Write the failing cache tests**

Add to the UI test module (import `ThumbnailCache`, `THUMB_CACHE_ENTRIES`, `THUMB_IN_FLIGHT`; add `Photo`, `PhotoStatus` imports if not already present):

```rust
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
    assert!(cache.get(1).is_some());

    assert!(cache.begin(999));
    cache.insert(999, test_render_image(), 0x445566);
    assert!(cache.get(1).is_some());
    assert!(cache.get(2).is_none());
}

#[test]
fn thumbnail_cache_does_not_retry_failed_or_pending_ids() {
    let mut cache = ThumbnailCache::default();
    assert!(cache.begin(1));
    assert!(!cache.begin(1));
    cache.fail(1);
    assert!(!cache.begin(1));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui thumbnail_cache`
Expected: FAIL because the cache does not exist.

- [ ] **Step 3: Implement `ThumbnailCache` and the decode helper**

Implementation notes:
- `get` returns the image and moves the ID to the back of `lru`.
- `begin` returns false when the ID is cached, pending, failed, or `in_flight == THUMB_IN_FLIGHT`; otherwise inserts into `pending`, increments `in_flight`, and returns true.
- `insert` removes from `pending`, decrements `in_flight`, inserts image/color, moves the ID to the back of `lru`, then evicts from the front while `images.len() > THUMB_CACHE_ENTRIES`, skipping `pinned`.
- `fail` removes from `pending`, decrements `in_flight`, and inserts into `failed`.
- `pin` sets `pinned = Some(id)`; the previous ID needs no separate bookkeeping because only one ID can be pinned and eviction compares against the current value.
- `decode_thumbnail` uses `cache_path` if it is a non-empty file; otherwise calls `thumbnail::ensure_thumbnail(id, source)` and uses the returned path. It opens the WebP/PNG, converts to RGBA, swaps R and B (`pixel.0.swap(0, 2)`), computes `average_bgra(rgba.as_raw())`, and returns `(color, rgba)`.
- `make_render_image` performs `RenderImage::new(SmallVec::from_elem(image::Frame::new(rgba), 1))` on the UI thread.

- [ ] **Step 4: Run the cache tests to verify they pass**

Run: `cargo test -p vrc-photo-ui thumbnail_cache`
Expected: PASS.

- [ ] **Step 5: Replace the eager thumbnail maps**

- Change `App.thumbnails: HashMap<...>` and `App.thumbnail_colors` to `thumbnails: ThumbnailCache`; initialize with `ThumbnailCache::default()`.
- Delete `App::load_thumbnails` and its call in `reload`.
- `color_for` becomes `self.thumbnails.color(photo.id).unwrap_or_else(|| scene_color(...))`.
- `selected_color` continues to use `color_for`.
- `photo_card` and the quick-look background use `self.thumbnails.get(id)`.
- In `select_photo`, call `self.thumbnails.pin(id)` before `load_selected`, and call `self.request_thumbnail(photo, cx)` with the loaded `selected_photo` when its thumbnail is missing.

- [ ] **Step 6: Add the background request path**

Add:

```rust
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
            .spawn(async move { decode_thumbnail(id, &cache_path, &source) })
            .await;
        this.update(cx, |this, cx| {
            match decoded {
                Some((color, rgba)) => {
                    this.thumbnails.insert(id, make_render_image(rgba), color);
                }
                None => this.thumbnails.fail(id),
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}
```

Call `request_thumbnail` from `photo_card` when `self.thumbnails.get(id)` is `None`. Do not call it from `Render` for photos that already have a thumbnail.

- [ ] **Step 7: Run the UI tests and clippy**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: decode grid thumbnails asynchronously"
```

---

### Task 3: Photo browser and virtualized library grid

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields ~155–223, `App::new` ~228–362, `reload` ~394, `refresh_live` ~376, `viewer_source_ids` ~632, `photo_list` ~1341–1428, `photo_card` ~1430, `search_screen` ~3065, helpers ~4278, tests ~4490)

**Interfaces:**
- Produces:

```rust
const PAGE_SIZE: usize = 256;
const MAX_PAGES: usize = 24;

struct PhotoIndex {
    entries: Vec<PhotoIndexEntry>,
    total: usize,
}

impl PhotoIndex {
    fn new(entries: Vec<PhotoIndexEntry>) -> Self;
}

fn search_available_width(window_width: f32, sidebar_collapsed: bool, inspector_collapsed: bool) -> f32;

struct PhotoPageCache { /* pages, order, pending, failed */ }
impl PhotoPageCache {
    fn new() -> Self;
    fn clear(&mut self);
    fn get_page(&mut self, page: usize) -> Option<Arc<Vec<Arc<Photo>>>>;
    fn insert_page(&mut self, page: usize, photos: Vec<Photo>);
    fn plan_pages(&mut self, start: usize, end: usize, limit: usize) -> Vec<usize>;
    fn fail_page(&mut self, page: usize);
}

struct PhotoBrowser {
    index: Arc<PhotoIndex>,
    pages: PhotoPageCache,
    scroll: UniformListScrollHandle,
    columns: usize,
    generation: u64,
}

impl PhotoBrowser {
    fn new(index: Arc<PhotoIndex>) -> Self;
    fn replace_index(&mut self, index: Arc<PhotoIndex>);
    fn photo_at(&mut self, photo_index: usize) -> Option<Arc<Photo>>;
    fn plan_pages(&mut self, start: usize, end: usize, limit: usize) -> Vec<usize>;
    fn insert_page(&mut self, page: usize, photos: Vec<Photo>, generation: u64);
    fn fail_page(&mut self, page: usize, generation: u64);
}
```

- Consumes: `Db::photo_index`, `Db::photos_by_ids`, `db.photo_index(PhotoSort::Newest)`, `thumbnail::...` from Task 2.
- App fields become `library: PhotoBrowser`, `search_view: PhotoBrowser`; `photos: Vec<Photo>` is removed. `search_results: Option<Vec<Photo>>` remains until Task 4.

- [ ] **Step 1: Write the failing page-cache and width tests**

Add to the UI tests:

```rust
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
    assert!(cache.get_page(0).is_some());
    cache.insert_page(MAX_PAGES, vec![test_photo(999)]);
    assert!(cache.get_page(0).is_none());
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
fn search_available_width_drops_to_the_filter_panel_width() {
    assert_eq!(grid_columns(search_available_width(1280.0, false, true)), 3);
    assert_eq!(grid_columns(search_available_width(1440.0, false, true)), 4);
    assert_eq!(grid_columns(search_available_width(1080.0, false, true)), 2);
    assert!(grid_columns(search_available_width(1280.0, false, false)) >= 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui page_cache`
Expected: FAIL because the types do not exist.

- [ ] **Step 3: Implement the page cache and width helper**

- `PhotoPageCache` uses `HashMap<usize, Arc<Vec<Arc<Photo>>>>`, `VecDeque<usize>` for LRU, and `HashSet` for `pending`/`failed`. `get_page` updates LRU order; `insert_page` clears `pending`/`failed` for the page and evicts from the front while over `MAX_PAGES`; `plan_pages` clamps the end to `photo_end`, computes the page range, skips cached/pending/failed pages, marks up to `limit` pages pending, and returns them.
- `PhotoBrowser::replace_index` increments `generation` and clears the page cache.
- `PhotoBrowser::photo_at` converts the page number and calls `get_page`.
- `PhotoBrowser::insert_page` and `fail_page` ignore a stale `generation`, then delegate and notify are the caller's job.
- `search_available_width` is `(window_width - sidebar_width - inspector_width - 290.0 - 26.0 - 52.0).max(0.0)`.

- [ ] **Step 4: Run the pure tests to verify they pass**

Run: `cargo test -p vrc-photo-ui page_cache search_available_width`
Expected: PASS.

- [ ] **Step 5: Replace `photos` with the library browser**

- In `App::new`, build `PhotoIndex` from `db.photo_index(PhotoSort::Newest).unwrap_or_default()`, wrap it in `Arc`, create `library` and `search_view`, and pass both into the `Self` literal.
- Replace `reload` with a method that clears the grid epoch/intro/hover, rebuilds the library index, replaces `search_view.index` only when `search_results.is_none()`, then reloads profiles/stats/settings/jobs/selected metadata as the current method does.
- Change `refresh_live` to compare `count_photos()` with `self.library.index.total`.
- Update `viewer_source_ids`: when `Screen::Search` and `search_results` is `Some`, use those IDs; otherwise use `self.library.index.entries`.
- Remove every remaining `self.photos` reference. Search with `rg "self\\.photos" crates/ui/src/main.rs` and fix all of them before compiling.

- [ ] **Step 6: Implement `photo_grid`**

Add a single reusable method:

```rust
fn photo_grid(&mut self, search: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement;
```

Implementation requirements:
- Compute `columns` from `grid_available_width(...)` for the library and `search_available_width(...)` for search; write it to the selected browser before computing rows.
- `rows = browser.index.total.div_ceil(columns.max(1))`.
- Clone `Arc<PhotoIndex>`, `UniformListScrollHandle`, `generation`, and `columns` for the `uniform_list` closure.
- Use stable list IDs `photo-grid-list` and `search-grid-list`.
- In the range callback: compute `photo_start`/`photo_end`, call `browser.plan_pages(photo_start, photo_end, 2)`, and schedule `cx.defer_in(window, move |this, _, cx| this.load_pages(...))` when pages are missing.
- Render each visible photo with `this.photo_card(photo.as_ref(), PhotoCardLayout::Grid { index }, cx)`.
- For a missing page/photo, render a placeholder with the same 128 px image area and `PANEL_2` background; never change the row height.
- Add `.track_scroll(&scroll)` to the `uniform_list` and wrap the list in a `.relative()` container so Task 5 can overlay the rail.
- Keep the existing `.pt(px(4.))`, `.flex_1()`, `.min_w_0()`, `.min_h_0()` styles.

Add:

```rust
fn load_pages(
    &mut self,
    search: bool,
    generation: u64,
    pages: Vec<usize>,
    cx: &mut Context<Self>,
);
```

It must collect the ID slice from the chosen browser, drop the mutable borrow, call `db.photos_by_ids`, re-borrow the browser, insert/fail pages, and call `cx.notify()`. Load at most two pages per call.

- [ ] **Step 7: Wire the library and search fallback**

- Replace `photo_list` with `self.photo_grid(false, window, cx)`. Pass `window` through `photos_screen` from `Render::render`.
- In `search_screen`, when `search_results` is `None`, replace the old `flex_wrap` fallback with `self.photo_grid(true, window, cx)` inside a fixed-height right column: header on top, grid filling the rest, no outer `overflow_y_scrollbar`. Pass `window` through `search_screen` from `Render::render`.
- When `search_results` is `Some`, keep the existing 300-card layout for this task only. Task 4 removes it.

- [ ] **Step 8: Run the UI tests and clippy**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo --demo-reset` manually and verify the library scrolls with placeholders briefly and thumbnails appear.

- [ ] **Step 9: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: virtualize the photo library over an index snapshot"
```

---

### Task 4: Search virtualization and 300-photo cap removal

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields ~215, `run_search` ~793, `search_screen` ~3065, `viewer_source_ids` ~632, imports ~20)

**Interfaces:**
- Produces: `App.search_active: bool`, `App.searching: bool`, `App.search_request: u64`
- Produces: `fn run_search(&mut self, cx: &mut Context<Self>)` using `Db::photo_index_text`
- Produces: `fn apply_search_result(&mut self, generation: u64, result: vrc_photo_core::error::Result<Vec<PhotoIndexEntry>>, cx: &mut Context<Self>)`
- Produces: `fn clear_search(&mut self, cx: &mut Context<Self>)`
- Consumes: `PhotoTextFilter`, `Db::path()`, `Db::open_read_only`, `PhotoIndex::new`, `search_view`, `photo_grid(true, window, cx)`.

- [ ] **Step 1: Write the failing width/limit tests**

The `search_available_width` assertions from Task 3 remain. Add a test for the result copy/removal is not practical in a pure test; verify it manually in Step 6.

- [ ] **Step 2: Replace `search_results` with the search browser**

- Add `search_active: bool` and `searching: bool`; initialize both to `false`.
- Remove `search_results: Option<Vec<Photo>>`.
- `clear_search` sets `search_active = false`, `searching = false`, replaces `search_view.index` with an `Arc` clone of `library.index`, clears its page cache, resets its scroll to row 0, and notifies.
- `viewer_source_ids` uses `search_view.index` when `screen == Search && search_active`, otherwise `library.index`.

- [ ] **Step 3: Implement `run_search`**

- Build `PhotoTextFilter` from the inputs and `search_days`.
- Increment `search_request`; capture the generation.
- If `db.path()` is `Some`, set `searching = true`, notify, and spawn:

```rust
let path = self.db.path().unwrap().to_path_buf();
let filter = filter.clone();
cx.spawn(async move |this, cx| {
    let result = cx
        .background_executor()
        .spawn(async move {
            Db::open_read_only(path).and_then(|db| db.photo_index_text(&filter))
        })
        .await;
    this.update(cx, |this, cx| this.apply_search_result(generation, result, cx))
        .ok();
})
.detach();
```

- If `db.path()` is `None` (in-memory demo/fallback), run `db.photo_index_text` synchronously and call `apply_search_result`.
- `apply_search_result` ignores a result whose generation is stale; on success, replaces `search_view.index` with `Arc::new(PhotoIndex::new(entries))`, sets `search_active = true`, clears `searching`, resets the search scroll to 0, and notifies. On error, sets `searching = false`, keeps the previous results, and puts a message in `self.message`.
- If `open_read_only` fails, fall back to the synchronous query rather than leaving a spinner.

- [ ] **Step 4: Rebuild the search screen around the virtual grid**

- Always render the right column as a fixed header plus `photo_grid(true, window, cx)`; no `flex_wrap`, no `overflow_y_scrollbar` on the result container.
- The count is `search_view.index.total`; show `検索中…` while `searching`.
- Remove the `結果は最大300枚です` line.
- The clear button calls `clear_search`.
- The empty state is shown when `search_active && search_view.index.total == 0`.

- [ ] **Step 5: Run the UI tests and clippy**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS.

Run the app manually in `--demo` mode: run a search with more than 300 results (the demo fixture may need a larger seed) and confirm the count is exact and all cards are reachable.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: virtualize search results and remove the 300-photo cap"
```

---

### Task 5: Year rail

**Files:**
- Modify: `crates/ui/src/main.rs` (imports, `App`/`PhotoBrowser` fields, `photo_grid` ~1341–1428, helpers ~4278, tests ~4490, `Info`/`MUTED` tokens)

**Interfaces:**
- Produces:

```rust
const PHOTO_ROW_HEIGHT: f32 = 196.0;
const RAIL_WIDTH: f32 = 88.0;
const RAIL_TOP_PAD: f32 = 24.0;
const RAIL_TRACK_INSET: f32 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YearBucket {
    year: i32,
    start_row: usize,
    display_row: usize,
    count: usize,
}

fn build_year_buckets(entries: &[PhotoIndexEntry]) -> Vec<YearBucket>;
fn rail_y(display_row: usize, total: usize, height: f32) -> f32;
fn rail_row_at(y: f32, total: usize, height: f32) -> usize;
fn format_captured_month(month: i32) -> String;
fn visible_year_buckets(buckets: &[YearBucket], height: f32) -> Vec<usize>;
fn list_scroll_top(handle: &UniformListScrollHandle) -> f32;

struct YearRailLabels { /* years, months, ShapedLine */ }
impl YearRailLabels {
    fn build(index: &PhotoIndex, window: &mut Window) -> Self;
}

struct YearRail { /* labels, index, scroll, columns, drag_active */ }
impl Element for YearRail { ... }
```

Extend `PhotoIndex` with `years: Vec<YearBucket>` and make `PhotoIndex::new` call `build_year_buckets`.

- Consumes: `PhotoIndex.years`, `PhotoIndex.entries`, `UniformListScrollHandle`, `PHOTO_ROW_HEIGHT`, `YearBucket`.
- Add `rail_drag: Rc<Cell<bool>>` to `PhotoBrowser` in this task so a re-render cannot drop the drag flag.

- [ ] **Step 1: Write the failing bucket and rail geometry tests**

```rust
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
            .map(|bucket| (bucket.year, bucket.start_row, bucket.display_row, bucket.count))
            .collect::<Vec<_>>(),
        vec![(2026, 0, 0, 2), (2025, 1, 2, 2)]
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
}

#[test]
fn rail_hides_labels_that_would_overlap() {
    let buckets: Vec<YearBucket> = (0..30)
        .map(|index| YearBucket {
            year: 2026 - index,
            start_row: index * 100,
            display_row: index * 100,
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui year_buckets rail_`
Expected: FAIL because the helpers do not exist.

- [ ] **Step 3: Implement the bucket and geometry helpers**

- `build_year_buckets` scans entries in order, records the first row of each `captured_month / 100`, counts every entry, then emits buckets in first-occurrence order with cumulative `display_row`.
- `rail_y(row, total, height) = RAIL_TOP_PAD + (row / total) * (height - 2 * RAIL_TOP_PAD)`, clamped to the track.
- `rail_row_at(y, total, height)` is the inverse, rounded and clamped to `0..total - 1`.
- `visible_year_buckets` keeps the current/newest year and every Nth bucket so adjacent label y positions are at least 16 px apart; dots are painted for all buckets independently.
- `format_captured_month` splits `YYYYMM` into `YYYY年M月`; return `"不明"` for 0.

- [ ] **Step 4: Implement `YearRailLabels` and `YearRail`**

`YearRailLabels::build`:
- Add `years: Vec<YearBucket>` to `PhotoIndex`; `PhotoIndex::new` calls `build_year_buckets`.
- Shape every year label once with `window.text_system().shape_line(...)`, using a 10 px `Noto Sans JP` `TextRun` and `MUTED_2`/`TEXT` colors.
- Shape one month label for every distinct `captured_month` in the index; store in a `HashMap<i32, ShapedLine>`.
- Store the labels in `Arc<YearRailLabels>` and cache on `PhotoBrowser` (`labels_generation: u64`, `labels: Option<Arc<YearRailLabels>>`). Rebuild only when the index generation changes or the cache is empty.

`YearRail` element:
- `request_layout` fills its 88 px-wide parent (`size_full`).
- `paint` computes `bounds`, `track_x = bounds.right() - px(12.)`, `top = bounds.top() + px(24.)`, `usable_height = bounds.height - px(48.)`.
- Draw the track with `window.paint_quad(fill(...))`, a 3 px dot at each visible year's `rail_y(display_row)`, and the selected/current year label.
- Determine `first_visible_photo` from `list_scroll_top(&scroll) / PHOTO_ROW_HEIGHT * columns`, clamped to `total - 1`; look up `entries[first_visible].captured_month`, draw the month pill at `rail_y(first_visible)`, and paint its cached `ShapedLine`.
- Labels are painted with `ShapedLine::paint(origin, px(12.), TextAlign::Right, Some(px(56.)), window, cx)`.
- Register `MouseDownEvent`, `MouseMoveEvent`, and `MouseUpEvent` with `window.on_mouse_event` during paint. On mouse down inside `bounds`, call `cx.stop_propagation()`, set `drag_active`, and jump. On move while `drag_active`, jump to the pointer row. On mouse up, clear the flag.
- `jump_to(y)`: if y is within 8 px of a year label's y, use that bucket's `start_row`; otherwise use `rail_row_at`. Convert to a grid row with `photo_row / columns`; set the base scroll offset directly to `-(grid_row * PHOTO_ROW_HEIGHT)` clamped to `base_handle.max_offset()`; call `window.refresh()`.

- [ ] **Step 5: Integrate the rail into `photo_grid`**

- Wrap the `uniform_list` in a `.relative()` container.
- When `browser.index.total > 0`, add:

```rust
div()
    .absolute()
    .right_0()
    .top_0()
    .bottom_0()
    .w(px(RAIL_WIDTH))
    .child(YearRail::new(...))
```

- Pass the browser's `scroll`, `index`, `columns`, and `rail_drag` to the element.
- Ensure the rail is above the list in paint order (child added after the list).
- Hide the rail for `total == 0`.

- [ ] **Step 6: Run the tests, clippy, and a manual rail check**

Run: `cargo test -p vrc-photo-ui rail_`
Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS.

Run the app with `--demo`, drag the rail from top to bottom, click a year label, and resize the window; the month pill must follow the top photo and the grid must not jump to the wrong year.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add a Google Photos-style year rail"
```

---

### Task 6: Reload contract, live refresh, import anchor, and trash

**Files:**
- Modify: `crates/ui/src/main.rs` (`refresh_live` ~376, `reload` ~394, `trash_selected` ~730, `enqueue_selected` ~761, `nav_item` ~1007, monitor/settings reload ~3344/3664, import ~3269, `App` fields)

**Interfaces:**
- Produces:

```rust
fn reload_all(&mut self, reset_scroll: bool, cx: &mut Context<Self>);
fn reload_library(&mut self, preserve_anchor: bool, cx: &mut Context<Self>);
fn reload_aux(&mut self, cx: &mut Context<Self>);
fn mark_library_dirty(&mut self);
fn switch_screen(&mut self, screen: Screen, cx: &mut Context<Self>);
fn anchor_photo_id(index: &PhotoIndex, offset_y: f32, columns: usize) -> Option<i64>;
impl PhotoIndex {
    fn row_of_id(&self, id: i64) -> Option<usize>;
}
impl PhotoBrowser {
    fn remove_photo(&mut self, id: i64);
}
```

- Consumes: `PhotoBrowser::replace_index`, `PhotoIndex::row_of_id`, `PhotoBrowser::remove_photo`.

- [ ] **Step 1: Write the failing anchor/removal tests**

```rust
#[test]
fn anchor_photo_id_uses_the_first_visible_row() {
    let entries = vec![index_entry(1, 202601), index_entry(2, 202512), index_entry(3, 202411)];
    let index = PhotoIndex::new(entries);
    assert_eq!(anchor_photo_id(&index, 0.0, 3), Some(1));
    assert_eq!(anchor_photo_id(&index, PHOTO_ROW_HEIGHT + 1.0, 1), Some(2));
    assert_eq!(anchor_photo_id(&index, PHOTO_ROW_HEIGHT * 2.0 + 1.0, 1), Some(3));
}

#[test]
fn removing_a_photo_clears_pages_and_rebuilds_year_counts() {
    let entries = vec![index_entry(1, 202601), index_entry(2, 202601), index_entry(3, 202412)];
    let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(entries)));
    let generation = browser.generation;
    browser.insert_page(0, vec![test_photo(1)], generation);
    browser.remove_photo(2);
    assert_eq!(browser.index.total, 2);
    assert_eq!(browser.index.years[0].count, 1);
    assert_eq!(browser.index.years[1].count, 1);
    assert!(browser.pages.get_page(0).is_none());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui anchor_photo_id removing_a_photo`
Expected: FAIL because the helpers do not exist.

- [ ] **Step 3: Implement `PhotoBrowser::remove_photo` and anchor helpers**

- `remove_photo` clones `index.entries`, removes the entry by ID, rebuilds `Arc::new(PhotoIndex::new(entries))` (which recomputes `years`), increments `generation`, clears the page cache, and does not notify. If the current scroll row is now past the new row count, clamp it with `scroll_to_item_strict(rows - 1, Nearest)`. 
- `row_of_id` performs a linear scan over `index.entries` and returns the position.
- `anchor_photo_id` returns `None` for an empty index; otherwise computes `first_photo = (offset_y.max(0) / PHOTO_ROW_HEIGHT).floor() as usize * columns`, clamps to `total - 1`, and returns `entries[first_photo].id`.

- [ ] **Step 4: Split the reload operations**

- `reload_aux` contains the current profiles/stats/jobs/settings/compressed/selected/monitor loading, but no index or page-cache work.
- `reload_library(preserve_anchor)`:
  - capture `anchor_photo_id(&self.library.index, -offset.y, columns)`;
  - rebuild `library.index`;
  - clear pages and increment generation;
  - if preserving, find `row_of_id(anchor)` and call `scroll_to_item_strict(row / columns, Top)`; otherwise scroll to 0;
  - update `search_view.index` only when `!search_active` (clone the library index and clear its pages);
  - call `reload_aux`.
- `reload_all(reset_scroll)` calls `reload_library(false)` plus the `grid_epoch`/intro/hover resets.
- `mark_library_dirty` sets `library_dirty = true`.

- [ ] **Step 5: Rewire the callers**

- `App::new`: `reload_all(true)`.
- Import completion: `reload_library(true)`.
- Photo-root change/explicit library refresh: `reload_library(false)`.
- `trash_selected`: `mark_missing`, `library.remove_photo(id)`, `search_view.remove_photo(id)`, clear selection/preview, then `reload_aux`; no full rebuild.
- `enqueue_selected`: `reload_aux`.
- Settings "設定を再読込": `reload_aux`.
- Monitor "再読込": `reload_aux`.
- `nav_item` click: `switch_screen(screen, cx)`.
- `focus_search`: call `switch_screen(Screen::Search, cx)` before focusing.
- `switch_screen`: if leaving Photos, keep the index; if entering Photos and `library_dirty`, call `reload_library(true)` and clear the flag.

- [ ] **Step 6: Debounce live refresh**

- Add `library_dirty: bool` and `last_library_refresh: Instant` (initialize to `Instant::now()`).
- `refresh_live` updates stats/jobs/selected metadata and compares `count_photos()` with `library.index.total`.
- If the count differs and the screen is Photos and `last_library_refresh.elapsed() >= 2 s`, call `reload_library(true)` and update the timestamp.
- If the count differs on another screen, call `mark_library_dirty()`.
- Do not call `reload_all` from `refresh_live`.

- [ ] **Step 7: Run the tests, clippy, and manual checks**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS.

Manual checks: scroll down, import a photo, confirm the top photo stays; press Settings reload, confirm no jump; trash a search result, confirm it disappears immediately.

- [ ] **Step 8: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "fix: preserve scroll across refresh and remove trashed search results"
```

---

### Task 7: Performance measurement and 100k fixture

**Files:**
- Modify: `crates/core/src/db.rs` (ignored test)
- Modify: `crates/ui/src/main.rs` (timing helper)
- No production behavior changes.

**Interfaces:**
- Produces: `fn timing_enabled() -> bool` and `fn log_timing(label: &str, start: Instant)` in the UI (or inline `eprintln!` gated by `VRC_PHOTO_TIMING`).
- Produces: `#[test] #[ignore = "100k timing benchmark"] fn photo_index_scales_to_100k()`.

- [ ] **Step 1: Add the ignored 100k test**

Seed 100k rows with a raw prepared statement inside one transaction (do not call `upsert_photo` 100k times). Use distinct `abs_path`/`rel_path`, timestamps spaced by one second, and a local date string. Then:

```rust
let start = Instant::now();
let entries = db.photo_index(PhotoSort::Newest).unwrap();
let index_elapsed = start.elapsed();
assert_eq!(entries.len(), 100_000);
eprintln!("photo_index 100k: {index_elapsed:?}");

let ids: Vec<i64> = entries[50_000..50_256].iter().map(|entry| entry.id).collect();
let start = Instant::now();
let photos = db.photos_by_ids(&ids).unwrap();
eprintln!("photos_by_ids 256: {:?}", start.elapsed());
assert_eq!(photos.len(), 256);
```

Use generous assertions (`index_elapsed < 2s`, page query `< 200ms`) so the test documents the budget without becoming flaky on CI; record the actual numbers in the PR description. Run it manually:

`cargo test -p vrc-photo-core photo_index_scales_to_100k -- --ignored --nocapture`

- [ ] **Step 2: Add lightweight UI timing**

Use `std::time::Instant` and `eprintln!` only when `VRC_PHOTO_TIMING` is set. Instrument:
- library index build,
- `load_pages` (page query time),
- background thumbnail decode time.

Do not add a logging dependency.

- [ ] **Step 3: Create a 100k fixture for UI QA**

Generate a temporary DB outside the repo (for example `/tmp/opencode/vrc-100k.db`) with the same schema and 100k rows, using the ignored test's SQL or `sqlite3`. Run:

`VRC_PHOTO_DB=/tmp/opencode/vrc-100k.db VRC_PHOTO_TIMING=1 cargo run -p vrc-photo-ui --release`

Record: first frame, first page, scroll to the bottom, rail drag, and a 300+ result search.

- [ ] **Step 4: Run the full verification**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Run: `cargo test --workspace`
Expected: all PASS (the ignored test is skipped).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/db.rs crates/ui/src/main.rs
git commit -m "test: measure 100k photo index and page queries"
```

---

### Task 8: Documentation and final QA

**Files:**
- Create (separate docs branch): `docs/adr/0008-photo-index-virtual-window.md`
- Modify (separate docs branch): `docs/adr/0007-photo-load-cap.md`, `docs/roadmap.md`, `docs/features.md`, `docs/ui.md`, `docs/HANDOFF.md`
- No code changes in this task.

**Interfaces:**
- Consumes: the measured numbers from Task 7.

- [ ] **Step 1: Write ADR-0008**

Record: the 300 cap problem, the ordered-index + 256-photo page cache decision, why keyset append was rejected for the rail UX, memory/performance numbers, and the add-only `idx_photos_captured_id` migration.

- [ ] **Step 2: Mark ADR-0007 superseded**

Change its status to `Superseded by ADR-0008`; do not delete the history.

- [ ] **Step 3: Update user-facing docs**

- `docs/roadmap.md`: remove the 300-photo current-state line and the 300-limit open question; mark the 100k item done only after the measurements are recorded.
- `docs/features.md`: describe unlimited library/search and the year rail.
- `docs/ui.md`: describe the rail interactions and the current-month callout.
- `docs/HANDOFF.md`: remove the 300-photo caveat.

- [ ] **Step 4: Final visual QA**

At 1280×820 and 1440×900:
- initial library paint with 100k rows;
- scroll to the bottom and back;
- drag the rail to 2009 and back;
- click a year label;
- search with more than 300 results;
- resize columns while scrolled;
- import while scrolled;
- trash a search result;
- select a photo, scroll it out of view, and confirm the inspector background stays.

Save temporary screenshots under `/tmp/opencode`, never in the repository.

- [ ] **Step 5: Open the docs-only PR**

```bash
git add docs/adr/0008-photo-index-virtual-window.md docs/adr/0007-photo-load-cap.md docs/roadmap.md docs/features.md docs/ui.md docs/HANDOFF.md
git commit -m "docs: document the 100k photo index and year rail"
```

Run `cargo fmt --all -- --check` (docs-only PR still uses the standard gate) and open a docs PR. Do not mix these files into the code PR.

---

## Plan Self-Review

- **Spec coverage:** index snapshot, page cache, library virtualization, search virtualization, 300 removal, year rail, async thumbnails, refresh/trash, performance, docs. The spec's "date section headers" and "sort UI" are explicitly non-goals and have no task.
- **Type consistency:** `PhotoIndexEntry`, `PhotoTextFilter`, `PhotoIndex`, `YearBucket`, `PhotoPageCache`, `PhotoBrowser`, `ThumbnailCache`, `YearRail` are named consistently across tasks. `captured_month` is always `YYYYMM`; `display_row` is always the cumulative photo count; `start_row` is always the jump target.
- **Review focus:** 100k first paint → Task 7 + page-cache tests; search width → Task 3; interleaved years → Task 3; import/refresh anchor → Task 6; trash from search → Task 6; pinned thumbnail → Task 2.
- **Proportion:** test snippets are limited to behavior that is easy to get subtly wrong (month encoding, bucket merging, LRU, rail math, anchors). Implementation bodies are left to the engineer except the bucket/rail algorithms and the offset formula.
