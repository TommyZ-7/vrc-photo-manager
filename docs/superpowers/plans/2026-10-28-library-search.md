# Library / Search Refit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the library/search grid sizing, card clipping, inspector alignment, month header, morph origin and smoothness, filter/sort removal, and click handling.

**Architecture:** Keep the existing virtualized `uniform_list` and shared `photo_card`. Change the pure layout helpers in `ui::theme`, then the card/header/inspector rendering in `ui::library` and `ui::search`, then the viewer morph source in `app::viewer`. Visual behavior is verified with the existing `hyprland-visual-qa` harness.

**Tech Stack:** Rust stable (edition 2024), gpui-kit 0.8, GPUI, SQLite.

**Spec:** `docs/superpowers/specs/2026-10-28-library-ui-refit-design.md`

## Global Constraints

- Preserve existing features unless the spec explicitly removes them.
- Library and Search share the same grid/card behavior.
- Keep the Search tab's left filter panel; remove only its sort label.
- No SQLite or image decoding in render/prepaint paths.
- Use `cargo fmt --all -- --check`, `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`, and `cargo test -p vrc-photo-ui` before each commit.

## Review Focus

- A photo whose aspect ratio is not 3:2, with world/date/size all hidden: the image must not bleed through the card's bottom corners.
- A window wide enough for 6+ Small cards and narrow enough for 1 ExtraLarge card: the column count must follow width × preset.
- Scrolling the grid: the month header must follow the first fully visible row, not the first index entry.
- Opening the viewer from the inspector while the selected card is scrolled out of view: fall back to the inspector bounds, not a stale card rect.
- A click that moves a few pixels during the press, including while the card's hover lift is animating: it must select the photo.

---

### Task 1: Grid metrics

**Files:**
- Modify: `crates/ui/src/ui/theme.rs`
- Test: `crates/ui/src/ui/theme.rs` (unit tests)

**Interfaces:**
- Produces: `GRID_GAP: f32 = 14.0`; `library_grid_metrics` and `search_grid_metrics` return `GridMetrics { columns, card_width, image_height, row_height }` without a column cap.

- [ ] **Step 1: Write the failing tests**

Update the existing column tests to the uncapped expectations. At 1440 px with the sidebar and inspector open, Medium must produce 4 columns (available width 806, min 190, gap 14). At 1080 px it must produce 3. Add a test that Small at 1920 px produces at least 6 columns and ExtraLarge produces at least 3. Add a test that `photo_card_width(available, columns)` uses the new gap.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui ui::theme::tests -v`
Expected: the new/updated column assertions fail.

- [ ] **Step 3: Implement the uncapped metrics**

In `theme.rs`, set `GRID_GAP = 14.0`; delete `library_column_cap`; in `grid_metrics_for`, use `let columns = raw_columns.max(1);`; update the tests that referenced the cap. Keep `photo_card_width` as the equal-share calculation.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ui ui::theme::tests -v`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/theme.rs
git commit -m "fix(ui): size library columns from width and card preset"
```

---

### Task 2: Card image fills and rounds the card

**Files:**
- Modify: `crates/ui/src/ui/library.rs` (`photo_card`)
- Test: visual harness

**Interfaces:**
- Produces: when `show_world == false` and `card_meta_line(...) == None`, the image uses the full card height and all four corners use `inner_corner_radius(card_radius, 1.0)`.

- [ ] **Step 1: Reproduce the bleed**

Run the app with `--demo`, hide world/date/size, and capture a card whose source aspect ratio is not 3:2. Confirm the image's bottom corners cross the card radius.

- [ ] **Step 2: Move the `has_info` calculation before the thumb**

Compute `meta_line` and `has_info` before building `thumb`. When `!has_info`, set the thumb height to the full card height and add `rounded_bl` / `rounded_br` to both the thumb and the image. Keep the top radii unchanged.

- [ ] **Step 3: Verify**

Run `cargo build -p vrc-photo-ui`, launch with `--demo`, hide all card text, and capture. Expected: no image pixels outside the card's rounded silhouette at any aspect ratio.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/library.rs
git commit -m "fix(ui): clip image-only cards to the card radius"
```

