# Card Display Settings for the Library and Search Tabs: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a display panel to the settings tab that shows or hides the compression state, world name, date, and size on photo cards, and adds four card-size presets shared by the library and search grids.

**Architecture:** Extend `AppSettings` with five persisted display fields and a `CardSize` enum. The UI derives a `CardMetrics` value (image height, row height, minimum card width) from those fields, then feeds the same metrics to the uniform list, the year rail, scroll-anchor helpers, and the placeholder. Card rendering conditionally composes the existing status overlay and text lines from the same flags.

**Tech Stack:** Rust 2024, gpui-kit 0.8 / gpui-component, SQLite-backed `AppSettings` JSON.

**Spec:** `docs/superpowers/specs/2026-10-25-card-display-settings-design.md`

## Global Constraints

- Both grids share one display setting; there is no per-tab variant.
- Defaults are: all four text groups visible, `CardSize::Medium`.
- Exact presets: Small = 96 px image / 140 px minimum card width; Medium = 128 / 170; Large = 168 / 220; ExtraLarge = 208 / 280.
- Exact row formula for the default (all text visible): `128 + 8 + 18 + 3 + 15 + 24 = 196`.
- Hidden text must reduce the row height; no blank metadata strip is left behind.
- The favorite marker is independent of the compression-state toggle.
- Settings save immediately; no new save button.
- Japanese labels are `表示（ライブラリ・検索）`, `圧縮状態`, `ワールド名`, `日付`, `サイズ`, `カードの大きさ`, `小`, `中`, `大`, `特大`.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` must pass before completion.
- Do not include `docs/superpowers/specs/2026-10-25-card-display-settings-design.md` or this plan in the code PR; they belong in the separate docs PR.

## Review Focus

- **Older or hand-edited settings JSON:** an older file has none of the new fields, and an unknown `ui_card_size` value must not reset every setting. Task 1 tests both cases through the real serde path.
- **All four text groups hidden:** the card must still render the image with no negative space, and the uniform row must be shorter. Task 2 tests the 152 px row and Task 5 checks it visually.
- **Narrow window with the ExtraLarge preset:** column calculation must clamp to one column instead of producing a zero-column or overflowing grid. Task 2 tests the minimum width, and Task 5 checks the live layout.
- **Only date or only size visible:** the shared metadata line must not leave a stray separator or an empty line. Task 3 tests all four combinations.
- **Changing a setting while scrolled deep, or while a search is pending:** the top-left photo should stay put for a layout change, but a new search result must still win over a pending layout anchor. Task 4 wires the precedence and Task 5 checks it manually.

---

### Task 1: Persist the display settings

**Files:**
- Modify: `crates/core/src/settings.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub enum CardSize { Small, Medium, Large, ExtraLarge }` with `Copy`, `Clone`, `Debug`, `PartialEq`, `Eq`, `Serialize`, `Deserialize`, `Default = Medium`.
  - `AppSettings.ui_card_show_status: bool`, `ui_card_show_world: bool`, `ui_card_show_date: bool`, `ui_card_show_size: bool`, `ui_card_size: CardSize`.

- [ ] **Step 1: Write the failing settings tests**

Add these tests to `crates/core/src/settings.rs` and extend the existing missing-UI-fields test:

```rust
#[test]
fn card_display_settings_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    let settings = AppSettings {
        ui_card_show_status: false,
        ui_card_show_world: false,
        ui_card_show_date: false,
        ui_card_show_size: false,
        ui_card_size: CardSize::ExtraLarge,
        ..Default::default()
    };
    settings.save(&db).unwrap();
    let loaded = AppSettings::load(&db).unwrap();
    assert!(!loaded.ui_card_show_status);
    assert!(!loaded.ui_card_show_world);
    assert!(!loaded.ui_card_show_date);
    assert!(!loaded.ui_card_show_size);
    assert_eq!(loaded.ui_card_size, CardSize::ExtraLarge);
}

