# Card Display Settings for the Library and Search Tabs: Design

- Date: 2026-10-25
- Status: Draft for review
- Target: `crates/core/src/settings.rs`, `crates/ui/src/main.rs`, docs

## 1. Intent

The photo cards in the library and search grids always show the compression
state, world name, date, and file size at the same size. Users who only care
about the image cannot remove the text, and users with large monitors cannot
make the cards denser or larger. This change adds a display section to the
settings tab so both grids can be tuned without changing any photo data.

## 2. Goals

- Add a "表示" panel to the settings tab with four independent switches:
  - 圧縮状態: the existing status overlay (圧縮済 / 未圧縮 / 重複 / 圧縮版のみ)
  - ワールド名
  - 日付
  - サイズ
- Add a four-step card size control: 小 / 中 / 大 / 特大.
- Apply every setting to both the library and the search grid immediately.
- Shrink the card when text is hidden, so the grid has no empty text area.
- Persist the settings in `AppSettings`; older settings JSON loads with all
  text visible and the current card size (中).
- Preserve the photo at the top-left of each grid when a setting changes the
  row layout.

## 3. Non-goals

- Per-tab display settings; both grids share one setting.
- Per-card individual sizes, masonry layout, or variable row heights.
- Hiding the favorite marker, the selection border, or the thumbnail itself.
- Changing the inspector card, full-screen viewer, or the filter panels.
- Filtering the library by compression state.
- Scaling the text along with the card size, or changing font sizes.

## 4. Approaches considered

### A. Preset sizes (chosen)

Four size presets change the thumbnail height and the minimum card width. The
grid remains a uniform list, so the year rail, scroll anchors, and page cache
keep their existing structure. The settings tab shows the four choices as
segmented buttons.

- Small: 96 px image, 140 px minimum card width
- Medium: 128 px image, 170 px minimum card width (current look)
- Large: 168 px image, 220 px minimum card width
- Extra large: 208 px image, 280 px minimum card width

This gives a useful range without introducing a second layout system, and
matches the request for "ある程度" adjustable card sizes.

### B. Continuous slider

A 140–280 px slider would offer finer control, but every drag step changes the
row height and column count. The scroll position and year rail would need
debouncing or anchor restoration on each frame, and a failed restore would be
more visible than with four discrete changes. The added state and interaction
complexity is not justified for this request.

### C. Header zoom controls

`+` / `-` buttons or Ctrl+wheel in the grid header would be faster to use, but
the request explicitly places the settings in the settings tab. The controls
would also be less discoverable and harder to reach from the search screen.

## 5. Decisions

### Settings model

`AppSettings` gains:

```rust
#[serde(default = "default_true")]
pub ui_card_show_status: bool,
#[serde(default = "default_true")]
pub ui_card_show_world: bool,
#[serde(default = "default_true")]
pub ui_card_show_date: bool,
#[serde(default = "default_true")]
pub ui_card_show_size: bool,
#[serde(default)]
pub ui_card_size: CardSize,
```

`CardSize` is a small serde enum in `crates/core/src/settings.rs` with variants
`Small`, `Medium`, `Large`, and `ExtraLarge`; `Default` is `Medium`. The core
crate doesn't need to know the pixel metrics; the UI maps the enum to a
`CardMetrics` value.

### Card metrics

A single UI helper computes the layout from the settings:

```rust
struct CardMetrics {
    image_height: f32,
    row_height: f32,
    min_card_width: f32,
}
```

The row height is derived from the visible text lines rather than fixed:

```
image_height
+ (world or meta line ? 8 px body gap : 0)
+ (world ? 18 px line : 0)
+ (world and meta line ? 3 px gap : 0)
+ (meta line ? 15 px line : 0)
+ 24 px bottom slack
```

For the defaults this is `128 + 8 + 18 + 3 + 15 + 24 = 196`, which is the
current row height. If all four text toggles are off, the row becomes
`128 + 24 = 152`.

### Grid integration

- `grid_columns` and `grid_columns_for` take the preset's minimum card width.
- `photo_grid` reads the metrics once, stores the row height on the browser for
  later scroll clamping, and uses the metrics for the row height and the
  placeholder height.
- `YearRail` carries the row height, and `first_visible_photo`,
  `anchor_photo_id`, and `rail_scroll_offset` take it as an argument.
- `PhotoBrowser::remove_photo` uses the stored row height instead of the old
  `PHOTO_ROW_HEIGHT` constant.
- `PHOTO_ROW_HEIGHT` stays as the default medium row height for tests and for
  callers that have no settings, but no production path assumes it when the
  user has changed the preset.

### Card rendering

- The status overlay (colored dot plus label) is rendered only when
  `ui_card_show_status` is on. This also covers 重複 and 圧縮版のみ; the
  favorite dot is independent and stays visible.
- The world line is rendered only when `ui_card_show_world` is on.
- Date and size share one line. Visible parts are joined with ` · `:
  - both on: `10-25 12:34:56 · 12.3 MB`
  - date only: `10-25 12:34:56`
  - size only: `12.3 MB`
  - both off: no line
  The date part keeps the existing `short_date` formatting (month, day, and
  time); this change only makes it independently hideable.
- The existing 12 px world text and 10 px metadata text are unchanged.

### Applying settings

- Each switch and each size button saves `AppSettings` immediately, as the
  existing autostart/compression switches do. There is no separate save button
  for the display panel.
- When a layout-affecting value changes, `App` captures the current top-left
  photo id for the library and the search grid before applying the new layout.
  The next `photo_grid` render for each grid computes the new column count and
  row height, scrolls to the photo's new row, and clears the pending anchor.
  If the photo disappeared in the meantime, the grid falls back to the first
  row.
- The anchor is captured only once while a grid is not mounted, so changing
  two settings in a row does not reinterpret the old scroll offset with the
  new metrics.

### Settings UI

- A new glass panel titled `表示（ライブラリ・検索）` is added to the settings
  screen, after the library path panel and before the startup/compression
  panel.
- The four switches reuse the existing `setting_switch` row style.
- The card size row uses four small buttons (小 / 中 / 大 / 特大) with the
  selected preset highlighted. A short hint explains that the choice applies
  to both grids.
- Defaults are all text visible and 中, so existing users see no change after
  upgrading.

## 6. Failure handling

- A failed `AppSettings::save` is logged and leaves the in-memory value
  applied for the current session, matching the existing switch behavior.
- A missing or malformed `ui_card_size` value falls back to `Medium` through
  serde's default, so a hand-edited settings row cannot break the grid.
- Changing the preset while a search is active does not re-run the query; it
  only changes the mounted result's layout.

## 7. Left to the builder

- Exact button styling and whether the size control uses `selected` plus
  `ghost` or a custom segmented look.
- The precise panel spacing and hint wording within the existing design tokens.
- Test names and whether the metrics helper lives as a free function or an
  `impl CardSize`.
- Updating `docs/ui.md` / `docs/features.md` in a separate docs PR.