---

### Task 3: Align the inspector with the header

**Files:**
- Modify: `crates/ui/src/ui/theme.rs` (`INSPECTOR_MARGIN`)
- Modify: `crates/ui/src/ui/library.rs` (`inspector`, `inspector_column`)
- Test: `crates/ui/src/ui/theme.rs`

**Interfaces:**
- Produces: `inspector_card_width(w) == inspector_column_width(w)`; the inspector card's right edge is the content right edge.

- [ ] **Step 1: Write the failing test**

In `theme.rs`, assert that `inspector_card_width(1280.0) == inspector_column_width(1280.0)` and that the card width at 1440 is 298.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-ui ui::theme::tests -v`
Expected: FAIL because the card width is 16 px smaller.

- [ ] **Step 3: Implement**

Set `INSPECTOR_MARGIN = 0.0` and remove the `pr(px(INSPECTOR_MARGIN))` from the inspector column/card. Keep the grid's available width based on `inspector_column_width`; it already does not subtract the margin.

- [ ] **Step 4: Verify**

Run the tests, then capture the library screen. Expected: the inspector card's right edge aligns with the chrome header card's right edge and the extra translucent strip is gone.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/theme.rs crates/ui/src/ui/library.rs
git commit -m "fix(ui): align the inspector card with the header"
```

---

### Task 4: Month header follows the first visible row

**Files:**
- Modify: `crates/ui/src/ui/library.rs` (`gallery_card`, `gallery_header`)
- Modify: `crates/ui/src/state/photo_browser.rs` (add a pure helper if needed)
- Test: `crates/ui/src/state/photo_browser.rs`

**Interfaces:**
- Consumes: `first_visible_photo(scroll_top, columns, row_height, total)` and `list_scroll_top(&self.library.scroll)`.
- Produces: the gallery header month and count come from the first visible entry, not `entries.first()`.

- [ ] **Step 1: Write the failing test**

Add a test in `photo_browser.rs` for a helper that returns the first visible entry's month and the number of entries with that month, given a scroll offset. Use a small index with two months and a row height.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-ui state::photo_browser::tests -v`
Expected: FAIL because the helper does not exist.

- [ ] **Step 3: Implement**

Add the helper and use it in `gallery_header`. Pass `window` to `gallery_header` so it can compute the same `library_grid_metrics` as the grid. Keep the total count.

- [ ] **Step 4: Verify**

Run the tests and scroll the library with `--demo`. Expected: the header month changes when the first visible row enters a new month.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/library.rs crates/ui/src/state/photo_browser.rs
git commit -m "fix(ui): follow the visible month in the gallery header"
```

---

### Task 5: Remove library filter/sort and the search sort label

**Files:**
- Modify: `crates/ui/src/ui/library.rs` (`gallery_header`, `gallery_filter_row`)
- Modify: `crates/ui/src/ui/search.rs` (results header)
- Modify: `crates/ui/src/app.rs` (remove `gallery_filters_open` if unused)
- Test: `cargo test -p vrc-photo-ui`

**Interfaces:**
- Produces: no filter button, filter row, or "並び: 新しい順" label in the library; the Search tab keeps `search_filter_panel` and loses only its sort label.

- [ ] **Step 1: Remove the library filter UI**

Delete `gallery_filters_open`, `gallery_filter_row`, the filter button, and the sort label from `library.rs`. Remove the field from `App` if no other screen uses it.

- [ ] **Step 2: Remove the search sort label**

Delete the "並び: 新しい順" child from the search results header. Keep the filter card and its inputs.

- [ ] **Step 3: Verify**

Run `cargo test -p vrc-photo-ui`. Launch `--demo` and confirm the library header is only month + count, the search left filter is intact, and the search header has no sort label.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/library.rs crates/ui/src/ui/search.rs crates/ui/src/app.rs
git commit -m "fix(ui): drop library filter and sort labels"
```

---

### Task 6: Inspector fullscreen morphs from the card

**Files:**
- Modify: `crates/ui/src/app/viewer.rs` (`actual_source`)
- Test: `crates/ui/src/state/viewer.rs`

