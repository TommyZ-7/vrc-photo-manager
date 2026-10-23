# 100k Photo Library: Infinite Scroll and Google Photos-style Year Rail

- Date: 2026-10-23
- Issue: [#8 feat: 一覧・検索の10万枚対応（キーセットページング）](https://github.com/TommyZ-7/vrc-photo-manager/issues/8)
- Status: Draft for review
- Target: `crates/core/src/db.rs`, `crates/core/src/model.rs`, `crates/ui/src/main.rs`, docs
- Supersedes: [ADR-0007](../adr/0007-photo-load-cap.md)

## 1. Intent

The library currently loads at most 300 photos into memory and the search
screen renders every result with `flex_wrap`. At 100k photos the user cannot
reach older photos, and the 300-photo thumbnail decode blocks the first frame.

This work makes the library and search feel like one continuous photo wall:
the grid renders only what is visible, photos are fetched in small pages as the
user scrolls, and a Google Photos-style rail on the right shows where the
current position sits in the whole library. Clicking or dragging the rail jumps
to a year; the current month is called out at the thumb position.

Performance is part of the feature, not a follow-up. The UI thread must not
decode images during layout, and the database must not be asked for rows that
are not near the viewport.

## 2. Goals

- Remove the 300-photo cap from the library and from search.
- Reach every matching photo by continuous scrolling; no page buttons,
  "load more" button, or duplicate/missing rows.
- Render the grid with `uniform_list`, including search results.
- Add a year rail (Google Photos reference image) with:
  - year labels positioned proportionally to the number of photos in each year;
  - a current-month callout (e.g. `2025年4月`);
  - the rail position following scroll, including inertial scrolling;
  - click and drag to jump to a year;
  - the same component on the library and search screens.
- Keep the initial library frame under 1 second on a 100k-photo database
  (development machine), and a visible-range page fetch under 200 ms.
- Keep memory bounded: index snapshot, page cache, and thumbnail cache all have
  explicit limits.
- Keep the existing card design, selection, quick look, inspector, filters, and
  import/refresh behavior working.

## 3. Non-goals

- Date section headers inside the grid, a timeline/masonry layout, or variable
  row heights.
- A sort/filter UI beyond the existing search filters. The library remains
  newest-first.
- Multi-select, bulk actions, or drag-to-import changes.
- Full-text search (FTS5) or a new search syntax.
- Windows visual verification; that remains in #9.
- Reworking the compression/monitor/settings screens.
- A database schema rewrite. Only an additive index migration is allowed.

## 4. Anti-goals

- Do not load 100k `Photo` rows (with paths, names, and context) into memory.
- Do not decode thumbnails synchronously in `Render`/`prepaint`/`paint`.
- Do not run a database query from inside the `uniform_list` render callback.
- Do not rebuild the whole grid on every wheel event.
- Do not change the visual card size, grid gaps, or column rules.
- Do not make search a second, divergent grid implementation.

## 5. Constraints

- `gpui-kit = "=0.8.0"` (GPUI `gpui-pre` 0.3.9). `crates/ui/src/main.rs` stays a
  single UI file; do not split the crate.
- `uniform_list` requires a uniform row height. The current row height is
  196 px with a 16 px gap and 4 px top padding; preserve it.
- The UI has no direct `gpui` dependency; use the `gpui_kit` facade.
- The demo mode uses an in-memory database and must keep working without a
  file path.
- Any new public core API must have unit tests in `crates/core/src/db.rs`.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

## 6. Approaches

### A. Ordered index snapshot + visible-page cache (chosen)

Load `(id, captured_at_utc, captured_month)` for all matching photos once,
ordered by the active sort. This is about 2.4 MB for 100k entries. The
`uniform_list` row count and year buckets are then exact. Photo rows for the
visible range are fetched by primary key in 256-photo pages and kept in a small
LRU cache.

- Pros: exact rail geometry and jumps; scrolling works in both directions;
  search and library share one model; no cursor/append bookkeeping; row count
  is known immediately; best fit for a Google Photos rail.
- Cons: one initial index query proportional to the result set; a full index
  rebuild is needed when photos are added or removed. At 100k this is a
  sub-second, single-query operation.

### B. Keyset append pages

Fetch 200 photos per keyset cursor and append at the end of the list.

- Pros: constant memory, no full index query, matches the wording in issue #8.
- Cons: the total row count is unknown until the end; the rail cannot map a
  position or jump accurately; a jump replaces the buffer, so the user cannot
  scroll upward past the jump without a second prepend path. Search and library
  would need separate anchor logic. This complicates the exact UX requested.

### C. Hybrid: keyset pages + separate year index

Fetch pages by keyset and maintain a separate year/month count index.

- Pros: bounded page memory and a jumpable rail.
- Cons: still needs bidirectional re-anchoring after a jump; two sources of
  truth for ordering; more edge cases for duplicates and gaps than A.

A is chosen because the rail is a random-access control, not an append-only
feed. ADR-0007 already anticipated "read all IDs + fetch the visible range
lazily" as the replacement for the 300-photo cap.

## 7. Design

### 7.1 Core data access

Add to `crates/core/src/model.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoIndexEntry {
    pub id: i64,
    pub captured_at_utc: i64,
    /// Local capture month encoded as YYYYMM (e.g. 202504).
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

`captured_month` is computed in SQL from `captured_at_local` so the rail follows
the photographer's local calendar, not UTC. The expression is
`CAST(strftime('%Y%m', p.captured_at_local) AS INTEGER)` with a fallback to
`strftime('%Y%m', p.captured_at_utc, 'unixepoch')` when the local string is
empty or malformed. The fallback is covered by a unit test.

Add to `Db`:

```rust
pub fn photo_index(&self, sort: PhotoSort) -> Result<Vec<PhotoIndexEntry>>;
pub fn photo_index_text(&self, filter: &PhotoTextFilter)
    -> Result<Vec<PhotoIndexEntry>>;
pub fn photos_by_ids(&self, ids: &[i64]) -> Result<Vec<Photo>>;
```

- `photo_index` uses the same `ORDER BY` as `list_photos` but no `LIMIT`.
- `photo_index_text` reuses the filter builder currently embedded in
  `search_photos_text`; extract a private `build_text_filter` so the two
  methods cannot drift. It returns all matches, newest first.
- `photos_by_ids` chunks the input into groups of at most 500 IDs, runs
  `SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE p.id IN (...)`, then reorders the
  result to match the input. Missing IDs are allowed (a photo can be deleted
  between the index and page query).
- Keep `list_photos` and `search_photos_text` for the daemon/tests; the UI stops
  calling them. Remove the 300-specific comments and docs.

Add migration `SCHEMA_V3`:

```sql
CREATE INDEX IF NOT EXISTS idx_photos_captured_id
    ON photos(captured_at_utc DESC, id DESC)
    WHERE status = 'present';
```

This index covers the newest/oldest index scans and the `status = 'present'`
predicate. The existing `idx_photos_captured` remains; do not drop it in this
change.

Search filters that use `EXISTS` (player/tag) still use their existing indexes.
The index scan for a 100k library is expected to stay below 300 ms; the
performance test records the actual value.

### 7.2 UI browser model

Replace the two ad-hoc data sources (`photos: Vec<Photo>` and
`search_results: Option<Vec<Photo>>`) with one model:

```rust
struct PhotoIndex {
    entries: Vec<PhotoIndexEntry>, // ordered by the active sort
    years: Vec<YearBucket>,        // descending for Newest
    total: usize,
}

struct YearBucket {
    year: i32,
    start_row: usize,   // first occurrence; jump target
    display_row: usize, // cumulative photo count; rail position
    count: usize,
}

struct PhotoBrowser {
    index: Arc<PhotoIndex>,
    pages: PhotoPageCache,
    scroll: UniformListScrollHandle,
    columns: usize,
    generation: u64,
}
```

`App` holds `library: PhotoBrowser` and `search_view: PhotoBrowser`.
`Screen::Photos` uses `library`; `Screen::Search` uses `search_view`.

When no query has been run, the Search screen still needs its own scroll
handle, page cache, and column count: the search result viewport is narrower
than the library viewport because of the 290 px filter panel, so sharing the
library's columns or pixel offset would place cards below the 170 px minimum.
Use a dedicated `search_view: PhotoBrowser`:
- no query: `search_view.index` is an `Arc` clone of `library.index`, but the
  scroll, page cache, and columns are its own;
- query: `search_view.index` is the result index and its scroll is reset to 0.

`columns` belongs to the browser, not to the app. Compute it from the width
actually available to that browser:

- library: `grid_available_width(window_width, sidebar_collapsed, inspector_collapsed)`;
- search: `search_available_width(window_width, sidebar_collapsed)`, computed as
  the window width minus the sidebar, the 290 px filter panel, its 26 px left
  margin, and the right column's 52 px horizontal padding. The Search screen
  has no inspector column, so the inspector state is deliberately not an
  argument and no inspector width is subtracted.

Add `search_available_width(...)` next to `grid_available_width` and unit tests
at 1280×820 and 1440×900 for each sidebar state (and confirm the inspector
state does not change the search column count). Both helpers feed the existing
`grid_columns` / `MIN_CARD_WIDTH` rule. At 1280 px with the sidebar expanded
and the filter panel shown, the search viewport gets 3 columns; at 1440 px it
gets 4; at 1080 px it gets 2.

`columns` is updated at the start of each screen's render, before the row count
is computed. A column change only changes `rows`; the index and page cache stay
put, and the stable `uniform_list` ID preserves the pixel offset. Rail target
conversion always uses the browser's current `columns`.

`PhotoPageCache`:

```rust
const PAGE_SIZE: usize = 256;
const MAX_PAGES: usize = 24;

struct PhotoPageCache {
    pages: HashMap<usize, Arc<Vec<Arc<Photo>>>>,
    order: VecDeque<usize>,       // access-order LRU, front = least recently used
    pending: HashSet<usize>,
    failed: HashSet<usize>,
}
```

- A page holds 256 photos; cache holds at most 24 pages (6,144 photos, a few
  MB). `photo_at(photo_index)` returns `Option<Arc<Photo>>`.
- The cache is an access-order LRU: `get_page` moves the page to the back of
  `order`, and `insert_page` evicts from the front. A test must not call
  `get_page(0)` before inserting the overflowing page and then expect page 0
  to be evicted; that call has already refreshed page 0. The eviction test
  inserts `MAX_PAGES` pages, inserts one more, and only then checks page 0.
- On a miss, the render path only records the page in `pending` and schedules
  a deferred load. It never queries SQLite during layout.
- The deferred load fetches `photos_by_ids` for that page, stores the result,
  and calls `cx.notify()`. At most two pages are loaded per deferred callback;
  if more are missing, the next frame schedules the next batch. This bounds a
  fast scrollbar drag to a few milliseconds of work per frame.
- `generation` increments whenever the index is replaced. A page load that
  finishes for an old generation is discarded.
- If `photos_by_ids` returns fewer rows than requested, the page is still
  cached; missing rows render as neutral placeholders until the next index
  refresh removes them.

### 7.3 Virtual grid and infinite scroll

- `uniform_list` row count is `index.total.div_ceil(columns)`. The row
  renderer fetches photos by photo index from the page cache. Missing pages
  produce a placeholder card with the same dimensions and a flat
  `PANEL_2` background; no spinner and no layout shift.
- Because every row exists from the first frame, "infinite scroll" is the
  natural result of virtualization: scrolling to any row triggers a page load
  for that range. There is no explicit "load next page" trigger and no
  end-of-list sentinel.
- Keep the stable list ID `photo-grid-list` for the library and use
  `search-grid-list` for search. Separate IDs prevent a search's scroll offset
  from leaking into the library.
- The library still uses `.track_scroll(&browser.scroll)` so the rail can read
  the offset and jump.
- On column-count changes, rows recompute but the photo index and page cache do
  not change. The existing stable-ID scroll preservation remains. Rail target
  conversion uses `photo_index / columns` at the time of the jump.
- The row renderer uses `Arc<Photo>` clones per visible card. It must not clone
  the full `Photo` vector.

### 7.4 Year rail

The rail is shared by the library and search.

**Data.** `YearBucket` is built from ordered entries in one pass. For `Newest`
the entries run newest to oldest. The current month is read from
`entries[first_visible_photo].captured_month`; no extra DB query is needed.

The index is ordered by `captured_at_utc`, but years are labelled from
`captured_at_local`. A photo taken around New Year with a different timezone
offset can therefore interleave two local years in the UTC sequence. The
bucket builder merges non-contiguous runs of the same local year into one
bucket:

- `start_row` is the first (newest) occurrence of that year in the ordered
  index and is the jump target;
- `count` is the total number of photos in that local year;
- `display_row` is the cumulative count of all newer years and is used for the
  rail's vertical position;
- bucket order follows the first occurrence, so labels and jump targets stay
  monotonic;
- rail positions use cumulative counts, so a year with many photos still gets
  more vertical space.

This keeps one label per year and makes jumps land on a photo from that year.
The visual error near a year boundary is at most the handful of photos whose
local offset differs. A unit test covers interleaved local years and asserts
that no year appears twice.

**Geometry.**

- Overlay width: 88 px, flush to the right edge of the grid viewport.
- Vertical padding: 24 px top and bottom; the track is 2 px wide, 12 px from
  the right edge.
- `rail_y(row) = padding + (row / total) * usable_height`, clamped to the
  track. This makes spacing proportional to photo count, matching the
  reference image (a year with many photos gets more space).
- A 3 px dot is drawn at each year's `display_row`. Year labels are
  right-aligned ending 8 px left of the track. Clicking a label jumps to its
  `start_row`.
- The current-month pill (`YYYY年M月`) is drawn at the current top row. It is
  80×18 px with 9 px corners, uses `PANEL_2` at 94% opacity with `TEXT`-colored
  text, and overlays the track.
- If year labels would be closer than 16 px, walk the buckets from the newest
  and keep the first label, then the next label that is at least 16 px below
  the last kept one. Dots remain for every year, so a 100-year library still
  does not paint 100 labels.

**Painting and caching.** Implement a small `YearRailPainter` custom element (same
pattern as `gpui_base::Scrollbar`):

- The year labels and every distinct month label are shaped once per index
  generation and stored in an `Arc<YearRailLabels>` on `PhotoBrowser` (or an
  equivalent cache keyed by generation and font size). `ShapedLine` is `Clone`
  and its layout is `Arc`-backed, so a render only clones cheap handles.
- Constructed during render with that cache plus the `UniformListScrollHandle`.
- `paint` reads the scroll offset from `handle.0.borrow().base_handle.offset()`
  every frame and draws the track, dots, labels, and month pill. It performs no
  DB query, no thumbnail request, and no label shaping.
- GPUI's own wheel handler calls `cx.notify` for the view, so wheel scrolling
  does re-render the view. That re-render is O(visible cards) and must stay that
  way: capture `Arc<PhotoIndex>` and the page cache through `this`, never clone
  the full entry/photo vectors.
- Dragging uses `window.refresh()` after setting the offset, so it redraws the
  retained tree without a full view render.
- The painter is focusable-free and has no accessibility role; the rail is a
  pointer shortcut, not the only way to scroll.
- The first visible photo index is derived from the offset and the known row
  height (196 px), clamped to `0..total`. `UniformListScrollState::last_item_size`
  stores the viewport size, not the row height, so it is not used for this
  calculation. Define `PHOTO_ROW_HEIGHT: f32 = 196.0` next to the row layout and
  add a test that the card row still uses that height.

**Interaction.** The `YearRail` element itself registers `MouseDownEvent`,
`MouseMoveEvent`, and `MouseUpEvent` with `window.on_mouse_event` during paint
(the same pattern as `gpui_base::Scrollbar`). This keeps the pointer mapping in
the same custom element that has the rail bounds, so no separate overlay div or
app-level drag state is needed:

- Use a shared `Rc<Cell<bool>>` for the drag flag, stored on `PhotoBrowser` so
  it survives a view re-render mid-drag.
- On mouse down inside the rail bounds, call `cx.stop_propagation()` so photo
  cards under the rail never receive the click, set the drag flag, and jump.
- If the pointer is within 8 px of a year label, jump to that bucket's
  `start_row`; otherwise map y proportionally to a photo row.
- For dragging, handle move events globally so the drag continues when the
  pointer leaves the rail. Compute the target pixel offset directly
  (`-(grid_row as f32 * PHOTO_ROW_HEIGHT)`, clamped to `-max_offset..0`, because
  GPUI's `ScrollHandle::max_offset()` is the non-negative scrollable extent and
  the actual offset is zero or negative) and call
  `base_handle.set_offset(...)` then `window.refresh()`. This redraws the
  retained element tree without rebuilding the app view or re-shaping the
  labels.
- On mouse up, clear the drag flag.
- Wheel events over the rail are allowed to bubble to the grid; do not consume
  them.

The rail is hidden when `total == 0`. The UI currently has no non-date sort, so
there is no additional sort-based visibility rule.

### 7.5 Search

- `run_search` builds a `PhotoTextFilter` from the existing inputs and the
  `search_days` preset.
- If `Db` has a file path (normal mode), build the index on a background
  executor with a short-lived read-only connection. This requires:
  - adding `path: Option<PathBuf>` to `Db`;
  - `Db::open_read_only(path)` using `OpenFlags::SQLITE_OPEN_READ_ONLY` and the
    same `configure` pragmas (without migrations).
  Demo/in-memory mode has no path and runs the (small) query synchronously.
- While the background search runs, keep the previous results visible and show
  `検索中…` in the result header; disable the Search button until it finishes.
  A generation counter discards a stale result if the user starts another
  search.
- On completion, replace `search_view.index` with the result index, clear its
  page cache, reset its scroll to 0, and notify. Clearing the search restores
  `search_view.index` to an `Arc` clone of `library.index` and resets the
  search view's scroll.
- The result count is `browser.index.total`, not the number of loaded cards.
  Remove the "結果は最大300枚です" line and all 300-limit copy.
- Render results with the same virtual grid/rail: the filter panel stays as the
  left column; the right column becomes a fixed result header plus a
  `uniform_list` that fills the remaining height. The whole right column must
  not be one outer scroll container anymore.
- Search is a snapshot. It does not auto-refresh when the library changes; the
  user presses Search again. This is existing behavior and is documented in the
  UI copy if needed.

### 7.6 Thumbnails

Remove `load_thumbnails`'s bulk synchronous decode. Replace `thumbnails` and
`thumbnail_colors` with:

```rust
const THUMB_CACHE_ENTRIES: usize = 192; // ~50 MB decoded RGBA

struct ThumbnailCache {
    images: HashMap<i64, Arc<RenderImage>>,
    colors: HashMap<i64, u32>,
    lru: VecDeque<i64>,
    pending: HashSet<i64>,
    failed: HashSet<i64>,
}
```

- Request thumbnails only for the rendered/visible cards. Skip IDs that are
  cached, pending, or failed.
- Keep at most 4 background decodes in flight; when no slot is free, the card
  waits for a later frame. A request:
  1. resolves the cache path (`thumbnails/{id}.webp`, or the demo asset path);
  2. if missing, calls `thumbnail::ensure_thumbnail(id, abs_path)` in the
     background (this is already atomic and shared with the daemon);
  3. opens the WebP, converts to RGBA, swaps to BGRA, and computes the average
     color;
  4. returns `(id, generation, color, buffer)` to the UI thread;
  5. the UI thread creates `RenderImage::new(...)` and inserts it under the
     generation check.
- Use `cx.spawn` with `cx.background_executor()` for the decode, then update the
  entity. Never create a `RenderImage` off the UI thread.
- LRU eviction keeps the cache at 192 entries. A pinned ID (the selected photo,
  and therefore the viewer photo) is never evicted. If all entries are visible
  (a very short window), skip eviction for that frame.
- `select_photo` calls `pin(id)` and requests the thumbnail if it is not cached
  or pending. The previous selection is unpinned. The painter/card still falls
  back to the scene color if the thumbnail file is genuinely missing.
- A failed/missing thumbnail is remembered in `failed` so a broken file does
  not trigger a retry loop. Show the existing scene-derived color placeholder.
- Page eviction does not evict thumbnails; thumbnails are keyed by photo ID and
  survive scrolling back.
- On demo mode, the path resolver uses `demo_thumb_path`, preserving the
  current demo behavior.

### 7.7 Refresh, import, and deletion

The old `reload` mixed index work with auxiliary state (profiles, stats, jobs,
settings, thumbnails) and always reset the grid. Split it into three explicit
operations:

- `reload_all(reset_scroll)`: rebuild the library index, clear the page cache,
  reload auxiliary state, and optionally reset the scroll. Used by `App::new`
  and by a photo-root change.
- `reload_library(preserve_anchor)`: rebuild only the library index and page
  cache; keep the current top photo in place when requested. Used after import
  completion and after a count change detected while the Photos screen is
  active.
- `reload_aux()`: refresh stats, jobs, profiles, compressed IDs, settings, and
  the selected photo metadata. It does not touch the index, page cache, scroll,
  or thumbnails. Used by compression enqueue, Settings reload, and Monitor
  reload.
- `mark_library_dirty()`: set a flag instead of rebuilding while the user is on
  Search, Compression, Monitor, or Settings. When the Photos screen is next
  shown, run `reload_library(preserve_anchor)`.

Caller contract:

| Caller | Operation |
|---|---|
| `App::new` | `reload_all(reset)` |
| import completion | `reload_library(preserve)` |
| photo-root change / explicit library refresh | `reload_library(reset)` |
| trash | remove from both browsers locally, then `reload_aux()`; no full rebuild |
| compression enqueue | `reload_aux()` |
| Settings "設定を再読込" | `reload_aux()` |
| Monitor "再読込" | `reload_aux()` |
| 3-second live refresh, count changed on Photos | debounce 2 s, then `reload_library(preserve)` |
| 3-second live refresh, count changed elsewhere | `mark_library_dirty()` |

`reload_library` steps:

1. Capture the current top photo ID and the pixel offset within its row from
   the tracked scroll handle.
2. Rebuild the index on the normal connection (the query is synchronous and
   fast). Recompute year buckets.
3. Clear `pages`, `pending`, and `failed` pages and increment `generation`.
4. Find the anchor ID's new row (linear scan over the index; 100k is cheap) and
   call `scroll_to_item_strict(new_row / columns, Top)` so the anchor photo
   stays at the top when a new photo is inserted above it. Sub-row pixel drift
   of less than one 196 px row is acceptable. When `preserve_anchor` is false,
   scroll to row 0 instead.
5. If the selected photo is gone, keep the existing `guard_viewer_selection`
   behavior.

`refresh_live` reads stats/jobs as today and updates selected metadata; it only
compares `count_photos()` with `library.index.total` to decide whether to
schedule a rebuild. It must not clear `quick_look_photo_ids` for an unrelated
library change.

The demo index rebuild is a no-op when the in-memory DB did not change.

### 7.8 Selection, quick look, and inspector

- `select_photo` remains ID-based; the card renderer can select a photo even if
  its page was loaded only moments ago.
- `viewer_source_ids()` uses the active browser's index IDs (search when on the
  Search screen, library otherwise). Return an `Arc<[i64]>` or clone the IDs
  once when the viewer opens; do not scan `Photo` rows.
- Quick look still loads the selected photo with `photo_by_id` and the 1200 px
  preview as today. The 100k index does not change preview memory.
- Trash removes the photo from both the library and the active search index:
  `PhotoBrowser::remove_photo(id)` removes the entry, decrements `total`,
  recomputes year buckets, clears the page cache (row indices after the removed
  entry shift), and clamps the scroll. This prevents a deleted photo from
  remaining selectable in a search snapshot. The viewer closes through
  `guard_viewer_selection`.
- `PhotoBrowser::remove_photo` is also used by the live refresh when a single
  photo disappears, avoiding a full 100k rebuild for a one-row change.
- The inspector's selected photo must stay thumbnailed: `select_photo` calls
  `ThumbnailCache::pin(id)` and requests the thumbnail immediately if missing.
  Pinning exempts that ID from LRU eviction. The previous ID is unpinned when
  the selection changes. This keeps the inspector's blurred background and the
  card filling in instead of falling back to a flat scene color after the card
  scrolls out of view.

### 7.9 Empty and error states

- Empty library: same message as today, no rail, no page requests.
- Search with no matches: same "条件に一致する写真がありません" panel, count 0.
- Index query failure: keep the previous index if one exists; otherwise show
  `写真一覧を読み込めませんでした` with a retry action (the existing message
  surface can be reused). Do not panic.
- Page query failure: mark the page failed for this generation and render
  placeholders; retry on the next explicit reload, not in a loop.
- Thumbnail failure: color placeholder only.

## 8. Performance plan

Budgets (development machine, 100k photos, release build):

| Operation | Budget | Target |
|---|---|---|
| `photo_index` query (100k, newest) | 300 ms | < 150 ms |
| First page (`photos_by_ids`, 256) | 50 ms | < 10 ms |
| Initial frame incl. window open | 1000 ms | < 700 ms |
| Page fetch while scrolling | 200 ms | < 50 ms |
| UI-thread thumbnail decode | 0 | 0 |
| Index memory | 10 MB | ~3 MB |
| Page cache | 10 MB | < 5 MB |
| Thumbnail cache | 70 MB | ~50 MB |

Instrumentation:

- `VRC_PHOTO_TIMING` env-gated timing probes for index build, page load,
  thumbnail decode, background search, and first frame. The probes are silent
  when the variable is unset and add no logging dependency.
- A `#[test] #[ignore]` in `crates/core/src/db.rs` seeds 100k photos in a
  transaction, runs `photo_index` and `photos_by_ids`, and prints elapsed times.
  It is run manually and not part of the normal CI time budget.
- Query-plan check for the new index is optional but recommended:
  `EXPLAIN QUERY PLAN SELECT ... ORDER BY captured_at_utc DESC, id DESC`.
- Manual QA with a generated 100k DB (or the ignored test) at 1280×820 and
  1440×900: initial paint, fast scrollbar drag, year jump, and a 300+ result
  search.

Do not use `photos_by_ids` with all 100k IDs; pages are always <= 256.

### 8.1 Measured results (2026-10-23, release, 100k synthetic DB)

| Probe | Result |
|---|---|
| `photo_index` 100k | 61.6 ms (release test); 51–55 ms on UI reload |
| `photos_by_ids` 256 | 0.58 ms (release test); 0.41–0.57 ms in the UI |
| `photo_index_text` 100k | 60.9 ms; 0.78 ms for a 529-result background search |
| First frame, warm | 229–243 ms |
| First frame, cold | 1.4995 s |
| Startup DB work | ~108 ms warm / ~140 ms cold (the index is built twice) |
| Seed 100k rows | 0.455 s |

The warm numbers meet the table budgets. **The cold first frame does not meet
the 1000 ms budget.** The DB portion is only about 140 ms, so the overage is
most likely window/GPU initialization and shader work; an empty-DB cold
baseline is still needed before that is treated as confirmed. The startup path
also queries the same index twice (`App::new` and the immediate `reload_all`),
costing about 108 ms warm and 140 ms cold. Both are follow-ups, not claims that
all budgets passed.

Memory figures in the table are design bounds, not measurements: the index is
about 2.4 MB at 100k entries, the page cache is capped at 24 pages (6,144
photos), and the thumbnail cache at 192 decoded entries (~50 MB estimated).

## 9. Testing

### Core

- `photo_index` returns all present photos in newest-first order, including
  photos without context.
- `photo_index_text` applies world/player/tag/favorite/date filters and returns
  more than 300 rows when the data has more.
- `photos_by_ids` preserves the requested order, tolerates missing IDs, and
  chunks correctly above 500 IDs.
- `captured_month` uses the local date and falls back to UTC for empty/invalid
  local strings.
- The new index exists after migration and `EXPLAIN QUERY PLAN` uses it.
- The ignored 100k timing test passes its printed budget assertions in release
  mode.

### UI helpers

- `build_year_buckets` handles multiple photos per year, the first/last year,
  empty input, and interleaved local years (one bucket per year, monotonic
  positions, jump target on a photo from that year).
- `rail_y` and `rail_row_at` round-trip within one pixel and clamp at the ends.
- `photo_row_to_grid_row` handles partial rows and column changes.
- `search_available_width(window_width, sidebar_collapsed)` plus `grid_columns`
  yields 3 columns at 1280×820 and 4 at 1440×900 with the sidebar expanded;
  the inspector state does not change the result.
- LRU eviction keeps recently touched pages, never evicts a pending page, and
  never evicts a pinned thumbnail. The eviction test does not touch page 0
  before the overflowing insert when it expects page 0 to be evicted.
- Anchor remapping preserves the top photo when new photos are inserted.
- `remove_photo` removes an ID from an index, recomputes year buckets, and
  invalidates only the affected page.

### UI behavior (manual or GPUI test where practical)

- Launch with 0, 1, and 100k photos.
- Scroll to the bottom and back to the top; no duplicate or missing IDs.
- Drag the rail from 2026 to 2009 and back; the grid keeps up and the month pill
  follows.
- Click a year label; that year's first photo appears at the top.
- Resize from 1280 to 1440 width; the rail still maps to the same year.
- Search with more than 300 results; count is exact and all results are
  reachable.
- At 1280×820 with the sidebar expanded, search cards stay at or above 170 px
  and do not borrow the library's column count.
- Trash a photo while a search result is open; the card disappears from the
  snapshot immediately and cannot be reopened from quick look.
- Press Settings "設定を再読込" while scrolled; the grid does not jump.
- Import a photo while scrolled; the top photo stays put after the refresh.
- Reduced-motion setting: no new animation is introduced, so behavior is
  unchanged.

## 10. Documentation

- Add `docs/adr/0009-photo-index-virtual-window.md` describing the chosen
  index-snapshot + page-cache approach and why keyset append was rejected for
  the rail UX. (ADR-0008 is already the privacy ADR, so this decision takes the
  next free number.)
- Mark ADR-0007 as `Superseded by ADR-0009`.
- Update `docs/roadmap.md`: remove "一覧・検索は最大300件" from the current
  state and remove the 300-limit open question; keep the performance item until
  the acceptance measurement is recorded.
- Update `docs/features.md`, `docs/ui.md`, and `docs/HANDOFF.md` with the rail
  and the removal of the cap.
- Remove UI copy that says results are limited to 300.

## 11. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Custom rail paint is hard to get right in GPUI | Follow the `gpui_base::Scrollbar` element pattern; keep the painter read-only and put pointer handling in a simple overlay div. If text shaping in paint becomes a problem, fall back to pre-shaped labels drawn with `ShapedLine::paint` (already planned) rather than child divs per label. |
| Index rebuild thrashes during a large import | Debounce 2 s, keep the old index until the new one is ready, preserve the top-photo anchor. |
| Search `EXISTS` filters are slow at 100k | Existing player/tag indexes cover the subqueries; measure with the ignored test; if a filter exceeds 300 ms, keep the background search path and show progress. |
| Page fetch blocks the UI | Page size 256 by primary key is a few milliseconds; at most two pages per deferred callback. Log a warning above 16 ms and move the loader to a read-only background connection if the measurement demands it. |
| Memory growth from 100k `Photo` values | Only 24 pages are cached; index entries are 24 bytes each. `Arc<Photo>` avoids row clones. |
| GPUI internal scroll-handle fields change on upgrade | `gpui-kit` is pinned; isolate rail offset math in one helper so an upgrade touches one place. |
| Thumbnail cache churn while scrubbing | 192 entries, 8 requests per frame, 4 in flight, failed-ID suppression. |

## 12. Decisions left to the builder

- Exact rail typography, colors, and hover brightness using the existing design
  tokens.
- Whether to expose the rail's year labels in a tooltip; not required.
- Page size (256) and cache size (24) are initial values; tune only with
  measurements.
- Whether to keep or remove the old `list_photos` / `search_photos_text` once
  the daemon and tests are checked; removing them is not required for this
  feature.
