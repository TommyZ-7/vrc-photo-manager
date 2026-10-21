# UI Redesign Design

- Date: 2026-10-22
- Issue: [#27 feat: UI刷新（ガラス調・背景ぼかし・サイドバー最小化）](https://github.com/TommyZ-7/vrc-photo-manager/issues/27)
- Status: Draft for review
- Target: GPUI UI (`crates/ui/src/main.rs`). The Tauri migration (#29) is withdrawn.
- Mock: [docs/ui-redesign-mockup.html on `docs/ui-redesign-mockup`](https://github.com/TommyZ-7/vrc-photo-manager/blob/docs/ui-redesign-mockup/docs/ui-redesign-mockup.html)

## 1. Intent

The current UI is functional but dense. Photos do not feel like the main
subject, the fixed 210 px sidebar squeezes the grid on narrow windows, and
the visual language is a flat dark panel stack. This work moves the app to a
photo-first composition: a quiet ambient background, a glass-like inspector
that borrows the selected photo's color, meaningful motion, and a collapsible
navigation rail.

The reading order in the library becomes **photo → information → action**.
Navigation and status stay at the edges; the selected photo's context sits
next to the grid; every control is text-first.

## 2. Goals

- Apply one design-token set to every screen: Photos, Search, Compression,
  Monitor, Settings, and the full-screen viewer.
- Make the photo grid comfortable at 1280×820 and 1440×900 by deriving the
  column count from the available width.
- Add a collapsible sidebar (238 px ↔ 78 px) with a floating toggle,
  `Ctrl/Cmd+B`, tooltips in rail mode, and persisted state.
- Make the full-screen viewer the primary detail surface: preview, metadata,
  players, tags, compression history, memo, favorite, compression, and trash.
- Let the library's right inspector be shown/hidden without leaving the grid.
- Open a full-screen image on double-click, with a Google Photos-style
  toggleable right detail panel and previous/next navigation.
- Add restrained motion: 150–450 ms, `cubic-bezier(.22,.8,.24,1)`-equivalent,
  card stagger, hover lift, preview cross-fade, and modal entrance. Respect a
  user setting for reduced motion.
- Approximate the mock's background blur with existing images and scrims.
  Do not add a GPUI renderer feature.

## 3. Non-goals

- Light theme, new screens, a larger icon set, or 100k-photo paging.
- True per-element backdrop blur. GPUI 0.2.x / gpui-pre 0.3.9 still has no
  element-level `backdrop-filter`; only whole-window blur/Mica is available.
- New search/filter behavior. The library header is restyled, not rewired.
- Date-grouped sections in the photo grid. `uniform_list` needs uniform row
  heights; the grid stays a flat newest-first virtual list.
- A custom window titlebar or window controls. The native titlebar remains.
- Library-grid arrow-key navigation (the viewer's Left/Right is in scope),
  keyboard shortcut expansion beyond the ones listed here, or the other
  items in #12.
- Windows visual verification. It is deferred to #9 (real-device/package QA).

## 4. Anti-goals

- The redesign must not make photos smaller or push them below the fold.
- It must not introduce a new image decode path or re-decode thumbnails on
  animation frames, resize, or the 3-second refresh.
- It must not regress import, search, compression, monitor, settings, tag
  editing, favorite, or trash behavior.
- Motion must never block input or make the UI feel slower than the current
  one. Reduced motion must produce a usable static UI.

## 5. Constraints

- GPUI/gpui-kit 0.8.0 (`gpui-pre` 0.3.9) is fixed. Do not add a direct `gpui`
  dependency.
- `crates/ui/src/main.rs` remains a single-file UI. Follow the existing
  single-file pattern; do not split the crate in this work.
- The photo grid is `uniform_list`. Row height is fixed; only the column count
  changes with width.
- GPUI supports opacity, offsets, widths, and background gradients, but not
  CSS transforms (`scale`, `rotate`) on arbitrary elements. Motion uses
  opacity, width, and small positional offsets.
- GPUI animations are keyed by element ID. Re-triggering requires changing the
  ID (an animation epoch) so a new animation state is created. A one-shot
  `HashSet::insert` gate is not enough for the card intro: the animation
  element must stay mounted for its full duration, so the intro stores each
  photo's first-seen `Instant` and keeps the wrapper until its duration
  elapses.
- The component theme is forced light by `gpui_component::init`. Set
  `Theme::change(ThemeMode::Dark, None, cx)` once after `gpui_kit::init` so
  Button/Input/Switch/Tooltip match the dark UI; do not patch buttons
  individually.
- `App::reduce_motion` defaults to false and does not read OS preferences.
  The app must set it from its own setting.
- Existing `AppSettings` JSON must keep deserializing. New fields need
  `#[serde(default)]` because `AppSettings::load` falls back to defaults on a
  parse error.
- `uniform_list` currently loads up to 300 thumbnails eagerly. Do not add
  another decode or a per-frame reload.

## 6. Decisions

These were confirmed with the project owner during planning.

| Area | Decision |
|---|---|
| Delivery | 2 PRs. PR1: shell, sidebar, Photos grid/inspector toggle, full-screen viewer, motion, reduced-motion setting. PR2: Compression, Search, Monitor, Settings token pass. |
| Detail screen | Consolidate into the full-screen viewer. Remove the dedicated `Screen::Detail` route; extract its body into a shared `detail_body` used by the viewer's right panel. |
| Library browsing flow | Specified: the library keeps its current grid + right inspector structure. The inspector can be shown/hidden. Double-click (and Space/Enter/button) opens a full-screen image viewer; its right detail panel is independently toggleable. |
| Inspector visibility | Persisted as `ui_inspector_collapsed` (default false = visible). A text toggle in the library header and `Ctrl/Cmd+I` flip it. The column width animates over 300 ms; the grid recomputes columns from the target state. |
| Full-screen details | 380 px right panel, closed on the first open. Toggle with the top-bar text button; later opens remember the last toggle for the session. Image area stays centered and expands when the panel closes. |
| Full-screen navigation | "前へ" / "次へ" text buttons in the top bar, plus Left/Right keys while focus is in the top bar. Navigation uses the current Photos list or the current Search results, clamps at the ends, and updates the library selection behind the viewer. Keys do not navigate while a details-panel input has focus. |
| Grid | Adaptive columns. Minimum card width 170 px, gap 16 px. At 1280×820 with the sidebar and inspector visible: 3 columns (~212 px cards); with the inspector hidden: 5. At 1440×900: 4 with the inspector visible, 6 hidden. Collapsing the sidebar adds one column. |
| Storage card | Keep photo count + total library bytes. Do not add disk-capacity APIs or a usage meter. |
| Reduced motion | A persisted Settings switch, applied through `cx.set_reduce_motion`. No OS-specific detection. |
| Windows QA | Deferred to #9. Linux screenshots at 1280×820 and 1440×900 are required for #27. |
| Search/filter | Appearance only. The existing four library badges become chips; no new filtering. `Ctrl/Cmd+K` goes to the Search screen and focuses the world input. |
| Message feedback | Keep the existing `message` string, shown in the screen header. No new toast/timer component. |
| Full-screen actions | All existing detail operations move into the right panel: favorite, tag add, compress, trash (two-step), history, memo. No "open folder" action (it does not exist today). |
| Nav counts | Photos shows the library count. Compression shows the queued/running job count when non-zero. Search, Monitor, and Settings have no count; Monitor uses its status dot. |
| Nav icons | Photos `GalleryVerticalEnd`, Search `Search`, Compression `Minimize`, Monitor `Eye`, Settings `Settings`. |
| Icons elsewhere | Text buttons. No icon-only controls except the sidebar toggle (with a tooltip). |
| Inspector blur | Upscaled 256 px thumbnail + translucent scrim + gradient. No new blur path. |
| Thumbnail color | Average color of the loaded 256 px thumbnail, fallback to the existing `scene_color(world)`. |
| Preview fit | The full-screen viewer uses `ObjectFit::Contain` on a dark matte so photos are never cropped. |
| Demo/QA data | Extend `--demo` with generated sample images/thumbnails and a temp file-backed DB so previews, color extraction, trash, and restart persistence can be exercised. Add `--demo-reset` and `--window-size WxH` for repeatable screenshots. |
| Viewer focus scope | Tab/Shift+Tab cycle every interactive viewer control in visual order: close, previous, next, details toggle, and (when the panel is open) favorite, tag input, add-tag, compress, trash. The shell is inert while the viewer is open; global `Ctrl/Cmd+B`, `Ctrl/Cmd+K`, and `Ctrl/Cmd+I` are ignored. |
| Card stagger scope | The intro runs for the first 24 cards of each grid generation, with the stagger index capped at 8. A `seen` set prevents replay when `uniform_list` remounts a row after scrolling. |
| Last sync | Derived from `daemon_heartbeat` age. No new DB field. Show "最終同期 N秒前" while active, "デーモン停止" otherwise, and "デモデータ" in demo mode. |
| Small windows | Set a 1080×700 minimum window size and keep the inspector toggleable. At 1080 with the inspector visible the grid uses 2 columns; hiding the inspector or collapsing the sidebar adds columns. |
| Viewer feedback | The right panel shows the existing `message` string. Compression keeps the viewer open so the confirmation is visible. |
| Inspector tint | Lerp the solid tint from the previous average color to the new one over 260 ms; cross-fade the preview image separately. |

## 7. Visual system

### 7.1 Tokens

Keep the existing constant names so the token pass does not require renaming
hundreds of references; change their values and add a few new names.

| Token | Value | Use |
|---|---|---|
| `BG` | `#07090d` | Window / workspace base |
| `PANEL` | `#121722` | Panel surface (solid approximation of glass) |
| `PANEL_2` | `#1a2130` | Chips, secondary surface |
| `SIDEBAR` | `#0d1119` | Sidebar base |
| `BORDER` | `#232b38` | Hairlines |
| `BORDER_STRONG` | `#364256` | Hover/focus borders, tooltips |
| `TEXT` | `#eef3fb` | Primary text |
| `MUTED` | `#a3adbd` | Secondary text |
| `MUTED_2` | `#768196` | Tertiary text / labels |
| `BLUE` | `#66b3ff` | Accent, selection |
| `BLUE_DARK` | `#2f8fea` | Primary button |
| `ACCENT_SOFT` | `#1a2f47` | Active nav background (solid approximation) |
| `GREEN` | `#5eead4` | Monitoring, completed |
| `AMBER` | `#fbbf24` | Uncompressed, favorites |
| `RED` | `#fb7185` | Failure, danger |
| `PURPLE` | `#b79cff` | Secondary highlight |

Spacing follows an 8 px grid for layout (8/16/24/32). The workspace uses a
26 px outer horizontal padding. Chips and tight metadata may use 4/6 px.

Type scale: 10 / 12 / 17 / 24 px. Use `.text_size(px(...))` explicitly; do
not rely on `.text_xs()` / `.text_lg()` for the new layout. The 24 px size is
for screen titles and the full-screen viewer's detail title; 17 px for inspector titles and
section headings; 12 px body; 10 px metadata, labels, counts, and buttons.

Radii: 20 px outer floating surfaces, 18 px inspector, 14 px photo cards,
8 px buttons/chips, 999 px pills.

Glass approximation: translucent dark fills (`rgba(0x141922c4)`), a 1 px
light border, and an inset top highlight shadow. The ambient background uses
large, low-opacity colored orbs with soft box shadows behind the content.
No new dependencies.

### 7.2 Shell

- Root: `BG`, native titlebar, sidebar + workspace.
- Workspace: ambient layer (absolute, behind content), screen header, screen
  content.
- Sidebar: 238 px expanded / 78 px collapsed, 440 ms width animation.
  Brand mark, nav, spacer, storage card, daemon status, version.
- The floating rail toggle sits on the sidebar's right edge (half outside).
  Render it as the last root-level overlay so it paints above the workspace;
  its `left` position follows the same width animation. It uses a chevron and
  a tooltip with the current action and `Ctrl+B`.
- Collapsed mode hides labels, counts, the storage card, and the text status;
  icons stay centered. Hovering a nav item shows a floating text tooltip to
  the right. No other icons are added.

### 7.3 Photos screen

- Header: "ライブラリ" (24 px), a subtitle with count / total bytes / last
  sync, message text, and import buttons ("コピー取込", "移動取込").
- Filter row: the four existing badges become chips. They remain
  non-interactive (appearance only). A right-aligned "並び: 新しい順" label.
- Header: a text toggle "詳細" / "詳細を隠す" (plus `Ctrl/Cmd+I`) shows or
  hides the inspector.
- Content: grid (left) + inspector column (right, 330 px). The inspector card
  is 314 px wide with 16 px top/bottom/right margins inside that column.
  Hiding the inspector animates the column to 0 over 300 ms; the grid expands
  and recomputes its column count from the target state.
- Grid: adaptive columns, 16 px gaps, 128 px image area, fixed row height.
  Cards keep the existing selection, compressed, unlinked, and favorite
  signals. The selected card gets a 2 px accent ring and a soft glow.
- Inspector: glass card with an upscaled thumbnail background at low opacity,
  a dark gradient scrim, a 16:10 preview, title/date/badges, metadata,
  players, tags, and two full-width text actions: "全画面表示" and "圧縮".
- Empty state: a quiet card with the existing guidance text.

### 7.4 Full-screen viewer

- Full-window overlay above the shell. Dark background with a subtle tint
  from the selected photo's average color.
- Top bar (56 px): "閉じる Esc" on the left, "前へ" / "次へ" navigation
  buttons, the file name, and a "詳細" / "詳細を閉じる" toggle on the right.
  Text buttons only. The top bar carries the `viewer_nav` key context; Left/
  Right navigate while focus is in the top bar. The buttons clamp at the
  first/last photo (no-op at the boundary).
- Content row: image area (flex) + details panel (380 px when open, 0 when
  closed). The image uses the 1200 px preview, `Contain`, centered with 24 px
  padding. The panel is scrollable and uses the shared `detail_body`.
- Double-click a photo card to open. Space/Enter while the grid is focused
  and the inspector's "全画面表示" button are equivalent entry points.
- Navigation source: the current Photos list, or the current Search results
  when opened from Search. The viewer keeps its own index into that list and
  updates the library selection (grid + inspector) as it moves.
- Details panel: closed on the first open. Toggle with the top-bar text
  button. The panel width animates over 300 ms; the image area expands to
  fill the space. Later opens remember the last toggle for the session.
- Close: "閉じる Esc", `Esc`, or a click on the dark image-area background
  (not the image itself, not the panel). The image sits in an aspect-ratio
  box so the photo and the letterbox have distinct click targets.
- Focus: opening focuses the close button; `Tab`/`Shift+Tab` cycle the close
  button, previous, next, the details toggle, and, when the panel is open,
  favorite, tag input, add-tag, compress, and trash. Closing restores the previously
  focused element (the photo grid in normal use). The shell is inert while
  the viewer is open: pointer events hit the viewer, and global
  `Ctrl/Cmd+B`, `Ctrl/Cmd+K`, `Ctrl/Cmd+I` are ignored.
- The viewer stays open across metadata edits (favorite, tag) and closes
  after a successful trash.
- Compression keeps the viewer open; the panel shows the existing `message`
  string so the confirmation is visible.
- No `Screen::Detail` route. The full-screen viewer hosts the shared detail
  body; any future alternate detail surface can reuse it.

### 7.5 Other screens (PR2)

Compression, Search, Monitor, and Settings keep their current behavior and
layout structure. Apply the tokens, 8 px spacing, type scale, glass panels,
and text-button styling. The Search results use the same photo card in a
fixed-width layout. The Settings screen gains the "アニメーションを減らす"
switch.

## 8. Motion

- Easing: cubic Bezier `(0.22, 0.8, 0.24, 1)`, implemented as a small solver
  helper, not a new dependency.
- Sidebar: width 440 ms; labels/counts fade in/out over ~320 ms. An epoch is
  incremented on toggle so the animation re-triggers.
- Library inspector: column width 300 ms; content disappears at the toggle
  while the column collapses, and the grid reflows once from the target state.
- Full-screen details: panel width 300 ms; the image area expands/contracts
  with it. The top bar and panel use the same glass surface.
- Cards: first appearance 600 ms with a 40 ms per-card stagger, capped at
  8 steps (max ~320 ms delay). Only the first 24 cards of a grid generation
  animate; a `seen` set keyed by photo ID prevents replay when
  `uniform_list` remounts a row after scrolling. Cards fade in and move up
  14 px. Hover lift is 4 px over 180 ms; leaving reverses it. Use nested
  animations: outer intro (opacity/offset), inner hover (offset).
- Preview: selection cross-fades over 260 ms. Keep the previous preview for
  the duration; stack the old and new layers and animate opacity. The
  inspector tint is lerped from `previous_color` to the new average color
  over the same 260 ms.
- Full-screen viewer: the whole viewer fades in over 240 ms; the details
  panel width animates over 300 ms; a newly selected photo fades in over
  220 ms. No scale transform.
- Reduced motion: `with_animation` automatically renders the end state when
  `cx.reduce_motion()` is true. Non-animation transitions must also be
  short-circuited by checking the setting.
- Performance: cap animations at 60 fps with `with_max_fps(60.0)` where
  useful. Do not trigger image loads or DB queries from animation closures.

## 9. State and persistence

New `AppSettings` fields (all `#[serde(default)]`):

- `ui_sidebar_collapsed: bool` (default false)
- `ui_inspector_collapsed: bool` (default false = visible)
- `ui_reduce_motion: bool` (default false)

New `App` state:

- `sidebar_collapsed`, `sidebar_motion_epoch`
- `inspector_collapsed`, `inspector_epoch`
- `hovered_nav: Option<Screen>`, `hovered_photo: Option<i64>`
- `grid_columns: usize` (updated from the window width during render)
- `grid_epoch: u64` (incremented when the photo set is reloaded)
- `thumbnail_colors: HashMap<i64, u32>`
- `selection_epoch`, `previous_preview: Option<Arc<RenderImage>>`,
  `previous_color: Option<u32>`
- `quick_look_open`, `quick_look_epoch`, `quick_look_return_focus:
  Option<FocusHandle>`, `quick_look_focus_index`
- `quick_look_details_open`, `quick_look_details_epoch`
- `quick_look_photo_ids: Vec<i64>`, `quick_look_index: usize`
- `intro_seen: HashMap<i64, Instant>` (first-seen time per photo; cleared on a new grid generation so the intro can replay for a new set)
- `daemon_age_secs: Option<i64>` (derived from `daemon_heartbeat`)
- `demo_assets: Option<DemoAssets>` (photo dir + thumbnail dir)
- Focus handles: `photo_grid_focus`, `quick_look_close_focus`,
  `quick_look_prev_focus`, `quick_look_next_focus`,
  `quick_look_details_focus`, `quick_look_favorite_focus`,
  `quick_look_compress_focus`, `quick_look_trash_focus`,
  `quick_look_add_tag_focus`

The header subtitle uses `daemon_age_secs`: "最終同期 N秒前" when the daemon
is active, "デーモン停止" otherwise, "デモデータ" in demo mode. No new DB
field is added.

### 9.1 Demo and QA data

The current `--demo` seeds `/demo/photos/...` rows that do not exist and uses
an in-memory DB. Extend it so visual QA can exercise the real rendering path:

- Generate 18 synthetic 1280×800 PNGs (vertical gradients, a sun disc, a
  simple silhouette) once into a temp demo directory. Each photo's world
  palette matches the mock's scene colors.
- Generate matching 256 px PNG thumbnails into a demo-specific thumbnail
  directory. Do not write into the user's real thumbnail cache; demo photo
  IDs must not collide with real library IDs.
- Use a stable temp file DB (`vrc-photo-manager-demo.db`) so sidebar/reduced
  motion settings survive a restart. Seed only when the DB has no photos.
- Add `--demo-reset` to delete the demo DB and asset directory before
  starting.
- Add `--window-size WxH` (e.g. `1440x900`) for repeatable screenshots; the
  default remains 1280×820. Set `window_min_size` to 1080×700.
- Keep the existing `--demo` seed data (worlds, players, tags, favorites,
  derivatives) and point it at the generated files.
- Do not change normal (non-demo) data paths or thumbnail cache behavior.

Keyboard bindings (registered in `main()` with `cx.bind_keys`):

| Binding | Context | Action |
|---|---|---|
| `ctrl-b` / `cmd-b` | global | Toggle sidebar |
| `ctrl-i` / `cmd-i` | global | Toggle library inspector |
| `ctrl-k` / `cmd-k` | global | Go to Search, focus world input |
| `space` / `enter` | `photo_grid` | Open full-screen viewer |
| `escape` | `quick_look` | Close full-screen viewer |
| `left` / `right` | `viewer_nav` | Previous / next photo |
| `tab` / `shift-tab` | `quick_look` | Cycle viewer focus |

The photo grid is a single focus stop. Clicking a card focuses it, so
Space/Enter operate on the selected photo. Typing in an input never reaches
the grid bindings.

## 10. Verification

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Unit tests for: easing endpoints/monotonicity, color averaging, color
  interpolation, grid columns at 1280/1440 with the sidebar and inspector
  shown/hidden, viewer-details width interpolation, compact count, stagger
  boundaries, focus wrap, window-size parsing, and old-settings
  deserialization.
- Manual Linux demo (`--demo`) at 1280×820 and 1440×900, screenshots attached
  to the PR:
  - sidebar toggle, persistence after restart, tooltips
  - inspector show/hide, persistence, and grid columns with it hidden
  - grid columns at both sizes and in rail mode
  - selection cross-fade and inspector background color
  - full-screen viewer open/close paths, details toggle, previous/next at
    the ends, focus return, tag/favorite/compress/trash
  - reduced-motion switch
  - search/compression/monitor/settings behavior unchanged
- Windows visual verification is tracked in #9.

## 11. Open items

- `docs/ui.md` must be updated in a separate docs-only PR (project rule: do
  not mix code and docs in one PR). The mock file may be added to `main` in
  the same docs PR.