#[test]
fn missing_card_display_fields_default_to_visible_medium() {
    let json = r#"{
        "photo_roots": [],
        "log_dir": null,
        "managed_dir": null,
        "language": "ja",
        "autostart": true,
        "tray": true,
        "auto_compress": true,
        "auto_compress_backlog": false,
        "estimated_link": true,
        "thumbnail_cache_mb": 2048,
        "compression_workers": 1
    }"#;
    let settings: AppSettings = serde_json::from_str(json).unwrap();
    assert!(settings.ui_card_show_status);
    assert!(settings.ui_card_show_world);
    assert!(settings.ui_card_show_date);
    assert!(settings.ui_card_show_size);
    assert_eq!(settings.ui_card_size, CardSize::Medium);
}

#[test]
fn unknown_card_size_falls_back_to_medium() {
    let mut value = serde_json::to_value(AppSettings::default()).unwrap();
    value["ui_card_size"] = serde_json::json!("gigantic");
    let settings: AppSettings = serde_json::from_value(value).unwrap();
    assert_eq!(settings.ui_card_size, CardSize::Medium);
}
```

In the existing `settings_deserialize_without_ui_fields` test, also assert the five new defaults so the older-JSON path stays covered.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core settings::`

Expected: compile errors for the missing fields/variants, or assertion failures for the default values.

- [ ] **Step 3: Implement the fields and enum**

In `crates/core/src/settings.rs`:

```rust
/// Presentation size for photo cards. Pixel metrics live in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CardSize {
    Small,
    #[default]
    Medium,
    Large,
    ExtraLarge,
}
```

Add the five fields to `AppSettings` with these serde defaults:

```rust
#[serde(default = "default_true")]
pub ui_card_show_status: bool,
#[serde(default = "default_true")]
pub ui_card_show_world: bool,
#[serde(default = "default_true")]
pub ui_card_show_date: bool,
#[serde(default = "default_true")]
pub ui_card_show_size: bool,
#[serde(default, deserialize_with = "deserialize_card_size")]
pub ui_card_size: CardSize,
```

Set all four booleans to `true` and `ui_card_size` to `CardSize::Medium` in `Default for AppSettings`. Add the custom deserializer so one bad value does not make `AppSettings::load` discard every setting:

```rust
fn deserialize_card_size<'de, D>(deserializer: D) -> std::result::Result<CardSize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(CardSize::deserialize(deserializer).unwrap_or_default())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core settings::`

Expected: all settings tests pass, including the existing round-trip and old-JSON tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings.rs
git commit -m "feat: add card display settings model"
```

---

### Task 2: Derive card metrics and thread them through the grid

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `CardSize` and the five `AppSettings` display fields from Task 1.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq)] struct CardMetrics { image_height: f32, row_height: f32, min_card_width: f32 }`
  - `fn card_metrics(settings: &AppSettings) -> CardMetrics`
  - `PhotoBrowser.row_height: f32`
  - `grid_columns(available_width: f32, min_card_width: f32) -> usize`
  - `grid_columns_for(search, window_width, sidebar_collapsed, inspector_collapsed, min_card_width) -> usize`
  - `grid_placeholder(image_height: f32) -> AnyElement`
  - `first_visible_photo(scroll_top, columns, row_height, total)`
  - `anchor_photo_id(index, offset_y, columns, row_height)`
  - `rail_scroll_offset(photo_row, columns, row_height, max_offset)`
  - `YearRail::new(..., columns: usize, row_height: f32, ...)`

- [ ] **Step 1: Write the failing metric and grid tests**

Add these tests to the UI test module and update the existing helper calls to the new signatures:

```rust
#[test]
fn default_card_metrics_match_the_current_grid() {
    let metrics = card_metrics(&AppSettings::default());
    assert_eq!(metrics.image_height, 128.0);
    assert_eq!(metrics.row_height, PHOTO_ROW_HEIGHT);
    assert_eq!(metrics.min_card_width, MIN_CARD_WIDTH);
}

#[test]
fn hidden_card_text_shrinks_the_row_but_keeps_the_image() {
    let settings = AppSettings {
        ui_card_show_world: false,
        ui_card_show_date: false,
        ui_card_show_size: false,
        ..Default::default()
    };
    let metrics = card_metrics(&settings);
    assert_eq!(metrics.image_height, 128.0);
    assert_eq!(metrics.row_height, 152.0);
}

#[test]
fn card_metrics_count_only_visible_text_lines() {
    let world_only = AppSettings {
        ui_card_show_date: false,
        ui_card_show_size: false,
        ..Default::default()
    };
    assert_eq!(card_metrics(&world_only).row_height, 178.0);

    let meta_only = AppSettings {
        ui_card_show_world: false,
        ..Default::default()
    };
    assert_eq!(card_metrics(&meta_only).row_height, 175.0);

    let status_hidden = AppSettings {
        ui_card_show_status: false,
        ..Default::default()
    };
    assert_eq!(card_metrics(&status_hidden).row_height, PHOTO_ROW_HEIGHT);
}

#[test]
fn card_size_presets_scale_the_grid() {
    let expected = [
        (CardSize::Small, 96.0, 140.0),
        (CardSize::Medium, 128.0, 170.0),
        (CardSize::Large, 168.0, 220.0),
        (CardSize::ExtraLarge, 208.0, 280.0),
    ];
    for (size, image, min_width) in expected {
        let metrics = card_metrics(&AppSettings {
            ui_card_size: size,
            ..Default::default()
        });
        assert_eq!(metrics.image_height, image);
        assert_eq!(metrics.min_card_width, min_width);
        assert_eq!(metrics.row_height, image + 68.0);
    }
}

#[test]
fn grid_columns_respect_the_card_size() {
    assert!(grid_columns(1280.0, 140.0) >= grid_columns(1280.0, 280.0));
    assert_eq!(grid_columns(300.0, 280.0), 1);
    assert_eq!(grid_columns_for(true, 1280.0, false, false, 170.0), 3);
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
```

Existing tests that call `first_visible_photo`, `anchor_photo_id`, `rail_scroll_offset`, `grid_columns`, or `grid_columns_for` must pass `PHOTO_ROW_HEIGHT` or `MIN_CARD_WIDTH` explicitly.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui tests::default_card_metrics_match_the_current_grid tests::card_size_presets_scale_the_grid`

Expected: compile errors for the missing `CardMetrics`, `card_metrics`, and the new parameters.

- [ ] **Step 3: Implement `CardMetrics` and the row formula**

Add near the grid constants in `crates/ui/src/main.rs`:

```rust
const CARD_BODY_GAP: f32 = 8.0;
const CARD_TEXT_GAP: f32 = 3.0;
const CARD_WORLD_LINE_HEIGHT: f32 = 18.0;
const CARD_META_LINE_HEIGHT: f32 = 15.0;
const CARD_ROW_SLACK: f32 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct CardMetrics {
    image_height: f32,
    row_height: f32,
    min_card_width: f32,
}

fn card_metrics(settings: &AppSettings) -> CardMetrics {
    let (image_height, min_card_width) = match settings.ui_card_size {
        CardSize::Small => (96.0, 140.0),
        CardSize::Medium => (128.0, 170.0),
        CardSize::Large => (168.0, 220.0),
        CardSize::ExtraLarge => (208.0, 280.0),
    };
    let meta_line = settings.ui_card_show_date || settings.ui_card_show_size;
    let mut row_height = image_height + CARD_ROW_SLACK;
    if settings.ui_card_show_world || meta_line {
        row_height += CARD_BODY_GAP;
    }
    if settings.ui_card_show_world {
        row_height += CARD_WORLD_LINE_HEIGHT;
    }
    if meta_line {
        if settings.ui_card_show_world {
            row_height += CARD_TEXT_GAP;
        }
        row_height += CARD_META_LINE_HEIGHT;
    }
    CardMetrics {
        image_height,
        row_height,
        min_card_width,
    }
}
```

Keep `PHOTO_ROW_HEIGHT` at `196.0` and `MIN_CARD_WIDTH` at `170.0` as the medium defaults. Add `fn card_metrics(&self) -> CardMetrics { card_metrics(&self.settings) }` to `impl App`.

- [ ] **Step 4: Thread the metrics through the grid**

Make these concrete changes:

- Add `row_height: f32` to `PhotoBrowser`, initialize it to `PHOTO_ROW_HEIGHT`, and use `self.row_height` in `remove_photo` instead of `PHOTO_ROW_HEIGHT`.
- Add `row_height: f32` to `YearRail`, pass it from `YearRail::new`, and use it in `jump_to` and in the `first_visible_photo` call in `paint`.
- Change `grid_columns` and `grid_columns_for` to accept `min_card_width`.
- Change `first_visible_photo`, `anchor_photo_id`, and `rail_scroll_offset` to accept `row_height` after `columns`.
- In `photo_grid`, compute `let metrics = self.card_metrics();`, pass `metrics.min_card_width` to `grid_columns_for`, set `browser.columns` and `browser.row_height`, use `metrics.row_height` for each row's `.h(px(...))`, and use `metrics.image_height` for `grid_placeholder`.
- Update `reload_library`, `apply_search_result`, and `refresh_search_after_tag_edit` to pass `self.card_metrics().row_height` to `anchor_photo_id`.
- Update every existing test call to the new helper signatures.

- [ ] **Step 5: Run the UI tests**

Run: `cargo test -p vrc-photo-ui tests::`

Expected: all UI unit tests pass, and the new metric tests report the default 196 px row.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: derive photo grid metrics from display settings"
```