**Interfaces:**
- Produces: `actual_source` prefers `card_image_bounds` for every `ViewerOpenSource`, falling back to `inspector_preview_bounds` only when the card is unavailable.

- [ ] **Step 1: Write the failing test**

Add a test around `preferred_bounds` that passes a card and inspector bound with `prefer_card = true` and asserts the card is chosen; add the reverse case for the fallback.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-ui state::viewer::tests -v`
Expected: the new expectation fails if `actual_source` still prefers the inspector.

- [ ] **Step 3: Implement**

In `actual_source`, set `prefer_card = true` regardless of `source`. Return `ViewerOpenSource::Card` when the chosen bounds are the card's.

- [ ] **Step 4: Verify**

Run the tests. Open a photo from the inspector and capture the first morph frame. Expected: the animation starts at the grid card's image rect, not the inspector preview.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/app/viewer.rs crates/ui/src/state/viewer.rs
git commit -m "fix(ui): morph the inspector viewer from the grid card"
```

---

### Task 7: Stabilize the morph bounds and smoothness

**Files:**
- Modify: `crates/ui/src/ui/viewer.rs`
- Modify: `crates/ui/src/app/viewer.rs`
- Test: visual harness + `state::viewer` tests

**Interfaces:**
- Consumes: the `card_image_bounds` rect and `content_bounds`.
- Produces: the morph `from` rect matches the card's painted image rect on the frame the viewer opens.

- [ ] **Step 1: Capture the mismatch**

Record the card image bounds and the morph `from` rect on the opening frame. Confirm whether the offset is the hover lift, the client inset, or a stale render epoch.

- [ ] **Step 2: Fix the source rect**

Use the current card image bounds at click time; if the hover lift moves the card, subtract the same offset or capture the post-layout bounds. Keep the inspector fallback.

- [ ] **Step 3: Measure the frame rate**

Use the existing timing/log hooks to measure the morph. If the whole app re-renders per frame, isolate the morph overlay from the grid or reduce the work done in the animation closure. Keep the existing visual shape unless it still stutters.

- [ ] **Step 4: Verify**

Capture the opening and closing morphs. Expected: no visible jump at the start, and no repeated long frames during the animation.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/viewer.rs crates/ui/src/app/viewer.rs
git commit -m "fix(ui): stabilize and smooth the viewer morph"
```

---

### Task 8: Register a click with small pointer movement

**Files:**
- Modify: `crates/ui/src/ui/library.rs` (`photo_card`)
- Test: visual harness

**Interfaces:**
- Produces: a card press selects the photo on mouse-down; the existing double-click still opens the viewer.

- [ ] **Step 1: Reproduce**

Press a card and move 2–4 px before releasing. Confirm the photo is not selected.

- [ ] **Step 2: Implement**

Handle `MouseDownEvent` on the card to select (respecting Shift/Ctrl modifiers), and keep `on_click` only for double-click. Ensure the hover lift does not move the hit area away from the pointer.

- [ ] **Step 3: Verify**

Repeat the press with 2 px and 4 px of movement. Expected: selection occurs both times; a double-click still opens the viewer.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/library.rs
git commit -m "fix(ui): select cards on press with small pointer movement"
```

---

### Task 9: Search parity and final verification

**Files:**
- Modify: `crates/ui/src/ui/search.rs` (only if the shared card/grid needs a search-specific flag)
- Test: `cargo fmt --all -- --check`, `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`, `cargo test -p vrc-photo-ui`

- [ ] **Step 1: Verify Search uses the shared fixes**

Open Search with a populated library. Confirm the uncapped columns, 14 px gap, image-only card clipping, first-visible header (if the search header shows a month), and click behavior.

- [ ] **Step 2: Run the full local verification**

```bash
cargo fmt --all -- --check
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
cargo test -p vrc-photo-ui
```

Expected: all pass.

- [ ] **Step 3: Capture the final screenshots**

Capture the library with text on/off, the search tab, the inspector, and the morph start. Keep them in `/tmp/opencode`.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/search.rs
git commit -m "test(ui): verify library fixes on the search tab"
```