---

### Task 3: Render the card from the display toggles

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `card_metrics` and `CardMetrics` from Task 2; `AppSettings` booleans from Task 1.
- Produces:
  - `fn card_meta_line(show_date: bool, show_size: bool, date: &str, size: String) -> Option<String>`
  - `App::photo_card(..., metrics: CardMetrics, ...)`.

- [ ] **Step 1: Write the failing metadata-line test**

Add to the UI test module:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-ui tests::card_meta_line_joins_only_visible_parts`

Expected: compile error for the missing `card_meta_line`.

- [ ] **Step 3: Implement the helper and conditional card rendering**

Add `card_meta_line` near `short_date`:

```rust
fn card_meta_line(show_date: bool, show_size: bool, date: &str, size: String) -> Option<String> {
    match (show_date, show_size) {
        (true, true) => Some(format!("{} · {}", short_date(date), size)),
        (true, false) => Some(short_date(date)),
        (false, true) => Some(size),
        (false, false) => None,
    }
}
```

In `photo_card`:

- Accept `metrics: CardMetrics` from `photo_grid` and use `px(metrics.image_height)` for the thumbnail instead of `128.0`.
- Read the four flags into locals before building the card.
- Wrap the bottom gradient and the status dot/label in `if show_status { ... }`; keep the favorite dot outside that block.
- Build the text block conditionally: add the world line only when `show_world`, and add the metadata line only when `card_meta_line(...)` returns `Some`.
- Leave the body gap at 8 px; when both text groups are hidden the body has only the thumbnail child.
- Pass `metrics` from `photo_grid` into the new `photo_card` parameter.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vrc-photo-ui tests::`

Expected: all UI tests pass, including the four metadata-line combinations.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: honor card display toggles when rendering photos"
```

---

### Task 4: Add the settings panel, size buttons, and scroll preservation

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `CardSize` from Task 1; `card_metrics` from Task 2; `card_meta_line` from Task 3.
- Produces:
  - `fn card_size_options() -> [(CardSize, &'static str); 4]`
  - `fn card_size_label(size: CardSize) -> &'static str`
  - `App::set_card_size(&mut self, size: CardSize, cx: &mut Context<Self>)`
  - `App::capture_display_layout_anchors(&mut self)`
  - `App.pending_layout_anchor_library: Option<i64>` and `App.pending_layout_anchor_search: Option<i64>`
  - Updated `setting_switch(..., capture_layout: bool, ...)`.

- [ ] **Step 1: Write the failing size-option test**

Add to the UI test module:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-ui tests::card_size_options_cover_every_preset_in_order`

Expected: compile error for the missing functions.

- [ ] **Step 3: Add the settings panel and size control**

Add the helpers near the settings screen:

```rust
fn card_size_options() -> [(CardSize, &'static str); 4] {
    [
        (CardSize::Small, "小"),
        (CardSize::Medium, "中"),
        (CardSize::Large, "大"),
        (CardSize::ExtraLarge, "特大"),
    ]
}

fn card_size_label(size: CardSize) -> &'static str {
    card_size_options()
        .into_iter()
        .find_map(|(option, label)| (option == size).then_some(label))
        .unwrap_or("中")
}
```

In `settings_screen`, insert a new glass panel after the library path panel and before the startup/compression panel:

- Title: `表示（ライブラリ・検索）`
- Four rows using the existing switch layout:
  - `圧縮状態` / `写真カード左下の圧縮状態ラベルを表示します`
  - `ワールド名` / `カードのワールド名を表示します`
  - `日付` / `撮影日を表示します`
  - `サイズ` / `ファイルサイズを表示します`
- A card-size row with the label `カードの大きさ`, the hint `ライブラリと検索の両方に適用されます`, and four small buttons from `card_size_options()`.

Implement the buttons with `Button::new(...)`, `.label(label)`, `.small()`, `.selected(size == self.settings.ui_card_size)`, `.toggled(...)`, and a click handler that calls `set_card_size(size, cx)`. Import `Selectable` if the `.selected` method is not already in scope.

Extend `setting_switch` with a `capture_layout: bool` parameter. Before `apply(&mut this.settings, *value)`, call `this.capture_display_layout_anchors()` when `capture_layout` is true. Pass `false` at the existing autostart/tray/compression/reduce-motion call sites and `true` for the four new display switches.

Implement these methods on `App`:

```rust
fn set_card_size(&mut self, size: CardSize, cx: &mut Context<Self>) {
    if self.settings.ui_card_size == size {
        return;
    }
    self.capture_display_layout_anchors();
    self.settings.ui_card_size = size;
    if let Err(error) = self.settings.save(&self.db) {
        tracing::warn!(%error, "cannot save card size");
    }
    cx.notify();
}

fn capture_display_layout_anchors(&mut self) {
    let metrics = self.card_metrics();
    if self.pending_layout_anchor_library.is_none() {
        self.pending_layout_anchor_library = anchor_photo_id(
            &self.library.index,
            list_scroll_top(&self.library.scroll),
            self.library.columns,
            metrics.row_height,
        );
    }
    if self.pending_layout_anchor_search.is_none() {
        self.pending_layout_anchor_search = anchor_photo_id(
            &self.search_view.index,
            list_scroll_top(&self.search_view.scroll),
            self.search_view.columns,
            metrics.row_height,
        );
    }
}
```

- [ ] **Step 4: Apply the pending anchor when a grid renders**

Add the two `Option<i64>` fields to `App` and initialize both to `None`.

At the start of `photo_grid`, after columns and metrics are known and `browser.columns`/`browser.row_height` are set, take the matching pending anchor:

```rust
let pending = if search {
    self.pending_layout_anchor_search.take()
} else {
    self.pending_layout_anchor_library.take()
};
if let Some(id) = pending {
    let browser = self.browser_mut(search);
    let row = browser
        .index
        .row_of_id(id)
        .map(|position| position / browser.columns.max(1))
        .unwrap_or(0);
    browser
        .scroll
        .scroll_to_item_strict(row, ScrollStrategy::Top);
}
```

Clear `pending_layout_anchor_search` when a new search begins or the view is cleared, and also inside `apply_search_result` before applying that result's own scroll, so a fresh search result always wins over a layout anchor. Specifically update `run_search`, `start_search`, `clear_search`, and `apply_search_result`.

- [ ] **Step 5: Run the UI tests and a workspace check**

Run: `cargo test -p vrc-photo-ui tests::`

Expected: all UI tests pass.

Run: `cargo check --workspace`

Expected: no warnings or errors from the changed signatures.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add card display settings panel"
```

---

### Task 5: Verify the whole branch

**Files:**
- No planned changes; fix anything the checks expose.

- [ ] **Step 1: Run formatting, linting, and all tests**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: no formatting differences, no clippy warnings, and every test passes.

- [ ] **Step 2: Verify the settings screen and both grids in demo mode**

Run: `cargo run -p vrc-photo-ui --bin vrc-photo-manager -- --demo`

Check all of the following:

- The settings screen shows the `表示（ライブラリ・検索）` panel with four switches and four size buttons.
- Turning each text switch off immediately changes the library card, with no leftover blank strip.
- Turning all four off leaves an image-only card, and the grid remains scrolled to the same top-left photo.
- `小` / `中` / `大` / `特大` visibly change the card size, and the same choice appears on the search tab.
- A narrow window with `特大` still shows a usable grid (one column, no horizontal overflow).
- With only `日付` on, the line shows just the date; with only `サイズ` on, it shows just the size.
- The favorite marker still appears when `圧縮状態` is off.
- Restarting the app keeps the chosen switches and size.

- [ ] **Step 3: Commit any fixes**

If the checks or the manual pass require changes, commit them with a `fix:` prefix and re-run Step 1.
